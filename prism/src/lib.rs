//! prism — injected into the (unpacked) game process to load native
//! plugins from ReDive's external `plugins/` directory. No bypass logic: the
//! unpacked game runs unprotected, so all this DLL does is find and load
//! plugins, following GateJumper's loader semantics.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, LoadLibraryW, SetDllDirectoryW};

#[allow(non_camel_case_types)]
type BOOL = i32;
#[allow(non_camel_case_types)]
type HINSTANCE = isize;

const DLL_PROCESS_ATTACH: u32 = 1;

/// Set once in DllMain — the payload's own directory (ReDive's folder).
static DLL_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();

fn log(msg: &str) {
    use std::io::Write;
    let path = match DLL_DIRECTORY.get() {
        Some(dir) => dir.join("redive.log"),
        None => PathBuf::from("redive.log"),
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[Prism] {msg}");
    }
}

/// Plugins live next to the payload (`<ReDive>/plugins`), unless the env
/// overrides it; with no known DLL directory, fall back to the CWD.
pub fn resolve_plugins_dir(
    dll_dir: Option<&Path>,
    env_override: Option<&str>,
    cwd: &Path,
) -> PathBuf {
    if let Some(d) = env_override {
        return PathBuf::from(d);
    }
    if let Some(d) = dll_dir {
        return d.join("plugins");
    }
    cwd.join("plugins")
}

/// `REDIVE_LOAD_PLUGINS`: a recognized value wins; unset or unrecognized
/// falls back to "the plugins directory exists".
pub fn auto_load(raw: Option<&str>, dir_exists: bool) -> bool {
    let Some(v) = raw else {
        return dir_exists;
    };
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => dir_exists,
    }
}

/// `REDIVE_PLUGIN_ALLOWLIST` — comma-separated file names; empty = everything.
pub fn allowlist(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// A loadable plugin file: a `.dll` that is not the payload itself (nor a
/// leftover copy under its former name) and, when an allowlist is set,
/// matches it by exact name or suffix.
pub fn is_plugin_candidate(file_name: &str, allowlist: &[String]) -> bool {
    let lower = file_name.to_ascii_lowercase();
    if !lower.ends_with(".dll")
        || lower == "prism.dll"
        || lower == "amulet.dll"
        || lower == "redive_payload.dll"
        || lower == "redive.dll"
    {
        return false;
    }
    allowlist.is_empty() || allowlist.iter().any(|e| lower == *e || lower.ends_with(e))
}

/// Recursive scan → `LoadLibraryW` per candidate. Returns the load count.
unsafe fn load_plugins_from(dir: &Path, allow: &[String]) -> usize {
    let mut loaded = 0usize;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return loaded;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            loaded += load_plugins_from(&path, allow);
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.to_ascii_lowercase().ends_with(".dll") {
            continue; // assets and logs are not our business
        }
        if !is_plugin_candidate(name, allow) {
            log(&format!(
                "skipping {}: rejected by name/allowlist",
                path.display()
            ));
            continue;
        }

        // Set DLL directory to the plugin's folder during load so sibling dependencies resolve
        if let Some(parent) = path.parent() {
            let mut p_w: Vec<u16> = parent.to_string_lossy().encode_utf16().collect();
            p_w.push(0);
            let _ = SetDllDirectoryW(PCWSTR(p_w.as_ptr()));
        }

        let mut w: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
        w.push(0);
        let result = LoadLibraryW(PCWSTR(w.as_ptr()));

        // Restore default DLL search path
        let _ = SetDllDirectoryW(PCWSTR(std::ptr::null()));

        match result {
            Ok(_) => {
                log(&format!("loaded plugin: {}", path.display()));
                loaded += 1;
            }
            Err(e) => log(&format!("FAILED to load {}: {e}", path.display())),
        }
    }
    loaded
}

