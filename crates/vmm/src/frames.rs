//! Saves what the guest shows on its display as a PNG file, refreshed while the guest runs.
//!
//! This is how a display can be looked at before there is a window: point `--frame-dump` at a file and
//! open it. The PNG is written without compression, which needs no library and is fast.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use devices::disk::gpt::crc32;
use devices::virtio::gpu::{BYTES_PER_PIXEL, Frame, FrameSink, FrameSlot};

use crate::config::FRAME_DUMP_INTERVAL;
use crate::copy;
use crate::shared::{SharedBuffer, SharedPool, lock};

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
/// Eight bits per channel, colour type 2 (RGB), default compression and filter methods, no interlace.
const IHDR_TAIL: [u8; 5] = [8, 2, 0, 0, 0];
const RGB_BYTES: usize = 3;
/// Every row starts with a filter byte; zero means the row is stored as it is.
const FILTER_NONE: u8 = 0;
/// zlib header for "deflate, 32 KiB window, no preset dictionary, fastest".
const ZLIB_HEADER: [u8; 2] = [0x78, 0x01];
/// A stored deflate block holds at most this many bytes.
const STORED_BLOCK_MAX: usize = 0xffff;
const ADLER_MODULUS: u32 = 65_521;

// `VIRTIO_GPU_FORMAT_*` values, grouped by the byte order of their channels.
pub(crate) const FORMATS_BGRX: [u32; 2] = [1, 2];
const FORMATS_XRGB: [u32; 2] = [3, 4];
const FORMATS_XBGR: [u32; 2] = [68, 121];
/// virgl's R8G8B8A8 and R8G8B8X8: red first.
pub(crate) const FORMATS_RGBX: [u32; 2] = [67, 134];

/// Positions of red, green and blue within one four-byte pixel of the given format.
pub(crate) fn channel_order(format: u32) -> [usize; RGB_BYTES] {
    if FORMATS_BGRX.contains(&format) {
        [2, 1, 0]
    } else if FORMATS_XRGB.contains(&format) {
        [1, 2, 3]
    } else if FORMATS_XBGR.contains(&format) {
        [3, 2, 1]
    } else {
        // R8G8B8A8, R8G8B8X8, and the fallback for a format this build does not know.
        [0, 1, 2]
    }
}

fn adler32(bytes: &[u8]) -> u32 {
    let (a, b) = bytes.iter().fold((1u32, 0u32), |(a, b), byte| {
        let a = (a + u32::from(*byte)) % ADLER_MODULUS;
        (a, (b + a) % ADLER_MODULUS)
    });
    b << 16 | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(data);
    let checksum = crc32(&out[start..]);
    out.extend(checksum.to_be_bytes());
}

/// Encodes a frame as an uncompressed RGB PNG. `None` if the frame's size and pixel count disagree.
pub fn png(frame: &Frame<'_>) -> Option<Vec<u8>> {
    let (width, height) = (frame.width as usize, frame.height as usize);
    if width == 0 || frame.pixels.len() != width * height * BYTES_PER_PIXEL {
        return None;
    }
    let order = channel_order(frame.format);
    let mut raw = Vec::with_capacity(height * (1 + width * RGB_BYTES));
    for row in frame.pixels.chunks_exact(width * BYTES_PER_PIXEL) {
        raw.push(FILTER_NONE);
        for pixel in row.as_chunks::<BYTES_PER_PIXEL>().0 {
            raw.extend(order.map(|channel| pixel[channel]));
        }
    }

    let mut zlib = ZLIB_HEADER.to_vec();
    let mut blocks = raw.chunks(STORED_BLOCK_MAX).peekable();
    while let Some(block) = blocks.next() {
        let len = block.len() as u16;
        zlib.push(u8::from(blocks.peek().is_none()));
        zlib.extend(len.to_le_bytes());
        zlib.extend((!len).to_le_bytes());
        zlib.extend(block);
    }
    zlib.extend(adler32(&raw).to_be_bytes());

    let mut header = Vec::new();
    header.extend(frame.width.to_be_bytes());
    header.extend(frame.height.to_be_bytes());
    header.extend(IHDR_TAIL);

    let mut out = PNG_SIGNATURE.to_vec();
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    Some(out)
}

