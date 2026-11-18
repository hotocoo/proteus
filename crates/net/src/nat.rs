//! The NAT itself: one thread that owns the guest link, a TCP/IP stack facing the guest, and the host
//! sockets that stand in for the guest on the real network.
//!
//! TCP: the guest's handshake is completed locally by the stack. The host connection is opened only
//! after the policy has seen the destination and, for TLS, the server name, so a rejected flow never
//! produces a packet on the host's network.
//! UDP: each guest source port gets one host socket; datagrams are forwarded both ways.

use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, ToSocketAddrs};
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use devices::virtio::DeviceError;
use devices::virtio::net::{MAX_FRAME_LEN, NetBackend, NetLink};
use mio::event::Event;
use mio::net::{TcpListener, TcpStream, UdpSocket};
use mio::{Events, Interest, Poll, Token, Waker};
use smoltcp::iface::{Config as StackConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as StackInstant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint,
};

use crate::config::{ADB_PORT_ATTEMPTS, NetConfig};
use crate::dns::{self, Answer};
use crate::frames::{self, Ingress, UdpDatagram};
use crate::policy::{FlowPolicy, Proto, Verdict};
use crate::sni::{self, Sni};
use crate::{copy, dhcp};

const WAKE: Token = Token(0);
const ADB: Token = Token(1);
const FIRST_DYNAMIC_TOKEN: usize = 2;
const EVENT_CAPACITY: usize = 256;
const DNS_PORT: u16 = 53;
const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
/// Largest UDP payload a host socket can hand back.
const MAX_DATAGRAM_LEN: usize = 65_535;
/// Source ports for connections the NAT opens toward the guest: the IANA dynamic range.
const EPHEMERAL_PORTS: RangeInclusive<u16> = 49_152..=65_535;

/// Turns a host name into addresses. The default asks the host's own resolver.
pub trait Resolver: Send + Sync {
    fn resolve(&self, name: &str) -> io::Result<Vec<Ipv4Addr>>;
}

/// Uses the operating system's resolver, so the host's VPN, split DNS and hosts file all apply.
pub struct HostResolver;

impl Resolver for HostResolver {
    fn resolve(&self, name: &str) -> io::Result<Vec<Ipv4Addr>> {
        let found = (name, 0).to_socket_addrs()?;
        Ok(found
            .filter_map(|addr| match addr.ip() {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(_) => None,
            })
            .collect())
    }
}

/// The frame queues between the guest link and the stack.
struct Wire {
    to_stack: VecDeque<Vec<u8>>,
    to_guest: VecDeque<Vec<u8>>,
    max_queued: usize,
}

struct WireRx(Vec<u8>);

impl RxToken for WireRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

struct WireTx<'a>(&'a mut VecDeque<Vec<u8>>);

impl TxToken for WireTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0u8; len];
        let result = f(&mut frame);
        self.0.push_back(frame);
        result
    }
}

impl Device for Wire {
    type RxToken<'a> = WireRx;
    type TxToken<'a> = WireTx<'a>;

    fn receive(&mut self, _now: StackInstant) -> Option<(WireRx, WireTx<'_>)> {
        // The stack may answer a received frame at once, so it only gets one while there is room.
        if self.to_guest.len() >= self.max_queued {
            return None;
        }
        let frame = self.to_stack.pop_front()?;
        Some((WireRx(frame), WireTx(&mut self.to_guest)))
    }

    fn transmit(&mut self, _now: StackInstant) -> Option<WireTx<'_>> {
        (self.to_guest.len() < self.max_queued).then_some(WireTx(&mut self.to_guest))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = MAX_FRAME_LEN;
        capabilities
    }
}

enum Phase {
    /// The guest is connected to the stack; its first bytes decide whether a host connection is made.
    Sniffing {
        deadline: Instant,
    },
    /// The host connection is being established.
    Connecting,
    Relaying,
}

