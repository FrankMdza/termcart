/// Buttons held right now, as a bitmask indexed by libretro `RETRO_DEVICE_ID_JOYPAD_*` ids.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Joypad(pub u16);

impl Joypad {
    pub const B: u16 = 0;
    pub const Y: u16 = 1;
    pub const SELECT: u16 = 2;
    pub const START: u16 = 3;
    pub const UP: u16 = 4;
    pub const DOWN: u16 = 5;
    pub const LEFT: u16 = 6;
    pub const RIGHT: u16 = 7;
    pub const A: u16 = 8;
    pub const X: u16 = 9;
    pub const L: u16 = 10;
    pub const R: u16 = 11;
    pub const L2: u16 = 12;
    pub const R2: u16 = 13;

    pub fn pressed(self, id: u16) -> bool {
        self.0 & (1 << id) != 0
    }

    pub fn set(&mut self, id: u16, down: bool) {
        if down {
            self.0 |= 1 << id;
        } else {
            self.0 &= !(1 << id);
        }
    }
}
