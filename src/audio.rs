//! Plays the core's audio on the default output device.
//!
//! `push` resamples to the device rate on the caller's thread and writes into a lock-free
//! ring. The device callback only reads the ring, so it never allocates or locks.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

/// Queue length the callback trims back to when the queue passes `MAX_QUEUE`.
const TARGET_QUEUE: Duration = Duration::from_millis(60);
/// Queue length past which the callback drops the oldest audio.
const MAX_QUEUE: Duration = Duration::from_millis(100);
/// After an underrun the callback stays silent until this much audio is queued again,
/// so a slow producer gives one gap instead of constant crackle.
const PRIME: Duration = Duration::from_millis(40);
/// Fade length used to hide the jump into and out of silence.
const FADE: Duration = Duration::from_millis(5);

/// Counters since `Audio::new`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioStats {
    /// Callbacks that ran out of audio while playing.
    pub underruns: u64,
    /// Stereo frames thrown away because the queue was too long, at the device rate.
    pub dropped_frames: u64,
    /// Errors cpal reported on the stream.
    pub stream_errors: u64,
}

#[derive(Default)]
struct Counters {
    underruns: AtomicU64,
    dropped_frames: AtomicU64,
    stream_errors: AtomicU64,
}

pub struct Audio {
    _stream: Stream,
    producer: Producer<f32>,
    resampler: Linear,
    scratch: Vec<f32>,
    device_rate: u32,
    counters: Arc<Counters>,
}

impl Audio {
    /// `sample_rate` is the core's rate; resampling to the device rate happens here.
    ///
    /// # Errors
    ///
    /// Fails when there is no output device or it cannot open a stream; the caller can run muted.
    pub fn new(sample_rate: f64) -> Result<Audio> {
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            bail!("invalid core sample rate {sample_rate}");
        }
        let device = cpal::default_host()
            .default_output_device()
            .context("no audio output device")?;
        let supported = device
            .default_output_config()
            .context("reading the output device's default config")?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let device_rate = config.sample_rate.0;

        let frames = |d: Duration| (d.as_secs_f64() * f64::from(device_rate)) as usize;
        // Stereo samples; twice the cap so the producer only fills it if the callback stalls.
        let (producer, consumer) = RingBuffer::new(frames(MAX_QUEUE) * 4);
        let counters = Arc::new(Counters::default());
        let playback = Playback {
            consumer,
            channels: usize::from(config.channels),
            target: frames(TARGET_QUEUE) * 2,
            max: frames(MAX_QUEUE) * 2,
            prime: frames(PRIME) * 2,
            priming: true,
            gain: 0.0,
            gain_step: 1.0 / frames(FADE).max(1) as f32,
            last: [0.0; 2],
            counters: Arc::clone(&counters),
        };

        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, playback),
            SampleFormat::I16 => build::<i16>(&device, &config, playback),
            SampleFormat::U16 => build::<u16>(&device, &config, playback),
            other => bail!("unsupported output sample format {other}"),
        }
        .context("opening the audio output stream")?;
        stream.play().context("starting the audio output stream")?;

        Ok(Audio {
            _stream: stream,
            producer,
            resampler: Linear::new(sample_rate / f64::from(device_rate)),
            scratch: Vec::new(),
            device_rate,
            counters,
        })
    }

    /// Switches to a new core rate, as `SET_SYSTEM_AV_INFO` asks. The stream keeps playing
    /// and audio already queued stays, since the ring holds samples at the device rate. The
    /// resampler keeps its last frame and position, so the waveform does not jump.
    ///
    /// Ignores a rate that is not finite and positive and keeps the current one.
    pub fn set_sample_rate(&mut self, rate: f64) {
        if rate.is_finite() && rate > 0.0 {
            self.resampler.step = rate / f64::from(self.device_rate);
        }
    }

    /// Queues interleaved stereo samples. Drops the oldest audio if the queue is too long.
    pub fn push(&mut self, samples: &[i16]) {
        self.scratch.clear();
        self.resampler.process(samples, &mut self.scratch);
        // The ring only fills if the callback has stalled; then the newest audio goes.
        let free = self.producer.slots() & !1;
        let (written, _) = self
            .producer
            .push_partial_slice(&self.scratch[..self.scratch.len().min(free)]);
        let lost = (self.scratch.len() - written.len()) / 2;
        if lost > 0 {
            self.counters
                .dropped_frames
                .fetch_add(lost as u64, Ordering::Relaxed);
        }
    }

    /// Audio waiting in the ring, not counting the device's own buffer (usually ~10 ms).
    pub fn queued(&self) -> Duration {
        let samples = self.producer.buffer().capacity() - self.producer.slots();
        Duration::from_secs_f64(samples as f64 / 2.0 / f64::from(self.device_rate))
    }

    pub fn device_rate(&self) -> u32 {
        self.device_rate
    }

    pub fn stats(&self) -> AudioStats {
        AudioStats {
            underruns: self.counters.underruns.load(Ordering::Relaxed),
            dropped_frames: self.counters.dropped_frames.load(Ordering::Relaxed),
            stream_errors: self.counters.stream_errors.load(Ordering::Relaxed),
        }
    }
}

