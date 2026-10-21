//! Frontend state the core reaches through plain `extern "C"` callbacks.
//!
//! libretro callbacks carry no user pointer, so the one live `Host` is published in a
//! static. `Core` owns it and only calls into the core while it holds no reference to it,
//! which makes the callbacks the sole users for the duration of each call.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};

use super::options::Overrides;
use super::pixels::{self, PixelFormat};
use super::{ffi, AvInfo};

pub struct Host {
    pub pixel_format: PixelFormat,
    pub video: Video,
    pub audio: Vec<i16>,
    pub joypad: u16,
    /// Left stick of port 0, libretro range and orientation (y positive is down).
    pub analog_left: (i16, i16),
    /// `None` until a game is loaded.
    pub av: Option<AvInfo>,
    /// Set when the core changes `av` through the environment.
    pub av_changed: bool,
    pub system_dir: Option<CString>,
    pub save_dir: Option<CString>,
    options: Vec<CoreOption>,
    overrides: Overrides,
}

#[derive(Default)]
pub struct Video {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub has_frame: bool,
}

struct CoreOption {
    key: CString,
    value: CString,
}

impl Host {
    pub fn new(system_dir: Option<CString>, overrides: Overrides) -> Host {
        Host {
            // The libretro default until the core asks for another.
            pixel_format: PixelFormat::Rgb1555,
            video: Video::default(),
            audio: Vec::with_capacity(8192),
            joypad: 0,
            analog_left: (0, 0),
            av: None,
            av_changed: false,
            system_dir,
            save_dir: None,
            options: Vec::new(),
            overrides,
        }
    }

    fn set_options(&mut self, mut options: Vec<CoreOption>) {
        for o in &mut options {
            if let Some(value) = self.overrides.get(&o.key) {
                log(format_args!("termcart: option {:?} = {value:?}", o.key));
                o.value = value;
            }
        }
        self.options = options;
    }

    /// The value for `key`. Keys the core never declared can still be overridden, since
    /// cores also read options their build compiled out of the declarations.
    fn option(&mut self, key: &CStr) -> Option<&CStr> {
        let i = match self.options.iter().position(|o| o.key.as_c_str() == key) {
            Some(i) => i,
            None => {
                let value = self.overrides.get(key)?;
                log(format_args!(
                    "termcart: undeclared option {key:?} = {value:?}"
                ));
                self.options.push(CoreOption {
                    key: key.to_owned(),
                    value,
                });
                self.options.len() - 1
            }
        };
        Some(&self.options[i].value)
    }

    fn set_av(&mut self, av: AvInfo) {
        log(format_args!("termcart: av changed to {av:?}"));
        self.av = Some(av);
        self.av_changed = true;
    }

    fn input(&self, port: c_uint, device: c_uint, index: c_uint, id: c_uint) -> i16 {
        if port != 0 {
            return 0;
        }
        match (device, index, id) {
            (ffi::DEVICE_JOYPAD, _, ffi::DEVICE_ID_JOYPAD_MASK) => self.joypad as i16,
            (ffi::DEVICE_JOYPAD, _, 0..16) => i16::from(self.joypad & (1 << id) != 0),
            (ffi::DEVICE_ANALOG, ffi::DEVICE_INDEX_ANALOG_LEFT, ffi::DEVICE_ID_ANALOG_X) => {
                self.analog_left.0
            }
            (ffi::DEVICE_ANALOG, ffi::DEVICE_INDEX_ANALOG_LEFT, ffi::DEVICE_ID_ANALOG_Y) => {
                self.analog_left.1
            }
            _ => 0,
        }
    }
}

static HOST: AtomicPtr<Host> = AtomicPtr::new(ptr::null_mut());

