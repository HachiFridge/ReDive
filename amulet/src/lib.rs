//! amulet — the hide layer. Deployed next to the DMM launcher renamed to
//! `version.dll` (its proxy keeps the launcher's version API working) and, on
//! Proton/Wine, loaded through a `WINEDLLOVERRIDES` native override instead of
//! any registry edit.
//!
//! Its file-API hooks implement the hide layer: when the launcher touches a
//! basename listed in `amulet.conf`, the call is re-aimed at `name + ".old"` —
//! so DMM's verify pass reads the untouched originals while the unpacked files
//! stay in place.

mod conf;
mod proxy;

use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use windows::core::PCSTR;
use windows::Win32::Foundation::{GetLastError, HMODULE};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleA, GetProcAddress};
use windows::Win32::System::Memory::{VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
use windows::Win32::System::Threading::{
    QueueUserAPC, ResumeThread, CREATE_SUSPENDED, PROCESS_INFORMATION,
};

#[allow(non_camel_case_types)]
type BOOL = i32;
#[allow(non_camel_case_types)]
type HINSTANCE = isize;
#[allow(non_camel_case_types)]
type HANDLE = isize;

const DLL_PROCESS_ATTACH: u32 = 1;

type CreateFileWFn =
    unsafe extern "system" fn(*const u16, u32, u32, *mut c_void, u32, u32, HANDLE) -> HANDLE;
type DeleteFileWFn = unsafe extern "system" fn(*const u16) -> BOOL;
type GetFileAttributesExWFn = unsafe extern "system" fn(*const u16, u32, *mut c_void) -> BOOL;
type GetFileAttributesWFn = unsafe extern "system" fn(*const u16) -> u32;
type MoveFileWFn = unsafe extern "system" fn(*const u16, *const u16) -> BOOL;
type MoveFileExWFn = unsafe extern "system" fn(*const u16, *const u16, u32) -> BOOL;
type CreateProcessWFn = unsafe extern "system" fn(
    *const u16,
    *mut u16,
    *mut c_void,
    *mut c_void,
    BOOL,
    u32,
    *mut c_void,
    *const u16,
    *mut c_void,
    *mut PROCESS_INFORMATION,
) -> BOOL;

static CREATE_FILE_W_ORIG: AtomicUsize = AtomicUsize::new(0);
static DELETE_FILE_W_ORIG: AtomicUsize = AtomicUsize::new(0);
static GET_FILE_ATTRIBUTES_EX_W_ORIG: AtomicUsize = AtomicUsize::new(0);
static GET_FILE_ATTRIBUTES_W_ORIG: AtomicUsize = AtomicUsize::new(0);
static MOVE_FILE_W_ORIG: AtomicUsize = AtomicUsize::new(0);
static MOVE_FILE_EX_W_ORIG: AtomicUsize = AtomicUsize::new(0);
static CREATE_PROCESS_W_ORIG: AtomicUsize = AtomicUsize::new(0);

static HIDDEN: OnceLock<Vec<String>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static DLL_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static OWN_IMAGE: OnceLock<(String, PathBuf)> = OnceLock::new();

fn is_carrier_exe(exe: &str) -> bool {
    let lower = exe.to_ascii_lowercase();
    matches!(lower.as_str(), "cmd.exe" | "powershell.exe" | "pwsh.exe")
}

fn is_own_image(name: &str, dir: &Path) -> bool {
    OWN_IMAGE
        .get()
        .map(|(n, d)| n == &name.to_ascii_lowercase() && (d == dir || dir.as_os_str().is_empty()))
        .unwrap_or(false)
}

fn command_line_carries_game(cmd_lower: &str) -> bool {
    if let Some(hidden) = HIDDEN.get() {
        hidden
            .iter()
            .any(|name| cmd_lower.contains(&name.to_ascii_lowercase()))
    } else {
        false
    }
}

// --- logging ---

fn log(msg: &str) {
    use std::io::Write;
    let Some(path) = LOG_PATH.get() else { return };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{stamp}] {msg}");
    }
}

pub(crate) unsafe fn get_proc(module: HMODULE, name: &str) -> Option<*mut c_void> {
    let mut n = name.as_bytes().to_vec();
    n.push(0);
    GetProcAddress(module, PCSTR(n.as_ptr())).map(|f| f as *mut c_void)
}

