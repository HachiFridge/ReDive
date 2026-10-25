//! `redive launch` — spawn the game directly with the payload, or route
//! through a DMM launcher (Game Player or a community fast launcher) with the
//! hide layer planted and the payload delivered once the game appears.
//!
//! Chain selection follows GateJumper's injector: first `.exe` argument →
//! `redive.ini [main]` → directory scan of ReDive's own folder.
//!
//!   - **Direct**: the game starts suspended and `redive_payload.dll` is
//!     queued by APC before it runs — earliest possible payload load.
//!   - **Launcher (dmm / fast)**: the launcher starts suspended and gets
//!     `amulet.dll` the same way (hide layer + version proxy, resolved from
//!     *our* folder — nothing is written into the launcher's or the game's
//!     directory). Then we watch the process table for the game and inject
//!     `redive_payload.dll` on sight, so the `cmd.exe` hop of a fast
//!     launcher's `shell=True` spawn is irrelevant: only the final process
//!     matters.
//!
//! Everything the chains need must ship next to `redive.exe`; a missing DLL
//! aborts before anything is launched.

use crate::resolve::{self, MainConfig};
use std::path::{Path, PathBuf};
use windows::core::{PCSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows::Win32::System::Memory::{VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
use windows::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeThread, OpenProcess, QueueUserAPC, ResumeThread,
    TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED, PROCESS_INFORMATION, STARTUPINFOW,
};

const PAYLOAD_DLL: &str = "prism.dll";
const HIDE_DLL: &str = "amulet.dll";
const INJECT_TIMEOUT_MS: u32 = 10_000;

#[derive(Debug, PartialEq, Eq)]
pub enum Chain {
    Direct,
    Dmm,
    FastLauncher,
}

pub type ResolvedLaunch = (Chain, PathBuf, Option<PathBuf>, Option<Vec<u16>>);

/// A launcher exe routes the chain; anything else is the game itself.
/// `fastlauncher` is checked first — `DMMGamePlayerFastLauncher.exe` also
/// contains "gameplayer".
pub fn classify_chain(exe_name: &str) -> Chain {
    let lower = exe_name.to_ascii_lowercase();
    if lower.contains("fastlauncher") {
        Chain::FastLauncher
    } else if lower.contains("gameplayer") || lower.contains("dmm") {
        Chain::Dmm
    } else {
        Chain::Direct
    }
}

fn log(msg: &str) {
    println!("[ReDive] {msg}");
    use std::io::Write;
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("redive.log"))
            {
                let _ = writeln!(f, "[ReDive] {msg}");
            }
        }
    }
}

fn our_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn path_exists(path: &str) -> bool {
    let p = Path::new(path);
    p.is_file() || p.is_dir()
}

/// First `.exe` argument that is not us and not a launcher helper — the
/// explicit choice that beats the ini. Returns `(raw, index)` so the command
/// line can be reconstructed from that argument forward.
pub fn find_target_arg(args: &[String], our_name: &str) -> Option<(String, usize)> {
    for (i, arg) in args.iter().enumerate() {
        if arg.starts_with("--") {
            continue;
        }
        let trimmed = arg.trim_matches('"');
        if Path::new(trimmed)
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| e.eq_ignore_ascii_case("exe"))
            != Some(true)
        {
            continue;
        }
        let name = Path::new(trimmed)
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if name == our_name || name == "redive.exe" || name == "start.exe" {
            continue;
        }
        return Some((trimmed.to_string(), i));
    }
    None
}

