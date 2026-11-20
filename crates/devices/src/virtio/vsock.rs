//! virtio-vsock: stream sockets between the guest and services inside this process. The guest
//! connects to the host (CID 2) on a port; a service registered for that port answers. The host
//! never connects to the guest.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;

use super::{Activate, DeviceError, VirtioDevice, VirtioInterrupt};
use crate::copy;

/// virtio's number for a socket device.
const VIRTIO_ID_VSOCK: u32 = 19;
/// The host's address.
pub const HOST_CID: u64 = 2;

const QUEUE_SIZES: [u16; 3] = [128, 128, 16];
const RX: usize = 0;
const TX: usize = 1;

/// Packet header: src_cid u64, dst_cid u64, src_port u32, dst_port u32, len u32, type u16,
/// op u16, flags u32, buf_alloc u32, fwd_cnt u32, all little-endian.
const HEADER_LEN: usize = 44;
const TYPE_STREAM: u16 = 1;
const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;

/// Bytes the host takes from one connection before the guest must wait; it reads everything at
/// once, so this only bounds a single burst.
const HOST_BUFFER: u32 = 256 << 10;
/// Most payload bytes put in one packet to the guest.
const MAX_PAYLOAD: usize = 64 << 10;
/// Most bytes one connection may have waiting for the guest; past it the guest is not reading
/// and the connection is reset.
const MAX_PENDING: usize = 4 << 20;

/// One accepted connection: bytes from the guest in, bytes for the guest out.
pub trait Connection: Send {
    /// The guest sent `bytes`; whatever is returned goes back to it.
    fn received(&mut self, bytes: &[u8]) -> Vec<u8>;
}

