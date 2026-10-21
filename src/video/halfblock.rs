//! Fallback for terminals without graphics: each cell shows two pixels stacked, the top
//! one as the foreground of U+2580 and the bottom one as its background.

use std::io::{self, Write};

use super::{div_round, Grid, Renderer, Shape};
use crate::frame::Frame;

const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
const UPPER: &[u8] = "\u{2580}".as_bytes();
const LOWER: &[u8] = "\u{2584}".as_bytes();
const FULL: &[u8] = "\u{2588}".as_bytes();
/// No real 24-bit color has the top byte set, so this forces the first comparison to fail.
const UNKNOWN: u32 = u32::MAX;

pub struct HalfBlock {
    grid: Option<Grid>,
    aspect: Option<f64>,
    layout: Option<Layout>,
    /// Top and bottom color of every cell in the image rectangle, as last written.
    cells: Vec<(u32, u32)>,
    prev: Vec<u8>,
    out: Vec<u8>,
    wipe: bool,
}

/// The image rectangle in cells and, per cell column and pixel row, the source pixel.
struct Layout {
    frame: (u32, u32),
    left: u16,
    top: u16,
    src_x: Vec<u32>,
    /// Two entries per cell row: top pixel row, then bottom.
    src_y: Vec<u32>,
}

impl Layout {
    fn new(grid: Grid, shape: Shape, width: u32, height: u32) -> Layout {
        let (cw, ch) = grid.cell.dims();
        // A half cell is cw x ch/2 pixels; fit the shape into cols x 2*rows of those.
        let (cols, sub_rows) = (u64::from(grid.cols), 2 * u64::from(grid.rows));
        let (w, h) = shape.ratio();
        let (cw, ch) = (u64::from(cw), u64::from(ch));
        let (out_w, out_h) = if w * sub_rows * ch <= h * cols * 2 * cw {
            (div_round(w * sub_rows * ch, h * 2 * cw), sub_rows)
        } else {
            (cols, div_round(h * cols * 2 * cw, w * ch))
        };
        let out_w = out_w.clamp(1, cols);
        // Even height keeps the rectangle on whole cells.
        let out_h = (out_h.min(sub_rows) & !1).max(2);
        let left = ((cols - out_w) / 2) as u16;
        let top = ((sub_rows - out_h) / 4) as u16;
        let (w, h) = (u64::from(width), u64::from(height));
        let src_x = (0..out_w).map(|x| (x * w / out_w) as u32).collect();
        let src_y = (0..out_h).map(|y| (y * h / out_h) as u32).collect();
        Layout {
            frame: (width, height),
            left,
            top,
            src_x,
            src_y,
        }
    }

    fn cols(&self) -> usize {
        self.src_x.len()
    }

    fn rows(&self) -> usize {
        self.src_y.len() / 2
    }
}

impl HalfBlock {
    pub fn new() -> HalfBlock {
        HalfBlock {
            grid: None,
            aspect: None,
            layout: None,
            cells: Vec::new(),
            prev: Vec::new(),
            out: Vec::new(),
            wipe: true,
        }
    }

    fn invalidate(&mut self) {
        self.layout = None;
        self.wipe = true;
    }
}

impl Default for HalfBlock {
    fn default() -> Self {
        HalfBlock::new()
    }
}

impl Renderer for HalfBlock {
    fn resize(&mut self, cols: u16, rows: u16) {
        let grid = Grid::query(cols, rows);
        if self.grid != Some(grid) {
            self.grid = Some(grid);
            self.invalidate();
        }
    }

    fn set_aspect(&mut self, aspect: Option<f64>) {
        if self.aspect != aspect {
            self.aspect = aspect;
            self.invalidate();
        }
    }

