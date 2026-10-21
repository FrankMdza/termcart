//! Kitty graphics protocol. The terminal scales one native-resolution image to a cell
//! rectangle, so we only ship `width * height` pixels per changed frame.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use flate2::{Compress, Compression, FlushCompress, Status};

use super::shm::ShmRing;
use super::{div_round, CellSize, Grid, Renderer, Shape};
use crate::frame::Frame;

const IMAGE_ID: u32 = 0x7e_c0de;
/// Base64 bytes per escape sequence, the limit the protocol asks for.
const CHUNK: usize = 4096;
const TEMP_SLOTS: usize = 4;
/// Dropped frames in a row, waiting on the terminal to read shm or files, before we
/// conclude it never will (no support, or not on this machine) and switch to escapes.
const GIVE_UP: u32 = 120;

/// How pixels reach the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `t=s`: a POSIX shared memory object; only its name crosses the pty.
    SharedMemory,
    /// `t=t`: a file in the temp dir that the terminal reads and deletes.
    TempFile,
    /// `t=d`: base64 pixels inside the escape sequences.
    Direct,
    /// `t=d,o=z`: like `Direct`, deflated first.
    Zlib,
}

/// Pixel format sent to the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `f=32`, the frame as is. Ghostty uploads it without conversion.
    Rgba,
    /// `f=24`, a quarter fewer bytes. Ghostty expands it back to RGBA.
    Rgb,
}

impl Format {
    fn code(self) -> u8 {
        match self {
            Format::Rgba => 32,
            Format::Rgb => 24,
        }
    }

    fn bytes_per_pixel(self) -> usize {
        match self {
            Format::Rgba => 4,
            Format::Rgb => 3,
        }
    }

    /// Writes `image` into `dst`, each source pixel as a `scale` x `scale` block.
    fn encode(self, image: &Image, dst: &mut [u8]) {
        if self == Format::Rgba && image.scale == 1 {
            dst.copy_from_slice(image.rgba);
            return;
        }
        let k = image.scale as usize;
        let line = image.width as usize * k * self.bytes_per_pixel();
        let src_rows = image.rgba.chunks_exact(image.width as usize * 4);
        for (src, block) in src_rows.zip(dst.chunks_exact_mut(line * k)) {
            let (first, rest) = block.split_at_mut(line);
            let pixels = src.as_chunks::<4>().0.iter();
            match self {
                Format::Rgba => {
                    for (px, run) in pixels.zip(first.as_chunks_mut::<4>().0.chunks_exact_mut(k)) {
                        run.fill(*px);
                    }
                }
                Format::Rgb if k == 1 => {
                    for (px, d) in pixels.zip(first.as_chunks_mut::<3>().0) {
                        *d = [px[0], px[1], px[2]];
                    }
                }
                Format::Rgb => {
                    for (px, run) in pixels.zip(first.as_chunks_mut::<3>().0.chunks_exact_mut(k)) {
                        run.fill([px[0], px[1], px[2]]);
                    }
                }
            }
            for d in rest.chunks_exact_mut(line) {
                d.copy_from_slice(first);
            }
        }
    }
}

/// The frame as it will be sent: native pixels plus an integer upscale factor.
struct Image<'a> {
    rgba: &'a [u8],
    width: u32,
    height: u32,
    scale: u32,
}

impl Image<'_> {
    fn sent_dims(&self) -> (u32, u32) {
        (self.width * self.scale, self.height * self.scale)
    }

    fn sent_pixels(&self) -> usize {
        let (w, h) = self.sent_dims();
        w as usize * h as usize
    }
}

/// Integer nearest-neighbor upscale done on our side before sending.
///
/// Ghostty magnifies images with linear filtering, so a 160x144 frame stretched 8x
/// turns every pixel edge into an 8 px gradient. Sending the frame already enlarged
/// leaves the terminal a smaller factor to blur, at `scale^2` the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
///
/// [`Prescale::Auto`] is the default; `Fixed(1)` turns prescaling off.
pub enum Prescale {
    Fixed(u32),
    /// The smallest factor that leaves the terminal at most 2x to interpolate, capped so
    /// the sent image stays within 1280x960 pixels. A 640x240 N64 frame gets 1 or 2.
    Auto,
}

impl Prescale {
    const MAX: u32 = 8;
    /// Most pixels `Auto` sends per frame, 1280x960 (4.9 MB at f=32).
    const BUDGET: u64 = 1280 * 960;