struct TcpFlow {
    handle: SocketHandle,
    /// Where the guest is connecting to (for an adb flow, the guest's own endpoint).
    dst: SocketAddrV4,
    host: Option<TcpStream>,
    phase: Phase,
    host_readable: bool,
    host_writable: bool,
    /// The host finished sending and that end-of-stream was passed on to the guest.
    host_eof: bool,
    /// The guest finished sending and that end-of-stream was passed on to the host.
    guest_eof: bool,
}

/// A connection the guest has started but not finished opening.
struct Opening {
    handle: SocketHandle,
    key: (SocketAddrV4, SocketAddrV4),
    deadline: Instant,
}

struct UdpPeer {
    socket: UdpSocket,
    guest_port: u16,
    last_used: Instant,
}

struct Lookup {
    query: dns::Query,
    guest_port: u16,
}

struct Resolved {
    lookup: Lookup,
    result: io::Result<Vec<Ipv4Addr>>,
}

fn v4(endpoint: IpEndpoint) -> SocketAddrV4 {
    let IpAddress::Ipv4(ip) = endpoint.addr;
    SocketAddrV4::new(ip, endpoint.port)
}

struct Worker {
    link: NetLink,
    cfg: NetConfig,
    policy: Arc<dyn FlowPolicy>,
    stop: Arc<AtomicBool>,
    poll: Poll,
    stack: Interface,
    sockets: SocketSet<'static>,
    wire: Wire,
    flows: HashMap<Token, TcpFlow>,
    opening: Vec<Opening>,
    /// Sockets that were reset; they stay one more turn so the stack can send the reset.
    graveyard: Vec<SocketHandle>,
    udp: HashMap<Token, UdpPeer>,
    udp_by_guest_port: HashMap<u16, Token>,
    lookups: Sender<Lookup>,
    resolved: Receiver<Resolved>,
    adb: Option<TcpListener>,
    next_token: usize,
    next_ephemeral: u16,
    /// Reused buffers for one guest frame and one host datagram.
    frame: Vec<u8>,
    datagram: Vec<u8>,
}

fn spawn_resolvers(
    resolver: &Arc<dyn Resolver>,
    threads: usize,
    waker: &Arc<Waker>,
) -> io::Result<(Sender<Lookup>, Receiver<Resolved>)> {
    let (lookups, pending) = mpsc::channel::<Lookup>();
    let (done, resolved) = mpsc::channel();
    let pending = Arc::new(Mutex::new(pending));
    for _ in 0..threads {
        let (resolver, pending, done, waker) = (
            Arc::clone(resolver),
            Arc::clone(&pending),
            done.clone(),
            Arc::clone(waker),
        );
        let serve = move || {
            loop {
                // Hold the lock only while waiting for a job, not while resolving it.
                let job = pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .recv();
                let Ok(lookup) = job else {
                    return;
                };
                let result = resolver.resolve(&lookup.query.name);
                if done.send(Resolved { lookup, result }).is_err() || waker.wake().is_err() {
                    return;
                }
            }
        };
        thread::Builder::new()
            .name(copy::RESOLVER_THREAD.into())
            .spawn(serve)?;
    }
    Ok((lookups, resolved))
}

/// Binds the loopback port `adb` connects to, moving up a little if the wanted one is taken.
fn bind_adb(wanted: u16) -> Option<TcpListener> {
    let found = (wanted..=wanted.saturating_add(ADB_PORT_ATTEMPTS)).find_map(|port| {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).ok()?;
        Some((port, listener))
    });
    match found {
        Some((port, listener)) if port == wanted => {
            log::info!("{}", copy::adb_listening(port));
            Some(listener)
        }
        Some((port, listener)) => {
            log::warn!("{}", copy::adb_port_moved(wanted, port));
            Some(listener)
        }
        None => {
            log::warn!("{}", copy::adb_unavailable(wanted));
            None
        }
    }
}