/// `CreateProcessW` with `CREATE_SUSPENDED`; child CWD is the exe's own
/// directory (launchers resolve their assets against it).
/// `detached = true` decouples the process from our console session so GUI
/// apps don't inherit console handles or dump logs into our terminal.
unsafe fn spawn_suspended(
    exe: &Path,
    cmd: Option<&mut Vec<u16>>,
    detached: bool,
) -> Result<PROCESS_INFORMATION, String> {
    let exe_w: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let cwd_w: Vec<u16> = exe
        .parent()
        .map(|p| {
            p.to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect()
        })
        .unwrap_or_default();
    let lp_cwd = if cwd_w.is_empty() {
        PCWSTR(std::ptr::null())
    } else {
        PCWSTR(cwd_w.as_ptr())
    };

    let mut si: STARTUPINFOW = std::mem::zeroed();
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
    let cmd_ptr = cmd.map(|c| PWSTR(c.as_mut_ptr()));

    let mut creation_flags = CREATE_SUSPENDED;
    if detached {
        creation_flags |= windows::Win32::System::Threading::DETACHED_PROCESS;
    }

    CreateProcessW(
        PCWSTR(exe_w.as_ptr()),
        cmd_ptr,
        None,
        None,
        false,
        creation_flags,
        None,
        lp_cwd,
        &si,
        &mut pi,
    )
    .map_err(|e| format!("CreateProcessW({}) failed: {e}", exe.display()))?;
    Ok(pi)
}