/// The newest frame the guest has shown, kept so that it can be saved at any time.
#[derive(Default)]
pub struct Screen {
    latest: Mutex<Latest>,
    /// A frame buffer no reader holds any more, refilled by the next frame instead of a new one.
    spare: Mutex<Vec<u8>>,
    /// The frame before the newest. Handed back one frame later than the newest is replaced, by
    /// when the window has nearly always finished uploading it, so that buffers are reused.
    previous: Mutex<Pixels>,
    /// Memory frames can be read into where the GPU samples it, once the window has a GPU.
    shared: Mutex<Option<Arc<SharedPool>>>,
    /// Called after every new frame, on the thread that delivered it.
    listener: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Which way up the picture is shown, in quarter turns (see `orientation`).
    turns: AtomicU8,
}

/// A frame's bytes: in a `Vec`, or in memory the GPU reads where it lies.
#[derive(Clone)]
enum Pixels {
    Owned(Arc<Vec<u8>>),
    Shared(Arc<SharedBuffer>),
}

impl Default for Pixels {
    fn default() -> Self {
        Self::Owned(Arc::default())
    }
}

impl Pixels {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Owned(pixels) => pixels,
            Self::Shared(buffer) => buffer.bytes(),
        }
    }
}

/// A shared buffer on loan to the device, to be read into and then shown.
struct Lent {
    buffer: Arc<SharedBuffer>,
    width: u32,
    height: u32,
}

impl FrameSlot for Lent {
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the pool lent this buffer to no one else, and it is shown only after the read
        // that fills it is done (`present_slot`), so this is its only holder but the pool.
        unsafe { self.buffer.bytes_mut() }
    }
}

#[derive(Default, Clone)]
struct Latest {
    width: u32,
    height: u32,
    format: u32,
    /// Shared, so readers copy or encode it without holding the lock the device thread needs.
    pixels: Pixels,
    /// Counts frames, so that a reader can tell whether it has seen this one. Zero: none yet.
    serial: u64,
}

impl FrameSink for Screen {
    fn present(&self, frame: &Frame<'_>) {
        let mut pixels =
            std::mem::take(&mut *self.spare.lock().unwrap_or_else(PoisonError::into_inner));
        pixels.clear();
        pixels.extend_from_slice(frame.pixels);
        let old = self.keep(
            frame.width,
            frame.height,
            frame.format,
            Pixels::Owned(Arc::new(pixels)),
        );
        *self.spare.lock().unwrap_or_else(PoisonError::into_inner) = old;
    }

    fn present_owned(&self, width: u32, height: u32, format: u32, pixels: Vec<u8>) -> Vec<u8> {
        self.keep(width, height, format, Pixels::Owned(Arc::new(pixels)))
    }

    fn slot(&self, width: u32, height: u32) -> Option<Box<dyn FrameSlot>> {
        let pool = lock(&self.shared).clone()?;
        let buffer = pool.acquire(width, height)?;
        Some(Box::new(Lent {
            buffer,
            width,
            height,
        }))
    }

    fn present_slot(&self, slot: Box<dyn FrameSlot>, format: u32) {
        let slot: Box<dyn std::any::Any> = slot;
        let Ok(lent) = slot.downcast::<Lent>() else {
            return;
        };
        let Lent {
            buffer,
            width,
            height,
        } = *lent;
        self.keep(width, height, format, Pixels::Shared(buffer));
    }
}