impl Worker {
    fn new(
        link: NetLink,
        cfg: NetConfig,
        policy: Arc<dyn FlowPolicy>,
        resolver: &Arc<dyn Resolver>,
        poll: Poll,
        waker: &Arc<Waker>,
        stop: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let mut wire = Wire {
            to_stack: VecDeque::new(),
            to_guest: VecDeque::new(),
            max_queued: cfg.max_queued_frames,
        };
        let mut stack_config =
            StackConfig::new(HardwareAddress::Ethernet(EthernetAddress(cfg.gateway_mac)));
        // Only seeds the stack's initial sequence numbers toward the guest.
        stack_config.random_seed = RandomState::new().hash_one(cfg.gateway_mac);
        let mut stack = Interface::new(stack_config, &mut wire, StackInstant::now());
        let own = IpCidr::new(IpAddress::Ipv4(cfg.gateway_ip), cfg.prefix_len);
        stack.update_ip_addrs(|addrs| {
            // One address always fits.
            let _ = addrs.push(own);
        });
        // With a default route through itself, the stack accepts packets for every destination.
        stack.set_any_ip(true);
        stack
            .routes_mut()
            .add_default_ipv4_route(cfg.gateway_ip)
            .map_err(|_| io::Error::other(copy::ROUTE_TABLE_FULL))?;

        let (lookups, resolved) = spawn_resolvers(resolver, cfg.resolver_threads, waker)?;
        let mut adb = cfg.adb.and_then(|forward| bind_adb(forward.host_port));
        if let Some(listener) = &mut adb {
            poll.registry()
                .register(listener, ADB, Interest::READABLE)?;
        }
        Ok(Self {
            link,
            policy,
            stop,
            poll,
            stack,
            sockets: SocketSet::new(Vec::new()),
            wire,
            flows: HashMap::new(),
            opening: Vec::new(),
            graveyard: Vec::new(),
            udp: HashMap::new(),
            udp_by_guest_port: HashMap::new(),
            lookups,
            resolved,
            adb,
            next_token: FIRST_DYNAMIC_TOKEN,
            next_ephemeral: *EPHEMERAL_PORTS.start(),
            frame: Vec::with_capacity(MAX_FRAME_LEN),
            datagram: vec![0u8; MAX_DATAGRAM_LEN],
            cfg,
        })
    }

    fn token(&mut self) -> Token {
        let token = Token(self.next_token);
        self.next_token += 1;
        token
    }

    fn ephemeral_port(&mut self) -> u16 {
        let port = self.next_ephemeral;
        self.next_ephemeral = if port == *EPHEMERAL_PORTS.end() {
            *EPHEMERAL_PORTS.start()
        } else {
            port + 1
        };
        port
    }

