/// One video frame from the core, converted to tightly packed RGBA8 (`width * height * 4` bytes).
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
}