fn bootstrap() {
    log("=== Prism loaded ===");
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let env_dir = std::env::var("REDIVE_PLUGINS_DIR").ok();
    let root = resolve_plugins_dir(
        DLL_DIRECTORY.get().map(PathBuf::as_path),
        env_dir.as_deref(),
        &cwd,
    );
    let gate = auto_load(
        std::env::var("REDIVE_LOAD_PLUGINS").ok().as_deref(),
        root.is_dir(),
    );
    let allow = allowlist(std::env::var("REDIVE_PLUGIN_ALLOWLIST").ok().as_deref());
    log(&format!(
        "plugins dir: {} (auto_load={gate})",
        root.display()
    ));

    if !gate {
        log("plugin loading disabled; nothing to do.");
        return;
    }
    if !root.is_dir() {
        log("'plugins' directory not found; nothing to load.");
        return;
    }
    let n = unsafe { load_plugins_from(&root, &allow) };
    log(&format!("plugin scan complete: {n} DLL(s) loaded."));
}

#[no_mangle]
pub extern "system" fn DllMain(
    module: HINSTANCE,
    reason: u32,
    _reserved: *mut std::ffi::c_void,
) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            let mut buf = [0u16; 512];
            let len = GetModuleFileNameW(Some(HMODULE(module as *mut std::ffi::c_void)), &mut buf);
            if len > 0 {
                let path = String::from_utf16_lossy(&buf[..len as usize]);
                if let Some(i) = path.rfind('\\') {
                    let _ = DLL_DIRECTORY.set(PathBuf::from(&path[..i]));
                }
            }
        }
        bootstrap();
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugins_dir_precedence() {
        let cwd = Path::new("C:\\cwd");
        assert_eq!(
            resolve_plugins_dir(Some(Path::new("C:\\ReDive")), None, cwd),
            PathBuf::from("C:\\ReDive\\plugins")
        );
        assert_eq!(
            resolve_plugins_dir(Some(Path::new("C:\\ReDive")), Some("D:\\custom"), cwd),
            PathBuf::from("D:\\custom")
        );
        assert_eq!(
            resolve_plugins_dir(None, None, cwd),
            PathBuf::from("C:\\cwd\\plugins")
        );
    }

    #[test]
    fn load_gate_recognized_values_win() {
        assert!(auto_load(Some("yes"), false));
        assert!(auto_load(Some("ON"), false));
        assert!(!auto_load(Some("0"), true));
        assert!(!auto_load(Some("False"), true));
        // Unset or unrecognized falls back to the directory's existence.
        assert!(auto_load(None, true));
        assert!(!auto_load(None, false));
        assert!(auto_load(Some("maybe"), true));
        assert!(!auto_load(Some("maybe"), false));
    }

    #[test]
    fn allowlist_parses_commas_and_trims() {
        assert!(allowlist(None).is_empty());
        assert!(allowlist(Some("  ")).is_empty());
        assert_eq!(
            allowlist(Some(" Foo.DLL , bar.dll ,,")),
            vec!["foo.dll", "bar.dll"]
        );
    }

    #[test]
    fn candidate_filtering() {
        let empty: Vec<String> = Vec::new();
        // Plain plugin: allowed by default.
        assert!(is_plugin_candidate("MyMod.dll", &empty));
        assert!(is_plugin_candidate("MYMOD.DLL", &empty));
        // Not a DLL, or the payload itself.
        assert!(!is_plugin_candidate("asset.pak", &empty));
        assert!(!is_plugin_candidate("redive.dll", &empty));
        assert!(!is_plugin_candidate("redive.DLL", &empty));
        assert!(!is_plugin_candidate("redive_payload.dll", &empty));
        assert!(!is_plugin_candidate("prism.dll", &empty));
        assert!(!is_plugin_candidate("amulet.dll", &empty));

        let allow = allowlist(Some("mymod.dll,helper.dll"));
        assert!(is_plugin_candidate("MyMod.dll", &allow));
        assert!(is_plugin_candidate("preHelper.dll", &allow)); // suffix match
        assert!(!is_plugin_candidate("Other.dll", &allow));
    }
}
