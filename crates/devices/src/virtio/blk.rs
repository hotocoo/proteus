//! virtio-blk backed by a raw image file.
//!
//! Requests are served on a worker thread; the vCPU thread that writes QueueNotify only wakes it.
//! Data moves straight between the file and guest memory, with no intermediate buffer.
//! Every field of a request comes from the guest and is checked before it is used.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use virtio_bindings::virtio_blk::*;
use virtio_bindings::virtio_ids::VIRTIO_ID_BLOCK;
use virtio_bindings::virtio_ring::VIRTIO_RING_F_INDIRECT_DESC;
use virtio_queue::desc::split::Descriptor;
use virtio_queue::{DescriptorChain, Queue, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};

use super::{Activate, DeviceError, VirtioDevice, VirtioInterrupt};
use crate::copy;
use crate::disk::{Disk, open_image};

/// virtio-blk addresses the disk in 512-byte sectors whatever the image's block size.
const SECTOR_SHIFT: u32 = 9;
const SECTOR_SIZE: u64 = 1 << SECTOR_SHIFT;
/// One request queue of this many descriptors.
const QUEUE_SIZES: [u16; 1] = [256];
/// Data buffers one request may carry. Without this the guest sends one buffer (often one 4 KiB
/// page) per request; the header and status take the other two descriptors of a full ring.
const SEG_MAX: u32 = QUEUE_SIZES[0] as u32 - 2;
/// `struct virtio_blk_config`: capacity (u64), size_max (u32), seg_max (u32), little-endian.
const CONFIG_SEG_MAX_AT: usize = 12;
const CONFIG_LEN: usize = CONFIG_SEG_MAX_AT + size_of::<u32>();
/// `struct virtio_blk_outhdr`: type (u32), priority (u32), sector (u64), little-endian.
const HEADER_LEN: usize = 16;
const HEADER_SECTOR_AT: usize = 8;
const ID_LEN: usize = VIRTIO_BLK_ID_BYTES as usize;
/// The status byte the device appends to every request.
const STATUS_LEN: u32 = 1;

type Status = u8;
const OK: Status = VIRTIO_BLK_S_OK as Status;
const IO_ERROR: Status = VIRTIO_BLK_S_IOERR as Status;
const UNSUPPORTED: Status = VIRTIO_BLK_S_UNSUPP as Status;

/// A request in progress: what it does and where the next data buffer goes.
enum Op {
    Read { offset: u64 },
    Write { offset: u64 },
    Flush,
    GetId { answered: bool },
}

pub struct DiskImage {
    disk: Disk,
    capacity_bytes: u64,
    read_only: bool,
    id: [u8; ID_LEN],
}

impl DiskImage {
    /// Opens one image file as a whole disk. An Android sparse image is expanded on the fly and is always read-only.
    /// `id` is what the guest sees under `/sys/block/*/serial`; longer strings are cut to 20 bytes.
    pub fn open(path: &Path, read_only: bool, id: &str) -> std::io::Result<Self> {
        let (disk, writable) = open_image(path, !read_only)?;
        let read_only = !writable;
        Ok(Self::from_disk(disk, read_only, id))
    }

    /// Presents an already assembled disk. Anything past its last whole sector is not exposed.
    pub fn from_disk(disk: Disk, read_only: bool, id: &str) -> Self {
        let capacity_bytes = disk.len() >> SECTOR_SHIFT << SECTOR_SHIFT;
        let mut id_bytes = [0u8; ID_LEN];
        let len = id.len().min(ID_LEN);
        id_bytes[..len].copy_from_slice(&id.as_bytes()[..len]);
        Self {
            disk,
            capacity_bytes,
            read_only,
            id: id_bytes,
        }
    }

    /// End offset of a data buffer of `len` bytes at `offset`, if it is whole sectors inside the disk.
    fn span(&self, offset: u64, len: u64) -> Result<u64, Status> {
        let end = offset.checked_add(len).ok_or(IO_ERROR)?;
        if !len.is_multiple_of(SECTOR_SIZE) || end > self.capacity_bytes {
            return Err(IO_ERROR);
        }
        Ok(end)
    }

