//! virtio-snd with one playback stream: stereo, 16-bit, 48 kHz.
//!
//! The guest sends its sound a period at a time and waits for each period to come back before it
//! counts it as played. The device hands a period back only once the host's sound output has taken
//! it, so the guest keeps time with what is actually heard.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use virtio_bindings::virtio_ids::VIRTIO_ID_SOUND;
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;

use super::{Activate, DeviceError, VirtioDevice, VirtioInterrupt};
use crate::copy;

/// Frames per second of the stream, and bytes in a frame (two channels of two bytes).
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
pub const BYTES_PER_FRAME: usize = 4;

// Request codes, as the specification numbers them.
const R_JACK_INFO: u32 = 1;
const R_PCM_INFO: u32 = 0x0100;
const R_PCM_SET_PARAMS: u32 = 0x0101;
const R_PCM_PREPARE: u32 = 0x0102;
const R_PCM_RELEASE: u32 = 0x0103;
const R_PCM_START: u32 = 0x0104;
const R_PCM_STOP: u32 = 0x0105;
const R_CHMAP_INFO: u32 = 0x0200;
const S_OK: u32 = 0x8000;
const S_BAD_MSG: u32 = 0x8001;
const S_NOT_SUPP: u32 = 0x8002;

/// Bit numbers of the one sample format and rate the stream has, in the bitmaps of a stream's
/// description, and their values in a "set parameters" request.
const FORMAT_S16: u8 = 5;
const RATE_48000: u8 = 7;
const DIRECTION_OUTPUT: u8 = 0;
const CHANNEL_FRONT_LEFT: u8 = 3;
const CHANNEL_FRONT_RIGHT: u8 = 4;

/// A stream's description: function group (u32), features (u32), formats (u64), rates (u64),
/// direction, fewest and most channels, padding.
const PCM_INFO_LEN: usize = 32;
/// A channel map's description: function group (u32), direction, channel count, 18 positions.
const CHMAP_INFO_LEN: usize = 24;
const CHMAP_POSITIONS_AT: usize = 6;
/// The longest request: "set parameters".
const REQUEST_MAX: usize = 24;
/// A playback buffer ends with a status the device fills in: a result (u32) and a latency (u32).
const XFER_STATUS_LEN: usize = 8;
/// The most sound taken from one playback buffer: more than a second of it.
const PERIOD_MAX: usize = 256 << 10;

const STREAMS: u32 = 1;
const CHMAPS: u32 = 1;
/// Configuration space: number of jacks, streams and channel maps (u32 each).
const CONFIG_LEN: usize = 12;
const QUEUE_SIZES: [u16; 4] = [64, 64, 64, 64];
const CONTROL_QUEUE: usize = 0;
const TX_QUEUE: usize = 2;

/// Where the guest's sound goes.
pub trait AudioSink: Send {
    /// Plays stereo 16-bit little-endian frames at `SAMPLE_RATE`. Returns once the sink has taken
    /// them, which should take about as long as they last: that wait is what paces the guest.
    fn play(&mut self, frames: &[u8]);
}

/// A sink nobody hears, which still takes as long over the sound as playing it would.
pub struct Silence {
    /// When the sound taken so far will have finished.
    played_until: Instant,
}

impl Default for Silence {
    fn default() -> Self {
        Self {
            played_until: Instant::now(),
        }
    }
}

impl AudioSink for Silence {
    fn play(&mut self, frames: &[u8]) {
        let lasts = Duration::from_secs((frames.len() / BYTES_PER_FRAME) as u64) / SAMPLE_RATE;
        // After a pause the clock starts again from now, not from where it stopped.
        let now = Instant::now();
        self.played_until = self.played_until.max(now) + lasts;
        thread::sleep(self.played_until - now);
    }
}

fn word(bytes: &[u8], at: usize) -> u32 {
    let mut raw = [0u8; 4];
    if let Some(found) = bytes.get(at..at + 4) {
        raw.copy_from_slice(found);
    }
    u32::from_le_bytes(raw)
}

/// The descriptions of `count` items starting at `start`, each made by `describe` and cut or padded
/// to the size the driver asked for. `None` if the driver asks for items the device does not have.
fn descriptions(
    request: &[u8],
    have: u32,
    full_len: usize,
    describe: impl Fn() -> Vec<u8>,
) -> Option<Vec<u8>> {
    let (start, count, size) = (word(request, 4), word(request, 8), word(request, 12));
    if start.checked_add(count)? > have || size as usize > full_len {
        return None;
    }
    let mut one = describe();
    one.resize(size as usize, 0);
    Some(one.repeat(count as usize))
}