    fn factor(self, magnification: Option<f64>, frame_pixels: u64) -> u32 {
        match (self, magnification) {
            (Prescale::Fixed(k), _) => k.clamp(1, Self::MAX),
            (Prescale::Auto, Some(m)) => {
                let fits = (Self::BUDGET as f64 / frame_pixels.max(1) as f64).sqrt();
                ((m / 2.0).min(fits).floor() as u32).clamp(1, Self::MAX)
            }
            (Prescale::Auto, None) => 1,
        }
    }

    /// Parses `auto`, `off` (the same as `1`) or a factor such as `3`.
    pub(super) fn parse(spec: &str) -> Option<Prescale> {
        match spec.trim() {
            "auto" => Some(Prescale::Auto),
            "off" => Some(Prescale::Fixed(1)),
            n => n.parse().ok().filter(|&k| k >= 1).map(Prescale::Fixed),
        }
    }
}

/// Parses `<shm|file|direct|zlib>[,24|,32]`.
pub(super) fn parse_spec(spec: &str) -> Option<(Transport, Format)> {
    let (transport, format) = spec.split_once(',').unwrap_or((spec, ""));
    let transport = match transport.trim() {
        "shm" => Transport::SharedMemory,
        "file" => Transport::TempFile,
        "direct" => Transport::Direct,
        "zlib" => Transport::Zlib,
        _ => return None,
    };
    let format = match format.trim() {
        "" if transport == Transport::Zlib => Format::Rgb,
        "" | "32" => Format::Rgba,
        "24" => Format::Rgb,
        _ => return None,
    };
    Some((transport, format))
}

pub struct Kitty {
    transport: Transport,
    format: Format,
    prescale: Prescale,
    aspect: Option<f64>,
    grid: Option<Grid>,
    /// The screen must be wiped before the next placement (first frame, resize, clear).
    stale: bool,
    prev: Vec<u8>,
    prev_dims: (u32, u32),
    out: Vec<u8>,
    pixels: Vec<u8>,
    packed: Vec<u8>,
    deflate: Compress,
    shm: Option<ShmRing>,
    temp: Option<TempRing>,
    /// Frames dropped in a row because the terminal had not consumed earlier ones.
    unread: u32,
}

impl Kitty {
    pub fn new(transport: Transport, format: Format, prescale: Prescale) -> Kitty {
        Kitty {
            transport,
            format,
            prescale,
            aspect: None,
            grid: None,
            stale: true,
            prev: Vec::new(),
            prev_dims: (0, 0),
            out: Vec::new(),
            pixels: Vec::new(),
            packed: Vec::new(),
            deflate: Compress::new(Compression::fast(), true),
            shm: (transport == Transport::SharedMemory).then(ShmRing::new),
            temp: (transport == Transport::TempFile).then(TempRing::new),
            unread: 0,
        }
    }

    /// Switches to compressed escapes, which work wherever the protocol does.
    fn fall_back(&mut self) {
        self.transport = Transport::Zlib;
        self.shm = None;
        self.temp = None;
        self.unread = 0;
    }

    /// Appends the transport keys and payload. Returns false when the terminal has not
    /// consumed earlier frames yet and this one should be dropped.
    fn push_payload(&mut self, image: &Image) -> io::Result<bool> {
        let len = image.sent_pixels() * self.format.bytes_per_pixel();
        let format = self.format;
        match self.transport {
            Transport::SharedMemory => {
                let ring = self.shm.as_mut().expect("shm ring exists for t=s");
                let Some(name) = ring.publish(len, |dst| format.encode(image, dst))? else {
                    return Ok(false);
                };
                self.out.extend_from_slice(b",t=s;");
                push_base64(&mut self.out, name.to_bytes());
                self.out.extend_from_slice(b"\x1b\\");
            }
            Transport::TempFile => {
                let ring = self.temp.as_mut().expect("temp ring exists for t=t");
                self.pixels.resize(len, 0);
                format.encode(image, &mut self.pixels);
                let Some(path) = ring.publish(&self.pixels)? else {
                    return Ok(false);
                };
                self.out.extend_from_slice(b",t=t;");
                push_base64(&mut self.out, path.as_os_str().as_encoded_bytes());
                self.out.extend_from_slice(b"\x1b\\");
            }
            Transport::Direct | Transport::Zlib => {
                let raw: &[u8] = if format == Format::Rgba && image.scale == 1 {
                    image.rgba
                } else {
                    self.pixels.resize(len, 0);
                    format.encode(image, &mut self.pixels);
                    &self.pixels
                };
                let body: &[u8] = if self.transport == Transport::Zlib {
                    self.out.extend_from_slice(b",o=z");
                    deflate(&mut self.deflate, raw, &mut self.packed);
                    &self.packed
                } else {
                    raw
                };
                push_chunked(&mut self.out, body);
            }
        }
        Ok(true)
    }
}

