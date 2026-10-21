//! Keyboard to joypad and analog stick, with key release events.
//!
//! `Layout::Handheld`, for GB, GBC and GBA. The left hand fits WASD plus Q/E, the right hand
//! fits J/K plus U/I, and the arrow keys pair with Z/X for players who prefer them:
//!
//! | Button | Keys                   |
//! |--------|------------------------|
//! | D-pad  | arrows, W A S D        |
//! | A      | X, K                   |
//! | B      | Z, J                   |
//! | L      | Q, U                   |
//! | R      | E, I                   |
//! | Start  | Enter                  |
//! | Select | Backspace, Right Shift |
//!
//! `Layout::N64`, for ParaLLEl N64 with `parallel-n64-alt-map = enabled`, where every N64
//! button is its own RetroPad button:
//!
//! | N64       | Keys                | RetroPad id |
//! |-----------|---------------------|-------------|
//! | Stick     | W A S D             | left analog |
//! | Walk      | Left Shift (held)   | stick x 0.4 |
//! | C buttons | arrows              | X, A, L, R  |
//! | A         | K                   | B           |
//! | B         | J                   | Y           |
//! | Z         | Space               | L2          |
//! | L         | Q                   | Select      |
//! | R         | E                   | R2          |
//! | Start     | Enter               | Start       |
//! | D-pad     | T F G H             | D-pad       |
//!
//! Esc and Ctrl+C quit, Tab toggles fast forward. Right and Left Shift only work in terminals
//! with the kitty keyboard protocol, since other terminals never report a bare modifier.

use std::io::{self, IsTerminal, Write};
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableFocusChange, EnableFocusChange, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, ModifierKeyCode, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};

use crate::joypad::Joypad;

pub enum Command {
    Quit,
    Resize(u16, u16),
    /// Toggles running the core without frame pacing.
    FastForward,
}

/// Which key table `Input` uses, chosen by the system the core emulates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Handheld,
    N64,
}