/// Publishes `host` for the callbacks. Fails if another core already owns the slot.
pub fn claim(host: *mut Host) -> bool {
    HOST.compare_exchange(ptr::null_mut(), host, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

pub fn release() {
    HOST.store(ptr::null_mut(), Ordering::Release);
}

fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> Option<R> {
    let host = HOST.load(Ordering::Acquire);
    // SAFETY: a non-null pointer is a live `Host` owned by `Core`, which does not touch it
    // while the core runs, and callbacks are not reentrant, so this is the only reference.
    unsafe { host.as_mut() }.map(f)
}

pub extern "C" fn environment(cmd: c_uint, data: *mut c_void) -> bool {
    // GET_INPUT_BITMASKS is the one command that passes no data.
    if data.is_null() && cmd != ffi::ENV_GET_INPUT_BITMASKS {
        return false;
    }
    let handled = with_host(|host| {
        // SAFETY (all arms): libretro.h defines the type `data` points to for each command,
        // and the core keeps it valid for the duration of the call.
        match cmd {
            ffi::ENV_GET_CAN_DUPE => unsafe { write(data, true) },
            ffi::ENV_GET_SYSTEM_DIRECTORY => unsafe { write_dir(data, &host.system_dir) },
            ffi::ENV_GET_SAVE_DIRECTORY => unsafe { write_dir(data, &host.save_dir) },
            ffi::ENV_SET_PIXEL_FORMAT => {
                let raw = unsafe { *data.cast::<c_int>() };
                let format = PixelFormat::from_raw(raw);
                log(format_args!(
                    "termcart: core pixel format {raw} -> {format:?}"
                ));
                format.map(|f| host.pixel_format = f).is_some()
            }
            ffi::ENV_SET_INPUT_DESCRIPTORS | ffi::ENV_GET_INPUT_BITMASKS => true,
            ffi::ENV_GET_LOG_INTERFACE => {
                log_sink().is_some()
                    && unsafe {
                        write(
                            data,
                            ffi::LogCallback {
                                log: ffi::termcart_log_printf,
                            },
                        )
                    }
            }
            ffi::ENV_GET_VARIABLE => {
                let var = unsafe { &mut *data.cast::<ffi::Variable>() };
                var.value = ptr::null();
                if var.key.is_null() {
                    return false;
                }
                let key = unsafe { CStr::from_ptr(var.key) };
                // The value lives in `host.options` until the next SET_* replaces them.
                host.option(key)
                    .map(|value| var.value = value.as_ptr())
                    .is_some()
            }
            // Options never change at runtime, so there is never an update.
            ffi::ENV_GET_VARIABLE_UPDATE => unsafe { write(data, false) },
            ffi::ENV_GET_CORE_OPTIONS_VERSION => unsafe { write::<c_uint>(data, 2) },
            ffi::ENV_SET_VARIABLES => {
                host.set_options(unsafe { variables(data.cast()) });
                true
            }
            ffi::ENV_SET_CORE_OPTIONS => {
                host.set_options(unsafe { options_v1(data.cast()) });
                true
            }
            ffi::ENV_SET_CORE_OPTIONS_INTL => {
                let intl = unsafe { &*data.cast::<ffi::CoreOptionsIntl>() };
                host.set_options(unsafe { options_v1(intl.us) });
                true
            }
            // These return whether the frontend shows option categories, which it does not.
            ffi::ENV_SET_CORE_OPTIONS_V2 => {
                host.set_options(unsafe { options_v2(data.cast()) });
                false
            }
            ffi::ENV_SET_CORE_OPTIONS_V2_INTL => {
                let intl = unsafe { &*data.cast::<ffi::CoreOptionsV2Intl>() };
                host.set_options(unsafe { options_v2(intl.us) });
                false
            }
            ffi::ENV_SET_GEOMETRY => {
                let geometry = unsafe { &*data.cast::<ffi::GameGeometry>() };
                // Before load the core's own av info, read right after, already has it.
                if let Some(av) = host.av {
                    host.set_av(av.with_geometry(geometry));
                }
                true
            }
            ffi::ENV_SET_SYSTEM_AV_INFO => {
                let av = unsafe { &*data.cast::<ffi::SystemAvInfo>() };
                host.set_av(AvInfo::from(av));
                true
            }
            ffi::ENV_GET_AUDIO_VIDEO_ENABLE => unsafe {
                write(data, ffi::AV_ENABLE_VIDEO_AND_AUDIO)
            },
            // Informational: the frontend has nothing to do with them.
            ffi::ENV_SET_CONTROLLER_INFO
            | ffi::ENV_SET_SUBSYSTEM_INFO
            | ffi::ENV_SET_MEMORY_MAPS
            | ffi::ENV_SET_SERIALIZATION_QUIRKS => true,
            // Not provided. Asked every frame by some cores, so not worth a log line.
            ffi::ENV_GET_RUMBLE_INTERFACE
            | ffi::ENV_GET_PERF_INTERFACE
            | ffi::ENV_GET_CURRENT_SOFTWARE_FRAMEBUFFER
            | ffi::ENV_SET_CORE_OPTIONS_UPDATE_DISPLAY_CALLBACK => false,
            _ => {
                log(format_args!(
                    "termcart: unhandled environment command {cmd:#x}"
                ));
                false
            }
        }
    });
    handled.unwrap_or(false)
}

/// # Safety
/// `data` must point to a writable `T`.
unsafe fn write<T>(data: *mut c_void, value: T) -> bool {
    // SAFETY: guaranteed by the caller.
    unsafe { data.cast::<T>().write(value) };
    true
}

/// # Safety
/// `data` must point to a writable `const char *`.
unsafe fn write_dir(data: *mut c_void, dir: &Option<CString>) -> bool {
    let Some(dir) = dir else { return false };
    // SAFETY: guaranteed by the caller. `dir` lives in `Host`, which outlives the core.
    unsafe { write(data, dir.as_ptr()) }
}

/// Reads a NULL-key-terminated `retro_variable` array, whose values look like
/// `"Description; default|other|..."`.
///
/// # Safety
/// `vars` must be such an array of valid C strings.
unsafe fn variables(mut vars: *const ffi::Variable) -> Vec<CoreOption> {
    let mut out = Vec::new();
    // SAFETY: guaranteed by the caller; the loop stops at the terminator.
    while let Some(var) = unsafe { vars.as_ref() }.filter(|v| !v.key.is_null()) {
        if !var.value.is_null() {
            // SAFETY: both pointers are non-null C strings.
            let (key, spec) = unsafe { (CStr::from_ptr(var.key), CStr::from_ptr(var.value)) };
            if let Some(value) = default_from_spec(spec.to_bytes()) {
                out.push(CoreOption {
                    key: key.to_owned(),
                    value,
                });
            }
        }
        // SAFETY: not past the terminator yet.
        vars = unsafe { vars.add(1) };
    }
    out
}

fn default_from_spec(spec: &[u8]) -> Option<CString> {
    let pos = spec.windows(2).position(|w| w == b"; ")?;
    let rest = &spec[pos + 2..];
    let first = rest.split(|&b| b == b'|').next()?;
    CString::new(first).ok()
}

/// # Safety
/// `key`, `default` and `values` must be the fields of a valid core option definition.
unsafe fn option(
    key: *const c_char,
    default: *const c_char,
    values: &[ffi::CoreOptionValue],
) -> Option<CoreOption> {
    // The default must be one of the values; without one, the first value is the default.
    let value = if default.is_null() {
        values[0].value
    } else {
        default
    };
    if value.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees both are C strings.
    let (key, value) = unsafe { (CStr::from_ptr(key), CStr::from_ptr(value)) };
    Some(CoreOption {
        key: key.to_owned(),
        value: value.to_owned(),
    })
}

/// # Safety
/// `defs` must be a NULL-key-terminated array of `retro_core_option_definition`.
unsafe fn options_v1(mut defs: *const ffi::CoreOptionDefinition) -> Vec<CoreOption> {
    let mut out = Vec::new();
    // SAFETY: guaranteed by the caller; the loop stops at the terminator.
    while let Some(def) = unsafe { defs.as_ref() }.filter(|d| !d.key.is_null()) {
        // SAFETY: a definition from a valid array.
        out.extend(unsafe { option(def.key, def.default_value, &def.values) });
        // SAFETY: not past the terminator yet.
        defs = unsafe { defs.add(1) };
    }
    out
}

/// # Safety
/// `opts` must be null or point to a valid `retro_core_options_v2`.
unsafe fn options_v2(opts: *const ffi::CoreOptionsV2) -> Vec<CoreOption> {
    let mut out = Vec::new();
    // SAFETY: guaranteed by the caller.
    let Some(opts) = (unsafe { opts.as_ref() }) else {
        return out;
    };
    let mut defs = opts.definitions;
    // SAFETY: `definitions` is a NULL-key-terminated array; the loop stops at the terminator.
    while let Some(def) = unsafe { defs.as_ref() }.filter(|d| !d.key.is_null()) {
        // SAFETY: a definition from a valid array.
        out.extend(unsafe { option(def.key, def.default_value, &def.values) });
        // SAFETY: not past the terminator yet.
        defs = unsafe { defs.add(1) };
    }
    out
}

pub extern "C" fn video_refresh(data: *const c_void, width: c_uint, height: c_uint, pitch: usize) {
    // NULL means the core duplicated the last frame, which is already in the buffer.
    if data.is_null() || width == 0 || height == 0 {
        return;
    }
    with_host(|host| {
        let (w, h) = (width as usize, height as usize);
        let format = host.pixel_format;
        let len = pitch * (h - 1) + w * format.bytes_per_pixel();
        // SAFETY: the core passes `height` rows of `width` pixels, `pitch` bytes apart.
        let src = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
        let video = &mut host.video;
        // Only reallocates when the size grows.
        video.rgba.resize(w * h * 4, 0);
        pixels::to_rgba(format, src, w, h, pitch, &mut video.rgba);
        (video.width, video.height, video.has_frame) = (width, height, true);
    });
}

pub extern "C" fn audio_sample(left: i16, right: i16) {
    with_host(|host| host.audio.extend_from_slice(&[left, right]));
}

pub extern "C" fn audio_sample_batch(data: *const i16, frames: usize) -> usize {
    if data.is_null() {
        return 0;
    }
    // SAFETY: the core passes `frames` interleaved stereo frames.
    let samples = unsafe { std::slice::from_raw_parts(data, frames * 2) };
    with_host(|host| host.audio.extend_from_slice(samples));
    frames
}

pub extern "C" fn input_poll() {}

pub extern "C" fn input_state(port: c_uint, device: c_uint, index: c_uint, id: c_uint) -> i16 {
    with_host(|host| host.input(port, device, index, id)).unwrap_or(0)
}

enum LogSink {
    Stderr,
    File(File),
}

/// `TERMCART_LOG=stderr` logs to stderr, any other value is a file to append to.
/// Unset means no logging. Never stdout: that is the game screen.
fn log_sink() -> Option<&'static Mutex<LogSink>> {
    static SINK: OnceLock<Option<Mutex<LogSink>>> = OnceLock::new();
    SINK.get_or_init(|| {
        let target = std::env::var_os("TERMCART_LOG")?;
        let sink = if target == "stderr" {
            LogSink::Stderr
        } else {
            LogSink::File(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(target)
                    .ok()?,
            )
        };
        Some(Mutex::new(sink))
    })
    .as_ref()
}

pub fn log(msg: std::fmt::Arguments) {
    let Some(sink) = log_sink() else { return };
    let Ok(mut sink) = sink.lock() else { return };
    let _ = match &mut *sink {
        LogSink::Stderr => writeln!(std::io::stderr(), "{msg}"),
        LogSink::File(f) => writeln!(f, "{msg}"),
    };
}

/// Called by log_shim.c with the already formatted message.
#[no_mangle]
extern "C" fn termcart_log_line(level: c_int, msg: *const c_char, len: usize) {
    // SAFETY: the shim passes `len` bytes of its stack buffer.
    let bytes = unsafe { std::slice::from_raw_parts(msg.cast::<u8>(), len) };
    let level = match level {
        0 => "debug",
        1 => "info",
        2 => "warn",
        _ => "error",
    };
    log(format_args!(
        "[{level}] {}",
        String::from_utf8_lossy(bytes).trim_end()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_spec_default_is_first_value() {
        let check = |spec: &str, want: Option<&str>| {
            let got = default_from_spec(spec.as_bytes());
            assert_eq!(got.as_deref().and_then(|c| c.to_str().ok()), want, "{spec}");
        };
        check("Solar sensor level; 0|1|2", Some("0"));
        check(
            "Idle loop; Remove Known|Detect and Remove|Don't Remove",
            Some("Remove Known"),
        );
        check("Single; only", Some("only"));
        check("no separator", None);
    }

    #[test]
    fn input_answers_port_0_joypad_and_left_stick() {
        let mut host = Host::new(None, Overrides::for_core(c"test"));
        host.joypad = 0b1001;
        host.analog_left = (-32767, 12000);
        let check = |port, device, index, id, want: i16| {
            assert_eq!(
                host.input(port, device, index, id),
                want,
                "{port} {device} {index} {id}"
            );
        };
        check(0, ffi::DEVICE_JOYPAD, 0, 0, 1);
        check(0, ffi::DEVICE_JOYPAD, 0, 1, 0);
        check(0, ffi::DEVICE_JOYPAD, 0, 3, 1);
        check(0, ffi::DEVICE_JOYPAD, 0, ffi::DEVICE_ID_JOYPAD_MASK, 0b1001);
        check(0, ffi::DEVICE_ANALOG, 0, ffi::DEVICE_ID_ANALOG_X, -32767);
        check(0, ffi::DEVICE_ANALOG, 0, ffi::DEVICE_ID_ANALOG_Y, 12000);
        // Right stick and analog buttons.
        check(0, ffi::DEVICE_ANALOG, 1, ffi::DEVICE_ID_ANALOG_X, 0);
        check(0, ffi::DEVICE_ANALOG, 2, 0, 0);
        for port in 1..4 {
            check(port, ffi::DEVICE_JOYPAD, 0, 0, 0);
            check(port, ffi::DEVICE_JOYPAD, 0, ffi::DEVICE_ID_JOYPAD_MASK, 0);
            check(port, ffi::DEVICE_ANALOG, 0, ffi::DEVICE_ID_ANALOG_X, 0);
        }
    }

    #[test]
    fn overrides_replace_declared_defaults_and_answer_undeclared_keys() {
        let mut host = Host::new(None, Overrides::for_core(c"ParaLLEl N64"));
        let declared = |key: &CStr, value: &CStr| CoreOption {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        host.set_options(vec![
            declared(c"parallel-n64-angrylion-multithread", c"all threads"),
            declared(c"parallel-n64-angrylion-vioverlay", c"Filtered"),
        ]);
        assert!(host
            .option(c"parallel-n64-angrylion-multithread")
            .is_some_and(|v| v != c"all threads"));
        assert_eq!(
            host.option(c"parallel-n64-angrylion-vioverlay"),
            Some(c"Filtered")
        );
        assert_eq!(host.option(c"parallel-n64-alt-map"), Some(c"enabled"));
        assert_eq!(host.option(c"parallel-n64-screensize"), None);
    }
}