impl Renderer for Kitty {
    fn resize(&mut self, cols: u16, rows: u16) {
        let grid = Grid::query(cols, rows);
        if self.grid != Some(grid) {
            self.grid = Some(grid);
            self.stale = true;
        }
    }

    fn set_aspect(&mut self, aspect: Option<f64>) {
        if self.aspect != aspect {
            self.aspect = aspect;
            self.stale = true;
        }
    }

    fn draw(&mut self, out: &mut dyn Write, frame: &Frame) -> io::Result<()> {
        let Some(grid) = self.grid else {
            return Ok(());
        };
        let dims = (frame.width, frame.height);
        if !self.stale && dims == self.prev_dims && frame.rgba == self.prev.as_slice() {
            return Ok(());
        }
        if grid.cols == 0 || grid.rows == 0 || frame.width == 0 || frame.height == 0 {
            return Ok(());
        }

        let shape = Shape::new(self.aspect, frame.width, frame.height);
        let place = Placement::fit(grid, shape);
        let image = Image {
            rgba: frame.rgba,
            width: frame.width,
            height: frame.height,
            scale: self.prescale.factor(
                magnification(grid, shape, frame.width, frame.height),
                u64::from(frame.width) * u64::from(frame.height),
            ),
        };
        let (sent_w, sent_h) = image.sent_dims();
        self.out.clear();
        if self.stale {
            self.out.extend_from_slice(b"\x1b[2J");
        }
        write!(
            self.out,
            "\x1b[{};{}H\x1b_Ga=T,i={IMAGE_ID},p=1,q=2,C=1,f={},s={},v={}",
            place.row + 1,
            place.col + 1,
            self.format.code(),
            sent_w,
            sent_h,
        )?;
        match place.size {
            Span::Cols(c) => write!(self.out, ",c={c}")?,
            Span::Rows(r) => write!(self.out, ",r={r}")?,
            Span::Both { cols, rows } => write!(self.out, ",c={cols},r={rows}")?,
        }
        if place.x > 0 {
            write!(self.out, ",X={}", place.x)?;
        }
        if place.y > 0 {
            write!(self.out, ",Y={}", place.y)?;
        }
        let header = self.out.len();
        let sent = match self.push_payload(&image) {
            Ok(sent) => sent,
            Err(_) if self.transport != Transport::Zlib => {
                self.fall_back();
                self.out.truncate(header);
                self.push_payload(&image)?
            }
            Err(err) => return Err(err),
        };
        if !sent {
            self.unread += 1;
            if self.unread >= GIVE_UP {
                self.fall_back();
            }
            return Ok(());
        }
        self.unread = 0;
        out.write_all(&self.out)?;
        out.flush()?;

        self.stale = false;
        self.prev_dims = dims;
        self.prev.clear();
        self.prev.extend_from_slice(frame.rgba);
        Ok(())
    }

    fn clear(&mut self, out: &mut dyn Write) -> io::Result<()> {
        write!(out, "\x1b_Ga=d,d=I,i={IMAGE_ID},q=2\x1b\\")?;
        out.flush()?;
        self.stale = true;
        Ok(())
    }
}

/// Where the image goes: its top-left cell, a pixel offset inside that cell, and its
/// size in cells. With one dimension pinned the terminal derives the other from the
/// image's proportions; with both it stretches the image to fill them. Ghostty takes
/// `X`/`Y` off the size of a stretched image instead of moving it, so a stretched image
/// is centered in whole cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placement {
    col: u32,
    row: u32,
    x: u32,
    y: u32,
    size: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Span {
    Cols(u32),
    Rows(u32),
    Both { cols: u32, rows: u32 },
}