impl Layout {
    fn bindings(self) -> &'static [(KeyCode, Action)] {
        match self {
            Layout::Handheld => &HANDHELD,
            Layout::N64 => &N64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Button(u16),
    Stick(Direction),
    /// Scales the stick down to `WALK` while held.
    Walk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

const HANDHELD: [(KeyCode, Action); 19] = [
    (KeyCode::Up, Action::Button(Joypad::UP)),
    (KeyCode::Char('w'), Action::Button(Joypad::UP)),
    (KeyCode::Down, Action::Button(Joypad::DOWN)),
    (KeyCode::Char('s'), Action::Button(Joypad::DOWN)),
    (KeyCode::Left, Action::Button(Joypad::LEFT)),
    (KeyCode::Char('a'), Action::Button(Joypad::LEFT)),
    (KeyCode::Right, Action::Button(Joypad::RIGHT)),
    (KeyCode::Char('d'), Action::Button(Joypad::RIGHT)),
    (KeyCode::Char('x'), Action::Button(Joypad::A)),
    (KeyCode::Char('k'), Action::Button(Joypad::A)),
    (KeyCode::Char('z'), Action::Button(Joypad::B)),
    (KeyCode::Char('j'), Action::Button(Joypad::B)),
    (KeyCode::Char('q'), Action::Button(Joypad::L)),
    (KeyCode::Char('u'), Action::Button(Joypad::L)),
    (KeyCode::Char('e'), Action::Button(Joypad::R)),
    (KeyCode::Char('i'), Action::Button(Joypad::R)),
    (KeyCode::Enter, Action::Button(Joypad::START)),
    (KeyCode::Backspace, Action::Button(Joypad::SELECT)),
    (
        KeyCode::Modifier(ModifierKeyCode::RightShift),
        Action::Button(Joypad::SELECT),
    ),
];

// RetroPad ids from parallel-n64's alt-map branch of inputGetKeys_default,
// mupen64plus-core/src/plugin/emulate_game_controller_via_libretro.c L885-L897 at 6f23339.
// The stick is read in inputGetKeys_reuse, L514-L517 of the same file.
const N64: [(KeyCode, Action); 19] = [
    (KeyCode::Char('w'), Action::Stick(Direction::Up)),
    (KeyCode::Char('a'), Action::Stick(Direction::Left)),
    (KeyCode::Char('s'), Action::Stick(Direction::Down)),
    (KeyCode::Char('d'), Action::Stick(Direction::Right)),
    (KeyCode::Modifier(ModifierKeyCode::LeftShift), Action::Walk),
    (KeyCode::Up, Action::Button(Joypad::X)),
    (KeyCode::Down, Action::Button(Joypad::A)),
    (KeyCode::Left, Action::Button(Joypad::L)),
    (KeyCode::Right, Action::Button(Joypad::R)),
    (KeyCode::Char('k'), Action::Button(Joypad::B)),
    (KeyCode::Char('j'), Action::Button(Joypad::Y)),
    (KeyCode::Char(' '), Action::Button(Joypad::L2)),
    (KeyCode::Char('q'), Action::Button(Joypad::SELECT)),
    (KeyCode::Char('e'), Action::Button(Joypad::R2)),
    (KeyCode::Enter, Action::Button(Joypad::START)),
    (KeyCode::Char('t'), Action::Button(Joypad::UP)),
    (KeyCode::Char('f'), Action::Button(Joypad::LEFT)),
    (KeyCode::Char('g'), Action::Button(Joypad::DOWN)),
    (KeyCode::Char('h'), Action::Button(Joypad::RIGHT)),
];

/// Bits in `Input::held`, so the most bindings a layout may have.
const SLOTS: usize = 32;
const _: () = assert!(HANDHELD.len() <= SLOTS && N64.len() <= SLOTS);

/// Full deflection on one axis, in libretro's analog range.
const STICK_MAX: f64 = 32767.0;
/// Stick deflection while walk is held.
const WALK: f64 = 0.4;

/// Without release events, a key counts as held this long after its last press or repeat.
const HOLD: Duration = Duration::from_millis(130);

/// How the terminal tells us a key went up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRelease {
    /// The terminal sends release events (kitty keyboard protocol).
    Reported,
    /// Keys expire `HOLD` after the last press or repeat.
    Timed,
}

/// Terminal state is process-wide, so only one `Input` may exist at a time.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static ENHANCED: AtomicBool = AtomicBool::new(false);

pub struct Input {
    bindings: &'static [(KeyCode, Action)],
    release: KeyRelease,
    /// Bit `i` set while `bindings[i]` is down, when releases are reported.
    held: u32,
    /// When each binding stops counting as held, when releases are timed.
    expires: [Option<Instant>; SLOTS],
}

impl Input {
    /// Enters raw mode and enables key release reporting when the terminal supports it.
    ///
    /// Detection queries the terminal and waits up to 2 s for an answer; terminals that
    /// answer device attributes (all common ones) reply at once.
    ///
    /// # Errors
    ///
    /// Fails when stdin is not a terminal, when another `Input` is alive, or when writing the
    /// setup sequences fails. Whatever was already changed is restored first.
    pub fn new(layout: Layout) -> Result<Input> {
        // crossterm falls back to /dev/tty, and a background process reading it gets errors
        // that its keyboard enhancement query retries forever.
        if !io::stdin().is_terminal() {
            bail!("stdin is not a terminal");
        }
        install_panic_hook();
        if ACTIVE.swap(true, Ordering::AcqRel) {
            bail!("the terminal is already in use by another Input");
        }
        if let Err(err) = enable_raw_mode() {
            ACTIVE.store(false, Ordering::Release);
            return Err(err).context("enabling raw mode");
        }
        // From here on, dropping `input` restores the terminal.
        let mut input = Input {
            bindings: layout.bindings(),
            release: KeyRelease::Timed,
            held: 0,
            expires: [None; SLOTS],
        };
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen, Hide, EnableFocusChange)
            .context("setting up the terminal")?;
        if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(
                out,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                        // Without this, Enter, Backspace and modifiers never report releases.
                        | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                )
            )
            .context("enabling the kitty keyboard protocol")?;
            ENHANCED.store(true, Ordering::Release);
            input.release = KeyRelease::Reported;
        }
        Ok(input)
    }

    /// Drains pending terminal events without blocking longer than `timeout`.
    ///
    /// # Errors
    ///
    /// Fails when reading from the terminal fails.
    pub fn poll(&mut self, timeout: Duration, commands: &mut Vec<Command>) -> Result<()> {
        self.poll_observed(timeout, commands, |_| {})
    }

    /// Same as `poll`, and hands every key event to `observe` before handling it.
    ///
    /// # Errors
    ///
    /// Fails when reading from the terminal fails.
    pub fn poll_observed(
        &mut self,
        timeout: Duration,
        commands: &mut Vec<Command>,
        mut observe: impl FnMut(&KeyEvent),
    ) -> Result<()> {
        let mut wait = timeout;
        while event::poll(wait).context("polling terminal events")? {
            wait = Duration::ZERO;
            match event::read().context("reading terminal events")? {
                Event::Key(key) => {
                    observe(&key);
                    self.key(key, commands);
                }
                Event::Resize(width, height) => commands.push(Command::Resize(width, height)),
                Event::FocusLost => {
                    self.held = 0;
                    self.expires = [None; SLOTS];
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn joypad(&self) -> Joypad {
        let now = Instant::now();
        let mut pad = Joypad::default();
        for (slot, &(_, action)) in self.bindings.iter().enumerate() {
            if let Action::Button(button) = action {
                if self.down(slot, now) {
                    pad.set(button, true);
                }
            }
        }
        pad
    }

    /// The left analog stick as libretro reads it: x positive right, y positive down, each
    /// axis in -32767..=32767. Always centered in `Layout::Handheld`.
    pub fn left_stick(&self) -> (i16, i16) {
        let now = Instant::now();
        let (mut x, mut y, mut walk) = (0, 0, false);
        for (slot, &(_, action)) in self.bindings.iter().enumerate() {
            if !self.down(slot, now) {
                continue;
            }
            match action {
                Action::Stick(Direction::Up) => y -= 1,
                Action::Stick(Direction::Down) => y += 1,
                Action::Stick(Direction::Left) => x -= 1,
                Action::Stick(Direction::Right) => x += 1,
                Action::Walk => walk = true,
                Action::Button(_) => {}
            }
        }
        stick(x, y, walk)
    }

    fn down(&self, slot: usize, now: Instant) -> bool {
        match self.release {
            KeyRelease::Reported => self.held & (1 << slot) != 0,
            KeyRelease::Timed => self.expires[slot].is_some_and(|t| t > now),
        }
    }

    pub fn key_release(&self) -> KeyRelease {
        self.release
    }

    fn key(&mut self, key: KeyEvent, commands: &mut Vec<Command>) {
        if key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Esc => commands.push(Command::Quit),
                KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    commands.push(Command::Quit);
                }
                KeyCode::Tab => commands.push(Command::FastForward),
                _ => {}
            }
        }
        // Shortcuts like Ctrl+X are not buttons, but a release always counts so no key sticks.
        let chord = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
        if key.kind != KeyEventKind::Release && key.modifiers.intersects(chord) {
            return;
        }
        let code = match key.code {
            KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
            code => code,
        };
        let Some(slot) = self.bindings.iter().position(|&(k, _)| k == code) else {
            return;
        };
        // A release proves the terminal reports them, whatever detection said.
        if key.kind == KeyEventKind::Release && self.release == KeyRelease::Timed {
            self.release = KeyRelease::Reported;
            self.held = 0;
        }
        match (self.release, key.kind) {
            (KeyRelease::Reported, KeyEventKind::Release) => self.held &= !(1 << slot),
            (KeyRelease::Reported, _) => self.held |= 1 << slot,
            (KeyRelease::Timed, _) => self.expires[slot] = Some(Instant::now() + HOLD),
        }
    }
}

