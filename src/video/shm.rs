//! POSIX shared memory objects handed to the terminal by name (`t=s`).
//!
//! The terminal maps the object, copies it and unlinks the name, so every frame needs a
//! fresh object. Names cycle through a few slots: a slot whose name still exists means
//! the terminal has not read that frame yet, and the caller should drop the new one.

use std::ffi::{CStr, CString};
use std::io;
use std::ptr;

const SLOTS: usize = 4;

pub struct ShmRing {
    names: [CString; SLOTS],
    next: usize,
}

impl ShmRing {
    pub fn new() -> ShmRing {
        let pid = std::process::id();
        // macOS caps names at 31 bytes (PSHMNAMLEN).
        let names = std::array::from_fn(|slot| {
            CString::new(format!("/termcart.{pid}.{slot}")).expect("name has no NUL")
        });
        ShmRing { names, next: 0 }
    }

    /// Creates the next object with `len` bytes, lets `fill` write them, and returns its
    /// name. Returns `None` when the terminal still holds the slot.
    pub fn publish(
        &mut self,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> io::Result<Option<&CStr>> {
        let slot = self.next;
        let name = &self.names[slot];
        // SAFETY: `name` is a valid NUL-terminated string.
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600,
            )
        };
        if fd < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::EEXIST) => Ok(None),
                _ => Err(err),
            };
        }
        let fd = Fd(fd);
        let result = Self::fill(&fd, len, fill);
        if result.is_err() {
            // SAFETY: as above.
            unsafe { libc::shm_unlink(name.as_ptr()) };
        }
        result?;
        self.next = (slot + 1) % SLOTS;
        Ok(Some(name))
    }

    fn fill(fd: &Fd, len: usize, fill: impl FnOnce(&mut [u8])) -> io::Result<()> {
        let size = libc::off_t::try_from(len).map_err(|_| io::ErrorKind::InvalidInput)?;
        // SAFETY: `fd` is an open shared memory object we own.
        if unsafe { libc::ftruncate(fd.0, size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: mapping `len` bytes of an object we just sized to `len`.
        let map = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.0,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the mapping is `len` writable bytes, exclusively ours until munmap.
        let bytes = unsafe { std::slice::from_raw_parts_mut(map.cast::<u8>(), len) };
        fill(bytes);
        // SAFETY: `map` came from mmap with this length and `bytes` is no longer used.
        unsafe { libc::munmap(map, len) };
        Ok(())
    }
}

impl Drop for ShmRing {
    fn drop(&mut self) {
        for name in &self.names {
            // SAFETY: valid C string; a name the terminal already removed gives ENOENT.
            unsafe { libc::shm_unlink(name.as_ptr()) };
        }
    }
}

struct Fd(libc::c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: we own this descriptor and close it once.
        unsafe { libc::close(self.0) };
    }
}
