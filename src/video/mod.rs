//! Draws frames into the terminal.

mod halfblock;
mod kitty;
mod shm;

use std::env;
use std::io::{self, Write};

use crate::frame::Frame;

pub use halfblock::HalfBlock;
pub use kitty::{Format, Kitty, Prescale, Transport};

pub trait Renderer {
    /// Called once before the first frame and on every terminal resize.
    fn resize(&mut self, cols: u16, rows: u16);
    /// Sets the display width/height ratio, `None` to take it from the frame's pixel size.
    /// Called before the first frame and whenever the core's geometry changes.
    fn set_aspect(&mut self, aspect: Option<f64>);
    fn draw(&mut self, out: &mut dyn Write, frame: &Frame) -> io::Result<()>;
    /// Removes anything the renderer left on screen.
    fn clear(&mut self, out: &mut dyn Write) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Kitty,
    HalfBlock,
}

/// Builds the renderer for `mode`. `Mode::Auto` reads the environment, see [`detect`].
///
/// Two variables tune the Kitty renderer: `TERMCART_KITTY` picks how pixels travel, as
/// `<shm|file|direct|zlib>[,24|,32]` (e.g. `zlib,24`), and `TERMCART_KITTY_SCALE`
/// (`auto`, the default, or a factor, with `1` or `off` to disable it) enlarges frames
/// before sending, see [`Prescale`].
pub fn make(mode: Mode) -> Box<dyn Renderer> {
    let mode = match mode {
        Mode::Auto => detect(),
        other => other,
    };
    match mode {
        Mode::Kitty => {
            let (transport, format) = env::var("TERMCART_KITTY")
                .ok()
                .and_then(|spec| kitty::parse_spec(&spec))
                .unwrap_or_else(default_kitty);
            let prescale = env::var("TERMCART_KITTY_SCALE")
                .ok()
                .and_then(|spec| Prescale::parse(&spec))
                .unwrap_or(Prescale::Auto);
            Box::new(Kitty::new(transport, format, prescale))
        }
        Mode::HalfBlock | Mode::Auto => Box::new(HalfBlock::new()),
    }
}

/// Picks a mode from the environment alone, so it never reads stdin and never blocks.
///
/// Ghostty sets `TERM_PROGRAM=ghostty` and `TERM=xterm-ghostty`; kitty sets
/// `TERM=xterm-kitty` and `KITTY_WINDOW_ID`. Over SSH only `TERM` survives, which is
/// enough. Inside tmux or screen the graphics protocol needs passthrough wrapping, so
/// those get half blocks even when the outer terminal is Ghostty.
pub fn detect() -> Mode {
    let var = |name| env::var(name).unwrap_or_default();
    let term = var("TERM");
    if term.starts_with("tmux") || term.starts_with("screen") || env::var_os("TMUX").is_some() {
        return Mode::HalfBlock;
    }
    let kitty_like = matches!(term.as_str(), "xterm-ghostty" | "xterm-kitty")
        || var("TERM_PROGRAM").eq_ignore_ascii_case("ghostty")
        || env::var_os("KITTY_WINDOW_ID").is_some();
    if kitty_like {
        Mode::Kitty
    } else {
        Mode::HalfBlock
    }
}

/// Shared memory when the terminal runs on this machine, compressed escapes over SSH.
fn default_kitty() -> (Transport, Format) {
    let remote = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| env::var_os(name).is_some());
    if remote {
        (Transport::Zlib, Format::Rgb)
    } else {
        (Transport::SharedMemory, Format::Rgba)
    }
}

/// The terminal grid in cells, plus the pixel size of one cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Grid {
    cols: u16,
    rows: u16,
    cell: CellSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CellSize {
    /// Reported by TIOCGWINSZ.
    Pixels { w: u16, h: u16 },
    /// The terminal did not say; assume cells twice as tall as wide.
    Unknown,
}

impl CellSize {
    /// Width and height in pixels, or a 1:2 stand-in.
    fn dims(self) -> (u32, u32) {
        match self {
            CellSize::Pixels { w, h } => (w.into(), h.into()),
            CellSize::Unknown => (1, 2),
        }
    }
}

impl Grid {
    fn query(cols: u16, rows: u16) -> Grid {
        let cell = match crossterm::terminal::window_size() {
            Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns == cols && ws.rows == rows => {
                CellSize::Pixels {
                    w: ws.width / cols.max(1),
                    h: ws.height / rows.max(1),
                }
            }
            _ => CellSize::Unknown,
        };
        Grid { cols, rows, cell }
    }
}

/// The proportions a frame is drawn at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// Square pixels: the frame's own width and height.
    Native { w: u64, h: u64 },
    /// A width:height ratio the frame is stretched to, as for N64's 640x240 at 4:3.
    Stretched { w: u64, h: u64 },
}

impl Shape {
    /// Fixed-point denominator for a core's floating aspect ratio.
    const ONE: u64 = 1 << 16;

    /// Any valid `aspect` stretches, even one that matches the frame, so a core whose
    /// frames flip between 640x240 and 640x480 keeps one placement for both.
    fn new(aspect: Option<f64>, width: u32, height: u32) -> Shape {
        match aspect.filter(|a| a.is_finite() && *a > 0.0) {
            Some(a) => Shape::Stretched {
                w: (a * Self::ONE as f64).round().max(1.0) as u64,
                h: Self::ONE,
            },
            None => Shape::Native {
                w: width.into(),
                h: height.into(),
            },
        }
    }

    fn ratio(self) -> (u64, u64) {
        match self {
            Shape::Native { w, h } | Shape::Stretched { w, h } => (w, h),
        }
    }
}

fn div_round(n: u64, d: u64) -> u64 {
    (n + d / 2) / d
}