impl Placement {
    fn fit(grid: Grid, shape: Shape) -> Placement {
        let (cw, ch) = grid.cell.dims();
        let (cw, ch) = (u64::from(cw), u64::from(ch));
        let (cols, rows) = (u64::from(grid.cols), u64::from(grid.rows));
        let (avail_w, avail_h) = (cols * cw, rows * ch);
        let (w, h) = shape.ratio();
        let height_bound = w * avail_h <= h * avail_w;
        let (col, row, x, y, size) = match (shape, height_bound) {
            (Shape::Native { .. }, true) => {
                let left = (avail_w - w * avail_h / h) / 2;
                (left / cw, 0, left % cw, 0, Span::Rows(rows as u32))
            }
            (Shape::Native { .. }, false) => {
                let top = (avail_h - h * avail_w / w) / 2;
                (0, top / ch, 0, top % ch, Span::Cols(cols as u32))
            }
            (Shape::Stretched { .. }, true) => {
                let c = div_round(w * avail_h, h * cw).clamp(1, cols);
                let size = Span::Both {
                    cols: c as u32,
                    rows: rows as u32,
                };
                ((cols - c) / 2, 0, 0, 0, size)
            }
            (Shape::Stretched { .. }, false) => {
                let r = div_round(h * avail_w, w * ch).clamp(1, rows);
                let size = Span::Both {
                    cols: cols as u32,
                    rows: r as u32,
                };
                (0, (rows - r) / 2, 0, 0, size)
            }
        };
        Placement {
            col: col as u32,
            row: row as u32,
            x: x as u32,
            y: y as u32,
            size,
        }
    }
}

/// Screen pixels per frame pixel along the less magnified axis when the image fills the
/// grid, if the cell size is known.
fn magnification(grid: Grid, shape: Shape, width: u32, height: u32) -> Option<f64> {
    let CellSize::Pixels { w, h } = grid.cell else {
        return None;
    };
    let avail_w = f64::from(grid.cols) * f64::from(w);
    let avail_h = f64::from(grid.rows) * f64::from(h);
    let (sw, sh) = shape.ratio();
    let ratio = sw as f64 / sh as f64;
    let (shown_w, shown_h) = if ratio * avail_h <= avail_w {
        (avail_h * ratio, avail_h)
    } else {
        (avail_w, avail_w / ratio)
    };
    Some((shown_w / f64::from(width)).min(shown_h / f64::from(height)))
}

fn push_base64(out: &mut Vec<u8>, data: &[u8]) {
    let start = out.len();
    out.resize(
        start + base64::encoded_len(data.len(), true).expect("small payload"),
        0,
    );
    let n = BASE64
        .encode_slice(data, &mut out[start..])
        .expect("buffer sized by encoded_len");
    out.truncate(start + n);
}

/// Ends the open header and splits `body` into `m=1` chunks.
fn push_chunked(out: &mut Vec<u8>, body: &[u8]) {
    let mut chunks = body.chunks(CHUNK / 4 * 3).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = chunks.peek().is_some();
        match (first, more) {
            (true, true) => out.extend_from_slice(b",m=1;"),
            (true, false) => out.push(b';'),
            (false, true) => out.extend_from_slice(b"\x1b_Gm=1;"),
            (false, false) => out.extend_from_slice(b"\x1b_Gm=0;"),
        }
        push_base64(out, chunk);
        out.extend_from_slice(b"\x1b\\");
        first = false;
    }
    if first {
        out.extend_from_slice(b";\x1b\\");
    }
}

fn deflate(z: &mut Compress, input: &[u8], out: &mut Vec<u8>) {
    z.reset();
    out.clear();
    out.reserve(input.len() + input.len() / 64 + 64);
    loop {
        let read = z.total_in() as usize;
        let status = z
            .compress_vec(&input[read..], out, FlushCompress::Finish)
            .expect("deflate of an in-memory buffer");
        if status == Status::StreamEnd {
            break;
        }
        out.reserve(out.capacity().max(4096));
    }
}

/// Temp files named so the terminal accepts and deletes them (`tty-graphics-protocol`).
struct TempRing {
    paths: [PathBuf; TEMP_SLOTS],
    next: usize,
}

impl TempRing {
    fn new() -> TempRing {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let paths = std::array::from_fn(|slot| {
            dir.join(format!("tty-graphics-protocol-termcart-{pid}-{slot}"))
        });
        TempRing { paths, next: 0 }
    }

    fn publish(&mut self, data: &[u8]) -> io::Result<Option<&PathBuf>> {
        let slot = self.next;
        let path = &self.paths[slot];
        let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Ok(None),
            Err(err) => return Err(err),
        };
        file.write_all(data)?;
        self.next = (slot + 1) % TEMP_SLOTS;
        Ok(Some(path))
    }
}