/// Queue `LoadLibraryA(dll)` on the suspended primary thread (early-bird APC,
/// the injector's proven path). The DLL must be an absolute path.
#[allow(clippy::manual_c_str_literals, clippy::missing_transmute_annotations)]
unsafe fn inject_apc(pi: &PROCESS_INFORMATION, dll: &Path) -> Result<(), String> {
    let dll_s = format!("{}\0", dll.display());
    let bytes = dll_s.as_bytes();
    let alloc = VirtualAllocEx(
        pi.hProcess,
        None,
        bytes.len(),
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if alloc.is_null() {
        return Err(format!("VirtualAllocEx failed ({})", GetLastError().0));
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
        return Err("WriteProcessMemory wrote a partial path".to_string());
    }
    let k32 = GetModuleHandleA(PCSTR(b"kernel32.dll\0".as_ptr()))
        .map_err(|e| format!("GetModuleHandleA(kernel32): {e}"))?;
    let load_lib = GetProcAddress(k32, PCSTR(b"LoadLibraryA\0".as_ptr()))
        .ok_or_else(|| "GetProcAddress(LoadLibraryA) failed".to_string())?;
    if QueueUserAPC(std::mem::transmute(load_lib), pi.hThread, alloc as usize) == 0 {
        return Err(format!("QueueUserAPC failed ({})", GetLastError().0));
    }
    Ok(())
}

/// Remote `LoadLibraryA(dll)` in a running process — the watcher's path.
#[allow(clippy::manual_c_str_literals, clippy::missing_transmute_annotations)]
unsafe fn inject_remote(process: HANDLE, dll: &Path) -> Result<(), String> {
    let dll_s = format!("{}\0", dll.display());
    let bytes = dll_s.as_bytes();
    let alloc = VirtualAllocEx(
        process,
        None,
        bytes.len(),
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if alloc.is_null() {
        return Err(format!("VirtualAllocEx failed ({})", GetLastError().0));
    }
    let mut written = 0usize;
    WriteProcessMemory(
        process,
        alloc,
        bytes.as_ptr() as _,
        bytes.len(),
        Some(&mut written),
    )
    .map_err(|e| format!("WriteProcessMemory failed: {e}"))?;
    if written != bytes.len() {
        return Err("WriteProcessMemory wrote a partial path".to_string());
    }
    let k32 = GetModuleHandleA(PCSTR(b"kernel32.dll\0".as_ptr()))
        .map_err(|e| format!("GetModuleHandleA(kernel32): {e}"))?;
    let load_lib = GetProcAddress(k32, PCSTR(b"LoadLibraryA\0".as_ptr()))
        .ok_or_else(|| "GetProcAddress(LoadLibraryA) failed".to_string())?;
    let thread = CreateRemoteThread(
        process,
        None,
        0,
        Some(std::mem::transmute(load_lib)),
        Some(alloc),
        0,
        None,
    )
    .map_err(|e| format!("CreateRemoteThread failed: {e}"))?;
    let wait = WaitForSingleObject(thread, INJECT_TIMEOUT_MS);
    let mut exit_code = 0u32;
    let checked = GetExitCodeThread(thread, &mut exit_code).is_ok();
    let _ = CloseHandle(thread);
    if wait.0 != 0 {
        return Err("WaitForSingleObject timed out/failed".to_string());
    }
    if checked && exit_code == 0 {
        return Err("LoadLibraryA returned NULL in the target".to_string());
    }
    Ok(())
}

/// Lowest common denominator for injecting into an already-running pid.
fn inject_into_pid(pid: u32, dll: &Path) -> Result<(), String> {
    unsafe {
        let access = windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS(
            0x0002 | 0x0040 | 0x0008 | 0x0020 | 0x0400, // create thread, query info, vm op, vm write, vm read
        );
        let process = OpenProcess(access, false, pid)
            .map_err(|e| format!("OpenProcess({pid}) failed: {e}"))?;
        let result = inject_remote(process, dll);
        let _ = CloseHandle(process);
        result
    }
}

/// First running process whose exe name matches the game set — skipping our
/// own pid and the launcher's, so a launcher that happens to live in the
/// game directory is never mistaken for the game. Names are lowercase basenames.
fn find_game_pid(names: &[String], exclude_pid: u32) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let our_pid = std::process::id();
        let mut found = None;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let exe = String::from_utf16_lossy(&entry.szExeFile);
                let exe = exe.trim_end_matches('\0').to_ascii_lowercase();
                let pid = entry.th32ProcessID;
                if pid != 0 && pid != our_pid && pid != exclude_pid && names.contains(&exe) {
                    found = Some(pid);
                    break;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// Poll the process table until the game shows up, then inject the payload.
fn watch_and_inject(names: &[String], dll: &Path, launcher_pid: u32) -> Result<u32, String> {
    log(&format!(
        "watching for game process [{}] …",
        names.join(", ")
    ));
    let mut tick = 0u32;
    loop {
        if let Some(pid) = find_game_pid(names, launcher_pid) {
            log(&format!(
                "game process found (pid {pid}); injecting {PAYLOAD_DLL}"
            ));
            inject_into_pid(pid, dll)?;
            log("payload injected.");
            return Ok(pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        tick += 1;
        if tick.is_multiple_of(500) {
            log("still waiting for the game to start…");
        }
    }
}

/// The payload (and hide layer for launchers) must ship in ReDive's folder —
/// fail before anything launches instead of running silently unarmed.
fn require_dll(our_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let path = our_dir.join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{name} not found next to redive.exe ({}); build it first",
            our_dir.display()
        ))
    }
}

/// Resolve `(chain, launcher exe, game)` from args + ini + directory scan.
fn resolve_launch(args: &[String], our_dir: &Path, our_name: &str) -> Result<ResolvedLaunch, String> {
    let ini = our_dir.join("redive.ini");
    let cfg = resolve::read_main_ini(&ini).unwrap_or_default();

    // 1. Explicit `.exe` argument — chain decided by the name.
    if let Some((raw, idx)) = find_target_arg(args, our_name) {
        let resolved = resolve::resolve_target_value(&raw, our_dir, &path_exists)?;
        let name = resolved
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let joined: String = args
            .iter()
            .skip(idx)
            .map(|s| {
                if s.contains(' ') && !s.starts_with('"') {
                    format!("\"{s}\"")
                } else {
                    s.clone()
                }
            })
            .collect::<Vec<String>>()
            .join(" ");
        let cmd: Vec<u16> = joined.encode_utf16().chain(std::iter::once(0)).collect();
        let chain = classify_chain(&name);
        // FastLauncher needs a game to watch for; DMM Game Player arms the launcher itself.
        let game = match chain {
            Chain::Direct => None,
            Chain::Dmm => game_from_config(&cfg, our_dir).ok().flatten(),
            Chain::FastLauncher => {
                let g = game_from_config(&cfg, our_dir)?;
                if g.is_none() {
                    return Err(
                        "FastLauncher chain needs the game to watch for — set `target = <game dir or exe>` in redive.ini [main]".to_string()
                    );
                }
                g
            }
        };
        log(&format!(
            "{chain:?} chain from argument: {}",
            resolved.display()
        ));
        return Ok((chain, resolved, game, Some(cmd)));
    }

    // 2. Explicit launcher key → launcher chain, game from `target`.
    if let Some(launcher) = &cfg.launcher {
        let launcher = resolve::resolve_target_value(launcher, our_dir, &path_exists)?;
        let chain = classify_chain(&launcher.to_string_lossy());
        log(&format!(
            "{chain:?} chain from redive.ini launcher: {}",
            launcher.display()
        ));

        let extra_args: Vec<String> = args
            .iter()
            .filter(|a| !a.starts_with("--") && a.as_str() != "launch")
            .cloned()
            .collect();

        let cmd = if !extra_args.is_empty() {
            let joined = format!("\"{}\" {}", launcher.display(), extra_args.join(" "));
            Some(joined.encode_utf16().chain(std::iter::once(0)).collect())
        } else if let Some(ini_args) = &cfg.args {
            let joined = format!("\"{}\" {}", launcher.display(), ini_args);
            Some(joined.encode_utf16().chain(std::iter::once(0)).collect())
        } else {
            None
        };

        let game = match chain {
            Chain::Direct => None,
            Chain::Dmm => game_from_config(&cfg, our_dir).ok().flatten(),
            Chain::FastLauncher => {
                let g = game_from_config(&cfg, our_dir)?;
                if g.is_none() {
                    return Err(
                        "FastLauncher chain needs the game to watch for — set `target = <game dir or exe>` in redive.ini [main]".to_string()
                    );
                }
                g
            }
        };

        return Ok((chain, launcher, game, cmd));
    }

    // 3. `dmm = true` shorthand → standard DMM Game Player locations.
    if cfg.dmm {
        const CANDIDATES: &[&str] = &[
            "C:\\Program Files\\DMMGamePlayer\\DMMGamePlayer.exe",
            "C:\\Program Files (x86)\\DMMGamePlayer\\DMMGamePlayer.exe",
            "Z:\\Program Files\\DMMGamePlayer\\DMMGamePlayer.exe",
            "Z:\\Program Files (x86)\\DMMGamePlayer\\DMMGamePlayer.exe",
        ];
        for candidate in CANDIDATES {
            let p = PathBuf::from(candidate);
            if p.is_file() {
                log(&format!("dmm=true: found {}", p.display()));
                return Ok((
                    Chain::Dmm,
                    p,
                    game_from_config(&cfg, our_dir).ok().flatten(),
                    None,
                ));
            }
        }
        return Err(
            "dmm = true, but DMMGamePlayer.exe was not found at standard locations — \
             set `launcher = <path to launcher exe>` in redive.ini"
                .to_string(),
        );
    }

    // 4. `target` alone → direct chain (directory targets are scanned).
    if let Some(target) = &cfg.target {
        let game = resolve::resolve_target_value(target, our_dir, &path_exists)?;
        let exe = if game.is_dir() {
            resolve::scan_dir_for_exe(&game, our_name)
                .ok_or_else(|| format!("no game executable found in {}", game.display()))?
        } else {
            game
        };
        log(&format!("direct chain from redive.ini: {}", exe.display()));
        return Ok((Chain::Direct, exe, None, None));
    }

    // 5. Directory scan of ReDive's own folder.
    log("no target in arguments or redive.ini; scanning ReDive's folder…");
    let found = resolve::scan_dir_for_exe(our_dir, our_name).ok_or_else(|| {
        "no target executable found (pass one, or set redive.ini [main])".to_string()
    })?;
    log(&format!("found via directory scan: {}", found.display()));
    Ok((Chain::Direct, found, None, None))
}

/// The game (dir or exe) the launcher chains watch for. Optional if the
/// launcher arms itself autonomously (e.g. official DMM Game Player).
fn game_from_config(cfg: &MainConfig, our_dir: &Path) -> Result<Option<PathBuf>, String> {
    match &cfg.target {
        Some(t) => {
            let game = resolve::resolve_target_value(t, our_dir, &path_exists)?;
            Ok(Some(game))
        }
        None => Ok(None),
    }
}

/// Lowercased exe basenames of the game: a directory yields every plausible
/// exe inside it, an exe path yields just itself.
fn watch_names(game: &Path, our_name: &str) -> Vec<String> {
    if game.is_dir() {
        resolve::exe_names_in_dir(game, our_name)
    } else {
        game.file_name()
            .map(|s| vec![s.to_string_lossy().to_ascii_lowercase()])
            .unwrap_or_default()
    }
}

pub fn run(args: &[String]) {
    let our_dir = our_dir();
    let our_name = std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.file_name()
                .map(|s| s.to_string_lossy().to_ascii_lowercase())
        })
        .unwrap_or_else(|| "redive.exe".to_string());

    let (chain, exe, game, mut cmd) = match resolve_launch(args, &our_dir, &our_name) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("launch failed: {e}");
            std::process::exit(2);
        }
    };

    if exe
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("redive.exe"))
        || exe == std::env::current_exe().unwrap_or_default()
    {
        eprintln!("launch failed: target executable cannot be redive itself");
        std::process::exit(2);
    }

    // Both DLLs must exist up front for launcher chains: the watcher needs
    // the payload even though only amulet goes into the launcher.
    let payload = match require_dll(&our_dir, PAYLOAD_DLL) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("launch failed: {e}");
            std::process::exit(1);
        }
    };
    let hide = match chain {
        Chain::Direct => None,
        _ => match require_dll(&our_dir, HIDE_DLL) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("launch failed: {e}");
                std::process::exit(1);
            }
        },
    };

    // Official DMM Game Player chain:
    // DMM Game Player is an interactive, long-lived GUI application. It is armed with
    // amulet.dll (which hooks file-verification APIs and intercepts CreateProcessW to
    // deliver prism.dll via early-bird APC autonomously when any game is clicked).
    if chain == Chain::Dmm {
        let hide_dll = hide.as_ref().unwrap();
        let exe_name = exe
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();

        // 1. If already running, arm the existing instance without spawning a redundant
        // process that would immediately close due to Electron single-instance lock.
        if let Some(existing_pid) = find_game_pid(std::slice::from_ref(&exe_name), 0) {
            log(&format!(
                "DMM Game Player is already running (pid {existing_pid}); injecting {HIDE_DLL}…"
            ));
            if let Err(e) = inject_into_pid(existing_pid, hide_dll) {
                eprintln!("failed to inject {HIDE_DLL} into running DMM Game Player: {e}");
                std::process::exit(1);
            }
            log("amulet.dll injected into running DMM Game Player; launcher armed.");
            return;
        }

        // 2. Otherwise spawn DMM Game Player detached so it does not inherit our
        // console or pollute the terminal with Electron logs.
        unsafe {
            let pi = match spawn_suspended(&exe, cmd.as_mut(), true) {
                Ok(pi) => pi,
                Err(e) => {
                    eprintln!("launch failed: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = inject_apc(&pi, hide_dll) {
                log(&format!("FATAL: injection failed: {e}"));
                let _ = TerminateProcess(pi.hProcess, 1);
                let _ = CloseHandle(pi.hProcess);
                let _ = CloseHandle(pi.hThread);
                std::process::exit(1);
            }
            log(&format!("queued {HIDE_DLL} into {}", exe.display()));
            let _ = ResumeThread(pi.hThread);
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
            log("DMM Game Player launched and armed; exiting.");
        }
        return;
    }

    // Direct and FastLauncher chains:
    unsafe {
        let detached = chain == Chain::FastLauncher;
        let pi = match spawn_suspended(&exe, cmd.as_mut(), detached) {
            Ok(pi) => pi,
            Err(e) => {
                eprintln!("launch failed: {e}");
                std::process::exit(1);
            }
        };

        // Direct: payload into the game itself. FastLauncher: hide layer into the launcher.
        let (into, what) = match (&chain, &hide) {
            (Chain::Direct, _) => (&payload, PAYLOAD_DLL),
            (_, Some(h)) => (h, HIDE_DLL),
            (_, None) => unreachable!("launcher chains always have a hide DLL"),
        };
        if let Err(e) = inject_apc(&pi, into) {
            log(&format!("FATAL: injection failed: {e}"));
            let _ = TerminateProcess(pi.hProcess, 1);
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
            std::process::exit(1);
        }
        log(&format!("queued {what} into {}", exe.display()));
        let launcher_pid = windows::Win32::System::Threading::GetProcessId(pi.hProcess);
        let _ = ResumeThread(pi.hThread);
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        log("process resumed.");

        if chain == Chain::Direct {
            return;
        }

        // FastLauncher chain: wait for the game, then deliver the payload. The
        // launcher keeps running independently — if redive is interrupted the
        // game still launches, just without plugins.
        let game = game.expect("FastLauncher chains always carry a game path");
        let names = watch_names(&game, &our_name);
        if names.is_empty() {
            eprintln!(
                "launch failed: no game executables to watch for under {}",
                game.display()
            );
            std::process::exit(2);
        }
        if let Err(e) = watch_and_inject(&names, &payload, launcher_pid) {
            eprintln!("payload delivery failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_ID: AtomicU32 = AtomicU32::new(0);

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!(
                "redive-launch-test-{}-{}",
                std::process::id(),
                TEST_ID.fetch_add(1, Ordering::SeqCst)
            ))
            .join(tag);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn chain_classification_prefers_fast_launcher() {
        // The FL name contains "gameplayer" too — fast must win.
        assert_eq!(
            classify_chain("DMMGamePlayerFastLauncher.exe"),
            Chain::FastLauncher
        );
        assert_eq!(
            classify_chain("dmmgameplayerfastlauncher.exe"),
            Chain::FastLauncher
        );
        assert_eq!(classify_chain("DMMGamePlayer.exe"), Chain::Dmm);
        assert_eq!(classify_chain("dmm.exe"), Chain::Dmm);
        assert_eq!(classify_chain("ExampleGame.exe"), Chain::Direct);
        assert_eq!(classify_chain("game.exe"), Chain::Direct);
    }

    #[test]
    fn arg_scan_skips_flags_and_self() {
        let args: Vec<String> = ["--verbose", "\"C:\\g\\game.exe\"", "extra"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            find_target_arg(&args, "redive.exe"),
            Some(("C:\\g\\game.exe".to_string(), 1))
        );

        let self_only: Vec<String> = ["redive.exe"].iter().map(|s| s.to_string()).collect();
        assert_eq!(find_target_arg(&self_only, "redive.exe"), None);

        let no_exe: Vec<String> = ["--flag", "readme.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(find_target_arg(&no_exe, "redive.exe"), None);
    }

    #[test]
    fn watch_names_from_dir_and_exe() {
        let dir = scratch("watch");
        std::fs::write(dir.join("MyGame.exe"), b"").unwrap();
        std::fs::write(dir.join("UnityCrashHandler64.exe"), b"").unwrap();
        std::fs::write(dir.join("data.pak"), b"").unwrap();

        assert_eq!(watch_names(&dir, "redive.exe"), vec!["mygame.exe"]);

        let exe = dir.join("MyGame.exe");
        assert_eq!(watch_names(&exe, "redive.exe"), vec!["mygame.exe"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_payload_aborts_resolution_preflight() {
        let dir = scratch("preflight");
        assert!(require_dll(&dir, PAYLOAD_DLL).is_err());
        std::fs::write(dir.join(PAYLOAD_DLL), b"").unwrap();
        assert_eq!(
            require_dll(&dir, PAYLOAD_DLL).unwrap(),
            dir.join(PAYLOAD_DLL)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