    fn new_socket(&self) -> tcp::Socket<'static> {
        let buffer = || tcp::SocketBuffer::new(vec![0u8; self.cfg.tcp_buffer_len]);
        tcp::Socket::new(buffer(), buffer())
    }

    /// The host address that stands for `dst`. Only differs for the gateway when it aliases loopback.
    fn host_addr(&self, dst: SocketAddrV4) -> SocketAddr {
        let aliased = self.cfg.host_loopback && *dst.ip() == self.cfg.gateway_ip;
        SocketAddr::from((
            if aliased {
                Ipv4Addr::LOCALHOST
            } else {
                *dst.ip()
            },
            dst.port(),
        ))
    }

    /// The guest may only reach the gateway address itself when that is switched on.
    fn reachable(&self, dst: SocketAddrV4) -> bool {
        let ip = dst.ip();
        !(ip.is_broadcast() || ip.is_multicast() || ip.is_unspecified())
            && (self.cfg.host_loopback || *ip != self.cfg.gateway_ip)
    }

    fn queue_to_guest(&mut self, frame: Vec<u8>) {
        if self.wire.to_guest.len() < self.cfg.max_queued_frames {
            self.wire.to_guest.push_back(frame);
        }
    }

    fn udp_to_guest(&mut self, src: SocketAddrV4, guest_port: u16, payload: &[u8]) {
        let dst = SocketAddrV4::new(self.cfg.guest_ip, guest_port);
        if let Some(frame) =
            frames::udp_frame(self.cfg.gateway_mac, self.cfg.guest_mac, src, dst, payload)
        {
            self.queue_to_guest(frame);
        }
    }

    // ---- guest to host ----

    fn ingress(&mut self) {
        let mut frame = std::mem::take(&mut self.frame);
        while self.link.next_from_guest(&mut frame) {
            match frames::classify(&frame) {
                Ingress::Udp(datagram) => self.udp_from_guest(&datagram),
                Ingress::TcpSyn { src, dst } => {
                    self.expect_connection(src, dst);
                    self.queue_to_stack(&frame);
                }
                Ingress::Stack => self.queue_to_stack(&frame),
                Ingress::Drop => {}
            }
        }
        self.frame = frame;
    }

    fn queue_to_stack(&mut self, frame: &[u8]) {
        if self.wire.to_stack.len() < self.cfg.max_queued_frames {
            self.wire.to_stack.push_back(frame.to_vec());
        }
    }

    /// Gives the stack a socket to accept the guest's connection with. Without one the stack answers
    /// the SYN with a reset, which is how refused destinations are refused.
    fn expect_connection(&mut self, src: SocketAddrV4, dst: SocketAddrV4) {
        let key = (src, dst);
        let known = self.opening.iter().any(|opening| opening.key == key);
        let full = self.flows.len() + self.opening.len() >= self.cfg.max_tcp_flows;
        if known || full || *src.ip() != self.cfg.guest_ip || !self.reachable(dst) {
            return;
        }
        if self.policy.connect(dst, Proto::Tcp) == Verdict::Sinkhole {
            log::info!("{}", copy::sinkholed(copy::BLOCKED_CONNECT, &dst));
            return;
        }
        let mut socket = self.new_socket();
        let endpoint = IpListenEndpoint {
            addr: Some(IpAddress::Ipv4(*dst.ip())),
            port: dst.port(),
        };
        if socket.listen(endpoint).is_ok() {
            let handle = self.sockets.add(socket);
            let deadline = Instant::now() + self.cfg.handshake_timeout;
            self.opening.push(Opening {
                handle,
                key,
                deadline,
            });
        }
    }

    fn udp_from_guest(&mut self, datagram: &UdpDatagram<'_>) {
        let gateway = self.cfg.gateway_ip;
        if datagram.dst.port() == DHCP_SERVER_PORT {
            if let Some(reply) = dhcp::reply(datagram.payload, &self.cfg) {
                let src = SocketAddrV4::new(gateway, DHCP_SERVER_PORT);
                let dst = SocketAddrV4::new(frames::BROADCAST_IP, DHCP_CLIENT_PORT);
                let frame = frames::udp_frame(
                    self.cfg.gateway_mac,
                    frames::BROADCAST_MAC,
                    src,
                    dst,
                    &reply,
                );
                frame
                    .into_iter()
                    .for_each(|frame| self.queue_to_guest(frame));
            }
            return;
        }
        if *datagram.src.ip() != self.cfg.guest_ip {
            return;
        }
        if datagram.dst == SocketAddrV4::new(gateway, DNS_PORT) {
            self.dns_query(datagram.src.port(), datagram.payload);
            return;
        }
        if !self.reachable(datagram.dst)
            || self.policy.connect(datagram.dst, Proto::Udp) == Verdict::Sinkhole
        {
            return;
        }
        let guest_port = datagram.src.port();
        let token = match self.udp_by_guest_port.get(&guest_port) {
            Some(token) => *token,
            None => match self.open_udp_peer(guest_port) {
                Some(token) => token,
                None => return,
            },
        };
        let host_dst = self.host_addr(datagram.dst);
        if let Some(peer) = self.udp.get_mut(&token) {
            peer.last_used = Instant::now();
            // A datagram the host cannot send right now is lost, as UDP allows.
            let _ = peer.socket.send_to(datagram.payload, host_dst);
        }
    }

    fn open_udp_peer(&mut self, guest_port: u16) -> Option<Token> {
        if self.udp.len() >= self.cfg.max_udp_peers {
            return None;
        }
        let mut socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).ok()?;
        let token = self.token();
        self.poll
            .registry()
            .register(&mut socket, token, Interest::READABLE)
            .ok()?;
        self.udp.insert(
            token,
            UdpPeer {
                socket,
                guest_port,
                last_used: Instant::now(),
            },
        );
        self.udp_by_guest_port.insert(guest_port, token);
        Some(token)
    }

    fn dns_query(&mut self, guest_port: u16, payload: &[u8]) {
        let Some(query) = dns::parse_query(payload) else {
            return;
        };
        if self.policy.dns(&query.name) == Verdict::Sinkhole {
            log::info!("{}", copy::sinkholed(copy::BLOCKED_DNS, &query.name));
            self.dns_answer(&query, guest_port, Answer::NameError);
        } else if query.qtype != dns::TYPE_A || query.qclass != dns::CLASS_IN {
            // Only IPv4 addresses are served; the guest network has no IPv6, and leaving HTTPS records
            // unanswered keeps Encrypted Client Hello keys from reaching the guest.
            self.dns_answer(&query, guest_port, Answer::NoData);
        } else if let Err(unsent) = self.lookups.send(Lookup { query, guest_port }) {
            self.dns_answer(&unsent.0.query, guest_port, Answer::ServerFailure);
        }
    }

    fn dns_answer(&mut self, query: &dns::Query, guest_port: u16, answer: Answer<'_>) {
        let response = dns::build_response(query, answer, self.cfg.dns_ttl_secs);
        let src = SocketAddrV4::new(self.cfg.gateway_ip, DNS_PORT);
        self.udp_to_guest(src, guest_port, &response);
    }

    fn finish_lookups(&mut self) {
        while let Ok(done) = self.resolved.try_recv() {
            let answer = match &done.result {
                Ok(addresses) if addresses.is_empty() => Answer::NoData,
                Ok(addresses) => Answer::Addresses(addresses),
                // The host resolver does not say why it failed; an unknown name is by far the usual reason.
                Err(_) => Answer::NameError,
            };
            self.dns_answer(&done.lookup.query, done.lookup.guest_port, answer);
        }
    }

    // ---- host to guest ----

    fn udp_from_host(&mut self, token: Token) {
        let mut buffer = std::mem::take(&mut self.datagram);
        while let Some(peer) = self.udp.get_mut(&token) {
            let Ok((len, SocketAddr::V4(mut from))) = peer.socket.recv_from(&mut buffer) else {
                break;
            };
            peer.last_used = Instant::now();
            let guest_port = peer.guest_port;
            if self.cfg.host_loopback && from.ip().is_loopback() {
                from.set_ip(self.cfg.gateway_ip);
            }
            self.udp_to_guest(from, guest_port, &buffer[..len]);
        }
        self.datagram = buffer;
    }

    fn expire_udp(&mut self, now: Instant) {
        let timeout = self.cfg.udp_idle_timeout;
        let idle: Vec<Token> = self
            .udp
            .iter()
            .filter(|(_, peer)| now.duration_since(peer.last_used) >= timeout)
            .map(|(token, _)| *token)
            .collect();
        for token in idle {
            if let Some(mut peer) = self.udp.remove(&token) {
                self.udp_by_guest_port.remove(&peer.guest_port);
                let _ = self.poll.registry().deregister(&mut peer.socket);
            }
        }
    }

    fn accept_adb(&mut self) {
        let Some(forward) = self.cfg.adb else {
            return;
        };
        while let Some(Ok((mut stream, _))) = self.adb.as_ref().map(TcpListener::accept) {
            let token = self.token();
            let interest = Interest::READABLE | Interest::WRITABLE;
            if self
                .poll
                .registry()
                .register(&mut stream, token, interest)
                .is_err()
            {
                continue;
            }
            let guest = SocketAddrV4::new(self.cfg.guest_ip, forward.guest_port);
            let local = IpListenEndpoint {
                addr: Some(IpAddress::Ipv4(self.cfg.gateway_ip)),
                port: self.ephemeral_port(),
            };
            let mut socket = self.new_socket();
            let remote = IpEndpoint::new(IpAddress::Ipv4(*guest.ip()), guest.port());
            if socket.connect(self.stack.context(), remote, local).is_err() {
                let _ = self.poll.registry().deregister(&mut stream);
                continue;
            }
            let handle = self.sockets.add(socket);
            self.flows.insert(
                token,
                TcpFlow {
                    handle,
                    dst: guest,
                    host: Some(stream),
                    phase: Phase::Relaying,
                    host_readable: false,
                    host_writable: false,
                    host_eof: false,
                    guest_eof: false,
                },
            );
        }
    }

    // ---- TCP flows ----

    /// Moves connections whose handshake finished into `flows`, and drops the ones that never did.
    fn settle_opening(&mut self, now: Instant) {
        let mut index = 0;
        while index < self.opening.len() {
            let opening = &self.opening[index];
            let socket = self.sockets.get::<tcp::Socket>(opening.handle);
            let handshaking =
                matches!(socket.state(), tcp::State::Listen | tcp::State::SynReceived);
            if handshaking && now < opening.deadline {
                index += 1;
                continue;
            }
            let opening = self.opening.swap_remove(index);
            // The socket knows the destination better than the SYN that prompted it: two connections
            // to one destination can be accepted by each other's sockets.
            match (handshaking, socket.local_endpoint()) {
                (false, Some(local)) if socket.state() != tcp::State::Closed => {
                    let token = self.token();
                    let deadline = now + self.cfg.sniff_timeout;
                    self.flows.insert(
                        token,
                        TcpFlow {
                            handle: opening.handle,
                            dst: v4(local),
                            host: None,
                            phase: Phase::Sniffing { deadline },
                            host_readable: false,
                            host_writable: false,
                            host_eof: false,
                            guest_eof: false,
                        },
                    );
                }
                _ => {
                    self.sockets.get_mut::<tcp::Socket>(opening.handle).abort();
                    self.graveyard.push(opening.handle);
                }
            }
        }
    }

    fn pump_flows(&mut self, now: Instant) {
        let tokens: Vec<Token> = self.flows.keys().copied().collect();
        for token in tokens {
            if self.pump(token, now) {
                continue;
            }
            if let Some(mut flow) = self.flows.remove(&token) {
                if let Some(host) = &mut flow.host {
                    let _ = self.poll.registry().deregister(host);
                }
                self.graveyard.push(flow.handle);
            }
        }
    }

    /// Advances one flow. Returns false when the flow is over; a flow that failed has been reset.
    fn pump(&mut self, token: Token, now: Instant) -> bool {
        let Some(flow) = self.flows.get_mut(&token) else {
            return false;
        };
        let socket = self.sockets.get_mut::<tcp::Socket>(flow.handle);
        if socket.state() == tcp::State::Closed {
            return false;
        }
        if let Phase::Sniffing { deadline } = flow.phase {
            let guest_may_send_more = socket.may_recv();
            let seen = socket.peek(sni::MAX_CLIENT_HELLO).unwrap_or_default();
            let (verdict, name) = match sni::client_hello_sni(seen) {
                Sni::Name(name) => (
                    self.policy.tls_sni(flow.dst, Some(name)),
                    Some(name.to_owned()),
                ),
                Sni::Incomplete if now < deadline && guest_may_send_more => return true,
                Sni::Incomplete | Sni::Absent | Sni::NotClientHello => {
                    (self.policy.tls_sni(flow.dst, None), None)
                }
            };
            if verdict == Verdict::Sinkhole {
                log::info!(
                    "{}",
                    copy::sinkholed(
                        copy::BLOCKED_TLS,
                        &name.unwrap_or_else(|| flow.dst.to_string())
                    )
                );
                socket.abort();
                return false;
            }
            let aliased = self.cfg.host_loopback && *flow.dst.ip() == self.cfg.gateway_ip;
            let ip = if aliased {
                Ipv4Addr::LOCALHOST
            } else {
                *flow.dst.ip()
            };
            let target = SocketAddr::from((ip, flow.dst.port()));
            let interest = Interest::READABLE | Interest::WRITABLE;
            let connected = TcpStream::connect(target).and_then(|mut host| {
                self.poll
                    .registry()
                    .register(&mut host, token, interest)
                    .map(|()| host)
            });
            match connected {
                Ok(host) => {
                    flow.host = Some(host);
                    flow.phase = Phase::Connecting;
                }
                Err(err) => {
                    log::debug!("{}", copy::host_connect_failed(&flow.dst, &err));
                    socket.abort();
                    return false;
                }
            }
        }
        let Some(host) = flow.host.as_mut() else {
            socket.abort();
            return false;
        };
        if matches!(flow.phase, Phase::Connecting) {
            if !flow.host_writable {
                return true;
            }
            // Writable after a non-blocking connect means it finished, one way or the other.
            match host
                .take_error()
                .and_then(|pending| pending.map_or(Ok(()), Err))
                .and_then(|()| host.peer_addr())
            {
                Ok(_) => {
                    let _ = host.set_nodelay(true);
                    flow.phase = Phase::Relaying;
                }
                Err(err) if err.kind() == ErrorKind::NotConnected => {
                    flow.host_writable = false;
                    return true;
                }
                Err(err) => {
                    log::debug!("{}", copy::host_connect_failed(&flow.dst, &err));
                    socket.abort();
                    return false;
                }
            }
        }

        let mut failed = false;
        // Guest to host: hand the stack's received bytes to the host socket until either runs dry.
        while flow.host_writable && socket.can_recv() && !failed {
            let moved = socket.recv(|data| match host.write(data) {
                Ok(written) => (written, written),
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    flow.host_writable = false;
                    (0, 0)
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => (0, 1),
                Err(_) => {
                    failed = true;
                    (0, 0)
                }
            });
            if !matches!(moved, Ok(1..)) {
                break;
            }
        }
        // Host to guest: read straight into the stack's send buffer while it has room.
        while flow.host_readable && !flow.host_eof && socket.can_send() && !failed {
            let moved = socket.send(|space| match host.read(space) {
                Ok(0) => {
                    flow.host_eof = true;
                    (0, 0)
                }
                Ok(read) => (read, read),
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    flow.host_readable = false;
                    (0, 0)
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => (0, 1),
                Err(_) => {
                    failed = true;
                    (0, 0)
                }
            });
            if !matches!(moved, Ok(1..)) {
                break;
            }
        }
        if failed {
            socket.abort();
            return false;
        }
        if flow.host_eof && socket.may_send() {
            socket.close();
        }
        // States in which the guest has sent its FIN and everything before it has been received.
        let guest_done = matches!(
            socket.state(),
            tcp::State::CloseWait
                | tcp::State::LastAck
                | tcp::State::Closing
                | tcp::State::TimeWait
        );
        if guest_done && !flow.guest_eof && !socket.can_recv() {
            flow.guest_eof = true;
            let _ = host.shutdown(Shutdown::Write);
        }
        true
    }

    // ---- the loop ----

    fn poll_stack(&mut self) {
        self.stack
            .poll(StackInstant::now(), &mut self.wire, &mut self.sockets);
    }

    fn egress(&mut self) {
        while let Some(frame) = self.wire.to_guest.front() {
            if !self.link.send_to_guest(frame) {
                break;
            }
            self.wire.to_guest.pop_front();
        }
        self.link.flush();
    }

    fn on_event(&mut self, event: &Event) {
        let token = event.token();
        if token == ADB {
            self.accept_adb();
        } else if let Some(flow) = self.flows.get_mut(&token) {
            let broken = event.is_error();
            flow.host_readable |= event.is_readable() || event.is_read_closed() || broken;
            flow.host_writable |= event.is_writable() || event.is_write_closed() || broken;
        } else if self.udp.contains_key(&token) {
            self.udp_from_host(token);
        }
    }

    fn turn(&mut self) {
        let now = Instant::now();
        for handle in self.graveyard.drain(..) {
            self.sockets.remove(handle);
        }
        self.ingress();
        self.finish_lookups();
        self.poll_stack();
        self.settle_opening(now);
        self.pump_flows(now);
        // Send what the pumps queued, and the resets of flows that just ended.
        self.poll_stack();
        self.expire_udp(now);
        self.egress();
    }

    /// How long the loop may sleep if nothing wakes it.
    fn sleep_for(&mut self) -> Option<Duration> {
        let now = Instant::now();
        let stack = self
            .stack
            .poll_delay(StackInstant::now(), &self.sockets)
            .map(|delay| Duration::from_micros(delay.total_micros()));
        let sniffing = self.flows.values().filter_map(|flow| match flow.phase {
            Phase::Sniffing { deadline } => Some(deadline),
            Phase::Connecting | Phase::Relaying => None,
        });
        let opening = self.opening.iter().map(|opening| opening.deadline);
        let udp = self
            .udp
            .values()
            .map(|peer| peer.last_used + self.cfg.udp_idle_timeout);
        let deadline = sniffing
            .chain(opening)
            .chain(udp)
            .min()
            .map(|at| at.saturating_duration_since(now));
        [stack, deadline].into_iter().flatten().min()
    }

    fn run(mut self) -> NetLink {
        let mut events = Events::with_capacity(EVENT_CAPACITY);
        while !self.stop.load(Ordering::SeqCst) {
            let timeout = self.sleep_for();
            match self.poll.poll(&mut events, timeout) {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) => {
                    log::error!("{}", copy::poll_failed(&err));
                    break;
                }
            }
            for event in &events {
                self.on_event(event);
            }
            self.turn();
        }
        self.link
    }
}