impl Drop for TempRing {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(cols: u16, rows: u16, cell: CellSize, image: (u32, u32), expected: Placement) {
        let grid = Grid { cols, rows, cell };
        let shape = Shape::new(None, image.0, image.1);
        assert_eq!(Placement::fit(grid, shape), expected);
    }

    fn check_stretched(cols: u16, rows: u16, cell: CellSize, expected: Placement) {
        let grid = Grid { cols, rows, cell };
        let shape = Shape::new(Some(4.0 / 3.0), 640, 240);
        assert_eq!(Placement::fit(grid, shape), expected);
    }

    #[test]
    fn stretched_frames_pin_both_spans() {
        let px = CellSize::Pixels { w: 10, h: 20 };
        // 2000x1000 px: 1333 px wide rounds to 133 cells, 33 cells left margin.
        check_stretched(
            200,
            50,
            px,
            Placement {
                col: 33,
                row: 0,
                x: 0,
                y: 0,
                size: Span::Both {
                    cols: 133,
                    rows: 50,
                },
            },
        );
        // 1200x1200 px: 900 px tall is 45 rows, 7 rows top margin.
        check_stretched(
            120,
            60,
            px,
            Placement {
                col: 0,
                row: 7,
                x: 0,
                y: 0,
                size: Span::Both {
                    cols: 120,
                    rows: 45,
                },
            },
        );
        // 1:2 stand-in cells, 80x48 units: 64 columns wide.
        check_stretched(
            80,
            24,
            CellSize::Unknown,
            Placement {
                col: 8,
                row: 0,
                x: 0,
                y: 0,
                size: Span::Both { cols: 64, rows: 24 },
            },
        );
    }

    #[test]
    fn only_a_missing_aspect_keeps_square_pixels() {
        assert_eq!(Shape::new(None, 160, 144), Shape::Native { w: 160, h: 144 });
        assert_eq!(
            Shape::new(Some(f64::NAN), 160, 144),
            Shape::Native { w: 160, h: 144 }
        );
        assert_eq!(
            Shape::new(Some(4.0 / 3.0), 640, 480),
            Shape::new(Some(4.0 / 3.0), 640, 240)
        );
    }

    #[test]
    fn auto_prescale_uses_the_displayed_size() {
        let grid = Grid {
            cols: 200,
            rows: 50,
            cell: CellSize::Pixels { w: 10, h: 20 },
        };
        // 640x240 shown at 1333x1000 px: 2.08x across, 4.17x down.
        let shape = Shape::new(Some(4.0 / 3.0), 640, 240);
        let m = magnification(grid, shape, 640, 240).unwrap();
        assert!((m - 2.083).abs() < 1e-3, "{m}");
        assert_eq!(Prescale::Auto.factor(Some(m), 640 * 240), 1);
    }

    #[test]
    fn frame_size_flip_keeps_the_placement() {
        let mut kitty = Kitty::new(Transport::Direct, Format::Rgba, Prescale::Fixed(1));
        kitty.grid = Some(Grid {
            cols: 200,
            rows: 50,
            cell: CellSize::Pixels { w: 10, h: 20 },
        });
        kitty.set_aspect(Some(4.0 / 3.0));
        let mut draw = |height: u32| {
            let rgba = vec![0u8; 640 * height as usize * 4];
            let mut out = Vec::new();
            let frame = Frame {
                width: 640,
                height,
                rgba: &rgba,
            };
            kitty.draw(&mut out, &frame).unwrap();
            String::from_utf8(out).unwrap()
        };
        let first = draw(240);
        assert!(
            first.starts_with("\x1b[2J\x1b[1;34H\x1b_Ga=T,"),
            "{first:.40}"
        );
        assert!(first.contains(",s=640,v=240,c=133,r=50,"));
        let second = draw(480);
        assert!(second.starts_with("\x1b[1;34H\x1b_Ga=T,"), "{second:.40}");
        assert!(second.contains(",s=640,v=480,c=133,r=50,"));
        assert!(!second.contains(",X="));
    }