    fn begin(&mut self, mem: &GuestMemoryMmap, header: &Descriptor) -> Result<Op, Status> {
        if header.is_write_only() || (header.len() as usize) < HEADER_LEN {
            return Err(IO_ERROR);
        }
        let mut raw = [0u8; HEADER_LEN];
        mem.read_slice(&mut raw, header.addr())
            .map_err(|_| IO_ERROR)?;
        let (kind, rest) = raw.split_at(size_of::<u32>());
        let kind = u32::from_le_bytes(kind.try_into().expect("four bytes"));
        let sector = &rest[HEADER_SECTOR_AT - size_of::<u32>()..];
        let sector = u64::from_le_bytes(sector.try_into().expect("eight bytes"));
        let offset = || sector.checked_mul(SECTOR_SIZE).ok_or(IO_ERROR);
        match kind {
            VIRTIO_BLK_T_IN => Ok(Op::Read { offset: offset()? }),
            VIRTIO_BLK_T_OUT if self.read_only => Err(IO_ERROR),
            VIRTIO_BLK_T_OUT => Ok(Op::Write { offset: offset()? }),
            VIRTIO_BLK_T_FLUSH => Ok(Op::Flush),
            VIRTIO_BLK_T_GET_ID => Ok(Op::GetId { answered: false }),
            _ => Err(UNSUPPORTED),
        }
    }

    /// Moves one data buffer. Returns how many bytes were written into guest memory.
    fn transfer(
        &mut self,
        mem: &GuestMemoryMmap,
        op: &mut Op,
        buffer: &Descriptor,
    ) -> Result<u32, Status> {
        let len = buffer.len();
        match op {
            Op::Read { offset } if buffer.is_write_only() => {
                let end = self.span(*offset, u64::from(len))?;
                self.disk
                    .read_to_guest(mem, buffer.addr(), *offset, u64::from(len))
                    .map_err(|_| IO_ERROR)?;
                *offset = end;
                Ok(len)
            }
            Op::Write { offset } if !buffer.is_write_only() => {
                let end = self.span(*offset, u64::from(len))?;
                self.disk
                    .write_from_guest(mem, buffer.addr(), *offset, u64::from(len))
                    .map_err(|_| IO_ERROR)?;
                *offset = end;
                Ok(0)
            }
            Op::GetId { answered } if buffer.is_write_only() && !*answered => {
                let take = ID_LEN.min(len as usize);
                mem.write_slice(&self.id[..take], buffer.addr())
                    .map_err(|_| IO_ERROR)?;
                *answered = true;
                Ok(take as u32)
            }
            // A buffer facing the wrong way, a second ID buffer, or data on a flush.
            Op::Read { .. } | Op::Write { .. } | Op::GetId { .. } | Op::Flush => Err(IO_ERROR),
        }
    }

    fn finish(&mut self, op: Op) -> Status {
        match op {
            Op::Flush if self.disk.flush().is_err() => IO_ERROR,
            Op::Flush | Op::Read { .. } | Op::Write { .. } | Op::GetId { .. } => OK,
        }
    }

    /// Serves one request. Returns the byte count for the used ring: everything the device wrote
    /// into guest memory, status byte included. Zero means the chain had no usable status buffer.
    ///
    /// A chain is `[header][data buffers...][status]`. The buffers are walked once; each one is known
    /// to be data only when another descriptor follows it, so one descriptor is always held back.
    fn execute(
        &mut self,
        mem: &GuestMemoryMmap,
        mut chain: DescriptorChain<&GuestMemoryMmap>,
    ) -> u32 {
        let (Some(header), Some(mut held)) = (chain.next(), chain.next()) else {
            return 0;
        };
        let mut written = 0u32;
        let mut outcome = self.begin(mem, &header);
        for next in chain {
            if let Ok(op) = &mut outcome {
                match self.transfer(mem, op, &held) {
                    Ok(bytes) => written = written.saturating_add(bytes),
                    Err(status) => outcome = Err(status),
                }
            }
            held = next;
        }
        let status = outcome.map_or_else(|status| status, |op| self.finish(op));
        if !held.is_write_only()
            || held.len() < STATUS_LEN
            || mem.write_obj(status, held.addr()).is_err()
        {
            return 0;
        }
        written.saturating_add(STATUS_LEN)
    }
}