fn build<T>(device: &cpal::Device, config: &StreamConfig, mut playback: Playback) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let counters = Arc::clone(&playback.counters);
    let stream = device.build_output_stream(
        config,
        move |out: &mut [T], _| playback.fill(out),
        move |_| {
            counters.stream_errors.fetch_add(1, Ordering::Relaxed);
        },
        None,
    )?;
    Ok(stream)
}

/// State owned by the device callback. Lengths are in samples of the stereo ring.
struct Playback {
    consumer: Consumer<f32>,
    channels: usize,
    target: usize,
    max: usize,
    prime: usize,
    priming: bool,
    gain: f32,
    gain_step: f32,
    last: [f32; 2],
    counters: Arc<Counters>,
}

impl Playback {
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T]) {
        let mut queued = self.consumer.slots();
        if queued > self.max {
            let excess = (queued - self.target) & !1;
            if let Ok(chunk) = self.consumer.read_chunk(excess) {
                chunk.commit_all();
            }
            queued -= excess;
            self.gain = 0.0;
            self.counters
                .dropped_frames
                .fetch_add(excess as u64 / 2, Ordering::Relaxed);
        }
        if self.priming && queued >= self.prime {
            self.priming = false;
        }

        let frames = out.len() / self.channels;
        let mut out_frames = out.chunks_exact_mut(self.channels);
        if !self.priming {
            let take = frames.min(queued / 2);
            let chunk = self
                .consumer
                .read_chunk(take * 2)
                .expect("the consumer is the only reader, so queued samples stay readable");
            let (a, b) = chunk.as_slices();
            // Every write and read moves whole frames and the capacity is even, so neither
            // slice splits a frame.
            for (&frame, out) in a
                .as_chunks()
                .0
                .iter()
                .chain(b.as_chunks().0)
                .zip(&mut out_frames)
            {
                self.gain = (self.gain + self.gain_step).min(1.0);
                self.last = frame;
                write_frame(out, self.last, self.gain);
            }
            chunk.commit_all();
            if take < frames {
                self.priming = true;
                self.counters.underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        for out in out_frames {
            self.gain = (self.gain - self.gain_step).max(0.0);
            write_frame(out, self.last, self.gain);
        }
    }
}

fn write_frame<T: SizedSample + FromSample<f32>>(
    out: &mut [T],
    [left, right]: [f32; 2],
    gain: f32,
) {
    match out {
        [mono] => *mono = T::from_sample((left + right) * 0.5 * gain),
        [l, r, rest @ ..] => {
            *l = T::from_sample(left * gain);
            *r = T::from_sample(right * gain);
            rest.fill(T::EQUILIBRIUM);
        }
        [] => {}
    }
}

/// Streaming linear interpolation between consecutive input frames.
struct Linear {
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame between `prev` (0) and the next input frame (1).
    pos: f64,
    prev: [f32; 2],
}

impl Linear {
    fn new(step: f64) -> Linear {
        Linear {
            step,
            pos: 0.0,
            prev: [0.0; 2],
        }
    }

    fn process(&mut self, input: &[i16], out: &mut Vec<f32>) {
        out.reserve((input.len() as f64 / self.step) as usize + 2);
        for &[left, right] in input.as_chunks().0 {
            let cur = [f32::from(left) / 32768.0, f32::from(right) / 32768.0];
            while self.pos < 1.0 {
                let t = self.pos as f32;
                out.push(self.prev[0] + (cur[0] - self.prev[0]) * t);
                out.push(self.prev[1] + (cur[1] - self.prev[1]) * t);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.prev = cur;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Linear;

    fn check(step: f64, input: &[i16], expected: &[f32]) {
        let mut out = Vec::new();
        Linear::new(step).process(input, &mut out);
        assert_eq!(out, expected);
    }

    #[test]
    fn upsamples_by_two() {
        check(
            0.5,
            &[16384, 0, 16384, 0],
            &[0.0, 0.0, 0.25, 0.0, 0.5, 0.0, 0.5, 0.0],
        );
    }

    #[test]
    fn rate_change_continues_the_waveform() {
        let mut resampler = Linear::new(0.5);
        let mut out = Vec::new();
        resampler.process(&[0, 0, 100, 100], &mut out);
        resampler.step = 0.25;
        resampler.process(&[200, 200], &mut out);
        let left: Vec<f32> = out.iter().step_by(2).map(|v| v * 32768.0).collect();
        // Step 0.5 up to the second frame, then step 0.25 from there to the third.
        assert_eq!(left, [0.0, 0.0, 0.0, 50.0, 100.0, 125.0, 150.0, 175.0]);
    }

    #[test]
    fn keeps_rate_over_many_calls() {
        let mut resampler = Linear::new(32768.0 / 48000.0);
        let mut out = Vec::new();
        for _ in 0..60 {
            resampler.process(&[0; 546 * 2], &mut out);
        }
        let expected = 60.0 * 546.0 * 48000.0 / 32768.0;
        assert!(((out.len() / 2) as f64 - expected).abs() <= 1.0);
    }
}
