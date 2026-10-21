//! Runs a ROM headless and writes the last frame as a PPM.
//!
//! cargo run --release --example smoke -- <core> <rom> <frames> <out.ppm> [save]
//!
//! Prints the core's av info, every av change and frame size change, and ms per frame over
//! the whole run and over its second half, after boot. Core options come from
//! `TERMCART_OPT_<key>`, see `src/libretro/options.rs`.

// The binary crate has no lib target, so pull the modules in by path. Not every item is used here.
#[allow(dead_code)]
#[path = "../src/frame.rs"]
mod frame;
#[allow(dead_code)]
#[path = "../src/joypad.rs"]
mod joypad;
#[allow(dead_code)]
#[path = "../src/libretro/mod.rs"]
mod libretro;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use joypad::Joypad;
use libretro::Core;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [core, rom, frames, out, rest @ ..] = args.as_slice() else {
        bail!("usage: smoke <core> <rom> <frames> <out.ppm> [save]");
    };
    let frames: u32 = frames.parse().context("frames must be a number")?;
    let save = rest.first().map(PathBuf::from);

    let mut core = Core::load(Path::new(core))?;
    let av = core.load_game(Path::new(rom), save.as_deref())?;
    println!("av: {av:?}");

    let mut audio = Vec::new();
    let mut size = None;
    let start = Instant::now();
    let mut half = start;
    for i in 0..frames {
        if i == frames / 2 {
            half = Instant::now();
        }
        // Hold START for 5 frames out of every 120.
        let mut pad = Joypad::default();
        pad.set(Joypad::START, i % 120 >= 115);
        core.set_joypad(pad);
        core.run_frame();
        core.take_audio(&mut audio);
        if let Some(change) = core.take_av_change() {
            println!("frame {i}: av changed to {change:?}");
        }
        let now = core.frame().map(|f| (f.width, f.height));
        if now != size {
            println!("frame {i}: video {now:?}");
            size = now;
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let late = frames - frames / 2;
    println!(
        "{frames} frames in {secs:.3} s: {:.2} ms/frame, second half {:.2} ms/frame ({:.1}x real time)",
        secs * 1000.0 / f64::from(frames),
        half.elapsed().as_secs_f64() * 1000.0 / f64::from(late),
        f64::from(frames) / secs / av.fps
    );
    println!(
        "audio: {} stereo frames, {:.0} per second of game time",
        audio.len() / 2,
        audio.len() as f64 / 2.0 / (f64::from(frames) / av.fps)
    );

    if let Some(save) = &save {
        core.write_save(save)?;
    }

    let frame = core.frame().context("core produced no frame")?;
    let mut ppm = Vec::with_capacity(frame.rgba.len());
    write!(ppm, "P6\n{} {}\n255\n", frame.width, frame.height)?;
    for px in frame.rgba.as_chunks::<4>().0 {
        ppm.extend_from_slice(&px[..3]);
    }
    std::fs::write(out, ppm).with_context(|| format!("writing {out}"))?;
    println!("frame {}x{} written to {out}", frame.width, frame.height);
    Ok(())
}