enum Wake {
    Queue,
    Stop,
}

enum State {
    Idle(DiskImage),
    Running {
        wake: Sender<Wake>,
        worker: JoinHandle<DiskImage>,
    },
    /// The worker died with the image; only a VM restart recovers.
    Lost,
}

pub struct Block {
    state: State,
    capacity_sectors: u64,
    read_only: bool,
}

impl Block {
    pub fn new(disk: DiskImage) -> Self {
        let capacity_sectors = disk.capacity_bytes >> SECTOR_SHIFT;
        let read_only = disk.read_only;
        Self {
            state: State::Idle(disk),
            capacity_sectors,
            read_only,
        }
    }
}

fn drain(
    disk: &mut DiskImage,
    queue: &mut Queue,
    mem: &GuestMemoryMmap,
    irq: &VirtioInterrupt,
) -> Result<(), virtio_queue::Error> {
    loop {
        queue.disable_notification(mem)?;
        let mut used_any = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let written = disk.execute(mem, chain);
            queue.add_used(mem, head, written)?;
            used_any = true;
        }
        if used_any && queue.needs_notification(mem)? {
            irq.signal_used();
        }
        // Re-arming reports requests that slipped in after the last pop; go round again for those.
        if !queue.enable_notification(mem)? {
            return Ok(());
        }
    }
}