// --- redirect ---

/// `Some(path + ".old")` when the path's basename is on the hidden list.
unsafe fn redirect(api: &str, lp_file_name: *const u16) -> Option<Vec<u16>> {
    if lp_file_name.is_null() {
        return None;
    }
    let mut len = 0usize;
    while len < 32768 && *lp_file_name.add(len) != 0 {
        len += 1;
    }
    let wide = std::slice::from_raw_parts(lp_file_name, len);
    let path = String::from_utf16_lossy(wide);
    if !conf::is_hidden(HIDDEN.get()?, &path) {
        return None;
    }
    let mut out = wide.to_vec();
    out.extend_from_slice(&[b'.' as u16, b'o' as u16, b'l' as u16, b'd' as u16, 0]);
    log(&format!("{api}: {path} -> {path}.old"));
    Some(out)
}

unsafe fn extract_string_from_wide(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while len < 32768 && *ptr.add(len) != 0 {
        len += 1;
    }
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(
        ptr, len,
    )))
}

unsafe fn inject_payload_apc(pi: &PROCESS_INFORMATION, payload_path: &Path) -> Result<(), String> {
    let dll_s = format!("{}\0", payload_path.display());
    let bytes = dll_s.as_bytes();
    let alloc = VirtualAllocEx(
        pi.hProcess,
        None,
        bytes.len(),
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if alloc.is_null() {
        return Err(format!("VirtualAllocEx failed: {}", GetLastError().0));
    }
    let mut written = 0usize;
    WriteProcessMemory(
        pi.hProcess,
        alloc,
        bytes.as_ptr() as _,
        bytes.len(),
        Some(&mut written),
    )
    .map_err(|e| format!("WriteProcessMemory failed: {e}"))?;
    if written != bytes.len() {
        return Err("WriteProcessMemory partial write".to_string());
    }
    let k32 = GetModuleHandleA(PCSTR(b"kernel32.dll\0".as_ptr()))
        .map_err(|e| format!("GetModuleHandleA(kernel32): {e}"))?;
    let load_lib = GetProcAddress(k32, PCSTR(b"LoadLibraryA\0".as_ptr()))
        .ok_or_else(|| "GetProcAddress(LoadLibraryA) failed".to_string())?;
    if QueueUserAPC(std::mem::transmute(load_lib), pi.hThread, alloc as usize) == 0 {
        return Err(format!("QueueUserAPC failed: {}", GetLastError().0));
    }
    Ok(())
}

// --- hooks (file hiding + launch interception) ---

unsafe extern "system" fn create_file_w_hook(
    lp_file_name: *const u16,
    access: u32,
    share: u32,
    security: *mut c_void,
    disposition: u32,
    flags: u32,
    template: HANDLE,
) -> HANDLE {
    let orig: CreateFileWFn = std::mem::transmute(CREATE_FILE_W_ORIG.load(Ordering::Acquire));
    match redirect("CreateFileW", lp_file_name) {
        Some(new) => orig(
            new.as_ptr(),
            access,
            share,
            security,
            disposition,
            flags,
            template,
        ),
        None => orig(
            lp_file_name,
            access,
            share,
            security,
            disposition,
            flags,
            template,
        ),
    }
}

unsafe extern "system" fn delete_file_w_hook(lp_file_name: *const u16) -> BOOL {
    let orig: DeleteFileWFn = std::mem::transmute(DELETE_FILE_W_ORIG.load(Ordering::Acquire));
    match redirect("DeleteFileW", lp_file_name) {
        Some(new) => orig(new.as_ptr()),
        None => orig(lp_file_name),
    }
}

unsafe extern "system" fn get_file_attributes_ex_w_hook(
    lp_file_name: *const u16,
    info_level: u32,
    file_info: *mut c_void,
) -> BOOL {
    let orig: GetFileAttributesExWFn =
        std::mem::transmute(GET_FILE_ATTRIBUTES_EX_W_ORIG.load(Ordering::Acquire));
    match redirect("GetFileAttributesExW", lp_file_name) {
        Some(new) => orig(new.as_ptr(), info_level, file_info),
        None => orig(lp_file_name, info_level, file_info),
    }
}

unsafe extern "system" fn get_file_attributes_w_hook(lp_file_name: *const u16) -> u32 {
    let orig: GetFileAttributesWFn =
        std::mem::transmute(GET_FILE_ATTRIBUTES_W_ORIG.load(Ordering::Acquire));
    match redirect("GetFileAttributesW", lp_file_name) {
        Some(new) => orig(new.as_ptr()),
        None => orig(lp_file_name),
    }
}

unsafe extern "system" fn move_file_w_hook(lp_existing: *const u16, lp_new: *const u16) -> BOOL {
    let orig: MoveFileWFn = std::mem::transmute(MOVE_FILE_W_ORIG.load(Ordering::Acquire));
    match redirect("MoveFileW", lp_new) {
        Some(redirected) => orig(lp_existing, redirected.as_ptr()),
        None => orig(lp_existing, lp_new),
    }
}

unsafe extern "system" fn move_file_ex_w_hook(
    lp_existing: *const u16,
    lp_new: *const u16,
    flags: u32,
) -> BOOL {
    let orig: MoveFileExWFn = std::mem::transmute(MOVE_FILE_EX_W_ORIG.load(Ordering::Acquire));
    match redirect("MoveFileExW", lp_new) {
        Some(redirected) => orig(lp_existing, redirected.as_ptr(), flags),
        None => orig(lp_existing, lp_new, flags),
    }
}

unsafe extern "system" fn create_process_w_hook(
    lp_application_name: *const u16,
    lp_command_line: *mut u16,
    lp_process_attributes: *mut c_void,
    lp_thread_attributes: *mut c_void,
    b_inherit_handles: BOOL,
    dw_creation_flags: u32,
    lp_environment: *mut c_void,
    lp_current_directory: *const u16,
    lp_startup_info: *mut c_void,
    lp_process_information: *mut PROCESS_INFORMATION,
) -> BOOL {
    let orig: CreateProcessWFn = std::mem::transmute(CREATE_PROCESS_W_ORIG.load(Ordering::Acquire));

    let app_name = extract_string_from_wide(lp_application_name);
    let cmd_line = extract_string_from_wide(lp_command_line);

    let is_target_game = if let Some(hidden) = HIDDEN.get() {
        let app_match = app_name
            .as_deref()
            .map(|n| conf::is_hidden(hidden, n))
            .unwrap_or(false);
        let cmd_match = cmd_line
            .as_deref()
            .map(|c| {
                let trimmed = c.trim().trim_start_matches('"');
                let token = match trimmed.find(['"', ' ']) {
                    Some(idx) => &trimmed[..idx],
                    None => trimmed,
                };
                conf::is_hidden(hidden, token)
            })
            .unwrap_or(false);
        app_match || cmd_match
    } else {
        false
    };

    if !is_target_game {
        let (exe_name, exe_dir) = match (&app_name, &cmd_line) {
            (Some(app), _) => {
                let p = Path::new(app);
                (
                    p.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    p.parent().unwrap_or(Path::new("")).to_path_buf(),
                )
            }
            (None, Some(cmd)) => {
                let trimmed = cmd.trim().trim_start_matches('"');
                let token = match trimmed.find(['"', ' ']) {
                    Some(idx) => &trimmed[..idx],
                    None => trimmed,
                };
                let p = Path::new(token);
                (
                    p.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    p.parent().unwrap_or(Path::new("")).to_path_buf(),
                )
            }
            _ => (String::new(), PathBuf::new()),
        };

        let cmd_lower = cmd_line.as_deref().unwrap_or_default().to_ascii_lowercase();
        let same_exe = !exe_name.is_empty() && is_own_image(&exe_name, &exe_dir);
        let carrier = !exe_name.is_empty()
            && is_carrier_exe(&exe_name)
            && command_line_carries_game(&cmd_lower);

        if same_exe || carrier {
            let kind = if same_exe {
                "runtime child (same executable)"
            } else {
                "carrier"
            };
            log(&format!(
                "CreateProcessW: arming {} '{}' (cmd={:?})",
                kind, exe_name, cmd_line
            ));

            let caller_wanted_suspended = (dw_creation_flags & CREATE_SUSPENDED.0) != 0;
            let modified_flags = dw_creation_flags | CREATE_SUSPENDED.0;

            let ret = orig(
                lp_application_name,
                lp_command_line,
                lp_process_attributes,
                lp_thread_attributes,
                b_inherit_handles,
                modified_flags,
                lp_environment,
                lp_current_directory,
                lp_startup_info,
                lp_process_information,
            );

            if ret != 0 && !lp_process_information.is_null() {
                let pi = &*lp_process_information;
                if let Some(dll_dir) = DLL_DIRECTORY.get() {
                    let amulet_dll = dll_dir.join("amulet.dll");
                    if amulet_dll.is_file() {
                        match inject_payload_apc(pi, &amulet_dll) {
                            Ok(()) => log(&format!(
                                "successfully armed {} with amulet.dll (pid {})",
                                kind,
                                windows::Win32::System::Threading::GetProcessId(pi.hProcess)
                            )),
                            Err(e) => log(&format!("WARN: carrier arming failed: {e}")),
                        }
                    }
                }

                if !caller_wanted_suspended {
                    ResumeThread(pi.hThread);
                }
            }
            return ret;
        }

        return orig(
            lp_application_name,
            lp_command_line,
            lp_process_attributes,
            lp_thread_attributes,
            b_inherit_handles,
            dw_creation_flags,
            lp_environment,
            lp_current_directory,
            lp_startup_info,
            lp_process_information,
        );
    }

    log(&format!(
        "CreateProcessW: intercepting game launch (app={:?}, cmd={:?})",
        app_name, cmd_line
    ));

    let caller_wanted_suspended = (dw_creation_flags & CREATE_SUSPENDED.0) != 0;
    let modified_flags = dw_creation_flags | CREATE_SUSPENDED.0;

    let ret = orig(
        lp_application_name,
        lp_command_line,
        lp_process_attributes,
        lp_thread_attributes,
        b_inherit_handles,
        modified_flags,
        lp_environment,
        lp_current_directory,
        lp_startup_info,
        lp_process_information,
    );

    if ret != 0 && !lp_process_information.is_null() {
        let pi = &*lp_process_information;
        if let Some(dll_dir) = DLL_DIRECTORY.get() {
            let payload = dll_dir.join("prism.dll");
            if payload.is_file() {
                match inject_payload_apc(pi, &payload) {
                    Ok(()) => log(&format!(
                        "successfully queued prism.dll via APC into game (pid {})",
                        windows::Win32::System::Threading::GetProcessId(pi.hProcess)
                    )),
                    Err(e) => log(&format!("WARN: APC injection into game failed: {e}")),
                }
            } else {
                log(&format!(
                    "WARN: {} not found next to amulet; skipping game injection",
                    payload.display()
                ));
            }
        }

        if !caller_wanted_suspended {
            ResumeThread(pi.hThread);
        }
    }

    ret
}

unsafe fn setup_hooks() {
    let Ok(k32) = GetModuleHandleA(PCSTR(b"kernel32.dll\0".as_ptr())) else {
        log("WARN: kernel32 not resolved; hooks not installed");
        return;
    };

    macro_rules! hook {
        ($name:literal, $detour:expr, $slot:expr) => {{
            match get_proc(k32, $name) {
                Some(func) => match minhook::MinHook::create_hook(func, $detour as *mut c_void) {
                    Ok(trampoline) => {
                        $slot.store(trampoline as usize, Ordering::Release);
                        let _ = minhook::MinHook::enable_hook(func);
                        log(concat!("hooked ", $name));
                    }
                    Err(e) => log(&format!("WARN: cannot hook {}: {e:?}", $name)),
                },
                None => log(&format!("WARN: {} not exported by kernel32", $name)),
            }
        }};
    }

    hook!("CreateFileW", create_file_w_hook, CREATE_FILE_W_ORIG);
    hook!("DeleteFileW", delete_file_w_hook, DELETE_FILE_W_ORIG);
    hook!(
        "GetFileAttributesExW",
        get_file_attributes_ex_w_hook,
        GET_FILE_ATTRIBUTES_EX_W_ORIG
    );
    hook!(
        "GetFileAttributesW",
        get_file_attributes_w_hook,
        GET_FILE_ATTRIBUTES_W_ORIG
    );
    hook!("MoveFileW", move_file_w_hook, MOVE_FILE_W_ORIG);
    hook!("MoveFileExW", move_file_ex_w_hook, MOVE_FILE_EX_W_ORIG);
    hook!(
        "CreateProcessW",
        create_process_w_hook,
        CREATE_PROCESS_W_ORIG
    );
}

// --- bootstrap ---

fn module_dir(handle: *mut c_void) -> Option<PathBuf> {
    unsafe {
        let mut buf = [0u16; 512];
        let len = GetModuleFileNameW(Some(HMODULE(handle)), &mut buf);
        if len == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        PathBuf::from(path).parent().map(|p| p.to_path_buf())
    }
}

fn process_dir() -> Option<PathBuf> {
    unsafe {
        let mut buf = [0u16; 512];
        let len = GetModuleFileNameW(None, &mut buf);
        if len == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        PathBuf::from(path).parent().map(|p| p.to_path_buf())
    }
}

fn bootstrap(module: HINSTANCE) {
    let dll_dir = module_dir(module as *mut c_void);
    let exe_dir = process_dir();

    unsafe {
        let mut buf = [0u16; 512];
        let len = GetModuleFileNameW(None, &mut buf);
        if len > 0 {
            let path = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let _ = OWN_IMAGE.set((name, dir));
        }
    }

    if let Some(d) = &dll_dir {
        let _ = LOG_PATH.set(d.join("amulet.log"));
        let _ = DLL_DIRECTORY.set(d.clone());
    }
    log("=== amulet hide layer loaded ===");
    log(&format!(
        "module dir: {}",
        dll_dir
            .as_deref()
            .unwrap_or(std::path::Path::new("?"))
            .display()
    ));

    proxy::init();

    match conf::find(dll_dir.as_deref(), exe_dir.as_deref()) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => {
                let names = conf::parse(&text);
                log(&format!("conf: {} ({} names)", path.display(), names.len()));
                for n in &names {
                    log(&format!("  hide {n}"));
                }
                if names.is_empty() {
                    log("conf lists no files; hooks not installed");
                    return;
                }
                let _ = HIDDEN.set(names);
                unsafe { setup_hooks() };
            }
            Err(e) => log(&format!(
                "WARN: cannot read {} ({e}); not hiding",
                path.display()
            )),
        },
        None => log(&format!(
            "WARN: {} not found next to the DLL, the process, or the CWD; not hiding",
            conf::CONF_NAME
        )),
    }
}