impl Screen {
    /// Makes `pixels` the newest frame and tells the listener. Returns the frame before the one
    /// it replaces if no reader still holds that one, or else an empty buffer.
    fn keep(&self, width: u32, height: u32, format: u32, pixels: Pixels) -> Vec<u8> {
        let mut latest = self.latest.lock().unwrap_or_else(PoisonError::into_inner);
        latest.width = width;
        latest.height = height;
        latest.format = format;
        let old = std::mem::replace(&mut latest.pixels, pixels);
        latest.serial += 1;
        drop(latest);
        let older = std::mem::replace(
            &mut *self.previous.lock().unwrap_or_else(PoisonError::into_inner),
            old,
        );
        let listener = self.listener.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(listener) = listener.as_ref() {
            listener();
        }
        match older {
            Pixels::Owned(pixels) => Arc::try_unwrap(pixels).unwrap_or_default(),
            Pixels::Shared(_) => Vec::new(),
        }
    }

    /// Has `listener` called after every new frame. It runs on a device thread, so it should only
    /// pass the news on.
    pub fn on_frame(&self, listener: impl Fn() + Send + Sync + 'static) {
        *self.listener.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(listener));
    }

    /// Which way up the picture is shown, in quarter turns (see `orientation`).
    pub fn turns(&self) -> u8 {
        self.turns.load(Ordering::SeqCst)
    }

    pub fn set_turns(&self, turns: u8) {
        self.turns.store(turns, Ordering::SeqCst);
    }

    /// Lets `look` see the newest frame, as the panel holds it (not turned upright). `None` if the guest has not shown anything yet.
    pub fn with_latest<R>(&self, look: impl FnOnce(&Frame<'_>) -> R) -> Option<R> {
        let latest = self.snapshot();
        (latest.serial != 0).then(|| {
            look(&Frame {
                width: latest.width,
                height: latest.height,
                format: latest.format,
                pixels: latest.pixels.bytes(),
            })
        })
    }

    /// As `with_latest`, and the buffer the frame lies in when it was read into shared memory.
    pub fn with_latest_buffer<R>(
        &self,
        look: impl FnOnce(&Frame<'_>, Option<&Arc<SharedBuffer>>) -> R,
    ) -> Option<R> {
        let latest = self.snapshot();
        (latest.serial != 0).then(|| {
            let buffer = match &latest.pixels {
                Pixels::Shared(buffer) => Some(buffer),
                Pixels::Owned(_) => None,
            };
            look(
                &Frame {
                    width: latest.width,
                    height: latest.height,
                    format: latest.format,
                    pixels: latest.pixels.bytes(),
                },
                buffer,
            )
        })
    }

    /// Lets frames be read into memory the GPU samples as it lies, from `pool`; `None` stops it.
    pub fn set_shared(&self, pool: Option<Arc<SharedPool>>) {
        *lock(&self.shared) = pool;
    }

    /// Saves the newest frame as a PNG and returns its number, or zero, saving nothing, if the guest
    /// has not shown anything yet.
    fn save_latest(&self, path: &Path) -> std::io::Result<u64> {
        let (image, serial) = {
            let latest = self.snapshot();
            let panel = Frame {
                width: latest.width,
                height: latest.height,
                format: latest.format,
                pixels: latest.pixels.bytes(),
            };
            // Saved upright, as the window shows it.
            let (width, height, pixels) = crate::orientation::upright(&panel, self.turns());
            let image = png(&Frame {
                width,
                height,
                format: latest.format,
                pixels: &pixels,
            });
            (image, latest.serial)
        };
        let Some(image) = image else {
            return Ok(0);
        };
        // Written beside the target and renamed over it, so a viewer never reads half a file.
        let partial = path.with_extension(copy::FRAME_DUMP_PARTIAL_EXTENSION);
        std::fs::write(&partial, image)?;
        std::fs::rename(&partial, path)?;
        Ok(serial)
    }

    /// Saves the newest frame as a PNG. `Ok(false)` if the guest has not shown anything yet.
    pub fn save(&self, path: &Path) -> std::io::Result<bool> {
        Ok(self.save_latest(path)? != 0)
    }

    /// The newest frame, the lock held only long enough to share it.
    fn snapshot(&self) -> Latest {
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn serial(&self) -> u64 {
        let latest = self.latest.lock().unwrap_or_else(PoisonError::into_inner);
        latest.serial
    }
}

/// Keeps the file at `path` showing what `screen` shows, checking every `FRAME_DUMP_INTERVAL`, for as
/// long as the screen exists.
pub fn spawn_dump(screen: &Arc<Screen>, path: PathBuf) -> std::io::Result<()> {
    let screen = Arc::downgrade(screen);
    let dump = move || {
        let mut saved = 0;
        loop {
            std::thread::sleep(FRAME_DUMP_INTERVAL);
            let Some(screen) = screen.upgrade() else {
                break;
            };
            if screen.serial() == saved {
                continue;
            }
            match screen.save_latest(&path) {
                Ok(serial) => saved = serial,
                Err(err) => log::warn!("{}", copy::file(&path, &err)),
            }
        }
    };
    std::thread::Builder::new()
        .name(copy::FRAME_DUMP_THREAD.into())
        .spawn(dump)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::*;

    const FORMAT_BGRX: u32 = 2;
    const FORMAT_RGBA: u32 = 67;

    fn frame(format: u32, width: u32, height: u32, pixels: &[u8]) -> Frame<'_> {
        Frame {
            width,
            height,
            format,
            pixels,
        }
    }

    #[test]
    fn screen_saves_whichever_frame_came_last() {
        let screen = Screen::default();
        let path =
            std::env::temp_dir().join(format!("proteus-screen-test-{}.png", std::process::id()));
        assert!(!screen.save(&path).unwrap());
        assert!(!path.exists());

        let first = [9, 9, 9, 0];
        let last = [1, 2, 3, 0];
        screen.present(&frame(FORMAT_RGBA, 1, 1, &first));
        screen.present(&frame(FORMAT_RGBA, 1, 1, &last));
        assert_eq!(screen.serial(), 2);
        assert_eq!(screen.save_latest(&path).unwrap(), 2);
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(saved, png(&frame(FORMAT_RGBA, 1, 1, &last)).unwrap());
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn frames_read_into_shared_memory_are_shown_and_left_alone_while_held() {
        use crate::config::SHARED_FRAME_SLOTS;

        let screen = Screen::default();
        let Some(pool) = SharedPool::new() else {
            eprintln!("no GPU; test skipped");
            return;
        };
        assert!(screen.slot(64, 32).is_none(), "no pool offered yet");
        screen.set_shared(Some(Arc::new(pool)));
        let show = |fill: u8| {
            let mut slot = screen.slot(64, 32).unwrap();
            slot.bytes().fill(fill);
            screen.present_slot(slot, FORMAT_BGRX);
        };
        show(7);
        let seen = screen.with_latest(|frame| {
            (
                frame.width,
                frame.height,
                frame.format,
                frame.pixels.to_vec(),
            )
        });
        assert_eq!(seen, Some((64, 32, FORMAT_BGRX, vec![7; 64 * 32 * 4])));
        // A reader holding the frame keeps it intact however many frames follow.
        let held = screen
            .with_latest_buffer(|_, buffer| buffer.cloned())
            .unwrap()
            .expect("the frame is in shared memory");
        for _ in 0..SHARED_FRAME_SLOTS * 2 {
            show(9);
        }
        assert!(held.bytes().iter().all(|byte| *byte == 7));
        assert_eq!(screen.serial(), 1 + SHARED_FRAME_SLOTS as u64 * 2);
        // When every buffer is held, the device falls back to a `Vec` instead of waiting.
        let lent: Vec<_> = std::iter::from_fn(|| screen.slot(64, 32)).collect();
        assert!(
            lent.len() < SHARED_FRAME_SLOTS,
            "the shown frames are not lent"
        );
        drop(lent);
        assert!(
            screen.slot(64, 32).is_some(),
            "an unshown slot is lent again"
        );
        screen.set_shared(None);
        assert!(screen.slot(64, 32).is_none());
    }

    #[test]
    fn adler32_matches_the_published_check_value() {
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn channels_are_reordered_to_rgb() {
        // One pixel that is red=1, green=2, blue=3 in each layout.
        assert_eq!(
            channel_order(FORMAT_BGRX).map(|at| [3, 2, 1, 0][at]),
            [1, 2, 3]
        );
        assert_eq!(channel_order(4).map(|at| [0, 1, 2, 3][at]), [1, 2, 3]);
        assert_eq!(channel_order(68).map(|at| [0, 3, 2, 1][at]), [1, 2, 3]);
        assert_eq!(
            channel_order(FORMAT_RGBA).map(|at| [1, 2, 3, 0][at]),
            [1, 2, 3]
        );
    }

    #[test]
    fn png_has_the_expected_chunks_and_pixel_bytes() {
        // 2x1, BGRX: a red pixel then a blue one.
        let image = png(&frame(FORMAT_BGRX, 2, 1, &[0, 0, 255, 0, 255, 0, 0, 0])).unwrap();
        assert_eq!(&image[..8], PNG_SIGNATURE);
        assert_eq!(&image[12..16], b"IHDR");
        assert_eq!(&image[16..24], [0, 0, 0, 2, 0, 0, 0, 1]);
        let idat = image
            .windows(4)
            .position(|window| window == b"IDAT")
            .unwrap();
        // zlib header, final stored block of 7 bytes, then filter byte and two RGB pixels.
        assert_eq!(
            &image[idat + 4..idat + 11],
            [0x78, 0x01, 1, 7, 0, 0xf8, 0xff]
        );
        assert_eq!(&image[idat + 11..idat + 18], [0, 255, 0, 0, 0, 0, 255]);
        assert_eq!(&image[image.len() - 8..image.len() - 4], b"IEND");
    }

    #[test]
    fn frames_whose_size_does_not_match_their_pixels_are_refused() {
        assert!(png(&frame(FORMAT_BGRX, 2, 2, &[0; 12])).is_none());
        assert!(png(&frame(FORMAT_BGRX, 0, 0, &[])).is_none());
    }

    /// A PNG reader that was not written here must accept the file and see the same pixels.
    #[test]
    fn python_reads_back_a_frame_larger_than_one_deflate_block() {
        let (width, height) = (200u32, 150u32);
        let pixels: Vec<u8> = (0..width * height)
            .flat_map(|n| [(n % 251) as u8, (n % 241) as u8, (n % 239) as u8, 0])
            .collect();
        let image = png(&frame(FORMAT_RGBA, width, height, &pixels)).unwrap();
        // Checks every chunk's CRC, inflates the data, and prints width, height and a digest of the rows.
        let script = "import sys,zlib,struct,hashlib\n\
            d=sys.stdin.buffer.read(); assert d[:8]==bytes([137,80,78,71,13,10,26,10]); p=8; idat=b''\n\
            while p<len(d):\n\
            \x20n,=struct.unpack('>I',d[p:p+4]); t=d[p+4:p+8]; b=d[p+8:p+8+n]\n\
            \x20assert zlib.crc32(t+b)==struct.unpack('>I',d[p+8+n:p+12+n])[0]\n\
            \x20if t==b'IHDR': w,h=struct.unpack('>II',b[:8])\n\
            \x20if t==b'IDAT': idat+=b\n\
            \x20p+=12+n\n\
            raw=zlib.decompress(idat); assert len(raw)==h*(1+3*w)\n\
            rows=b''.join(raw[i*(1+3*w)+1:(i+1)*(1+3*w)] for i in range(h))\n\
            print(w,h,hashlib.sha256(rows).hexdigest())";
        let child = Command::new("python3")
            .args(["-I", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        let Ok(mut child) = child else {
            eprintln!("python3 is not installed; PNG compatibility test skipped");
            return;
        };
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || stdin.write_all(&image));
        let output = child.wait_with_output().unwrap();
        writer.join().unwrap().unwrap();
        assert!(output.status.success());
        let rgb: Vec<u8> = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect();
        let digest = Command::new("python3")
            .args([
                "-I",
                "-c",
                "import sys,hashlib;print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().unwrap().write_all(&rgb)?;
                child.wait_with_output()
            })
            .unwrap();
        let expected = format!(
            "{width} {height} {}",
            String::from_utf8_lossy(&digest.stdout).trim()
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    }
}
