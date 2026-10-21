mod audio;
mod frame;
mod input;
mod joypad;
mod libretro;
mod video;

use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use audio::Audio;
use input::{Command, Input, Layout};
use libretro::Core;

const USAGE: &str = "usage: termcart <rom> [--core <path>] [--video auto|kitty|half]

Plays .gb, .gbc, .gba and .z64/.n64/.v64 files. Saves go next to the ROM as <name>.sav.
Handheld keys: arrows/WASD d-pad, X/K = A, Z/J = B, Q/U = L, E/I = R,
               Enter = Start, Backspace = Select.
N64 keys: WASD stick (Left Shift walks), arrows = C buttons, K = A, J = B,
          Space = Z, Q = L, E = R, Enter = Start, T/F/G/H = d-pad.
Tab = fast forward, Esc = quit.";

/// Frames run per drawn frame while fast forward is on.
const FAST_FORWARD: u32 = 4;
const SAVE_EVERY: Duration = Duration::from_secs(10);

/// What the session cost, printed after the terminal is restored.
#[derive(Default)]
struct Stats {
    frames: u64,
    late: u64,
    emulate: Duration,
    emulate_max: Duration,
    draw: Duration,
}

impl Stats {
    fn print(&self, elapsed: Duration) {
        if self.frames == 0 {
            return;
        }
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let n = self.frames as f64;
        eprintln!(
            "termcart: {} frames in {:.1} s ({:.1} fps), emulation {:.2} ms/frame (max {:.1}), \
             draw {:.2} ms/frame, {} frames late ({:.1}%)",
            self.frames,
            elapsed.as_secs_f64(),
            n / elapsed.as_secs_f64(),
            ms(self.emulate) / n,
            ms(self.emulate_max),
            ms(self.draw) / n,
            self.late,
            self.late as f64 * 100.0 / n,
        );
        if let Ok(size) = crossterm::terminal::window_size() {
            eprintln!(
                "termcart: terminal {}x{} cells, {}x{} px",
                size.columns, size.rows, size.width, size.height
            );
        }
    }
}

struct Args {
    rom: PathBuf,
    core: Option<PathBuf>,
    video: video::Mode,
}

fn parse_args() -> Result<Args> {
    let mut rom = None;
    let mut core = None;
    let mut video = video::Mode::Auto;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-h" | "--help") => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            Some("--core") => core = Some(args.next().context("--core needs a path")?.into()),
            Some("--video") => {
                video = match args.next().as_deref().and_then(|v| v.to_str()) {
                    Some("auto") => video::Mode::Auto,
                    Some("kitty") => video::Mode::Kitty,
                    Some("half") => video::Mode::HalfBlock,
                    _ => bail!("--video takes auto, kitty or half"),
                }
            }
            _ if rom.is_none() => rom = Some(PathBuf::from(arg)),
            _ => bail!("unexpected argument {arg:?}\n\n{USAGE}"),
        }
    }
    let rom = rom.with_context(|| USAGE.to_string())?;
    Ok(Args { rom, core, video })
}

/// Which core library plays this ROM, by file extension.
fn core_name(rom: &Path) -> Result<&'static str> {
    let ext = rom
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("gb" | "gbc" | "sgb" | "gba" | "agb") => Ok("mgba_libretro"),
        Some("z64" | "n64" | "v64") => Ok("parallel_n64_libretro"),
        _ => bail!("no core for {}", rom.display()),
    }
}