/// `x` and `y` are -1, 0 or 1. A diagonal keeps the length of a straight push.
fn stick(x: i8, y: i8, walk: bool) -> (i16, i16) {
    let mut scale = if walk { STICK_MAX * WALK } else { STICK_MAX };
    if x != 0 && y != 0 {
        scale *= std::f64::consts::FRAC_1_SQRT_2;
    }
    // Truncating keeps a rounded diagonal from overshooting full deflection.
    let axis = |v: i8| (f64::from(v) * scale) as i16;
    (axis(x), axis(y))
}

impl Drop for Input {
    fn drop(&mut self) {
        restore();
    }
}

/// Best effort: every step runs even if an earlier one fails. Safe to call twice.
fn restore() {
    if !ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    let mut out = io::stdout();
    if ENHANCED.swap(false, Ordering::AcqRel) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableFocusChange, Show, LeaveAlternateScreen);
    let _ = out.flush();
    let _ = disable_raw_mode();
}

/// Restores the terminal before the panic message prints. The hook runs before the abort
/// that `panic = "abort"` triggers, so this also covers release builds.
fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::stick;

    fn check(x: i8, y: i8, walk: bool, expected: (i16, i16)) {
        assert_eq!(stick(x, y, walk), expected, "stick({x}, {y}, {walk})");
    }

    #[test]
    fn centered_without_keys() {
        check(0, 0, false, (0, 0));
        check(0, 0, true, (0, 0));
    }

    #[test]
    fn straight_pushes_reach_full_deflection() {
        check(1, 0, false, (32767, 0));
        check(-1, 0, false, (-32767, 0));
        check(0, -1, false, (0, -32767));
        check(0, 1, false, (0, 32767));
    }

    #[test]
    fn diagonals_keep_the_straight_length() {
        check(1, 1, false, (23169, 23169));
        check(-1, -1, false, (-23169, -23169));
        check(1, -1, false, (23169, -23169));
        for (x, y) in [(1, 1), (-1, 1), (1, -1), (-1, -1)] {
            for walk in [false, true] {
                let (sx, sy) = stick(x, y, walk);
                let length = f64::from(sx).hypot(f64::from(sy));
                assert!(length <= 32767.0, "({x}, {y}, {walk}) has length {length}");
            }
        }
    }

    #[test]
    fn walk_scales_to_forty_percent() {
        check(1, 0, true, (13106, 0));
        check(0, -1, true, (0, -13106));
        check(-1, 1, true, (-9267, 9267));
    }
}