fn serve(
    mut disk: DiskImage,
    mut queue: Queue,
    mem: GuestMemoryMmap,
    irq: Arc<VirtioInterrupt>,
    wake: Receiver<Wake>,
) -> DiskImage {
    while let Ok(Wake::Queue) = wake.recv() {
        if let Err(err) = drain(&mut disk, &mut queue, &mem, &irq) {
            log::warn!("{}", copy::virtio_queue_error(&err));
        }
    }
    disk
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_BLOCK
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        let read_only = if self.read_only {
            1 << VIRTIO_BLK_F_RO
        } else {
            0
        };
        // Indirect tables let one request of many buffers take a single ring slot.
        1 << VIRTIO_BLK_F_FLUSH
            | 1 << VIRTIO_BLK_F_SEG_MAX
            | 1 << VIRTIO_RING_F_INDIRECT_DESC
            | read_only
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // Capacity in sectors, no size_max (zero), then seg_max; the fields after it are unused.
        let mut config = [0u8; CONFIG_LEN];
        config[..size_of::<u64>()].copy_from_slice(&self.capacity_sectors.to_le_bytes());
        config[CONFIG_SEG_MAX_AT..].copy_from_slice(&SEG_MAX.to_le_bytes());
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
        let Some(queue) = ctx.queues.into_iter().next() else {
            return Err(DeviceError::MissingQueue);
        };
        let State::Idle(disk) = std::mem::replace(&mut self.state, State::Lost) else {
            return Err(DeviceError::AlreadyActive);
        };
        let (wake, wakes) = mpsc::channel();
        let worker = thread::Builder::new()
            .name(copy::DISK_WORKER_NAME.into())
            .spawn(move || serve(disk, queue, ctx.mem, ctx.irq, wakes))?;
        self.state = State::Running { wake, worker };
        Ok(())
    }

    fn queue_notify(&mut self, _queue: u16) {
        if let State::Running { wake, .. } = &self.state {
            // A send fails only if the worker is gone, which `reset` reports.
            let _ = wake.send(Wake::Queue);
        }
    }

    fn reset(&mut self) {
        self.state = match std::mem::replace(&mut self.state, State::Lost) {
            State::Running { wake, worker } => {
                let _ = wake.send(Wake::Stop);
                worker.join().map_or_else(
                    |_| {
                        log::error!("{}", copy::DISK_WORKER_GONE);
                        State::Lost
                    },
                    State::Idle,
                )
            }
            other => other,
        };
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use virtio_bindings::virtio_ring::VRING_DESC_F_WRITE;
    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::mock::MockSplitQueue;
    use vm_memory::{Address, GuestAddress};

    use super::super::testing::LineLog;
    use super::*;

    const MEM_SIZE: usize = 4 << 20;
    const HEADER_AT: u64 = 0x10_0000;
    const DATA_AT: u64 = 0x11_0000;
    const STATUS_AT: u64 = 0x20_0000;
    const DISK_SECTORS: u64 = 16;
    const QUEUE_LEN: u16 = 16;
    /// A status value no request produces, so an unwritten status byte is detectable.
    const UNTOUCHED: u8 = 0xee;
    const WORKER_TIMEOUT: Duration = Duration::from_secs(5);

    static NEXT_IMAGE: AtomicU32 = AtomicU32::new(0);

    /// A scratch image filled with a recognisable pattern, deleted on drop.
    struct TempImage(PathBuf);

    impl TempImage {
        fn new() -> Self {
            let name = format!(
                "emu-blk-{}-{}.img",
                std::process::id(),
                NEXT_IMAGE.fetch_add(1, Ordering::SeqCst)
            );
            let path = std::env::temp_dir().join(name);
            File::create(&path).unwrap().write_all(&pattern()).unwrap();
            Self(path)
        }

        fn contents(&self) -> Vec<u8> {
            let mut bytes = Vec::new();
            File::open(&self.0)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            bytes
        }
    }

    impl Drop for TempImage {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Sector `n` is filled with byte `n + 1`.
    fn pattern() -> Vec<u8> {
        (0..DISK_SECTORS)
            .flat_map(|sector| vec![sector as u8 + 1; SECTOR_SIZE as usize])
            .collect()
    }

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap()
    }

    fn readable(addr: u64, len: u32) -> RawDescriptor {
        RawDescriptor::from(Descriptor::new(addr, len, 0, 0))
    }

    fn writable(addr: u64, len: u32) -> RawDescriptor {
        RawDescriptor::from(Descriptor::new(addr, len, VRING_DESC_F_WRITE as u16, 0))
    }

    fn put_header(mem: &GuestMemoryMmap, kind: u32, sector: u64) {
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&kind.to_le_bytes());
        header[HEADER_SECTOR_AT..].copy_from_slice(&sector.to_le_bytes());
        mem.write_slice(&header, GuestAddress(HEADER_AT)).unwrap();
        mem.write_obj(UNTOUCHED, GuestAddress(STATUS_AT)).unwrap();
    }

    /// Runs one request made of the given data descriptors and returns (used length, status byte).
    fn run(
        disk: &mut DiskImage,
        mem: &GuestMemoryMmap,
        kind: u32,
        sector: u64,
        data: &[RawDescriptor],
    ) -> (u32, u8) {
        put_header(mem, kind, sector);
        let mut descs = vec![readable(HEADER_AT, HEADER_LEN as u32)];
        descs.extend_from_slice(data);
        descs.push(writable(STATUS_AT, 1));
        let queue = MockSplitQueue::new(mem, QUEUE_LEN);
        let used = disk.execute(mem, queue.build_desc_chain(&descs).unwrap());
        (used, mem.read_obj(GuestAddress(STATUS_AT)).unwrap())
    }

    fn guest_bytes(mem: &GuestMemoryMmap, addr: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0; len];
        mem.read_slice(&mut bytes, GuestAddress(addr)).unwrap();
        bytes
    }

    #[test]
    fn read_spanning_two_buffers_returns_consecutive_sectors() {
        let image = TempImage::new();
        let mut disk = DiskImage::open(&image.0, true, "").unwrap();
        let mem = memory();
        let second = DATA_AT + 0x1000;
        let (used, status) = run(
            &mut disk,
            &mem,
            VIRTIO_BLK_T_IN,
            3,
            &[writable(DATA_AT, 512), writable(second, 1024)],
        );
        assert_eq!((used, status), (512 + 1024 + 1, OK));
        assert_eq!(guest_bytes(&mem, DATA_AT, 512), vec![4; 512]);
        assert_eq!(
            guest_bytes(&mem, second, 1024),
            [vec![5; 512], vec![6; 512]].concat()
        );
    }

    #[test]
    fn write_lands_in_the_file_and_reports_only_the_status_byte() {
        let image = TempImage::new();
        let mut disk = DiskImage::open(&image.0, false, "").unwrap();
        let mem = memory();
        mem.write_slice(&[0xab; 1024], GuestAddress(DATA_AT))
            .unwrap();
        let (used, status) = run(
            &mut disk,
            &mem,
            VIRTIO_BLK_T_OUT,
            2,
            &[readable(DATA_AT, 1024)],
        );
        assert_eq!((used, status), (1, OK));
        assert_eq!(run(&mut disk, &mem, VIRTIO_BLK_T_FLUSH, 0, &[]), (1, OK));

        let mut expected = pattern();
        expected[2 * 512..4 * 512].fill(0xab);
        assert_eq!(image.contents(), expected);
    }

    #[test]
    fn read_only_disk_refuses_writes_and_keeps_its_contents() {
        let image = TempImage::new();
        let mut disk = DiskImage::open(&image.0, true, "").unwrap();
        let mem = memory();
        let (used, status) = run(
            &mut disk,
            &mem,
            VIRTIO_BLK_T_OUT,
            0,
            &[readable(DATA_AT, 512)],
        );
        assert_eq!((used, status), (1, IO_ERROR));
        assert_eq!(image.contents(), pattern());
    }

    #[test]
    fn requests_outside_the_disk_fail_without_touching_guest_memory() {
        let image = TempImage::new();
        let mut disk = DiskImage::open(&image.0, false, "").unwrap();
        let mem = memory();
        mem.write_slice(&[0x77; 1024], GuestAddress(DATA_AT))
            .unwrap();
        let cases = [
            ("one sector past the end", DISK_SECTORS, 512),
            ("straddling the end", DISK_SECTORS - 1, 1024),
            ("sector times 512 overflows", u64::MAX, 512),
            (
                "offset plus length overflows",
                u64::MAX >> SECTOR_SHIFT,
                1024,
            ),
            ("not a whole number of sectors", 0, 100),
        ];
        for (what, sector, len) in cases {
            let (used, status) = run(
                &mut disk,
                &mem,
                VIRTIO_BLK_T_IN,
                sector,
                &[writable(DATA_AT, len)],
            );
            assert_eq!((used, status), (1, IO_ERROR), "{what}");
            let (used, status) = run(
                &mut disk,
                &mem,
                VIRTIO_BLK_T_OUT,
                sector,
                &[readable(DATA_AT, len)],
            );
            assert_eq!((used, status), (1, IO_ERROR), "{what}");
        }
        assert_eq!(guest_bytes(&mem, DATA_AT, 1024), vec![0x77; 1024]);
        assert_eq!(image.contents(), pattern());
    }

    #[test]
    fn buffers_facing_the_wrong_way_are_refused() {
        let image = TempImage::new();
        let mut disk = DiskImage::open(&image.0, false, "").unwrap();
        let mem = memory();
        assert_eq!(
            run(
                &mut disk,
                &mem,
                VIRTIO_BLK_T_IN,
                0,
                &[readable(DATA_AT, 512)]
            ),
            (1, IO_ERROR)
        );
        assert_eq!(
            run(
                &mut disk,
                &mem,
                VIRTIO_BLK_T_OUT,
                0,
                &[writable(DATA_AT, 512)]
            ),
            (1, IO_ERROR)
        );
        assert_eq!(
            run(
                &mut disk,
                &mem,
                VIRTIO_BLK_T_FLUSH,
                0,
                &[writable(DATA_AT, 512)]
            ),
            (1, IO_ERROR)
        );
        assert_eq!(image.contents(), pattern());
    }

    #[test]
    fn buffer_outside_guest_memory_is_an_io_error() {
        let image = TempImage::new();
        let mut disk = DiskImage::open(&image.0, false, "").unwrap();
        let mem = memory();
        let beyond = MEM_SIZE as u64;
        assert_eq!(
            run(
                &mut disk,
                &mem,
                VIRTIO_BLK_T_IN,
                0,
                &[writable(beyond, 512)]
            ),
            (1, IO_ERROR)
        );
    }

    #[test]
    fn device_id_is_returned_and_truncated_to_the_buffer() {
        let image = TempImage::new();
        let mut disk =
            DiskImage::open(&image.0, true, "a-serial-longer-than-twenty-bytes").unwrap();
        let mem = memory();
        let (used, status) = run(
            &mut disk,
            &mem,
            VIRTIO_BLK_T_GET_ID,
            0,
            &[writable(DATA_AT, ID_LEN as u32)],
        );
        assert_eq!((used, status), (ID_LEN as u32 + 1, OK));
        assert_eq!(guest_bytes(&mem, DATA_AT, ID_LEN), b"a-serial-longer-than");
        let (used, status) = run(
            &mut disk,
            &mem,
            VIRTIO_BLK_T_GET_ID,
            0,
            &[writable(DATA_AT, 4)],
        );
        assert_eq!((used, status), (4 + 1, OK));
    }

    #[test]
    fn sparse_image_is_served_expanded_and_read_only() {
        use crate::disk::sparse::tests::{Chunk, image};
        let data = vec![0x5a; 4096];
        let sparse =
            crate::disk::testing::TempFile::new(&image(&[Chunk::Skip(1), Chunk::Raw(&data)]));
        let mut disk = DiskImage::open(&sparse.0, true, "").unwrap();
        assert_eq!(disk.capacity_bytes, 8192);
        let mem = memory();
        let (used, status) = run(
            &mut disk,
            &mem,
            VIRTIO_BLK_T_IN,
            7,
            &[writable(DATA_AT, 1024)],
        );
        assert_eq!((used, status), (1024 + 1, OK));
        assert_eq!(
            guest_bytes(&mem, DATA_AT, 1024),
            [vec![0; 512], vec![0x5a; 512]].concat()
        );
        // Asking for it writable changes nothing: a sparse image is never exposed as raw bytes.
        let writable = DiskImage::open(&sparse.0, false, "").unwrap();
        assert_eq!(writable.capacity_bytes, 8192);
        assert!(writable.read_only);
    }

    #[test]
    fn unknown_request_types_are_unsupported() {
        let image = TempImage::new();
        let mut disk = DiskImage::open(&image.0, false, "").unwrap();
        let mem = memory();
        assert_eq!(
            run(
                &mut disk,
                &mem,
                VIRTIO_BLK_T_DISCARD,
                0,
                &[readable(DATA_AT, 16)]
            ),
            (1, UNSUPPORTED)
        );
        assert_eq!(image.contents(), pattern());
    }

    #[test]
    fn malformed_chains_use_zero_bytes() {
        let image = TempImage::new();
        let mut disk = DiskImage::open(&image.0, false, "").unwrap();
        let mem = memory();
        let queue = MockSplitQueue::new(&mem, QUEUE_LEN);
        put_header(&mem, VIRTIO_BLK_T_IN, 0);

        // Header only: nowhere to put a status.
        let chain = queue
            .build_desc_chain(&[readable(HEADER_AT, HEADER_LEN as u32)])
            .unwrap();
        assert_eq!(disk.execute(&mem, chain), 0);
        // Status buffer the device may not write to.
        let chain = queue
            .build_desc_chain(&[
                readable(HEADER_AT, HEADER_LEN as u32),
                readable(STATUS_AT, 1),
            ])
            .unwrap();
        assert_eq!(disk.execute(&mem, chain), 0);
        assert_eq!(
            mem.read_obj::<u8>(GuestAddress(STATUS_AT)).unwrap(),
            UNTOUCHED
        );
    }

    #[test]
    fn short_or_device_writable_header_is_an_io_error() {
        let image = TempImage::new();
        let mut disk = DiskImage::open(&image.0, false, "").unwrap();
        let mem = memory();
        let queue = MockSplitQueue::new(&mem, QUEUE_LEN);
        for header in [
            readable(HEADER_AT, HEADER_LEN as u32 - 1),
            writable(HEADER_AT, HEADER_LEN as u32),
        ] {
            put_header(&mem, VIRTIO_BLK_T_IN, 0);
            let chain = queue
                .build_desc_chain(&[header, writable(DATA_AT, 512), writable(STATUS_AT, 1)])
                .unwrap();
            assert_eq!(disk.execute(&mem, chain), 1);
            assert_eq!(
                mem.read_obj::<u8>(GuestAddress(STATUS_AT)).unwrap(),
                IO_ERROR
            );
        }
    }

    #[test]
    fn config_space_reports_capacity_in_sectors_and_features_match_the_mode() {
        let image = TempImage::new();
        let block = Block::new(DiskImage::open(&image.0, true, "").unwrap());
        let mut config = [0xffu8; 12];
        block.read_config(0, &mut config);
        assert_eq!(config[..8], DISK_SECTORS.to_le_bytes());
        assert_eq!(config[8..], [0; 4]);
        let mut tail = [0xffu8; 2];
        block.read_config(7, &mut tail);
        assert_eq!(tail, [0, 0]);
        let mut seg_max = [0u8; 4];
        block.read_config(CONFIG_SEG_MAX_AT as u64, &mut seg_max);
        assert_eq!(u32::from_le_bytes(seg_max), SEG_MAX);
        assert_ne!(block.features() & 1 << VIRTIO_BLK_F_SEG_MAX, 0);
        assert_ne!(block.features() & 1 << VIRTIO_BLK_F_RO, 0);

        let writable_disk = Block::new(DiskImage::open(&image.0, false, "").unwrap());
        assert_eq!(writable_disk.features() & 1 << VIRTIO_BLK_F_RO, 0);
        assert_ne!(writable_disk.features() & 1 << VIRTIO_BLK_F_FLUSH, 0);
    }

    #[test]
    fn worker_serves_a_notified_queue_raises_the_interrupt_and_survives_a_reset() {
        let image = TempImage::new();
        let mut block = Block::new(DiskImage::open(&image.0, true, "").unwrap());
        let mem = memory();
        let line = Arc::new(LineLog::default());

        for round in 1..=2u16 {
            let mock = MockSplitQueue::new(&mem, QUEUE_LEN);
            put_header(&mem, VIRTIO_BLK_T_IN, 1);
            let descs = [
                RawDescriptor::from(Descriptor::new(HEADER_AT, HEADER_LEN as u32, 1, 1)),
                RawDescriptor::from(Descriptor::new(
                    DATA_AT,
                    512,
                    1 | VRING_DESC_F_WRITE as u16,
                    2,
                )),
                writable(STATUS_AT, 1),
            ];
            mock.add_desc_chains(&descs, 0).unwrap();
            let ctx = Activate {
                mem: mem.clone(),
                queues: vec![mock.create_queue().unwrap()],
                irq: VirtioInterrupt::new(line.clone()),
                features: 0,
            };
            block.activate(ctx).unwrap();
            block.queue_notify(0);

            let deadline = Instant::now() + WORKER_TIMEOUT;
            while line.0.lock().unwrap().len() < usize::from(round) {
                assert!(
                    Instant::now() < deadline,
                    "worker never raised the interrupt in round {round}"
                );
                thread::yield_now();
            }
            assert_eq!(mem.read_obj::<u8>(GuestAddress(STATUS_AT)).unwrap(), OK);
            assert_eq!(guest_bytes(&mem, DATA_AT, 512), vec![2; 512]);
            let used_idx: u16 = mem.read_obj(mock.used_addr().unchecked_add(2)).unwrap();
            assert_eq!(used_idx, 1);

            block.reset();
            assert!(matches!(block.state, State::Idle(_)));
            mem.write_slice(&[0; 512], GuestAddress(DATA_AT)).unwrap();
        }
    }
}
