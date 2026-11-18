//! The host's sound output.
//!
//! The guest's sound card hands its sound to a `Speaker`. The first time there is any, the speaker
//! opens the host's default output; from then on it feeds a small ring of samples that the host's
//! audio thread drains. The ring being full is what makes the guest wait, so the guest plays at the
//! pace of the host's real sound hardware.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use devices::virtio::snd::{AudioSink, BYTES_PER_FRAME, CHANNELS, SAMPLE_RATE, Silence};

use crate::config::{AUDIO_RING_FRAMES, AUDIO_RING_POLL, AUDIO_STALL_MARGIN};
use crate::copy;

const BYTES_PER_SAMPLE: usize = 2;
/// One past the largest sample value, for scaling samples to the range -1.0 to 1.0.
const SAMPLE_SCALE: f32 = 32_768.0;

enum Output {
    /// No sound has been played yet.
    Unopened,
    Open(rtrb::Producer<i16>),
    /// The host has no usable sound output; the sound is dropped at the pace it would have played.
    Silent(Silence),
}

/// Where the guest's sound goes on this host. Cheap to clone; every clone feeds the same output.
#[derive(Clone)]
pub struct Speaker(Arc<Mutex<Output>>);

impl Default for Speaker {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Output::Unopened)))
    }
}

/// Opens the host's default output for stereo sound at the guest's rate, fed from `samples`.
fn open(mut samples: rtrb::Consumer<i16>) -> Result<cpal::Stream, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or(copy::AUDIO_NO_DEVICE)?;
    let config = cpal::StreamConfig {
        channels: CHANNELS.into(),
        sample_rate: SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Default,
    };
    let fill = move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
        for slot in out {
            // A ring that has run dry plays as silence, not as a stall.
            *slot = samples
                .pop()
                .map_or(0.0, |sample| f32::from(sample) / SAMPLE_SCALE);
        }
    };
    let failed = |err| log::warn!("{}", copy::audio_failed(&err));
    let stream = device
        .build_output_stream(config, fill, failed, None)
        .map_err(|err| err.to_string())?;
    stream.play().map_err(|err| err.to_string())?;
    Ok(stream)
}

/// Starts the thread that owns the host's output stream (the stream cannot change threads) and
/// returns the feeding end of its ring.
fn start() -> Result<rtrb::Producer<i16>, String> {
    let (feed, drain) = rtrb::RingBuffer::new(AUDIO_RING_FRAMES * usize::from(CHANNELS));
    let (opened, result) = mpsc::channel();
    let own = move || match open(drain) {
        Ok(stream) => {
            let _ = opened.send(Ok(()));
            // The stream plays for as long as it exists, which is for as long as the program runs.
            loop {
                thread::park();
                let _ = &stream;
            }
        }
        Err(why) => {
            let _ = opened.send(Err(why));
        }
    };
    thread::Builder::new()
        .name(copy::AUDIO_THREAD.into())
        .spawn(own)
        .map_err(|err| err.to_string())?;
    result.recv().map_err(|err| err.to_string())??;
    Ok(feed)
}

/// Puts `frames` into the ring, waiting for room. If the ring does not drain (the output has gone
/// away), what is left is dropped once the sound would have been over anyway.
fn feed(ring: &mut rtrb::Producer<i16>, frames: &[u8]) {
    let lasts = Duration::from_secs((frames.len() / BYTES_PER_FRAME) as u64) / SAMPLE_RATE;
    let give_up = Instant::now() + lasts + AUDIO_STALL_MARGIN;
    let (samples, _) = frames.as_chunks::<BYTES_PER_SAMPLE>();
    for sample in samples {
        let sample = i16::from_le_bytes(*sample);
        while ring.push(sample).is_err() {
            if Instant::now() >= give_up {
                return;
            }
            thread::sleep(AUDIO_RING_POLL);
        }
    }
}

impl AudioSink for Speaker {
    fn play(&mut self, frames: &[u8]) {
        let mut output = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*output, Output::Unopened) {
            *output = match start() {
                Ok(ring) => {
                    log::info!("{}", copy::AUDIO_OPENED);
                    Output::Open(ring)
                }
                Err(why) => {
                    log::warn!("{}", copy::audio_failed(&why));
                    Output::Silent(Silence::default())
                }
            };
        }
        match &mut *output {
            Output::Open(ring) => feed(ring, frames),
            Output::Silent(silence) => silence.play(frames),
            Output::Unopened => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(samples: &[i16]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    #[test]
    fn samples_reach_the_ring_in_order_and_a_stray_byte_is_ignored() {
        let (mut feed_end, mut drain) = rtrb::RingBuffer::new(8);
        let mut sound = frames(&[1, -2, 300, -32768]);
        sound.push(0x7f);
        feed(&mut feed_end, &sound);
        let heard: Vec<i16> = std::iter::from_fn(|| drain.pop().ok()).collect();
        assert_eq!(heard, [1, -2, 300, -32768]);
    }

    #[test]
    fn a_ring_that_never_drains_costs_about_the_sounds_length_then_drops_it() {
        let (mut feed_end, _drain) = rtrb::RingBuffer::new(2);
        let before = Instant::now();
        feed(&mut feed_end, &frames(&[1, 2, 3, 4, 5, 6]));
        let took = before.elapsed();
        assert!(took >= AUDIO_STALL_MARGIN, "{took:?}");
        assert!(took < AUDIO_STALL_MARGIN * 4, "{took:?}");
    }

    #[test]
    fn a_speaker_nobody_plays_through_opens_nothing() {
        let speaker = Speaker::default();
        assert!(matches!(*speaker.0.lock().unwrap(), Output::Unopened));
    }
}
