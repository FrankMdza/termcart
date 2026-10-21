//! Keyboard to joypad and analog stick, with key release events.
//!
//! Two key schemes, picked with `TERMCART_KEYS=wasd|lol` (default `wasd`).
//!
//! `wasd`: the left hand moves, the right hand presses buttons.
//!
//! | Button        | Handheld (GB, GBC, GBA) | N64                       |
//! |---------------|-------------------------|---------------------------|
//! | Move          | W A S D, arrows         | W A S D (stick)           |
//! | Walk          |                         | Left Shift (held)         |
//! | A             | K                       | K                         |
//! | B             | J                       | J                         |
//! | L             | U                       | U                         |
//! | R             | I                       | I                         |
//! | Z             |                         | Space                     |
//! | C buttons     |                         | arrows                    |
//! | Start         | Enter, L                | Enter, L                  |
//! | Select        | Space, Backspace        |                           |
//! | D-pad         |                         | T F G H                   |
//!
//! `lol`: the right hand moves with the arrows, the left hand presses Q W E R as in League of
//! Legends, with D and F next to them.
//!
//! | Button        | Handheld (GB, GBC, GBA) | N64                       |
//! |---------------|-------------------------|---------------------------|
//! | Move          | arrows                  | arrows (stick)            |
//! | Walk          |                         | Left Shift (held)         |
//! | A             | W                       | W                         |
//! | B             | Q                       | Q                         |
//! | L             | E                       | T                         |
//! | R             | R                       | R                         |
//! | Z             |                         | E                         |
//! | C buttons     |                         | A S D F (left, down, up, right) |
//! | Start         | F, Enter                | Space, Enter              |
//! | Select        | D, Space                |                           |
//! | D-pad         |                         | I J K L                   |
//!
//! N64 buttons go through ParaLLEl N64 with `parallel-n64-alt-map = enabled`, where every N64
//! button is its own RetroPad id: A = B, B = Y, L = Select, R = R2, Z = L2, C-up = X,
//! C-down = A, C-left = L, C-right = R.
//!
//! Esc and Ctrl+C quit, Tab toggles fast forward, F2 switches between the two schemes. Left Shift only works in terminals with the
//! kitty keyboard protocol, since other terminals never report a bare modifier.

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

/// The key scheme, see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keys {
    Wasd,
    Lol,
}

impl Keys {
    /// Reads `TERMCART_KEYS`; anything but `lol` means `wasd`.
    pub fn from_env() -> Keys {
        match std::env::var("TERMCART_KEYS") {
            Ok(v) if v.trim().eq_ignore_ascii_case("lol") => Keys::Lol,
            _ => Keys::Wasd,
        }
    }
}

