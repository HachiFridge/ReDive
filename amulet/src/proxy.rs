//! `version.dll` proxy: every export forwards to the real system
//! `version.dll`, loaded by absolute path so the loader can never resolve
//! the forward back into ourselves (we carry the same file name at deploy).

use std::sync::OnceLock;
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, LoadLibraryW};

/// One slot per forwarded export; wrappers transmute the slot to their own
/// signature. An unresolved slot (0) fails closed instead of jumping.
macro_rules! forwarders {
    ($($name:ident : fn($($arg:ident : $aty:ty),*) -> $ret:ty;)*) => {
        #[allow(non_snake_case)]
        struct Api { $($name: usize),* }

        static API: OnceLock<Api> = OnceLock::new();

        fn api() -> &'static Api {
            API.get_or_init(|| Api { $($name: 0),* })
        }

        /// Resolve every export against the real module; returns the names
        /// the real DLL did not provide.
        pub(crate) unsafe fn resolve(module: HMODULE) -> Vec<&'static str> {
            let api = Api {
                $($name: crate::get_proc(module, stringify!($name)).map(|p| p as usize).unwrap_or(0)),*
            };
            let mut missing = Vec::new();
            $(if api.$name == 0 { missing.push(stringify!($name)); })*
            let _ = API.set(api);
            missing
        }

        $(
            #[no_mangle]
            #[allow(non_snake_case)]
            pub unsafe extern "system" fn $name($($arg: $aty),*) -> $ret {
                let addr = api().$name;
                if addr == 0 {
                    return 0 as $ret;
                }
                let f: unsafe extern "system" fn($($aty),*) -> $ret = std::mem::transmute(addr);
                f($($arg),*)
            }
        )*
    };
}

forwarders! {
    GetFileVersionInfoA: fn(filename: *const u8, handle: u32, len: u32, data: *mut c_void) -> i32;
    GetFileVersionInfoByHandle: fn(flags: u32, file: isize, a: u32, b: u32, data: *mut c_void) -> i32;
    GetFileVersionInfoExA: fn(flags: u32, filename: *const u8, handle: u32, len: u32, data: *mut c_void) -> i32;
    GetFileVersionInfoExW: fn(flags: u32, filename: *const u16, handle: u32, len: u32, data: *mut c_void) -> i32;
    GetFileVersionInfoSizeA: fn(filename: *const u8, handle: *mut u32) -> u32;
    GetFileVersionInfoSizeExA: fn(flags: u32, filename: *const u8, handle: *mut u32) -> u32;
    GetFileVersionInfoSizeExW: fn(flags: u32, filename: *const u16, handle: *mut u32) -> u32;
    GetFileVersionInfoSizeW: fn(filename: *const u16, handle: *mut u32) -> u32;
    GetFileVersionInfoW: fn(filename: *const u16, handle: u32, len: u32, data: *mut c_void) -> i32;
    VerFindFileA: fn(flags: u32, name: *const u8, windir: *const u8, appdir: *const u8, curdir: *mut u8, curlen: *mut u32, dest: *mut u8, destlen: *mut u32) -> u32;
    VerFindFileW: fn(flags: u32, name: *const u16, windir: *const u16, appdir: *const u16, curdir: *mut u16, curlen: *mut u32, dest: *mut u16, destlen: *mut u32) -> u32;
    VerInstallFileA: fn(flags: u32, srcname: *const u8, srcpath: *const u8, dest: *const u8, destpath: *const u8, curdir: *const u8, tmp: *mut u8, tmplen: *mut u32) -> u32;
    VerInstallFileW: fn(flags: u32, srcname: *const u16, srcpath: *const u16, dest: *const u16, destpath: *const u16, curdir: *const u16, tmp: *mut u16, tmplen: *mut u32) -> u32;
    VerLanguageNameA: fn(lang: u32, buf: *mut u8, size: u32) -> u32;
    VerLanguageNameW: fn(lang: u32, buf: *mut u16, size: u32) -> u32;
    VerQueryValueA: fn(block: *const c_void, sub: *const u8, buffer: *mut *mut c_void, len: *mut u32) -> i32;
    VerQueryValueW: fn(block: *const c_void, sub: *const u16, buffer: *mut *mut c_void, len: *mut u32) -> i32;
}

use std::os::raw::c_void;

/// `%SystemRoot%\System32` as text (always ASCII).
fn system_dir() -> Option<String> {
    unsafe {
        let k32 = GetModuleHandleA(PCSTR(b"kernel32.dll\0".as_ptr())).ok()?;
        let get: unsafe extern "system" fn(*mut u16, u32) -> u32 =
            std::mem::transmute(crate::get_proc(k32, "GetSystemDirectoryW")?);
        let mut buf = vec![0u16; 512];
        let len = get(buf.as_mut_ptr(), buf.len() as u32) as usize;
        if len == 0 || len >= buf.len() {
            return None;
        }
        buf.truncate(len);
        Some(String::from_utf16_lossy(&buf))
    }
}

/// Load the real system `version.dll` by absolute path and bind every
/// forwarder. Called once from DllMain before anything else.
pub fn init() {
    unsafe {
        let Some(dir) = system_dir() else {
            crate::log("WARN: could not resolve the system directory; version exports fail closed");
            return;
        };
        let mut path: Vec<u16> = format!("{dir}\\version.dll").encode_utf16().collect();
        path.push(0);
        match LoadLibraryW(PCWSTR(path.as_ptr())) {
            Ok(module) => {
                let missing = resolve(module);
                if missing.is_empty() {
                    crate::log("version.dll proxy bound to the system DLL");
                } else {
                    crate::log(&format!(
                        "WARN: system version.dll lacks export(s): {}",
                        missing.join(", ")
                    ));
                }
            }
            Err(e) => crate::log(&format!(
                "WARN: cannot load {dir}\\version.dll ({e}); version exports fail closed"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_forward_to_real_version_dll() {
        init();
        let dir = system_dir().expect("system directory");
        let mut path: Vec<u16> = format!("{dir}\\kernel32.dll").encode_utf16().collect();
        path.push(0);
        let mut handle = 0u32;
        unsafe {
            let size = GetFileVersionInfoSizeW(path.as_ptr(), &mut handle);
            assert!(size > 0, "GetFileVersionInfoSizeW did not forward");
            let mut buf = vec![0u8; size as usize];
            assert_eq!(
                GetFileVersionInfoW(path.as_ptr(), 0, size, buf.as_mut_ptr() as *mut c_void),
                1,
                "GetFileVersionInfoW did not forward"
            );
            // VS_FIXEDFILEINFO lives behind VerQueryValueW("\\"), not at the
            // buffer start (that is the VS_VERSIONINFO header).
            let mut ffi: *mut c_void = std::ptr::null_mut();
            let mut ffi_len = 0u32;
            let root: Vec<u16> = "\\".encode_utf16().chain(std::iter::once(0)).collect();
            assert_eq!(
                VerQueryValueW(
                    buf.as_ptr() as *const c_void,
                    root.as_ptr(),
                    &mut ffi,
                    &mut ffi_len
                ),
                1,
                "VerQueryValueW did not forward"
            );
            assert_eq!(ffi_len, 52, "unexpected VS_FIXEDFILEINFO length");
            assert_eq!(
                *(ffi as *const u32),
                0xFEEF_04BD,
                "unexpected version resource signature"
            );
        }
    }
}