    #[test]
    fn fit_centers_on_the_free_axis() {
        let px = CellSize::Pixels { w: 10, h: 20 };
        // 1000x1000 px for a 160x144 image: width-bound, 900 px tall, 50 px top margin.
        check(
            100,
            50,
            px,
            (160, 144),
            Placement {
                col: 0,
                row: 2,
                x: 0,
                y: 10,
                size: Span::Cols(100),
            },
        );
        // 2000x1000 px: height-bound, 1111 px wide, 444 px left margin.
        check(
            200,
            50,
            px,
            (160, 144),
            Placement {
                col: 44,
                row: 0,
                x: 4,
                y: 0,
                size: Span::Rows(50),
            },
        );
        // No pixel size: 1:2 cells, whole-cell offsets only.
        check(
            200,
            50,
            CellSize::Unknown,
            (240, 160),
            Placement {
                col: 25,
                row: 0,
                x: 0,
                y: 0,
                size: Span::Rows(50),
            },
        );
    }

    #[test]
    fn chunks_carry_m_flags() {
        let mut out = Vec::new();
        push_chunked(&mut out, &[0; 3072 * 2 + 1]);
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with(",m=1;"));
        assert_eq!(text.matches("\x1b_Gm=1;").count(), 1);
        assert_eq!(text.matches("\x1b_Gm=0;").count(), 1);
    }

    #[test]
    fn parses_spec() {
        assert_eq!(
            parse_spec("shm"),
            Some((Transport::SharedMemory, Format::Rgba))
        );
        assert_eq!(parse_spec("zlib"), Some((Transport::Zlib, Format::Rgb)));
        assert_eq!(
            parse_spec("direct,24"),
            Some((Transport::Direct, Format::Rgb))
        );
        assert_eq!(parse_spec("pigeon"), None);
    }

    #[test]
    fn prescale_repeats_pixels() {
        let rgba = [1, 2, 3, 255, 4, 5, 6, 255];
        let image = Image {
            rgba: &rgba,
            width: 2,
            height: 1,
            scale: 2,
        };
        let mut dst = vec![0; image.sent_pixels() * 3];
        Format::Rgb.encode(&image, &mut dst);
        assert_eq!(
            dst,
            [1, 2, 3, 1, 2, 3, 4, 5, 6, 4, 5, 6, 1, 2, 3, 1, 2, 3, 4, 5, 6, 4, 5, 6]
        );
    }

    #[test]
    fn auto_prescale_leaves_at_most_2x() {
        let gb = 160 * 144;
        assert_eq!(Prescale::Auto.factor(Some(9.8), gb), 4);
        assert_eq!(Prescale::Auto.factor(Some(3.9), gb), 1);
        assert_eq!(Prescale::Auto.factor(None, gb), 1);
        // 8x would be 1280x1152, over the pixel budget.
        assert_eq!(Prescale::Auto.factor(Some(20.0), gb), 7);
        assert_eq!(Prescale::Fixed(40).factor(None, gb), Prescale::MAX);
        assert_eq!(Prescale::parse("off"), Some(Prescale::Fixed(1)));
    }

    #[test]
    fn auto_prescale_keeps_n64_frames_small() {
        // A 6K-wide window would ask for 4x; the pixel budget allows 2x and 1x.
        assert_eq!(Prescale::Auto.factor(Some(9.0), 640 * 240), 2);
        assert_eq!(Prescale::Auto.factor(Some(9.0), 640 * 480), 2);
        assert_eq!(Prescale::Auto.factor(Some(9.0), 640 * 481), 1);
    }

    #[test]
    fn unread_shared_memory_falls_back_to_escapes() {
        let mut kitty = Kitty::new(Transport::SharedMemory, Format::Rgba, Prescale::Fixed(1));
        kitty.grid = Some(Grid {
            cols: 80,
            rows: 24,
            cell: CellSize::Unknown,
        });
        let mut rgba = vec![0u8; 4 * 4];
        let mut out = Vec::new();
        // Nobody reads the objects, so after the ring fills every frame is dropped.
        for i in 0..=GIVE_UP + 4 {
            rgba[0] = i as u8;
            kitty
                .draw(
                    &mut out,
                    &Frame {
                        width: 2,
                        height: 2,
                        rgba: &rgba,
                    },
                )
                .unwrap();
        }
        assert_eq!(kitty.transport, Transport::Zlib);
        rgba[0] = 255;
        out.clear();
        kitty
            .draw(
                &mut out,
                &Frame {
                    width: 2,
                    height: 2,
                    rgba: &rgba,
                },
            )
            .unwrap();
        assert!(String::from_utf8(out).unwrap().contains(",o=z;"));
    }
}
