//! Loads a libretro core from a dynamic library and drives it one frame at a time.

mod ffi;
mod host;
mod options;
mod pixels;

use std::ffi::{CStr, CString};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::ptr::{self, NonNull};

use anyhow::{bail, ensure, Context, Result};
use libloading::Library;

use crate::frame::Frame;
use crate::joypad::Joypad;
use host::Host;
use options::Overrides;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AvInfo {
    pub base_width: u32,
    pub base_height: u32,
    /// Display aspect (width / height) of a frame, which can differ from its pixel aspect:
    /// ParaLLEl N64 sends 640x240 frames meant to be shown at 4:3.
    pub aspect_ratio: f64,
    pub fps: f64,
    pub sample_rate: f64,
}

impl AvInfo {
    /// Takes the core's new geometry and keeps the timing.
    fn with_geometry(self, g: &ffi::GameGeometry) -> AvInfo {
        AvInfo {
            base_width: g.base_width,
            base_height: g.base_height,
            aspect_ratio: aspect_ratio(g),
            ..self
        }
    }
}

impl From<&ffi::SystemAvInfo> for AvInfo {
    fn from(av: &ffi::SystemAvInfo) -> AvInfo {
        AvInfo {
            base_width: av.geometry.base_width,
            base_height: av.geometry.base_height,
            aspect_ratio: aspect_ratio(&av.geometry),
            fps: av.timing.fps,
            sample_rate: av.timing.sample_rate,
        }
    }
}

/// libretro.h: an aspect of zero or less means the frame's pixels are square.
fn aspect_ratio(g: &ffi::GameGeometry) -> f64 {
    if g.aspect_ratio > 0.0 {
        f64::from(g.aspect_ratio)
    } else if g.base_height > 0 {
        f64::from(g.base_width) / f64::from(g.base_height)
    } else {
        1.0
    }
}

/// A loaded core. Only one can exist per process, because libretro callbacks are global.
pub struct Core {
    api: ffi::Api,
    /// Owned here, shared with the callbacks through `host::claim`. Freed in `Drop`.
    host: NonNull<Host>,
    game: Option<Game>,
    need_fullpath: bool,
    /// Save RAM as of the last load or write, to skip writes when nothing changed.
    saved: Vec<u8>,
    // Last field so it is dropped after `Drop::drop` has deinitialized the core.
    _lib: Library,
}

/// What the core may still point into while a game is loaded.
struct Game {
    _path: CString,
    _data: Vec<u8>,
}

impl Core {
    /// Opens the core library and calls `retro_init`.
    pub fn load(core_path: &Path) -> Result<Core> {
        // SAFETY: loading runs the library's initializers; the caller vouches it is a core.
        let lib = unsafe { Library::new(core_path) }
            .with_context(|| format!("opening core {}", core_path.display()))?;
        // SAFETY: a libretro core exports these symbols, and `lib` outlives `api` in `Core`.
        let api = unsafe { ffi::Api::resolve(&lib) }
            .with_context(|| format!("{} is not a libretro core", core_path.display()))?;
        // SAFETY: no preconditions.
        let version = unsafe { (api.api_version)() };
        ensure!(
            version == ffi::API_VERSION,
            "core speaks libretro API {version}, want 1"
        );

        let mut sys = ffi::SystemInfo {
            library_name: ptr::null(),
            library_version: ptr::null(),
            valid_extensions: ptr::null(),
            need_fullpath: false,
            block_extract: false,
        };
        // SAFETY: `sys` is a valid out-parameter; libretro allows this call before init.
        unsafe { (api.get_system_info)(&mut sys) };
        let library_name = if sys.library_name.is_null() {
            c""
        } else {
            // SAFETY: a non-null name is a C string with static storage.
            unsafe { CStr::from_ptr(sys.library_name) }
        };
        let overrides = Overrides::for_core(library_name);

        // BIOS files live next to the core.
        let system_dir = core_path.parent().and_then(dir_cstring);
        let host = NonNull::from(Box::leak(Box::new(Host::new(system_dir, overrides))));
        if !host::claim(host.as_ptr()) {
            // SAFETY: `host` came from `Box::leak` above and was never published.
            drop(unsafe { Box::from_raw(host.as_ptr()) });
            bail!("another libretro core is already loaded in this process");
        }

        // SAFETY: the callbacks match libretro.h and the host they use is published.
        unsafe {
            (api.set_environment)(host::environment);
            (api.set_video_refresh)(host::video_refresh);
            (api.set_audio_sample)(host::audio_sample);
            (api.set_audio_sample_batch)(host::audio_sample_batch);
            (api.set_input_poll)(host::input_poll);
            (api.set_input_state)(host::input_state);
            (api.init)();
        }
        Ok(Core {
            api,
            host,
            game: None,
            need_fullpath: sys.need_fullpath,
            saved: Vec::new(),
            _lib: lib,
        })
    }

