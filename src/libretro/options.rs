//! Core option values termcart picks over the core's own defaults.
//!
//! A value comes from, in order:
//!
//! 1. The environment. `TERMCART_OPT_<key>=<value>` sets option `<key>` for any core. Since
//!    shells only allow `[A-Za-z0-9_]` in names, `-` and `.` in the key may be written as
//!    `_`: `TERMCART_OPT_parallel_n64_angrylion_multithread=8` sets
//!    `parallel-n64-angrylion-multithread`. The exact spelling wins when both are set.
//! 2. The per-core table below, chosen by `retro_system_info.library_name`.
//! 3. The default the core declared.
//!
//! Values are not checked against the ones the core declared; a typo reaches the core as is.

use std::ffi::{CStr, CString, OsString};

const ENV_PREFIX: &str = "TERMCART_OPT_";

/// ParaLLEl N64 built without GL or Vulkan: only Angrylion draws, so pin the software path
/// and set up the keyboard-friendly controls.
const PARALLEL_N64: &[(&str, &str)] = &[
    ("parallel-n64-gfxplugin", "angrylion"),
    ("parallel-n64-rspplugin", "auto"),
    ("parallel-n64-cpucore", "dynamic_recompiler"),
    ("parallel-n64-angrylion-multithread", "4"),
    ("parallel-n64-upscaling", "1x"),
    // Every N64 button becomes a RetroPad bit, so C buttons need no right stick.
    ("parallel-n64-alt-map", "enabled"),
    // A keyboard stick has no noise, and the core rescales past the deadzone, which would
    // shrink a partial tilt such as a walk modifier.
    ("parallel-n64-astick-deadzone", "0"),
    // Controller Pak saves live in save RAM with the cartridge ones. The core's default is
    // no pak, which makes pak-only games refuse to save.
    ("parallel-n64-pak1", "memory"),
];

pub struct Overrides {
    table: &'static [(&'static str, &'static str)],
}

impl Overrides {
    pub fn for_core(library_name: &CStr) -> Overrides {
        let table = match library_name.to_bytes() {
            b"ParaLLEl N64" => PARALLEL_N64,
            _ => &[],
        };
        Overrides { table }
    }

    /// The value termcart wants for `key`, if it overrides the core's.
    pub fn get(&self, key: &CStr) -> Option<CString> {
        self.lookup(key, |name| std::env::var_os(name))
    }

    fn lookup(&self, key: &CStr, env: impl Fn(&str) -> Option<OsString>) -> Option<CString> {
        let key = key.to_str().ok()?;
        let exact = format!("{ENV_PREFIX}{key}");
        let shell = exact.replace(['-', '.'], "_");
        let from_env = env(&exact).or_else(|| env(&shell));
        if let Some(value) = from_env {
            return CString::new(value.into_encoded_bytes()).ok();
        }
        let (_, value) = self.table.iter().find(|(k, _)| *k == key)?;
        CString::new(*value).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_beats_table_beats_core() {
        let n64 = Overrides::for_core(c"ParaLLEl N64");
        let check = |key: &CStr, env: &[(&str, &str)], want: Option<&str>| {
            let got = n64.lookup(key, |name| {
                env.iter().find(|(k, _)| *k == name).map(|(_, v)| v.into())
            });
            assert_eq!(
                got.as_deref().and_then(|c| c.to_str().ok()),
                want,
                "{key:?}"
            );
        };
        let threads = c"parallel-n64-angrylion-multithread";
        check(threads, &[], Some("4"));
        check(
            threads,
            &[("TERMCART_OPT_parallel_n64_angrylion_multithread", "8")],
            Some("8"),
        );
        check(
            threads,
            &[
                ("TERMCART_OPT_parallel-n64-angrylion-multithread", "2"),
                ("TERMCART_OPT_parallel_n64_angrylion_multithread", "8"),
            ],
            Some("2"),
        );
        check(c"parallel-n64-screensize", &[], None);
        check(
            c"parallel-n64-send_allist_to_hle_rsp",
            &[(
                "TERMCART_OPT_parallel_n64_send_allist_to_hle_rsp",
                "enabled",
            )],
            Some("enabled"),
        );
    }

    #[test]
    fn other_cores_get_no_table() {
        let mgba = Overrides::for_core(c"mGBA");
        assert!(mgba.lookup(c"parallel-n64-gfxplugin", |_| None).is_none());
        assert_eq!(
            mgba.lookup(c"mgba_skip_bios", |_| Some("ON".into()))
                .as_deref(),
            Some(c"ON")
        );
    }
}