    fn draw(&mut self, out: &mut dyn Write, frame: &Frame) -> io::Result<()> {
        let Some(grid) = self.grid else {
            return Ok(());
        };
        if grid.cols == 0 || grid.rows == 0 || frame.width == 0 || frame.height == 0 {
            return Ok(());
        }
        let dims = (frame.width, frame.height);
        if self.layout.as_ref().is_none_or(|l| l.frame != dims) {
            let shape = Shape::new(self.aspect, frame.width, frame.height);
            let layout = Layout::new(grid, shape, frame.width, frame.height);
            self.cells.clear();
            self.cells
                .resize(layout.cols() * layout.rows(), (UNKNOWN, UNKNOWN));
            self.layout = Some(layout);
            self.wipe = true;
        }
        // A frame equal to the last one changes no cell; skip the per-cell walk.
        if !self.wipe && frame.rgba == self.prev.as_slice() {
            return Ok(());
        }
        self.prev.clear();
        self.prev.extend_from_slice(frame.rgba);
        let layout = self.layout.as_ref().expect("layout set above");

        self.out.clear();
        self.out.extend_from_slice(SYNC_BEGIN);
        if self.wipe {
            self.out.extend_from_slice(b"\x1b[0m\x1b[2J");
        }
        let header = self.out.len();
        let mut pen = Pen::default();
        let stride = frame.width as usize;
        for row in 0..layout.rows() {
            let top_line = layout.src_y[2 * row] as usize * stride;
            let bottom_line = layout.src_y[2 * row + 1] as usize * stride;
            let y = layout.top + row as u16;
            let cells = &mut self.cells[row * layout.cols()..][..layout.cols()];
            for (col, (&sx, cell)) in layout.src_x.iter().zip(cells).enumerate() {
                let pair = (
                    rgb(frame.rgba, top_line + sx as usize),
                    rgb(frame.rgba, bottom_line + sx as usize),
                );
                if *cell == pair {
                    continue;
                }
                *cell = pair;
                let x = layout.left + col as u16;
                pen.move_to(&mut self.out, x, y);
                pen.paint(&mut self.out, pair);
                pen.at = if x + 1 < grid.cols {
                    Some((x + 1, y))
                } else {
                    None
                };
            }
        }
        if self.out.len() == header && !self.wipe {
            return Ok(());
        }
        self.out.extend_from_slice(b"\x1b[0m");
        self.out.extend_from_slice(SYNC_END);
        out.write_all(&self.out)?;
        out.flush()?;
        self.wipe = false;
        Ok(())
    }

    fn clear(&mut self, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(b"\x1b[0m\x1b[2J")?;
        out.flush()?;
        self.invalidate();
        Ok(())
    }
}

fn rgb(rgba: &[u8], pixel: usize) -> u32 {
    let p = &rgba[pixel * 4..pixel * 4 + 3];
    u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2])
}

/// What the terminal's cursor and SGR state are while we build a frame.
#[derive(Default)]
struct Pen {
    /// Cursor cell, unknown at frame start and after writing the last column.
    at: Option<(u16, u16)>,
    fg: Option<u32>,
    bg: Option<u32>,
}

impl Pen {
    fn move_to(&self, out: &mut Vec<u8>, x: u16, y: u16) {
        match self.at {
            Some(at) if at == (x, y) => {}
            Some((cx, cy)) if cy == y && cx < x => {
                out.extend_from_slice(b"\x1b[");
                if x - cx > 1 {
                    push_num(out, x - cx);
                }
                out.push(b'C');
            }
            _ => {
                out.extend_from_slice(b"\x1b[");
                push_num(out, y + 1);
                out.push(b';');
                push_num(out, x + 1);
                out.push(b'H');
            }
        }
    }

    /// Writes one cell, picking the glyph that needs the fewest color changes.
    fn paint(&mut self, out: &mut Vec<u8>, (top, bottom): (u32, u32)) {
        let (glyph, fg, bg) = if top == bottom {
            if self.bg == Some(top) {
                (&b" "[..], None, None)
            } else if self.fg == Some(top) {
                (FULL, None, None)
            } else {
                (&b" "[..], None, Some(top))
            }
        } else {
            let changes = |fg, bg| u8::from(self.fg != Some(fg)) + u8::from(self.bg != Some(bg));
            let (glyph, fg, bg) = if changes(bottom, top) < changes(top, bottom) {
                (LOWER, bottom, top)
            } else {
                (UPPER, top, bottom)
            };
            (
                glyph,
                (self.fg != Some(fg)).then_some(fg),
                (self.bg != Some(bg)).then_some(bg),
            )
        };
        if fg.is_some() || bg.is_some() {
            out.extend_from_slice(b"\x1b[");
            if let Some(c) = fg {
                out.extend_from_slice(b"38;2;");
                push_rgb(out, c);
                self.fg = Some(c);
            }
            if let Some(c) = bg {
                if fg.is_some() {
                    out.push(b';');
                }
                out.extend_from_slice(b"48;2;");
                push_rgb(out, c);
                self.bg = Some(c);
            }
            out.push(b'm');
        }
        out.extend_from_slice(glyph);
    }
}