    fn host(&mut self) -> &mut Host {
        // SAFETY: `host` is live until `Drop`, and the core only calls back while one of
        // our methods is inside a core call, never while this borrow is held.
        unsafe { self.host.as_mut() }
    }

    /// Loads the ROM and returns the core's audio/video timing.
    /// If `save_path` exists its bytes go into the core's save RAM.
    pub fn load_game(&mut self, rom: &Path, save_path: Option<&Path>) -> Result<AvInfo> {
        ensure!(self.game.is_none(), "a game is already loaded");
        let save_dir = save_path.and_then(Path::parent).or_else(|| rom.parent());
        self.host().save_dir = save_dir.and_then(dir_cstring);

        let path = CString::new(rom.as_os_str().as_encoded_bytes())
            .with_context(|| format!("ROM path {} contains a NUL byte", rom.display()))?;
        let data = if self.need_fullpath {
            Vec::new()
        } else {
            fs::read(rom).with_context(|| format!("reading ROM {}", rom.display()))?
        };
        let info = ffi::GameInfo {
            path: path.as_ptr(),
            data: if data.is_empty() {
                ptr::null()
            } else {
                data.as_ptr().cast()
            },
            size: data.len(),
            meta: ptr::null(),
        };
        // SAFETY: `info` points at `path` and `data`, which `Game` keeps alive until unload.
        let loaded = unsafe { (self.api.load_game)(&info) };
        ensure!(loaded, "core rejected ROM {}", rom.display());
        let game = self.game.insert(Game {
            _path: path,
            _data: data,
        });

        if let Some(ram) = save_ram(&self.api, game) {
            if let Some(save_path) = save_path.filter(|p| p.exists()) {
                let bytes = fs::read(save_path)
                    .with_context(|| format!("reading save {}", save_path.display()))?;
                let n = bytes.len().min(ram.len());
                ram[..n].copy_from_slice(&bytes[..n]);
            }
            self.saved = ram.to_vec();
        }

        let mut av = ffi::SystemAvInfo::default();
        // SAFETY: `av` is a valid out-parameter and a game is loaded.
        unsafe { (self.api.get_system_av_info)(&mut av) };
        let av = AvInfo::from(&av);
        let host = self.host();
        (host.av, host.av_changed) = (Some(av), false);
        Ok(av)
    }

    /// The new audio/video info, once after the core changed it while running.
    /// Geometry changes leave `fps` and `sample_rate` as they were.
    pub fn take_av_change(&mut self) -> Option<AvInfo> {
        let host = self.host();
        std::mem::take(&mut host.av_changed)
            .then_some(host.av)
            .flatten()
    }

    pub fn set_joypad(&mut self, pad: Joypad) {
        self.host().joypad = pad.0;
    }

    /// The port 0 left stick in libretro units: -32767..=32767 per axis, y positive down.
    pub fn set_analog_left(&mut self, x: i16, y: i16) {
        self.host().analog_left = (x, y);
    }

    /// Runs exactly one emulated frame.
    pub fn run_frame(&mut self) {
        if self.game.is_none() {
            return;
        }
        // SAFETY: a game is loaded and no reference to the host is held.
        unsafe { (self.api.run)() };
    }