fn pcm_info() -> Vec<u8> {
    let mut info = vec![0u8; PCM_INFO_LEN];
    info[8..16].copy_from_slice(&(1u64 << FORMAT_S16).to_le_bytes());
    info[16..24].copy_from_slice(&(1u64 << RATE_48000).to_le_bytes());
    info[24] = DIRECTION_OUTPUT;
    info[25] = CHANNELS;
    info[26] = CHANNELS;
    info
}

fn chmap_info() -> Vec<u8> {
    let mut info = vec![0u8; CHMAP_INFO_LEN];
    info[4] = DIRECTION_OUTPUT;
    info[5] = CHANNELS;
    info[CHMAP_POSITIONS_AT] = CHANNEL_FRONT_LEFT;
    info[CHMAP_POSITIONS_AT + 1] = CHANNEL_FRONT_RIGHT;
    info
}

/// The playback stream, as the driver has set it up.
#[derive(Default)]
struct Stream {
    /// Between "start" and "stop": sound that arrives is played. Otherwise it is handed back unplayed.
    running: bool,
}

impl Stream {
    /// The answer to one control request: a status, then whatever the request asked for.
    fn control(&mut self, request: &[u8]) -> Vec<u8> {
        let stream_is_ours = word(request, 4) < STREAMS;
        let (status, body) = match word(request, 0) {
            // There are no jacks; asking about none of them is fine.
            R_JACK_INFO => match descriptions(request, 0, 0, Vec::new) {
                Some(body) => (S_OK, body),
                None => (S_BAD_MSG, Vec::new()),
            },
            R_PCM_INFO => match descriptions(request, STREAMS, PCM_INFO_LEN, pcm_info) {
                Some(body) => (S_OK, body),
                None => (S_BAD_MSG, Vec::new()),
            },
            R_CHMAP_INFO => match descriptions(request, CHMAPS, CHMAP_INFO_LEN, chmap_info) {
                Some(body) => (S_OK, body),
                None => (S_BAD_MSG, Vec::new()),
            },
            _ if !stream_is_ours => (S_BAD_MSG, Vec::new()),
            R_PCM_SET_PARAMS => {
                // Buffer and period sizes (words 2 and 3) are the driver's business; the device
                // takes periods as they come. Channels, format and rate follow the feature word.
                let wanted = request.get(20..23);
                let supported = wanted == Some(&[CHANNELS, FORMAT_S16, RATE_48000]);
                (if supported { S_OK } else { S_NOT_SUPP }, Vec::new())
            }
            R_PCM_PREPARE => (S_OK, Vec::new()),
            R_PCM_START => {
                self.running = true;
                (S_OK, Vec::new())
            }
            R_PCM_STOP | R_PCM_RELEASE => {
                self.running = false;
                (S_OK, Vec::new())
            }
            _ => (S_NOT_SUPP, Vec::new()),
        };
        let mut response = status.to_le_bytes().to_vec();
        response.extend(body);
        response
    }
}

/// Answers every pending control request. Returns whether anything was used.
fn drain_control(
    queue: &mut Queue,
    mem: &GuestMemoryMmap,
    stream: &mut Stream,
) -> Result<bool, virtio_queue::Error> {
    let mut used_any = false;
    while let Some(chain) = queue.pop_descriptor_chain(mem) {
        let head = chain.head_index();
        let mut request = Vec::new();
        let read = chain.clone().reader(mem).ok().and_then(|reader| {
            reader
                .take(REQUEST_MAX as u64)
                .read_to_end(&mut request)
                .ok()
        });
        // A chain with no request in it, or with no room for an answer, is returned unused.
        let written = read
            .and_then(|_| {
                let response = stream.control(&request);
                let mut writer = chain.writer(mem).ok()?;
                writer.write_all(&response).ok()?;
                Some(writer.bytes_written())
            })
            .unwrap_or(0);
        queue.add_used(mem, head, written as u32)?;
        used_any = true;
    }
    Ok(used_any)
}

