//! Addresses, limits and timings of the guest network. Code reads them from here by name.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::AtomicU16;
use std::time::Duration;

use devices::virtio::net::MAC_LEN;

/// The guest's address and its gateway, in the 10.0.2.0/24 layout emulators conventionally use.
const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const PREFIX_LEN: u8 = 24;
/// Locally administered unicast addresses; they never leave the virtual link.
const GUEST_MAC: [u8; MAC_LEN] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const GATEWAY_MAC: [u8; MAC_LEN] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x02];

/// Where `adb` connects on the host, and the port `adbd` listens on in the guest.
pub const ADB_PORT: u16 = 5555;
/// How many ports above the requested one are tried when it is taken.
pub const ADB_PORT_ATTEMPTS: u16 = 16;

/// How long a new TCP flow may stay silent before it is judged without a server name.
/// TLS clients speak at once and never wait this long; protocols where the server speaks first do.
const SNIFF_TIMEOUT: Duration = Duration::from_millis(250);
/// How long the guest has to finish a TCP handshake it started.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A UDP port mapping unused for this long is released.
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const DHCP_LEASE_SECS: u32 = 24 * 60 * 60;
/// Lifetime of DNS answers handed to the guest. Short, because the host resolver's own TTLs are not visible.
const DNS_TTL_SECS: u32 = 60;

/// Send and receive buffer of each relayed TCP flow, in bytes. It must hold a whole TLS ClientHello.
const TCP_BUFFER_LEN: usize = 64 * 1024;
/// Limits that bound host memory no matter what the guest does.
const MAX_TCP_FLOWS: usize = 1024;
const MAX_UDP_PEERS: usize = 1024;
const MAX_QUEUED_FRAMES: usize = 1024;
/// Concurrent host name lookups.
const RESOLVER_THREADS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdbForward {
    /// Loopback port on the host.
    pub host_port: u16,
    pub guest_port: u16,
}

#[derive(Clone, Debug)]
pub struct NetConfig {
    pub guest_ip: Ipv4Addr,
    pub gateway_ip: Ipv4Addr,
    pub prefix_len: u8,
    pub guest_mac: [u8; MAC_LEN],
    pub gateway_mac: [u8; MAC_LEN],
    pub adb: Option<AdbForward>,
    /// Where the `adb` listener ended up, once it is bound (it may move off a taken port); 0 until
    /// then, or if it could not be bound. Whoever made the config can read it.
    pub adb_bound: Arc<AtomicU16>,
    /// Make the gateway address reach the host's own loopback interface. Off by default: it would let
    /// any app in the guest talk to services that listen only on the host's localhost.
    pub host_loopback: bool,
    pub sniff_timeout: Duration,
    pub handshake_timeout: Duration,
    pub udp_idle_timeout: Duration,
    pub dhcp_lease_secs: u32,
    pub dns_ttl_secs: u32,
    pub tcp_buffer_len: usize,
    pub max_tcp_flows: usize,
    pub max_udp_peers: usize,
    pub max_queued_frames: usize,
    pub resolver_threads: usize,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            guest_ip: GUEST_IP,
            gateway_ip: GATEWAY_IP,
            prefix_len: PREFIX_LEN,
            guest_mac: GUEST_MAC,
            gateway_mac: GATEWAY_MAC,
            adb: Some(AdbForward {
                host_port: ADB_PORT,
                guest_port: ADB_PORT,
            }),
            adb_bound: Arc::default(),
            host_loopback: false,
            sniff_timeout: SNIFF_TIMEOUT,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            udp_idle_timeout: UDP_IDLE_TIMEOUT,
            dhcp_lease_secs: DHCP_LEASE_SECS,
            dns_ttl_secs: DNS_TTL_SECS,
            tcp_buffer_len: TCP_BUFFER_LEN,
            max_tcp_flows: MAX_TCP_FLOWS,
            max_udp_peers: MAX_UDP_PEERS,
            max_queued_frames: MAX_QUEUED_FRAMES,
            resolver_threads: RESOLVER_THREADS,
        }
    }
}