    /// The last frame the core produced. `None` until the first video callback.
    pub fn frame(&self) -> Option<Frame<'_>> {
        // SAFETY: see `host()`; `&self` keeps the core from running while the frame is borrowed.
        let video = &unsafe { self.host.as_ref() }.video;
        video.has_frame.then_some(Frame {
            width: video.width,
            height: video.height,
            rgba: &video.rgba,
        })
    }

    /// Interleaved stereo i16 samples produced since the last call.
    pub fn take_audio(&mut self, out: &mut Vec<i16>) {
        let audio = &mut self.host().audio;
        out.extend_from_slice(audio);
        audio.clear();
    }

    /// Writes save RAM to `path` if the core has any and it changed since the last write.
    /// The write goes to a temporary file first and is renamed over `path`.
    pub fn write_save(&mut self, path: &Path) -> Result<()> {
        let Some(ram) = self.game.as_mut().and_then(|g| save_ram(&self.api, g)) else {
            return Ok(());
        };
        // mGBA reports 128 KiB of GBA save RAM until the game's save type is detected, then
        // the real size. Shrinking with the same bytes is not a change.
        if self.saved.get(..ram.len()) == Some(&*ram) {
            return Ok(());
        }
        let mut tmp_name = path
            .file_name()
            .context("save path has no file name")?
            .to_owned();
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(ram)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
        self.saved.clear();
        self.saved.extend_from_slice(ram);
        Ok(())
    }
}

/// `None` for the empty parent of a bare file name, which the core would treat as the root.
fn dir_cstring(dir: &Path) -> Option<CString> {
    let bytes = dir.as_os_str().as_encoded_bytes();
    (!bytes.is_empty())
        .then(|| CString::new(bytes).ok())
        .flatten()
}

/// The core's save RAM, if the loaded game has any. Borrowing `game` mutably keeps the
/// core from running, and so from touching the RAM, while the slice lives.
fn save_ram<'a>(api: &ffi::Api, _game: &'a mut Game) -> Option<&'a mut [u8]> {
    // SAFETY: a game is loaded, which is all these calls need.
    let (data, size) = unsafe {
        (
            (api.get_memory_data)(ffi::MEMORY_SAVE_RAM),
            (api.get_memory_size)(ffi::MEMORY_SAVE_RAM),
        )
    };
    if data.is_null() || size == 0 {
        return None;
    }
    // SAFETY: the core owns `size` bytes at `data` until the game is unloaded.
    Some(unsafe { std::slice::from_raw_parts_mut(data.cast::<u8>(), size) })
}

impl Drop for Core {
    fn drop(&mut self) {
        // SAFETY: the core was initialized in `load`; unload only if a game is loaded.
        unsafe {
            if self.game.is_some() {
                (self.api.unload_game)();
            }
            (self.api.deinit)();
        }
        host::release();
        // SAFETY: `host` came from `Box::leak` in `load` and the callbacks can no longer see it.
        drop(unsafe { Box::from_raw(self.host.as_ptr()) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(base_width: u32, base_height: u32, aspect_ratio: f32) -> ffi::GameGeometry {
        ffi::GameGeometry {
            base_width,
            base_height,
            max_width: 640,
            max_height: 480,
            aspect_ratio,
        }
    }

    #[test]
    fn aspect_falls_back_to_square_pixels() {
        let check = |g: ffi::GameGeometry, want: f64| {
            let got = aspect_ratio(&g);
            assert!((got - want).abs() < 1e-6, "{got} != {want}");
        };
        check(geometry(640, 240, 4.0 / 3.0), 4.0 / 3.0);
        check(geometry(240, 160, 0.0), 1.5);
        check(geometry(160, 144, -1.0), 160.0 / 144.0);
        check(geometry(0, 0, 0.0), 1.0);
    }

    #[test]
    fn geometry_change_keeps_timing() {
        let av = AvInfo::from(&ffi::SystemAvInfo {
            geometry: geometry(640, 480, 4.0 / 3.0),
            timing: ffi::SystemTiming {
                fps: 60.0,
                sample_rate: 32040.0,
            },
        });
        let changed = av.with_geometry(&geometry(640, 240, 0.0));
        assert_eq!((changed.base_width, changed.base_height), (640, 240));
        assert!((changed.aspect_ratio - 640.0 / 240.0).abs() < 1e-9);
        assert_eq!((changed.fps, changed.sample_rate), (60.0, 32040.0));
    }
}