/// Takes one playback buffer, plays it if the stream is running, and hands it back.
/// Returns whether there was one.
fn play_one(
    queue: &mut Queue,
    mem: &GuestMemoryMmap,
    stream: &Stream,
    sink: &mut dyn AudioSink,
) -> Result<bool, virtio_queue::Error> {
    let Some(chain) = queue.pop_descriptor_chain(mem) else {
        return Ok(false);
    };
    let head = chain.head_index();
    let mut sound = Vec::new();
    let mut header = [0u8; 4];
    let read = chain.clone().reader(mem).ok().and_then(|mut reader| {
        reader.read_exact(&mut header).ok()?;
        reader.take(PERIOD_MAX as u64).read_to_end(&mut sound).ok()
    });
    let ours = read.is_some() && u32::from_le_bytes(header) < STREAMS;
    if ours && stream.running {
        // Whole frames only; a stray byte at the end would shift every later sample.
        sink.play(&sound[..sound.len() - sound.len() % BYTES_PER_FRAME]);
    }
    let status = if ours { S_OK } else { S_BAD_MSG };
    let mut answer = [0u8; XFER_STATUS_LEN];
    answer[..4].copy_from_slice(&status.to_le_bytes());
    let written = chain
        .writer(mem)
        .ok()
        .and_then(|mut writer| writer.write_all(&answer).ok())
        .map_or(0, |()| XFER_STATUS_LEN);
    queue.add_used(mem, head, written as u32)?;
    Ok(true)
}

enum Wake {
    Queue,
    Stop,
}

fn serve(
    mut sink: Box<dyn AudioSink>,
    mut queues: Vec<Queue>,
    mem: GuestMemoryMmap,
    irq: std::sync::Arc<VirtioInterrupt>,
    wake: Receiver<Wake>,
) -> Box<dyn AudioSink> {
    let mut stream = Stream::default();
    while let Ok(Wake::Queue) = wake.recv() {
        // Control requests are looked at again after every period, so that "stop" does not have
        // to wait behind all the sound already queued.
        loop {
            let step =
                drain_control(&mut queues[CONTROL_QUEUE], &mem, &mut stream).and_then(|answered| {
                    let played = play_one(&mut queues[TX_QUEUE], &mem, &stream, sink.as_mut())?;
                    Ok((answered, played))
                });
            match step {
                Ok((answered, played)) => {
                    if answered || played {
                        irq.signal_used();
                    }
                    if !played {
                        break;
                    }
                }
                Err(err) => {
                    log::warn!("{}", copy::virtio_queue_error(&err));
                    break;
                }
            }
        }
    }
    sink
}

type Worker = (Sender<Wake>, JoinHandle<Box<dyn AudioSink>>);

pub struct Sound {
    /// The sink, while no worker has it.
    sink: Option<Box<dyn AudioSink>>,
    worker: Option<Worker>,
}

impl Sound {
    pub fn new(sink: Box<dyn AudioSink>) -> Self {
        Self {
            sink: Some(sink),
            worker: None,
        }
    }
}

impl VirtioDevice for Sound {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_SOUND
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        0
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut config = [0u8; CONFIG_LEN];
        config[4..8].copy_from_slice(&STREAMS.to_le_bytes());
        config[8..12].copy_from_slice(&CHMAPS.to_le_bytes());
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
        if ctx.queues.len() != QUEUE_SIZES.len() {
            return Err(DeviceError::MissingQueue);
        }
        let Some(sink) = self.sink.take() else {
            return Err(DeviceError::AlreadyActive);
        };
        let (wake, wakes) = mpsc::channel();
        let thread = thread::Builder::new()
            .name(copy::SOUND_WORKER_NAME.into())
            .spawn(move || serve(sink, ctx.queues, ctx.mem, ctx.irq, wakes))?;
        self.worker = Some((wake, thread));
        Ok(())
    }

    fn queue_notify(&mut self, _queue: u16) {
        if let Some((wake, _)) = &self.worker {
            let _ = wake.send(Wake::Queue);
        }
    }

    fn reset(&mut self) {
        if let Some((wake, thread)) = self.worker.take() {
            let _ = wake.send(Wake::Stop);
            match thread.join() {
                Ok(sink) => self.sink = Some(sink),
                Err(_) => log::error!("{}", copy::SOUND_WORKER_GONE),
            }
        }
    }
}

impl Drop for Sound {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use virtio_bindings::virtio_ring::VRING_DESC_F_WRITE;
    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::desc::split::Descriptor;
    use virtio_queue::mock::MockSplitQueue;
    use vm_memory::{Address, Bytes, GuestAddress};