/// Makes a new connection for each guest that connects to its port.
pub type Service = Box<dyn Fn() -> Box<dyn Connection> + Send>;
/// The services by port, handed back when the worker stops.
type Services = HashMap<u32, Service>;
type Worker = (Sender<Wake>, JoinHandle<Services>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    kind: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl Header {
    fn parse(raw: &[u8; HEADER_LEN]) -> Self {
        let u64_at = |at: usize| u64::from_le_bytes(raw[at..at + 8].try_into().unwrap_or([0; 8]));
        let u32_at = |at: usize| u32::from_le_bytes(raw[at..at + 4].try_into().unwrap_or([0; 4]));
        let u16_at = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
        Self {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            kind: u16_at(28),
            op: u16_at(30),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        }
    }

    fn bytes(&self) -> [u8; HEADER_LEN] {
        let mut raw = [0u8; HEADER_LEN];
        raw[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        raw[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        raw[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        raw[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        raw[24..28].copy_from_slice(&self.len.to_le_bytes());
        raw[28..30].copy_from_slice(&self.kind.to_le_bytes());
        raw[30..32].copy_from_slice(&self.op.to_le_bytes());
        raw[32..36].copy_from_slice(&self.flags.to_le_bytes());
        raw[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        raw[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        raw
    }
}

/// A connection's two ends: (guest port, host port).
type Key = (u32, u32);

struct Conn {
    service: Box<dyn Connection>,
    /// Bytes taken from the guest so far: what the host has "forwarded".
    received: u32,
    /// `received` as last told to the guest.
    told: u32,
    /// Bytes sent to the guest so far.
    sent: u32,
    /// The guest's receive buffer and how much of what it was sent it has consumed.
    peer_buf: u32,
    peer_fwd: u32,
    pending: VecDeque<u8>,
}

impl Conn {
    /// Bytes the guest has room for now.
    fn credit(&self) -> usize {
        let in_flight = self.sent.wrapping_sub(self.peer_fwd);
        self.peer_buf.saturating_sub(in_flight) as usize
    }
}

/// The socket state, apart from the queues: what the guest is connected to and what it is owed.
struct Sockets {
    guest_cid: u64,
    services: HashMap<u32, Service>,
    conns: HashMap<Key, Conn>,
    /// Packets for the guest, waiting for receive buffers.
    out: VecDeque<(Header, Vec<u8>)>,
}

impl Sockets {
    fn header(&self, key: Key, op: u16, len: usize, conn: Option<&Conn>) -> Header {
        Header {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: key.1,
            dst_port: key.0,
            len: len as u32,
            kind: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: HOST_BUFFER,
            fwd_cnt: conn.map_or(0, |conn| conn.received),
        }
    }

    fn reply(&mut self, key: Key, op: u16) {
        let header = self.header(key, op, 0, self.conns.get(&key));
        if let Some(conn) = self.conns.get_mut(&key) {
            conn.told = conn.received;
        }
        self.out.push_back((header, Vec::new()));
    }

    fn reset(&mut self, key: Key) {
        self.conns.remove(&key);
        self.reply(key, OP_RST);
    }

    /// One packet from the guest.
    fn handle(&mut self, header: Header, payload: &[u8]) {
        if header.kind != TYPE_STREAM || header.dst_cid != HOST_CID {
            return;
        }
        let key = (header.src_port, header.dst_port);
        if header.op == OP_REQUEST {
            let Some(service) = self.services.get(&header.dst_port) else {
                self.reply(key, OP_RST);
                return;
            };
            let conn = Conn {
                service: service(),
                received: 0,
                told: 0,
                sent: 0,
                peer_buf: header.buf_alloc,
                peer_fwd: header.fwd_cnt,
                pending: VecDeque::new(),
            };
            self.conns.insert(key, conn);
            self.reply(key, OP_RESPONSE);
            return;
        }
        let Some(conn) = self.conns.get_mut(&key) else {
            if header.op != OP_RST {
                self.reply(key, OP_RST);
            }
            return;
        };
        conn.peer_buf = header.buf_alloc;
        conn.peer_fwd = header.fwd_cnt;
        match header.op {
            OP_RW => {
                conn.received = conn.received.wrapping_add(payload.len() as u32);
                let answer = conn.service.received(payload);
                conn.pending.extend(answer);
                let behind = conn.received.wrapping_sub(conn.told);
                if conn.pending.len() > MAX_PENDING {
                    self.reset(key);
                } else if behind > HOST_BUFFER / 2 {
                    self.reply(key, OP_CREDIT_UPDATE);
                }
            }
            OP_SHUTDOWN => self.reset(key),
            OP_RST => {
                self.conns.remove(&key);
            }
            OP_CREDIT_REQUEST => self.reply(key, OP_CREDIT_UPDATE),
            _ => {}
        }
    }

    /// Moves what the connections have for the guest into packets, as far as its credit goes.
    fn pack(&mut self) {
        let keys: Vec<Key> = self.conns.keys().copied().collect();
        for key in keys {
            while let Some(conn) = self.conns.get_mut(&key) {
                let take = conn.pending.len().min(conn.credit()).min(MAX_PAYLOAD);
                if take == 0 {
                    break;
                }
                let payload: Vec<u8> = conn.pending.drain(..take).collect();
                conn.sent = conn.sent.wrapping_add(take as u32);
                conn.told = conn.received;
                let conn = self.conns.get(&key);
                let header = self.header(key, OP_RW, take, conn);
                self.out.push_back((header, payload));
            }
        }
    }
}

/// Reads every packet the guest sent. Returns whether any buffer was used.
fn take_sent(
    sockets: &mut Sockets,
    queue: &mut Queue,
    mem: &GuestMemoryMmap,
) -> Result<bool, virtio_queue::Error> {
    let mut used_any = false;
    while let Some(chain) = queue.pop_descriptor_chain(mem) {
        let head = chain.head_index();
        let mut raw = [0u8; HEADER_LEN];
        let mut payload = Vec::new();
        let read = chain.clone().reader(mem).ok().and_then(|mut reader| {
            reader.read_exact(&mut raw).ok()?;
            let header = Header::parse(&raw);
            payload.resize(header.len as usize, 0);
            reader.read_exact(&mut payload).ok()?;
            Some(header)
        });
        if let Some(header) = read {
            sockets.handle(header, &payload);
        }
        queue.add_used(mem, head, 0)?;
        used_any = true;
    }
    Ok(used_any)
}

/// Fills the guest's receive buffers with waiting packets. Returns whether any buffer was used.
fn give(
    sockets: &mut Sockets,
    queue: &mut Queue,
    mem: &GuestMemoryMmap,
) -> Result<bool, virtio_queue::Error> {
    let mut used_any = false;
    while !sockets.out.is_empty() {
        let Some(chain) = queue.pop_descriptor_chain(mem) else {
            break;
        };
        let head = chain.head_index();
        let Some((mut header, mut payload)) = sockets.out.pop_front() else {
            break;
        };
        let written = chain.writer(mem).ok().and_then(|mut writer| {
            let room = writer.available_bytes().saturating_sub(HEADER_LEN);
            if payload.len() > room {
                // The rest waits for the next buffer.
                let rest = payload.split_off(room);
                let mut next = header;
                next.len = rest.len() as u32;
                sockets.out.push_front((next, rest));
                header.len = room as u32;
            }
            writer.write_all(&header.bytes()).ok()?;
            writer.write_all(&payload).ok()?;
            Some(writer.bytes_written())
        });
        queue.add_used(mem, head, written.unwrap_or(0) as u32)?;
        used_any = true;
    }
    Ok(used_any)
}

enum Wake {
    Work,
    Stop,
}

fn serve(
    mut sockets: Sockets,
    mut queues: Vec<Queue>,
    mem: GuestMemoryMmap,
    irq: Arc<VirtioInterrupt>,
    wake: Receiver<Wake>,
) -> Services {
    while let Ok(Wake::Work) = wake.recv() {
        let done = take_sent(&mut sockets, &mut queues[TX], &mem).and_then(|sent| {
            sockets.pack();
            give(&mut sockets, &mut queues[RX], &mem).map(|given| sent | given)
        });
        match done {
            Ok(true) => irq.signal_used(),
            Ok(false) => {}
            Err(err) => log::warn!("{}", copy::virtio_queue_error(&err)),
        }
    }
    sockets.services
}

pub struct Vsock {
    guest_cid: u64,
    services: HashMap<u32, Service>,
    worker: Option<Worker>,
}

impl Vsock {
    pub fn new(guest_cid: u64) -> Self {
        Self {
            guest_cid,
            services: HashMap::new(),
            worker: None,
        }
    }

    /// Answers guests that connect to the host on `port` with `service`.
    pub fn listen(&mut self, port: u32, service: Service) {
        self.services.insert(port, service);
    }
}

impl VirtioDevice for Vsock {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_VSOCK
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        0
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let config = self.guest_cid.to_le_bytes();
        for (at, byte) in (offset..).zip(data.iter_mut()) {
            *byte = usize::try_from(at)
                .ok()
                .and_then(|at| config.get(at))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, ctx: Activate) -> Result<(), DeviceError> {
        if self.worker.is_some() {
            return Err(DeviceError::AlreadyActive);
        }
        if ctx.queues.len() != QUEUE_SIZES.len() {
            return Err(DeviceError::MissingQueue);
        }
        let sockets = Sockets {
            guest_cid: self.guest_cid,
            services: std::mem::take(&mut self.services),
            conns: HashMap::new(),
            out: VecDeque::new(),
        };
        let (wake, wakes) = mpsc::channel();
        let thread = thread::Builder::new()
            .name(copy::VSOCK_WORKER_NAME.into())
            .spawn(move || serve(sockets, ctx.queues, ctx.mem, ctx.irq, wakes))?;
        self.worker = Some((wake, thread));
        Ok(())
    }

    fn queue_notify(&mut self, _queue: u16) {
        if let Some((wake, _)) = &self.worker {
            let _ = wake.send(Wake::Work);
        }
    }

    fn reset(&mut self) {
        if let Some((wake, thread)) = self.worker.take() {
            let _ = wake.send(Wake::Stop);
            match thread.join() {
                // The services come back, so that a driver that starts again finds them.
                Ok(services) => self.services = services,
                Err(_) => log::error!("{}", copy::VSOCK_WORKER_GONE),
            }
        }
    }
}

impl Drop for Vsock {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST: u64 = 3;
    const PORT: u32 = 9600;
    const GUEST_PORT: u32 = 1234;

    struct Echo;
    impl Connection for Echo {
        fn received(&mut self, bytes: &[u8]) -> Vec<u8> {
            bytes.to_vec()
        }
    }

    fn sockets() -> Sockets {
        let mut services: HashMap<u32, Service> = HashMap::new();
        services.insert(PORT, Box::new(|| Box::new(Echo) as Box<dyn Connection>));
        Sockets {
            guest_cid: GUEST,
            services,
            conns: HashMap::new(),
            out: VecDeque::new(),
        }
    }

    fn from_guest(op: u16, port: u32, len: usize, buf_alloc: u32) -> Header {
        Header {
            src_cid: GUEST,
            dst_cid: HOST_CID,
            src_port: GUEST_PORT,
            dst_port: port,
            len: len as u32,
            kind: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc,
            fwd_cnt: 0,
        }
    }

    #[test]
    fn headers_survive_a_round_trip() {
        let header = from_guest(OP_RW, PORT, 5, 77);
        assert_eq!(Header::parse(&header.bytes()), header);
    }

    #[test]
    fn a_service_accepts_and_answers_and_others_are_refused() {
        let mut sockets = sockets();
        sockets.handle(from_guest(OP_REQUEST, PORT, 0, 1024), &[]);
        let (answer, _) = sockets.out.pop_front().unwrap();
        assert_eq!(
            (answer.op, answer.dst_port, answer.src_port),
            (OP_RESPONSE, GUEST_PORT, PORT)
        );

        sockets.handle(from_guest(OP_RW, PORT, 3, 1024), b"AT\r");
        sockets.pack();
        let (data, payload) = sockets.out.pop_front().unwrap();
        assert_eq!((data.op, payload.as_slice()), (OP_RW, &b"AT\r"[..]));
        assert_eq!(data.fwd_cnt, 3);

        sockets.handle(from_guest(OP_REQUEST, PORT + 1, 0, 1024), &[]);
        assert_eq!(sockets.out.pop_front().unwrap().0.op, OP_RST);
    }

    #[test]
    fn the_guest_is_never_sent_more_than_it_has_room_for() {
        let mut sockets = sockets();
        sockets.handle(from_guest(OP_REQUEST, PORT, 0, 2), &[]);
        sockets.out.clear();
        sockets.handle(from_guest(OP_RW, PORT, 5, 2), b"hello");
        sockets.pack();
        let (_, payload) = sockets.out.pop_front().unwrap();
        assert_eq!(payload, b"he");
        assert!(sockets.out.is_empty());
        // The guest consumed those two bytes and says so; the rest follows.
        let mut update = from_guest(OP_CREDIT_UPDATE, PORT, 0, 8);
        update.fwd_cnt = 2;
        sockets.handle(update, &[]);
        sockets.pack();
        assert_eq!(sockets.out.pop_front().unwrap().1, b"llo");
    }
}