struct Running {
    waker: Arc<Waker>,
    stop: Arc<AtomicBool>,
    worker: JoinHandle<NetLink>,
}

/// The network as the virtio-net device sees it.
pub struct Network {
    cfg: NetConfig,
    policy: Arc<dyn FlowPolicy>,
    resolver: Arc<dyn Resolver>,
    running: Option<Running>,
}

impl Network {
    pub fn new(cfg: NetConfig, policy: Arc<dyn FlowPolicy>, resolver: Arc<dyn Resolver>) -> Self {
        Self {
            cfg,
            policy,
            resolver,
            running: None,
        }
    }

    fn launch(&self, link: NetLink) -> io::Result<Running> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
        let stop = Arc::new(AtomicBool::new(false));
        let policy = Arc::clone(&self.policy);
        let worker = Worker::new(
            link,
            self.cfg.clone(),
            policy,
            &self.resolver,
            poll,
            &waker,
            Arc::clone(&stop),
        )?;
        let worker = thread::Builder::new()
            .name(copy::WORKER_THREAD.into())
            .spawn(move || worker.run())?;
        Ok(Running {
            waker,
            stop,
            worker,
        })
    }
}

impl NetBackend for Network {
    fn start(&mut self, link: NetLink) -> Result<(), DeviceError> {
        if self.running.is_some() {
            return Err(DeviceError::AlreadyActive);
        }
        let running = self
            .launch(link)
            .map_err(|err| DeviceError::Backend(copy::worker_start_failed(&err)))?;
        self.running = Some(running);
        Ok(())
    }

    fn notify(&mut self) {
        if let Some(running) = &self.running {
            let _ = running.waker.wake();
        }
    }

    fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop.store(true, Ordering::SeqCst);
            let _ = running.waker.wake();
            if running.worker.join().is_err() {
                log::error!("{}", copy::WORKER_PANICKED);
            }
        }
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        self.stop();
    }
}