fn push_rgb(out: &mut Vec<u8>, c: u32) {
    push_num(out, (c >> 16) as u8 as u16);
    out.push(b';');
    push_num(out, (c >> 8) as u8 as u16);
    out.push(b';');
    push_num(out, c as u8 as u16);
}

fn push_num(out: &mut Vec<u8>, n: u16) {
    let mut digits = [0u8; 5];
    let mut i = digits.len();
    let mut n = n;
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[i..]);
}

#[cfg(test)]
mod tests {
    use super::super::CellSize;
    use super::*;

    fn render(r: &mut HalfBlock, w: u32, h: u32, rgba: &[u8]) -> String {
        let mut out = Vec::new();
        r.draw(
            &mut out,
            &Frame {
                width: w,
                height: h,
                rgba,
            },
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn renderer(cols: u16, rows: u16) -> HalfBlock {
        let mut r = HalfBlock::new();
        r.grid = Some(Grid {
            cols,
            rows,
            cell: CellSize::Pixels { w: 10, h: 20 },
        });
        r
    }

    #[test]
    fn two_by_two_frame_in_one_cell_row() {
        // Red over blue on the left, green over green on the right.
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0, 255, 0, 255,
        ];
        let mut r = renderer(2, 1);
        let first = render(&mut r, 2, 2, &rgba);
        assert_eq!(
            first,
            "\x1b[?2026h\x1b[0m\x1b[2J\x1b[1;1H\x1b[38;2;255;0;0;48;2;0;0;255m\u{2580}\
             \x1b[48;2;0;255;0m \x1b[0m\x1b[?2026l"
        );
        assert_eq!(
            render(&mut r, 2, 2, &rgba),
            "",
            "unchanged frame writes nothing"
        );
    }

    #[test]
    fn only_changed_cells_are_rewritten() {
        let mut rgba = vec![0u8; 4 * 4 * 2];
        let mut r = renderer(4, 1);
        render(&mut r, 4, 2, &rgba);
        rgba[3 * 4] = 9; // top pixel of the last column
        assert_eq!(
            render(&mut r, 4, 2, &rgba),
            "\x1b[?2026h\x1b[1;4H\x1b[38;2;9;0;0;48;2;0;0;0m\u{2580}\x1b[0m\x1b[?2026l"
        );
    }

    #[test]
    fn layout_keeps_aspect_and_centers() {
        // 100x50 cells of 10x20 px: half cells are square, 100x100 of them.
        let grid = Grid {
            cols: 100,
            rows: 50,
            cell: CellSize::Pixels { w: 10, h: 20 },
        };
        let layout = |aspect, w, h| Layout::new(grid, Shape::new(aspect, w, h), w, h);
        let l = layout(None, 160, 144);
        assert_eq!((l.cols(), l.rows(), l.left, l.top), (100, 45, 0, 2));
        let l = layout(None, 240, 160);
        assert_eq!((l.cols(), l.rows(), l.left, l.top), (100, 33, 0, 8));
        let l = layout(Some(10.0 / 9.0), 160, 144);
        assert_eq!((l.cols(), l.rows(), l.left, l.top), (100, 45, 0, 2));
    }

    #[test]
    fn layout_stretches_to_the_aspect() {
        // 120x60 cells of 10x20 px: 120x120 square half cells; 4:3 is 120x90.
        let grid = Grid {
            cols: 120,
            rows: 60,
            cell: CellSize::Pixels { w: 10, h: 20 },
        };
        for (height, last_row) in [(240, 237), (480, 474)] {
            let l = Layout::new(grid, Shape::new(Some(4.0 / 3.0), 640, height), 640, height);
            assert_eq!((l.cols(), l.rows(), l.left, l.top), (120, 45, 0, 7));
            assert_eq!(l.src_x[119], 634);
            assert_eq!(l.src_y.last(), Some(&last_row));
        }
    }
}