#[no_mangle]
pub extern "system" fn DllMain(module: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        bootstrap(module);
    }
    1
}

// --- fixture: a fake verify pass against a temp game tree ---

#[cfg(test)]
mod tests {
    use super::*;

    type ReadFileFn =
        unsafe extern "system" fn(HANDLE, *mut c_void, u32, *mut u32, *mut c_void) -> BOOL;
    type CloseHandleFn = unsafe extern "system" fn(HANDLE) -> BOOL;

    type GetFileAttributesExWRawFn =
        unsafe extern "system" fn(*const u16, u32, *mut c_void) -> BOOL;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn raw(name: &str) -> *mut c_void {
        let k32 = GetModuleHandleA(PCSTR(b"kernel32.dll\0".as_ptr()))
            .ok()
            .expect("kernel32");
        get_proc(k32, name).unwrap_or_else(|| panic!("{name} not found"))
    }

    /// Open through the (hooked) API and read up to 64 bytes; None when the
    /// hidden view cannot see the file.
    unsafe fn read_via_api(path: &str) -> Option<Vec<u8>> {
        let create: CreateFileWFn = std::mem::transmute(raw("CreateFileW"));
        let read: ReadFileFn = std::mem::transmute(raw("ReadFile"));
        let close: CloseHandleFn = std::mem::transmute(raw("CloseHandle"));
        let w = wide(path);
        let h = create(
            w.as_ptr(),
            0x8000_0000,
            0x7,
            std::ptr::null_mut(),
            3,
            0x80,
            0,
        );
        if h == -1 {
            return None;
        }
        let mut buf = vec![0u8; 64];
        let mut n = 0u32;
        read(
            h,
            buf.as_mut_ptr() as *mut c_void,
            64,
            &mut n,
            std::ptr::null_mut(),
        );
        close(h);
        buf.truncate(n as usize);
        Some(buf)
    }