    use super::*;

    const MEM_SIZE: usize = 2 << 20;
    const REQUEST_AT: u64 = 0x10_0000;
    const RESPONSE_AT: u64 = 0x11_0000;
    const QUEUE_LEN: u16 = 16;
    const NEXT: u16 = 1;

    /// A sink that records what it was asked to play.
    #[derive(Clone, Default)]
    struct Heard(Arc<Mutex<Vec<u8>>>);

    impl AudioSink for Heard {
        fn play(&mut self, frames: &[u8]) {
            self.0.lock().unwrap().extend_from_slice(frames);
        }
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    fn set_params(stream: u32, channels: u8, format: u8, rate: u8) -> Vec<u8> {
        let mut request = words(&[R_PCM_SET_PARAMS, stream, 19_200, 1920, 0]);
        request.extend([channels, format, rate, 0]);
        request
    }

    fn status(response: &[u8]) -> u32 {
        word(response, 0)
    }

    #[test]
    fn stream_description_offers_exactly_stereo_s16_at_48k() {
        let mut stream = Stream::default();
        let response = stream.control(&words(&[R_PCM_INFO, 0, 1, PCM_INFO_LEN as u32]));
        assert_eq!(status(&response), S_OK);
        let info = &response[4..];
        assert_eq!(info.len(), PCM_INFO_LEN);
        assert_eq!(u64::from_le_bytes(info[8..16].try_into().unwrap()), 1 << 5);
        assert_eq!(u64::from_le_bytes(info[16..24].try_into().unwrap()), 1 << 7);
        assert_eq!(info[24..27], [DIRECTION_OUTPUT, 2, 2]);

        let response = stream.control(&words(&[R_CHMAP_INFO, 0, 1, CHMAP_INFO_LEN as u32]));
        assert_eq!(status(&response), S_OK);
        assert_eq!(response[4 + 4..4 + 8], [DIRECTION_OUTPUT, 2, 3, 4]);

        // No jacks: asking for none is fine, asking for one is not.
        assert_eq!(
            status(&stream.control(&words(&[R_JACK_INFO, 0, 0, 0]))),
            S_OK
        );
        assert_eq!(
            status(&stream.control(&words(&[R_JACK_INFO, 0, 1, 24]))),
            S_BAD_MSG
        );
        // A second stream does not exist, and an item cannot be larger than it is.
        assert_eq!(
            status(&stream.control(&words(&[R_PCM_INFO, 1, 1, PCM_INFO_LEN as u32]))),
            S_BAD_MSG
        );
        assert_eq!(
            status(&stream.control(&words(&[R_PCM_INFO, 0, 1, 4096]))),
            S_BAD_MSG
        );
        assert_eq!(
            status(&stream.control(&words(&[R_PCM_INFO, u32::MAX, 2, 32]))),
            S_BAD_MSG
        );
    }

    #[test]
    fn only_the_offered_parameters_are_accepted_and_start_and_stop_switch_playing() {
        let mut stream = Stream::default();
        let ok = set_params(0, CHANNELS, FORMAT_S16, RATE_48000);
        assert_eq!(status(&stream.control(&ok)), S_OK);
        const RATE_44100: u8 = 6;
        let other_rate = set_params(0, CHANNELS, FORMAT_S16, RATE_44100);
        assert_eq!(status(&stream.control(&other_rate)), S_NOT_SUPP);
        assert_eq!(
            status(&stream.control(&set_params(0, 1, FORMAT_S16, RATE_48000))),
            S_NOT_SUPP
        );
        assert_eq!(
            status(&stream.control(&set_params(3, CHANNELS, FORMAT_S16, RATE_48000))),
            S_BAD_MSG
        );
        assert_eq!(status(&stream.control(&ok[..10])), S_NOT_SUPP);

        assert!(!stream.running);
        assert_eq!(status(&stream.control(&words(&[R_PCM_PREPARE, 0]))), S_OK);
        assert_eq!(status(&stream.control(&words(&[R_PCM_START, 0]))), S_OK);
        assert!(stream.running);
        assert_eq!(status(&stream.control(&words(&[R_PCM_STOP, 0]))), S_OK);
        assert!(!stream.running);
        assert_eq!(
            status(&stream.control(&words(&[R_PCM_START, 1]))),
            S_BAD_MSG
        );
        assert_eq!(status(&stream.control(&words(&[0x7777, 0]))), S_NOT_SUPP);
        assert_eq!(status(&stream.control(&[])), S_NOT_SUPP);
    }

