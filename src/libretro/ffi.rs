//! Hand-written subset of libretro.h (API version 1).

use std::ffi::{c_char, c_int, c_uint, c_void};

use anyhow::{Context, Result};
use libloading::Library;

pub const API_VERSION: c_uint = 1;

pub const DEVICE_JOYPAD: c_uint = 1;
pub const DEVICE_ID_JOYPAD_MASK: c_uint = 256;
pub const DEVICE_ANALOG: c_uint = 5;
pub const DEVICE_INDEX_ANALOG_LEFT: c_uint = 0;
pub const DEVICE_ID_ANALOG_X: c_uint = 0;
pub const DEVICE_ID_ANALOG_Y: c_uint = 1;
pub const MEMORY_SAVE_RAM: c_uint = 0;

const EXPERIMENTAL: c_uint = 0x10000;
pub const ENV_GET_CAN_DUPE: c_uint = 3;
pub const ENV_GET_SYSTEM_DIRECTORY: c_uint = 9;
pub const ENV_SET_PIXEL_FORMAT: c_uint = 10;
pub const ENV_SET_INPUT_DESCRIPTORS: c_uint = 11;
pub const ENV_GET_VARIABLE: c_uint = 15;
pub const ENV_SET_VARIABLES: c_uint = 16;
pub const ENV_GET_VARIABLE_UPDATE: c_uint = 17;
pub const ENV_GET_RUMBLE_INTERFACE: c_uint = 23;
pub const ENV_GET_LOG_INTERFACE: c_uint = 27;
pub const ENV_GET_PERF_INTERFACE: c_uint = 28;
pub const ENV_GET_SAVE_DIRECTORY: c_uint = 31;
pub const ENV_SET_SYSTEM_AV_INFO: c_uint = 32;
pub const ENV_SET_SUBSYSTEM_INFO: c_uint = 34;
pub const ENV_SET_CONTROLLER_INFO: c_uint = 35;
pub const ENV_SET_MEMORY_MAPS: c_uint = 36 | EXPERIMENTAL;
pub const ENV_SET_GEOMETRY: c_uint = 37;
pub const ENV_GET_CURRENT_SOFTWARE_FRAMEBUFFER: c_uint = 40 | EXPERIMENTAL;
pub const ENV_SET_SERIALIZATION_QUIRKS: c_uint = 44;
pub const ENV_GET_AUDIO_VIDEO_ENABLE: c_uint = 47 | EXPERIMENTAL;
pub const ENV_GET_INPUT_BITMASKS: c_uint = 51 | EXPERIMENTAL;
pub const ENV_GET_CORE_OPTIONS_VERSION: c_uint = 52;
pub const ENV_SET_CORE_OPTIONS: c_uint = 53;
pub const ENV_SET_CORE_OPTIONS_INTL: c_uint = 54;
pub const ENV_SET_CORE_OPTIONS_V2: c_uint = 67;
pub const ENV_SET_CORE_OPTIONS_V2_INTL: c_uint = 68;
pub const ENV_SET_CORE_OPTIONS_UPDATE_DISPLAY_CALLBACK: c_uint = 69;

/// `GET_AUDIO_VIDEO_ENABLE` bits: render video, produce audio.
pub const AV_ENABLE_VIDEO_AND_AUDIO: c_int = 0b11;

pub const PIXEL_FORMAT_0RGB1555: c_int = 0;
pub const PIXEL_FORMAT_XRGB8888: c_int = 1;
pub const PIXEL_FORMAT_RGB565: c_int = 2;

const NUM_CORE_OPTION_VALUES_MAX: usize = 128;

