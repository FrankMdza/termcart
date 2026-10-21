//! Drives the video renderers with synthetic frames.
//!
//! `cargo run --release --example video_bench [-- --size <size>]` prints bytes and CPU per
//! frame for every renderer, writing to a counting sink that plays the terminal's part for
//! shm and temp files (it unlinks what it is handed).
//!
//! `cargo run --release --example video_bench -- --play <renderer> [--scene <scene>]
//! [--size <size>] [--seconds N] [--report <path>]` draws to the real terminal at 60 fps.
//!
//! Sizes: `gb` 160x144, `gba` 240x160, `n64` 640x240 and `n64i` 640x480 shown at 4:3, and
//! `n64flip`, which switches between the two N64 sizes every second.

#[path = "../src/frame.rs"]
mod frame;
#[path = "../src/video/mod.rs"]
#[allow(dead_code)]
mod video;

use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use frame::Frame;
use video::{Format, HalfBlock, Kitty, Prescale, Renderer, Transport};

const RENDERERS: &[&str] = &[
    "shm-32",
    "shm-24",
    "file-32",
    "file-24",
    "direct-32",
    "direct-24",
    "zlib-32",
    "zlib-24",
    "halfblock",
];

fn renderer(name: &str) -> Box<dyn Renderer> {
    // BENCH_SCALE takes what TERMCART_KITTY_SCALE takes; unset means no prescale.
    let scale = match std::env::var("BENCH_SCALE").as_deref() {
        Ok("auto") => Prescale::Auto,
        Ok(k) => Prescale::Fixed(k.parse().expect("BENCH_SCALE is auto or a factor")),
        Err(_) => Prescale::Fixed(1),
    };
    let kitty = |t, f| -> Box<dyn Renderer> { Box::new(Kitty::new(t, f, scale)) };
    match name {
        "shm-32" => kitty(Transport::SharedMemory, Format::Rgba),
        "shm-24" => kitty(Transport::SharedMemory, Format::Rgb),
        "file-32" => kitty(Transport::TempFile, Format::Rgba),
        "file-24" => kitty(Transport::TempFile, Format::Rgb),
        "direct-32" => kitty(Transport::Direct, Format::Rgba),
        "direct-24" => kitty(Transport::Direct, Format::Rgb),
        "zlib-32" => kitty(Transport::Zlib, Format::Rgba),
        "zlib-24" => kitty(Transport::Zlib, Format::Rgb),
        "halfblock" => Box::new(HalfBlock::new()),
        "auto" => video::make(video::Mode::Auto),
        other => panic!("unknown renderer {other}; one of {RENDERERS:?} or auto"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scene {
    /// Tiled background scrolling under a fixed HUD, with moving sprites.
    Scroll,
    /// The same frame forever, as in a pause menu.
    Static,
    /// Every pixel changes every frame: a worst case for diffing and compression.
    Fade,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Size {
    Gb,
    Gba,
    N64,
    N64Interlaced,
    N64Flip,
}

impl Size {
    const ALL: [Size; 5] = [
        Size::Gb,
        Size::Gba,
        Size::N64,
        Size::N64Interlaced,
        Size::N64Flip,
    ];

    fn parse(name: &str) -> Option<Size> {
        Size::ALL.into_iter().find(|s| s.name() == name)
    }

    fn name(self) -> &'static str {
        match self {
            Size::Gb => "gb",
            Size::Gba => "gba",
            Size::N64 => "n64",
            Size::N64Interlaced => "n64i",
            Size::N64Flip => "n64flip",
        }
    }

    /// Frame size at `tick`.
    fn dims(self, tick: u32) -> (u32, u32) {
        match self {
            Size::Gb => (160, 144),
            Size::Gba => (240, 160),
            Size::N64 => (640, 240),
            Size::N64Interlaced => (640, 480),
            Size::N64Flip if (tick / 60).is_multiple_of(2) => (640, 240),
            Size::N64Flip => (640, 480),
        }
    }

    fn aspect(self) -> Option<f64> {
        match self {
            Size::Gb | Size::Gba => None,
            Size::N64 | Size::N64Interlaced | Size::N64Flip => Some(4.0 / 3.0),
        }
    }
}

struct Game {
    size: Size,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    tick: u32,
}

impl Game {
    fn new(size: Size) -> Game {
        Game {
            size,
            width: 0,
            height: 0,
            rgba: Vec::new(),
            tick: 0,
        }
    }

    fn color(&self, shade: u32) -> [u8; 3] {
        if self.size != Size::Gb {
            // 15-bit color like the GBA, spread over many hues.
            let r = (shade * 7) % 32;
            let g = (shade * 13 + 5) % 32;
            let b = (shade * 3 + 11) % 32;
            [(r << 3) as u8, (g << 3) as u8, (b << 3) as u8]
        } else {
            const DMG: [[u8; 3]; 4] =
                [[224, 248, 208], [136, 192, 112], [52, 104, 86], [8, 24, 32]];
            DMG[(shade % 4) as usize]
        }
    }

    fn step(&mut self, scene: Scene) -> Frame<'_> {
        let t = match scene {
            Scene::Static => 0,
            _ => self.tick,
        };
        (self.width, self.height) = self.size.dims(self.tick);
        self.tick += 1;
        let (w, h) = (self.width, self.height);
        self.rgba.resize((w * h * 4) as usize, 0);
        let shades = if self.size == Size::Gb { 4 } else { 61 };
        for y in 0..h {
            for x in 0..w {
                let shade = match scene {
                    Scene::Fade => (x + y + t) % 256,
                    _ if y < 16 => (x / 8 + 1) % 2 * 3, // HUD
                    _ => {
                        let (sx, sy) = (x + t, y + (t / 4) % 8);
                        let (tx, ty) = (sx / 8, sy / 8);
                        let tile = (tx * 7 + ty * 13 + (tx * ty) % 5) % shades;
                        if (sx % 8 == 0 || sy % 8 == 0) && tile % 3 == 0 {
                            tile + 1
                        } else {
                            tile
                        }
                    }
                };
                let mut c = if scene == Scene::Fade {
                    let s = shade as u8;
                    [s, s.wrapping_mul(3), 255 - s]
                } else {
                    self.color(shade)
                };
                if scene != Scene::Fade {
                    for i in 0..4u32 {
                        let sx = (t * (i + 1) + i * 40) % w;
                        let sy = 40 + i * 20;
                        if x.wrapping_sub(sx) < 16 && y.wrapping_sub(sy) < 16 {
                            c = self.color(i + 2);
                        }
                    }
                }
                let p = ((y * w + x) * 4) as usize;
                self.rgba[p..p + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
        Frame {
            width: w,
            height: h,
            rgba: &self.rgba,
        }
    }
}

/// Counts bytes and consumes shm objects and temp files the way the terminal would.
#[derive(Default)]
struct Sink {
    bytes: u64,
    last: Vec<u8>,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes += buf.len() as u64;
        self.last.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Sink {
    fn consume(&mut self) {
        for (key, unlink) in [(&b",t=s;"[..], true), (&b",t=t;"[..], false)] {
            let Some(at) = find(&self.last, key) else {
                continue;
            };
            let rest = &self.last[at + key.len()..];
            let end = find(rest, b"\x1b\\").expect("payload terminated");
            let name = BASE64.decode(&rest[..end]).expect("base64 name");
            if unlink {
                let name = CString::new(name).expect("no NUL");
                // SAFETY: a valid C string naming an object we created.
                unsafe { libc::shm_unlink(name.as_ptr()) };
            } else {
                let _ = fs::remove_file(String::from_utf8(name).expect("utf-8 path"));
            }
        }
        self.last.clear();
    }
}

struct Counted<W> {
    inner: W,
    bytes: u64,
    writes: u64,
    /// The `s=..,v=..,c=..` keys of the last kitty image sent.
    image: String,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(at) = find(buf, b",f=") {
            let keys = &buf[at + 1..];
            let end = keys.iter().position(|&b| b == b';').unwrap_or(keys.len());
            self.image = String::from_utf8_lossy(&keys[..end]).into_owned();
        }
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        self.writes += 1;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn process_cpu() -> Duration {
    // SAFETY: zeroed rusage is a valid out-parameter.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid out-pointer.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

fn bench(sizes: &[Size]) {
    const FRAMES: u32 = 600;
    let (cols, rows) = (200, 56);
    println!("{FRAMES} frames into a {cols}x{rows} cell sink; CPU is the draw call only");
    println!(
        "{:<10} {:<4} {:<7} {:>12} {:>12} {:>9}",
        "renderer", "size", "scene", "bytes/frame", "us/frame", "skipped"
    );
    for &size in sizes {
        for (scene, scene_name) in [
            (Scene::Scroll, "scroll"),
            (Scene::Fade, "fade"),
            (Scene::Static, "static"),
        ] {
            for &name in RENDERERS {
                let mut r = renderer(name);
                r.set_aspect(size.aspect());
                r.resize(cols, rows);
                let mut game = Game::new(size);
                let mut sink = Sink::default();
                let mut cpu = Duration::ZERO;
                let mut written = 0;
                for _ in 0..FRAMES {
                    let frame = game.step(scene);
                    let before = sink.bytes;
                    let start = thread_cpu();
                    r.draw(&mut sink, &frame).expect("draw");
                    cpu += thread_cpu() - start;
                    if sink.bytes > before {
                        written += 1;
                    }
                    sink.consume();
                }
                println!(
                    "{:<10} {:<4} {:<7} {:>12} {:>12.1} {:>9}",
                    name,
                    size.name(),
                    scene_name,
                    sink.bytes / u64::from(FRAMES),
                    cpu.as_secs_f64() * 1e6 / f64::from(FRAMES),
                    FRAMES - written,
                );
            }
        }
    }
}

fn play(
    name: &str,
    scene: Scene,
    size: Size,
    seconds: u64,
    report: Option<&str>,
) -> io::Result<()> {
    let mut r = renderer(name);
    let mut out = Counted {
        inner: io::stdout().lock(),
        bytes: 0,
        writes: 0,
        image: String::new(),
    };
    out.write_all(b"\x1b[?1049h\x1b[?25l")?;
    let (mut cols, mut rows) = crossterm::terminal::size()?;
    r.set_aspect(size.aspect());
    r.resize(cols, rows);
    let mut game = Game::new(size);
    let frame_time = Duration::from_nanos(1_000_000_000 / 60);
    let (start, cpu0) = (Instant::now(), process_cpu());
    let mut next = start;
    let mut frames = 0u64;
    let mut draw_cpu = Duration::ZERO;
    let mut drawn = 0u64;
    while start.elapsed() < Duration::from_secs(seconds) {
        if frames.is_multiple_of(30) {
            let size = crossterm::terminal::size()?;
            if size != (cols, rows) {
                (cols, rows) = size;
                r.resize(cols, rows);
            }
        }
        let frame = game.step(scene);
        let (t, writes) = (thread_cpu(), out.writes);
        r.draw(&mut out, &frame)?;
        draw_cpu += thread_cpu() - t;
        frames += 1;
        drawn += u64::from(out.writes > writes);
        next += frame_time;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    let (wall, cpu) = (start.elapsed(), process_cpu() - cpu0);
    let bytes = out.bytes;
    r.clear(&mut out)?;
    out.write_all(b"\x1b[?25h\x1b[?1049l")?;
    out.flush()?;
    let line = format!(
        "{name} {cols}x{rows} cells: {frames} frames in {:.1}s, {:.1} fps; process CPU {:.2}% \
         ({:.1} us/frame), draw {:.1} us/frame; {drawn} frames sent, {} bytes/frame; last image {}\n",
        wall.as_secs_f64(),
        frames as f64 / wall.as_secs_f64(),
        cpu.as_secs_f64() / wall.as_secs_f64() * 100.0,
        cpu.as_secs_f64() * 1e6 / frames as f64,
        draw_cpu.as_secs_f64() * 1e6 / frames as f64,
        bytes / frames,
        out.image,
    );
    eprint!("{line}");
    if let Some(path) = report {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(line.as_bytes())?;
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .map(|i| args[i + 1].as_str())
    };
    let size = flag("--size").map(|s| Size::parse(s).unwrap_or_else(|| panic!("unknown size {s}")));
    let Some(name) = flag("--play") else {
        let all = [Size::Gb, Size::Gba, Size::N64, Size::N64Interlaced];
        bench(size.as_ref().map_or(&all[..], std::slice::from_ref));
        return Ok(());
    };
    let scene = match flag("--scene").unwrap_or("scroll") {
        "scroll" => Scene::Scroll,
        "static" => Scene::Static,
        "fade" => Scene::Fade,
        other => panic!("unknown scene {other}"),
    };
    let seconds = flag("--seconds").map_or(5, |s| s.parse().expect("seconds"));
    play(
        name,
        scene,
        size.unwrap_or(Size::Gb),
        seconds,
        flag("--report"),
    )
}