    #[test]
    fn sound_is_played_only_while_the_stream_runs_and_every_buffer_comes_back() {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap();
        let mock = MockSplitQueue::create(&mem, GuestAddress(0), QUEUE_LEN);
        let write = VRING_DESC_F_WRITE as u16;
        // One buffer: stream number and nine bytes of sound (two frames and a stray byte), then
        // room for the status.
        let mut buffer = words(&[0]);
        buffer.extend([1, 2, 3, 4, 5, 6, 7, 8, 9]);
        mem.write_slice(&buffer, GuestAddress(REQUEST_AT)).unwrap();
        let chain = |index: u16| {
            [
                RawDescriptor::from(Descriptor::new(
                    REQUEST_AT,
                    buffer.len() as u32,
                    NEXT,
                    index + 1,
                )),
                RawDescriptor::from(Descriptor::new(
                    RESPONSE_AT,
                    XFER_STATUS_LEN as u32,
                    write,
                    0,
                )),
            ]
        };
        mock.add_desc_chains(&chain(0), 0).unwrap();
        mock.add_desc_chains(&chain(2), 2).unwrap();
        let mut queue: Queue = mock.create_queue().unwrap();
        let heard = Heard::default();
        let mut sink = heard.clone();

        // Stopped: the buffer comes back at once, unplayed.
        let mut stream = Stream::default();
        assert!(play_one(&mut queue, &mem, &stream, &mut sink).unwrap());
        assert!(heard.0.lock().unwrap().is_empty());

        stream.running = true;
        assert!(play_one(&mut queue, &mem, &stream, &mut sink).unwrap());
        assert_eq!(*heard.0.lock().unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(!play_one(&mut queue, &mem, &stream, &mut sink).unwrap());

        let used: u16 = mem.read_obj(mock.used_addr().unchecked_add(2)).unwrap();
        assert_eq!(used, 2);
        let answered: u32 = mem.read_obj(GuestAddress(RESPONSE_AT)).unwrap();
        assert_eq!(answered, S_OK);
    }

    #[test]
    fn control_queue_answers_in_the_guests_buffer() {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap();
        let mock = MockSplitQueue::create(&mem, GuestAddress(0), QUEUE_LEN);
        let request = words(&[R_PCM_INFO, 0, 1, PCM_INFO_LEN as u32]);
        mem.write_slice(&request, GuestAddress(REQUEST_AT)).unwrap();
        let write = VRING_DESC_F_WRITE as u16;
        let response_len = (4 + PCM_INFO_LEN) as u32;
        mock.add_desc_chains(
            &[
                RawDescriptor::from(Descriptor::new(REQUEST_AT, request.len() as u32, NEXT, 1)),
                RawDescriptor::from(Descriptor::new(RESPONSE_AT, response_len, write, 0)),
            ],
            0,
        )
        .unwrap();
        let mut queue: Queue = mock.create_queue().unwrap();
        let mut stream = Stream::default();
        assert!(drain_control(&mut queue, &mem, &mut stream).unwrap());
        let status: u32 = mem.read_obj(GuestAddress(RESPONSE_AT)).unwrap();
        assert_eq!(status, S_OK);
        let used_len: u32 = mem.read_obj(mock.used_addr().unchecked_add(8)).unwrap();
        assert_eq!(used_len, response_len);
    }

    #[test]
    fn silence_takes_as_long_as_the_sound_would() {
        let mut silence = Silence::default();
        let tenth_of_a_second = vec![0u8; SAMPLE_RATE as usize / 10 * BYTES_PER_FRAME];
        let before = Instant::now();
        silence.play(&tenth_of_a_second);
        let took = before.elapsed();
        assert!(took >= Duration::from_millis(95), "{took:?}");
        assert!(took < Duration::from_millis(400), "{took:?}");
    }

    #[test]
    fn config_space_reports_one_stream_and_one_channel_map() {
        let device = Sound::new(Box::new(Silence::default()));
        let mut config = [0xffu8; CONFIG_LEN];
        device.read_config(0, &mut config);
        assert_eq!(config, [0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(device.queue_max_sizes().len(), 4);
    }
}