    #[test]
    fn hidden_view_reads_originals() {
        let dir = std::env::temp_dir().join(format!("redive-amulet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("GameAssembly.dll");
        let original = dir.join("GameAssembly.dll.old");
        let plain = dir.join("readme.txt");
        // Fixtures are created before the hooks exist — once hooked, writes to
        // a hidden basename would be redirected too.
        std::fs::write(&target, "unpacked").unwrap();
        std::fs::write(&original, "the-original").unwrap();
        std::fs::write(&plain, "plain-data").unwrap();

        HIDDEN.set(vec!["GameAssembly.dll".to_string()]).ok();
        unsafe { setup_hooks() };
        assert_ne!(
            CREATE_FILE_W_ORIG.load(Ordering::Acquire),
            0,
            "CreateFileW not hooked"
        );
        assert_ne!(
            DELETE_FILE_W_ORIG.load(Ordering::Acquire),
            0,
            "DeleteFileW not hooked"
        );
        assert_ne!(
            GET_FILE_ATTRIBUTES_EX_W_ORIG.load(Ordering::Acquire),
            0,
            "GetFileAttributesExW not hooked"
        );
        assert_ne!(
            GET_FILE_ATTRIBUTES_W_ORIG.load(Ordering::Acquire),
            0,
            "GetFileAttributesW not hooked"
        );
        assert_ne!(
            MOVE_FILE_W_ORIG.load(Ordering::Acquire),
            0,
            "MoveFileW not hooked"
        );
        assert_ne!(
            MOVE_FILE_EX_W_ORIG.load(Ordering::Acquire),
            0,
            "MoveFileExW not hooked"
        );
        assert_ne!(
            CREATE_PROCESS_W_ORIG.load(Ordering::Acquire),
            0,
            "CreateProcessW not hooked"
        );

        let path = target.to_string_lossy().to_string();
        unsafe {
            // The verify pass sees the original, not the unpacked copy.
            assert_eq!(
                read_via_api(&path).as_deref(),
                Some(&b"the-original"[..]),
                "CreateFileW must be redirected to X.old"
            );

            let attr: GetFileAttributesExWRawFn = std::mem::transmute(raw("GetFileAttributesExW"));
            let w = wide(&path);
            // windows-rs owns the struct layout (FILETIME fields are two u32s,
            // so a hand-rolled u64 version would misalign nFileSizeLow).
            let mut data =
                windows::Win32::Storage::FileSystem::WIN32_FILE_ATTRIBUTE_DATA::default();
            assert_eq!(
                attr(w.as_ptr(), 0, &mut data as *mut _ as *mut c_void),
                1,
                "attributes unreadable"
            );
            assert_eq!(data.nFileSizeLow, 12, "size must come from X.old, not X");

            // Unlisted files pass through untouched.
            let other = plain.to_string_lossy().to_string();
            assert_eq!(
                read_via_api(&other).as_deref(),
                Some(&b"plain-data"[..]),
                "unlisted file must not be redirected"
            );

            // Delete of the hidden name hits X.old; the real X survives.
            let del: DeleteFileWFn = std::mem::transmute(raw("DeleteFileW"));
            assert_ne!(del(w.as_ptr()), 0, "delete should succeed on X.old");
            let names: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            assert!(
                names.iter().any(|n| n == "GameAssembly.dll"),
                "X must survive: {names:?}"
            );
            assert!(
                !names.iter().any(|n| n == "GameAssembly.dll.old"),
                "X.old must be gone: {names:?}"
            );

            // The hidden view now reports X as missing — DMM's whole world
            // is the .old side of the tree.
            assert_eq!(
                read_via_api(&path),
                None,
                "X should look missing after .old delete"
            );
        }

        // X is unreachable through the hook, so cleanup is best-effort.
        let _ = std::fs::remove_file(&original);
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_file(&plain);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
