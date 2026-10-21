//! Plays a 440 Hz square wave through `Audio`, pushed one 60 Hz frame at a time like the core
//! would, and switches the core rate mid-run the way N64 games do through
//! `SET_SYSTEM_AV_INFO`. Reports queue depth, underruns, drops and push cost per rate.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

#[path = "../src/audio.rs"]
mod audio;

use audio::{Audio, AudioStats};

const RATES: [f64; 3] = [32768.0, 44100.0, 22050.0];
const SEGMENT: Duration = Duration::from_secs(2);
const FPS: f64 = 60.0;
const TONE: f64 = 440.0;

fn main() -> Result<()> {
    let opening = Instant::now();
    let mut audio = Audio::new(RATES[0])?;
    println!(
        "device rate {} Hz, opened in {:?}",
        audio.device_rate(),
        opening.elapsed()
    );

    let frame = Duration::from_secs_f64(1.0 / FPS);
    let frames_per_segment = (SEGMENT.as_secs_f64() * FPS) as u32;
    let mut samples = Vec::new();
    // In cycles of the tone, so the wave stays continuous across rate changes.
    let mut phase = 0.0f64;
    let start = Instant::now();
    let mut n = 0u32;
    let mut before = audio.stats();
    for (segment, &rate) in RATES.iter().enumerate() {
        if segment > 0 {
            audio.set_sample_rate(rate);
        }
        let mut produced = 0.0;
        let mut push_total = Duration::ZERO;
        let mut push_max = Duration::ZERO;
        let mut queued_min = Duration::MAX;
        let mut queued_max = Duration::ZERO;
        for k in 0..frames_per_segment {
            let target = (f64::from(k + 1) * rate / FPS).round();
            samples.clear();
            while produced < target {
                let value = if phase.fract() < 0.5 { 4000 } else { -4000 };
                samples.extend([value, value]);
                phase += TONE / rate;
                produced += 1.0;
            }
            let t = Instant::now();
            audio.push(&samples);
            let spent = t.elapsed();
            push_total += spent;
            push_max = push_max.max(spent);
            // The first segment needs time to fill; later ones are measured from the switch.
            if segment > 0 || k > 10 {
                queued_min = queued_min.min(audio.queued());
                queued_max = queued_max.max(audio.queued());
            }
            n += 1;
            if let Some(wait) = (start + frame * n).checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }
        let now = audio.stats();
        println!(
            "{rate:>7} Hz: push mean {:?} max {push_max:?}, queued {queued_min:?}..{queued_max:?}, {}",
            push_total / frames_per_segment,
            delta(before, now)
        );
        before = now;
    }
    // Read before the queue drains, which is an expected underrun.
    let stats = audio.stats();
    thread::sleep(Duration::from_millis(100));
    println!(
        "total over {:.2} s: {stats:?}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn delta(before: AudioStats, now: AudioStats) -> String {
    format!(
        "underruns {}, dropped frames {}, stream errors {}",
        now.underruns - before.underruns,
        now.dropped_frames - before.dropped_frames,
        now.stream_errors - before.stream_errors
    )
}