/// Looks in `$TERMCART_CORES`, then `cores/` beside the binary and up to two levels
/// above it (so `target/release/termcart` finds the repo's `cores/`).
fn find_core(name: &str) -> Result<PathBuf> {
    let file = format!("{name}.{}", env::consts::DLL_EXTENSION);
    let mut dirs = Vec::new();
    if let Some(dir) = env::var_os("TERMCART_CORES") {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(exe_dir) = env::current_exe()?.parent() {
        dirs.extend(exe_dir.ancestors().take(3).map(|d| d.join("cores")));
    }
    dirs.into_iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
        .with_context(|| format!("{file} not found; run scripts/build-cores.sh or pass --core"))
}

/// N64 cores get the stick layout, everything else the handheld one.
fn layout_for(core_path: &Path) -> Layout {
    let stem = core_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if stem.contains("n64") {
        Layout::N64
    } else {
        Layout::Handheld
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let core_path = match args.core {
        Some(path) => path,
        None => find_core(core_name(&args.rom)?)?,
    };
    let save_path = args.rom.with_extension("sav");

    let mut core = Core::load(&core_path)?;
    let av = core.load_game(&args.rom, Some(&save_path))?;
    let mut audio = Audio::new(av.sample_rate).ok();

    // Input owns raw mode and the alternate screen, so it comes up before any drawing.
    let layout = layout_for(&core_path);
    let mut input = Input::new(layout)?;
    let mut renderer = video::make(args.video);
    let (cols, rows) = crossterm::terminal::size()?;
    renderer.resize(cols, rows);
    // Handheld frames have square pixels; N64 frames are 640x240 shown at 4:3.
    let stretch = matches!(layout, Layout::N64);
    renderer.set_aspect(stretch.then_some(av.aspect_ratio));

    let started = Instant::now();
    let mut stats = Stats::default();
    let result = run(
        &mut core,
        &mut stats,
        &mut input,
        &mut *renderer,
        audio.as_mut(),
        av.fps,
        stretch,
        &save_path,
    );

    let mut out = io::stdout().lock();
    let _ = renderer.clear(&mut out);
    let _ = out.flush();
    drop(out);
    let saved = core.write_save(&save_path);
    drop(input);
    stats.print(started.elapsed());
    result.and(saved)
}

#[allow(clippy::too_many_arguments)]
fn run(
    core: &mut Core,
    stats: &mut Stats,
    input: &mut Input,
    renderer: &mut dyn video::Renderer,
    mut audio: Option<&mut Audio>,
    fps: f64,
    stretch: bool,
    save_path: &Path,
) -> Result<()> {
    let mut frame_time = Duration::from_secs_f64(1.0 / fps);
    let mut commands = Vec::new();
    let mut samples = Vec::with_capacity(4096);
    let mut fast_forward = false;
    let mut next_frame = Instant::now();
    let mut last_save = Instant::now();
    let mut out = io::stdout().lock();

    loop {
        // Sleep in poll until the next frame is due, handling input as it arrives.
        loop {
            commands.clear();
            let wait = next_frame.saturating_duration_since(Instant::now());
            input.poll(wait, &mut commands)?;
            for command in commands.drain(..) {
                match command {
                    Command::Quit => return Ok(()),
                    Command::Resize(cols, rows) => renderer.resize(cols, rows),
                    Command::FastForward => fast_forward = !fast_forward,
                }
            }
            if Instant::now() >= next_frame {
                break;
            }
        }

        core.set_joypad(input.joypad());
        let (x, y) = input.left_stick();
        core.set_analog_left(x, y);
        let runs = if fast_forward { FAST_FORWARD } else { 1 };
        let emulate_start = Instant::now();
        for _ in 0..runs {
            core.run_frame();
        }
        let emulated = emulate_start.elapsed();
        stats.emulate += emulated;
        stats.emulate_max = stats.emulate_max.max(emulated);
        // N64 cores change geometry after boot and audio rate per game.
        if let Some(av) = core.take_av_change() {
            renderer.set_aspect(stretch.then_some(av.aspect_ratio));
            if let Some(audio) = audio.as_deref_mut() {
                audio.set_sample_rate(av.sample_rate);
            }
            frame_time = Duration::from_secs_f64(1.0 / av.fps);
        }
        samples.clear();
        core.take_audio(&mut samples);
        // At fast forward speed the audio would only pile up, so it is dropped.
        if let (Some(audio), false) = (audio.as_deref_mut(), fast_forward) {
            audio.push(&samples);
        }
        let draw_start = Instant::now();
        if let Some(frame) = core.frame() {
            renderer.draw(&mut out, &frame)?;
            out.flush()?;
        }
        stats.draw += draw_start.elapsed();
        stats.frames += 1;

        if last_save.elapsed() >= SAVE_EVERY {
            core.write_save(save_path)?;
            last_save = Instant::now();
        }

        next_frame += frame_time;
        // After a stall (suspend, slow terminal), resync instead of racing to catch up.
        let now = Instant::now();
        if now > next_frame {
            stats.late += 1;
        }
        if now > next_frame + frame_time * 4 {
            next_frame = now;
        }
    }
}
