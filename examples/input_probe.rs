//! Prints the joypad bitmask, the left stick and every key event live, to check key releases
//! by hand.
//!
//! `cargo run --example input_probe -- [--layout handheld|n64] [--timeout SECS]`. Esc or
//! Ctrl+C quits. With `--layout n64` buttons print under their N64 names.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

// The crate is a binary, so the example compiles the modules it needs directly.
// Not every item is used here.
#[allow(dead_code)]
#[path = "../src/input.rs"]
mod input;
#[allow(dead_code)]
#[path = "../src/joypad.rs"]
mod joypad;

use input::{Command, Input, Keys, Layout};
use joypad::Joypad;

/// Button names by RetroPad id.
const RETROPAD: [&str; 14] = [
    "B", "Y", "Select", "Start", "Up", "Down", "Left", "Right", "A", "X", "L", "R", "L2", "R2",
];
/// The N64 button each RetroPad id drives under parallel-n64's alt-map.
const N64: [&str; 14] = [
    "A", "B", "L", "Start", "D-Up", "D-Down", "D-Left", "D-Right", "C-Down", "C-Up", "C-Left",
    "C-Right", "Z", "R",
];

struct Args {
    layout: Layout,
    timeout: Option<Duration>,
}

fn main() -> Result<()> {
    let Args { layout, timeout } = parse_args()?;
    let names = match layout {
        Layout::Handheld => &RETROPAD,
        Layout::N64 => &N64,
    };
    let started = Instant::now();
    let mut input = Input::new(layout, Keys::from_env())?;
    let mut out = io::stdout();
    write!(
        out,
        "layout {layout:?}, key release {:?}. Esc or Ctrl+C quits.\r\n",
        input.key_release()
    )?;

    let mut commands = Vec::new();
    let mut last = (Joypad::default(), (0, 0));
    let mut events = 0usize;
    let mut quit = false;
    while !quit && timeout.is_none_or(|t| started.elapsed() < t) {
        input.poll_observed(Duration::from_millis(16), &mut commands, |key| {
            events += 1;
            let _ = write!(out, "{:?} {:?} {:?}\r\n", key.kind, key.code, key.modifiers);
        })?;
        for command in commands.drain(..) {
            match command {
                Command::Quit => quit = true,
                Command::Resize(w, h) => write!(out, "resize {w}x{h}\r\n")?,
                Command::FastForward => write!(out, "fast forward\r\n")?,
            }
        }
        let now = (input.joypad(), input.left_stick());
        if now != last {
            let (pad, (x, y)) = now;
            write!(
                out,
                "joypad {:#016b} [{}] stick {x:>6} {y:>6}\r\n",
                pad.0,
                held(pad, names)
            )?;
            last = now;
        }
        out.flush()?;
    }
    let release = input.key_release();
    drop(input);
    println!(
        "exited after {:.1} s, {events} key events, key release {release:?}",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn held(pad: Joypad, names: &[&str; 14]) -> String {
    (0..14)
        .filter(|&id| pad.pressed(id))
        .map(|id| names[usize::from(id)])
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_args() -> Result<Args> {
    let mut parsed = Args {
        layout: Layout::Handheld,
        timeout: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--timeout" => {
                let secs: f64 = args
                    .next()
                    .context("--timeout needs seconds")?
                    .parse()
                    .context("parsing --timeout")?;
                parsed.timeout = Some(Duration::from_secs_f64(secs));
            }
            "--layout" => {
                parsed.layout = match args.next().as_deref() {
                    Some("handheld") => Layout::Handheld,
                    Some("n64") => Layout::N64,
                    other => anyhow::bail!("--layout takes handheld or n64, got {other:?}"),
                };
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(parsed)
}