#[repr(C)]
pub struct SystemInfo {
    pub library_name: *const c_char,
    pub library_version: *const c_char,
    pub valid_extensions: *const c_char,
    pub need_fullpath: bool,
    pub block_extract: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GameGeometry {
    pub base_width: c_uint,
    pub base_height: c_uint,
    pub max_width: c_uint,
    pub max_height: c_uint,
    pub aspect_ratio: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SystemTiming {
    pub fps: f64,
    pub sample_rate: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SystemAvInfo {
    pub geometry: GameGeometry,
    pub timing: SystemTiming,
}

#[repr(C)]
pub struct GameInfo {
    pub path: *const c_char,
    pub data: *const c_void,
    pub size: usize,
    pub meta: *const c_char,
}

#[repr(C)]
pub struct Variable {
    pub key: *const c_char,
    pub value: *const c_char,
}

#[repr(C)]
pub struct LogCallback {
    pub log: unsafe extern "C" fn(level: c_int, fmt: *const c_char, ...),
}

#[repr(C)]
pub struct CoreOptionValue {
    pub value: *const c_char,
    pub label: *const c_char,
}

#[repr(C)]
pub struct CoreOptionDefinition {
    pub key: *const c_char,
    pub desc: *const c_char,
    pub info: *const c_char,
    pub values: [CoreOptionValue; NUM_CORE_OPTION_VALUES_MAX],
    pub default_value: *const c_char,
}

#[repr(C)]
pub struct CoreOptionsIntl {
    pub us: *const CoreOptionDefinition,
    pub local: *const CoreOptionDefinition,
}

#[repr(C)]
pub struct CoreOptionV2Definition {
    pub key: *const c_char,
    pub desc: *const c_char,
    pub desc_categorized: *const c_char,
    pub info: *const c_char,
    pub info_categorized: *const c_char,
    pub category_key: *const c_char,
    pub values: [CoreOptionValue; NUM_CORE_OPTION_VALUES_MAX],
    pub default_value: *const c_char,
}

#[repr(C)]
pub struct CoreOptionsV2 {
    pub categories: *const c_void,
    pub definitions: *const CoreOptionV2Definition,
}

#[repr(C)]
pub struct CoreOptionsV2Intl {
    pub us: *const CoreOptionsV2,
    pub local: *const CoreOptionsV2,
}

pub type EnvironmentFn = extern "C" fn(cmd: c_uint, data: *mut c_void) -> bool;
pub type VideoRefreshFn =
    extern "C" fn(data: *const c_void, width: c_uint, height: c_uint, pitch: usize);
pub type AudioSampleFn = extern "C" fn(left: i16, right: i16);
pub type AudioSampleBatchFn = extern "C" fn(data: *const i16, frames: usize) -> usize;
pub type InputPollFn = extern "C" fn();
pub type InputStateFn =
    extern "C" fn(port: c_uint, device: c_uint, index: c_uint, id: c_uint) -> i16;

extern "C" {
    /// Defined in log_shim.c.
    pub fn termcart_log_printf(level: c_int, fmt: *const c_char, ...);
}

/// The `retro_*` entry points, copied out of the library. Valid while the `Library` lives.
pub struct Api {
    pub api_version: unsafe extern "C" fn() -> c_uint,
    pub init: unsafe extern "C" fn(),
    pub deinit: unsafe extern "C" fn(),
    pub get_system_info: unsafe extern "C" fn(info: *mut SystemInfo),
    pub get_system_av_info: unsafe extern "C" fn(info: *mut SystemAvInfo),
    pub set_environment: unsafe extern "C" fn(EnvironmentFn),
    pub set_video_refresh: unsafe extern "C" fn(VideoRefreshFn),
    pub set_audio_sample: unsafe extern "C" fn(AudioSampleFn),
    pub set_audio_sample_batch: unsafe extern "C" fn(AudioSampleBatchFn),
    pub set_input_poll: unsafe extern "C" fn(InputPollFn),
    pub set_input_state: unsafe extern "C" fn(InputStateFn),
    pub load_game: unsafe extern "C" fn(game: *const GameInfo) -> bool,
    pub unload_game: unsafe extern "C" fn(),
    pub run: unsafe extern "C" fn(),
    pub get_memory_data: unsafe extern "C" fn(id: c_uint) -> *mut c_void,
    pub get_memory_size: unsafe extern "C" fn(id: c_uint) -> usize,
}

impl Api {
    /// # Safety
    /// `lib` must be a libretro core, so each `retro_*` symbol has the signature declared here,
    /// and it must outlive the returned `Api`.
    pub unsafe fn resolve(lib: &Library) -> Result<Api> {
        /// # Safety
        /// `T` must be the type of the symbol `name`.
        unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> Result<T> {
            // SAFETY: the caller guarantees `T` matches the symbol's type.
            let s = unsafe { lib.get::<T>(name.as_bytes()) };
            Ok(*s.with_context(|| format!("core has no symbol {name}"))?)
        }
        // SAFETY: every name below is declared in libretro.h with the type of its field.
        unsafe {
            Ok(Api {
                api_version: sym(lib, "retro_api_version")?,
                init: sym(lib, "retro_init")?,
                deinit: sym(lib, "retro_deinit")?,
                get_system_info: sym(lib, "retro_get_system_info")?,
                get_system_av_info: sym(lib, "retro_get_system_av_info")?,
                set_environment: sym(lib, "retro_set_environment")?,
                set_video_refresh: sym(lib, "retro_set_video_refresh")?,
                set_audio_sample: sym(lib, "retro_set_audio_sample")?,
                set_audio_sample_batch: sym(lib, "retro_set_audio_sample_batch")?,
                set_input_poll: sym(lib, "retro_set_input_poll")?,
                set_input_state: sym(lib, "retro_set_input_state")?,
                load_game: sym(lib, "retro_load_game")?,
                unload_game: sym(lib, "retro_unload_game")?,
                run: sym(lib, "retro_run")?,
                get_memory_data: sym(lib, "retro_get_memory_data")?,
                get_memory_size: sym(lib, "retro_get_memory_size")?,
            })
        }
    }
}