fn bindings(layout: Layout, keys: Keys) -> &'static [(KeyCode, Action)] {
    match (layout, keys) {
        (Layout::Handheld, Keys::Wasd) => &HANDHELD_WASD,
        (Layout::Handheld, Keys::Lol) => &HANDHELD_LOL,
        (Layout::N64, Keys::Wasd) => &N64_WASD,
        (Layout::N64, Keys::Lol) => &N64_LOL,
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

const HANDHELD_WASD: [(KeyCode, Action); 16] = [
    (KeyCode::Char('w'), Action::Button(Joypad::UP)),
    (KeyCode::Char('a'), Action::Button(Joypad::LEFT)),
    (KeyCode::Char('s'), Action::Button(Joypad::DOWN)),
    (KeyCode::Char('d'), Action::Button(Joypad::RIGHT)),
    (KeyCode::Up, Action::Button(Joypad::UP)),
    (KeyCode::Left, Action::Button(Joypad::LEFT)),
    (KeyCode::Down, Action::Button(Joypad::DOWN)),
    (KeyCode::Right, Action::Button(Joypad::RIGHT)),
    (KeyCode::Char('k'), Action::Button(Joypad::A)),
    (KeyCode::Char('j'), Action::Button(Joypad::B)),
    (KeyCode::Char('u'), Action::Button(Joypad::L)),
    (KeyCode::Char('i'), Action::Button(Joypad::R)),
    (KeyCode::Enter, Action::Button(Joypad::START)),
    (KeyCode::Char('l'), Action::Button(Joypad::START)),
    (KeyCode::Char(' '), Action::Button(Joypad::SELECT)),
    (KeyCode::Backspace, Action::Button(Joypad::SELECT)),
];

const HANDHELD_LOL: [(KeyCode, Action); 12] = [
    (KeyCode::Up, Action::Button(Joypad::UP)),
    (KeyCode::Left, Action::Button(Joypad::LEFT)),
    (KeyCode::Down, Action::Button(Joypad::DOWN)),
    (KeyCode::Right, Action::Button(Joypad::RIGHT)),
    (KeyCode::Char('w'), Action::Button(Joypad::A)),
    (KeyCode::Char('q'), Action::Button(Joypad::B)),
    (KeyCode::Char('e'), Action::Button(Joypad::L)),
    (KeyCode::Char('r'), Action::Button(Joypad::R)),
    (KeyCode::Char('f'), Action::Button(Joypad::START)),
    (KeyCode::Enter, Action::Button(Joypad::START)),
    (KeyCode::Char('d'), Action::Button(Joypad::SELECT)),
    (KeyCode::Char(' '), Action::Button(Joypad::SELECT)),
];

// RetroPad ids from parallel-n64's alt-map branch of inputGetKeys_default,
// mupen64plus-core/src/plugin/emulate_game_controller_via_libretro.c L885-L897 at 6f23339.
// The stick is read in inputGetKeys_reuse, L514-L517 of the same file.
const N64_WASD: [(KeyCode, Action); 20] = [
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
    (KeyCode::Char('u'), Action::Button(Joypad::SELECT)),
    (KeyCode::Char('i'), Action::Button(Joypad::R2)),
    (KeyCode::Char(' '), Action::Button(Joypad::L2)),
    (KeyCode::Enter, Action::Button(Joypad::START)),
    (KeyCode::Char('l'), Action::Button(Joypad::START)),
    (KeyCode::Char('t'), Action::Button(Joypad::UP)),
    (KeyCode::Char('f'), Action::Button(Joypad::LEFT)),
    (KeyCode::Char('g'), Action::Button(Joypad::DOWN)),
    (KeyCode::Char('h'), Action::Button(Joypad::RIGHT)),
];

const N64_LOL: [(KeyCode, Action); 20] = [
    (KeyCode::Up, Action::Stick(Direction::Up)),
    (KeyCode::Left, Action::Stick(Direction::Left)),
    (KeyCode::Down, Action::Stick(Direction::Down)),
    (KeyCode::Right, Action::Stick(Direction::Right)),
    (KeyCode::Modifier(ModifierKeyCode::LeftShift), Action::Walk),
    (KeyCode::Char('w'), Action::Button(Joypad::B)),
    (KeyCode::Char('q'), Action::Button(Joypad::Y)),
    (KeyCode::Char('e'), Action::Button(Joypad::L2)),
    (KeyCode::Char('r'), Action::Button(Joypad::R2)),
    (KeyCode::Char('t'), Action::Button(Joypad::SELECT)),
    (KeyCode::Char('a'), Action::Button(Joypad::L)),
    (KeyCode::Char('s'), Action::Button(Joypad::A)),
    (KeyCode::Char('d'), Action::Button(Joypad::X)),
    (KeyCode::Char('f'), Action::Button(Joypad::R)),
    (KeyCode::Char(' '), Action::Button(Joypad::START)),
    (KeyCode::Enter, Action::Button(Joypad::START)),
    (KeyCode::Char('i'), Action::Button(Joypad::UP)),
    (KeyCode::Char('j'), Action::Button(Joypad::LEFT)),
    (KeyCode::Char('k'), Action::Button(Joypad::DOWN)),
    (KeyCode::Char('l'), Action::Button(Joypad::RIGHT)),
];

/// Bits in `Input::held`, so the most bindings a layout may have.
const SLOTS: usize = 32;
const _: () = assert!(
    HANDHELD_WASD.len() <= SLOTS
        && HANDHELD_LOL.len() <= SLOTS
        && N64_WASD.len() <= SLOTS
        && N64_LOL.len() <= SLOTS
);

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
    layout: Layout,
    keys: Keys,
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
    pub fn new(layout: Layout, keys: Keys) -> Result<Input> {
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
            layout,
            keys,
            bindings: bindings(layout, keys),
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
                KeyCode::F(2) => {
                    self.switch_keys();
                    return;
                }
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

impl Input {
    /// Swaps between the `wasd` and `lol` schemes mid-game. Held keys are dropped, since
    /// their slots mean other buttons in the new table.
    fn switch_keys(&mut self) {
        self.keys = match self.keys {
            Keys::Wasd => Keys::Lol,
            Keys::Lol => Keys::Wasd,
        };
        self.bindings = bindings(self.layout, self.keys);
        self.held = 0;
        self.expires = [None; SLOTS];
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
    use super::{bindings, stick, Keys, Layout};

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

    #[test]
    fn no_key_is_bound_twice() {
        for layout in [Layout::Handheld, Layout::N64] {
            for keys in [Keys::Wasd, Keys::Lol] {
                let table = bindings(layout, keys);
                for (i, (key, _)) in table.iter().enumerate() {
                    assert!(
                        table[i + 1..].iter().all(|(other, _)| other != key),
                        "{key:?} bound twice in {layout:?} {keys:?}"
                    );
                }
            }
        }
    }
}
