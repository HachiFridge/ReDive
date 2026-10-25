//! Target path resolution for ReDive.
//!
//! Ported from GateJumper's injector as an independent copy: the proven
//! resolution order (first `.exe` argument → `[main]` ini → directory scan)
//! and the Linux-to-Windows path mapping for Wine/Proton prefixes
//! (Steam/Proton, Bottles, Lutris, Faugus, plain Wine).
//!
//! The pure logic lives here with the "does this path exist" check injected,
//! so every branch is unit-testable without Win32.

use std::path::{Path, PathBuf};

/// True when text is a Windows absolute path: `D:\…`, `Z:\…`, `\\server\share`.
/// The drive letter is whatever the Wine/Proton prefix actually provides.
pub fn is_windows_absolute(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.starts_with("\\\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/'))
}

/// `/…/drive_c/games/g` → `c:\games\g`; None when no `drive_<letter>` segment.
pub fn drive_mapped_candidate(lower_unix: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(p) = lower_unix[from..].find("/drive_") {
        let s = from + p;
        let rest = &lower_unix[s + 7..]; // past "/drive_"
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b'/' {
            let tail = &rest[1..];
            return Some(format!("{}:{}", b[0] as char, tail.replace('/', "\\")));
        }
        from = s + 7;
    }
    None
}

/// Classify a raw `target` value into a Windows path for CreateProcessW:
///   1. Windows-style — used as-is (slashes normalized to `\`).
///   2. Unix absolute (`/media/…`) — tried as `Z:…` (Wine's mount of `/`)
///      and via the prefix's `drive_<letter>` mapping, both verified by
///      `exists`; otherwise an actionable error instead of guessing drives.
///   3. Relative — anchored to `base_dir` (the ReDive folder).
pub fn resolve_target_value(
    raw: &str,
    base_dir: &Path,
    exists: &dyn Fn(&str) -> bool,
) -> Result<PathBuf, String> {
    let text = raw.trim().trim_matches('"');
    if text.is_empty() {
        return Err("empty target value".to_string());
    }

    if is_windows_absolute(text) {
        return Ok(PathBuf::from(text.replace('/', "\\")));
    }

    if text.starts_with('/') {
        let z_candidate = format!("Z:{}", text.replace('/', "\\"));
        if exists(&z_candidate) {
            return Ok(PathBuf::from(&z_candidate));
        }

        // Prefix managers store drives as drive_c, drive_d, … inside the
        // prefix — try the <letter>:\ view too.
        if let Some(mapped) = drive_mapped_candidate(&text.to_ascii_lowercase()) {
            if exists(&mapped) {
                return Ok(PathBuf::from(mapped));
            }
        }

        return Err(format!(
            "'{}' is not reachable as '{}' (or its prefix drive mapping) in this Wine/Proton prefix. Use a path relative to redive.ini (e.g. 'Game.exe'), a Windows path for this prefix (e.g. from 'winepath -w'), or check the prefix's drive mappings.",
            text, z_candidate
        ));
    }

    // Relative: anchor to the config's own directory.
    let joined = base_dir.join(text);
    Ok(PathBuf::from(joined.to_string_lossy().replace('/', "\\")))
}

/// Strip a trailing ` # ...` / ` ; ...` comment from a value line. A comment
/// marker only counts when preceded by whitespace so paths containing `#`
/// or `;` are left intact.
pub fn strip_trailing_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if (b == b'#' || b == b';') && i > 0 && bytes[i - 1].is_ascii_whitespace() {
            return line[..i].trim_end();
        }
    }
    line
}

/// Keys from the `[main]` section of `redive.ini`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MainConfig {
    /// The game — a directory or its exe. Always the *game*: launchers are
    /// named separately, so `target` means the same thing for `unpack` and
    /// `launch`.
    pub target: Option<String>,
    /// `dmm = true` shorthand — route through DMM Game Player.
    pub dmm: bool,
    /// Explicit launcher exe (e.g. a community fast launcher). Implies a
    /// launcher chain; classified by file name.
    pub launcher: Option<String>,
    /// Arguments to pass to the launcher or game process.
    pub args: Option<String>,
    /// Explicit plugin loading toggle (`plugins = true/false`).
    pub plugins: Option<bool>,
    /// Custom plugins directory.
    pub plugins_dir: Option<String>,
    /// Path to senbei unpacker executable.
    pub senbei: Option<String>,
}

/// Read `target` / `dmm` / `launcher` / `plugins` / `senbei` from `[main]`.
/// Files without `[section]` headers are treated as `[main]`; per-game sections
/// are ignored. None when `[main]` holds no recognized key.
pub fn read_main_ini(ini_path: &Path) -> Option<MainConfig> {
    let content = std::fs::read_to_string(ini_path).ok()?;
    let mut section = String::from("main");
    let mut config = MainConfig::default();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            if let Some(rest) = line.strip_prefix('[') {
                if let Some(name) = rest.strip_suffix(']') {
                    section = name.trim().to_string();
                }
            }
            continue;
        }
        if !section.eq_ignore_ascii_case("main") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("target") {
            if let Some(val) = rest.trim().strip_prefix('=') {
                let val = strip_trailing_comment(val.trim()).trim().trim_matches('"');
                if !val.is_empty() {
                    config.target = Some(val.to_string());
                }
            }
        }
        if let Some(rest) = line.strip_prefix("launcher") {
            if let Some(val) = rest.trim().strip_prefix('=') {
                let val = strip_trailing_comment(val.trim()).trim().trim_matches('"');
                if !val.is_empty() {
                    config.launcher = Some(val.to_string());
                }
            }
        }
        if let Some(rest) = line.strip_prefix("args") {
            if let Some(val) = rest.trim().strip_prefix('=') {
                let val = strip_trailing_comment(val.trim()).trim().trim_matches('"');
                if !val.is_empty() {
                    config.args = Some(val.to_string());
                }
            }
        }
        if let Some(rest) = line.strip_prefix("dmm") {
            if let Some(val) = rest.trim().strip_prefix('=') {
                let val = val.trim().to_ascii_lowercase();
                if val == "true" || val == "1" || val == "yes" {
                    config.dmm = true;
                }
            }
        }
        if let Some(rest) = line.strip_prefix("plugins_dir") {
            if let Some(val) = rest.trim().strip_prefix('=') {
                let val = strip_trailing_comment(val.trim()).trim().trim_matches('"');
                if !val.is_empty() {
                    config.plugins_dir = Some(val.to_string());
                }
            }
        } else if let Some(rest) = line.strip_prefix("plugins") {
            if let Some(val) = rest.trim().strip_prefix('=') {
                let val = val.trim().to_ascii_lowercase();
                if val == "true" || val == "1" || val == "yes" {
                    config.plugins = Some(true);
                } else if val == "false" || val == "0" || val == "no" {
                    config.plugins = Some(false);
                }
            }
        }
        if let Some(rest) = line.strip_prefix("senbei") {
            if let Some(val) = rest.trim().strip_prefix('=') {
                let val = strip_trailing_comment(val.trim()).trim().trim_matches('"');
                if !val.is_empty() {
                    config.senbei = Some(val.to_string());
                }
            }
        }
    }
    (config != MainConfig::default()).then_some(config)
}

/// Executables never treated as the game: ourselves, crash handlers,
/// uninstallers. `our_name` must already be lowercased.
pub fn is_boring_exe(name_lower: &str, our_name: &str) -> bool {
    name_lower == our_name
        || name_lower == "redive.exe"
        || name_lower == "start.exe"
        || name_lower == "unitycrashhandler64.exe"
        || name_lower.contains("uninstall")
        || name_lower.contains("installer")
}

/// Scan a directory for the first plausible game executable, skipping our
/// own binary and known-boring ones (crash handlers, uninstallers).
pub fn scan_dir_for_exe(dir: &Path, our_name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
            != Some("exe")
        {
            continue;
        }
        let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
        if is_boring_exe(&name, our_name) {
            continue;
        }
        return Some(path);
    }
    None
}

/// Every game-looking exe name in a directory, lowercased — the launcher
/// chains watch the process table against this set to find the game.
pub fn exe_names_in_dir(dir: &Path, our_name: &str) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return names;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir()
            || path
                .extension()
                .and_then(|s| s.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref()
                != Some("exe")
        {
            continue;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        if !is_boring_exe(&name, our_name) {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_ID: AtomicU32 = AtomicU32::new(0);

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "redive-resolve-test-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::SeqCst)
        ));
        let dir = dir.join(tag);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn drive_mapping_from_prefix_layouts() {
        // Steam/Proton, Bottles, Lutris, Faugus and plain Wine prefixes all
        // store drives as drive_c directories inside the prefix.
        assert_eq!(
            drive_mapped_candidate("/home/u/.steam/steam/compatdata/123/pfx/drive_c/games/g/g.exe"),
            Some("c:\\games\\g\\g.exe".to_string())
        );
        assert_eq!(
            drive_mapped_candidate("/home/u/games/drive_d/mods/x.dll"),
            Some("d:\\mods\\x.dll".to_string())
        );
        // No drive segment, or a segment that is not exactly drive_<letter>/.
        assert_eq!(drive_mapped_candidate("/media/data/games/g.exe"), None);
        assert_eq!(drive_mapped_candidate("/x/drive_cd/games/g.exe"), None);
        assert_eq!(drive_mapped_candidate("/x/drive_c"), None);
        // Tries further occurrences when the first is malformed.
        assert_eq!(
            drive_mapped_candidate("/x/drive_1/y/drive_c/z/g.exe"),
            Some("c:\\z\\g.exe".to_string())
        );
    }

    #[test]
    fn windows_absolute_forms() {
        assert!(is_windows_absolute("C:\\Games\\g.exe"));
        assert!(is_windows_absolute("Z:/Games/g.exe"));
        assert!(is_windows_absolute("\\\\server\\share\\g.exe"));
        assert!(!is_windows_absolute("/unix/abs/g.exe"));
        assert!(!is_windows_absolute("relative\\g.exe"));
        assert!(!is_windows_absolute("C:g.exe")); // drive-relative, not absolute
        assert!(!is_windows_absolute("C:"));
    }

    #[test]
    fn resolve_windows_absolute_normalizes_slashes() {
        let base = Path::new("E:\\ReDive");
        let out = resolve_target_value("E:/Games/g.exe", base, &|_| false).unwrap();
        assert_eq!(out, PathBuf::from("E:\\Games\\g.exe"));
    }

    #[test]
    fn resolve_unix_absolute_prefers_z_drive() {
        let base = Path::new("E:\\ReDive");
        let out = resolve_target_value("/mnt/games/g.exe", base, &|p| p == "Z:\\mnt\\games\\g.exe")
            .unwrap();
        assert_eq!(out, PathBuf::from("Z:\\mnt\\games\\g.exe"));
    }

    #[test]
    fn resolve_unix_absolute_falls_back_to_drive_mapping() {
        let base = Path::new("E:\\ReDive");
        let out = resolve_target_value("/prefix/drive_d/games/g.exe", base, &|p| {
            p == "d:\\games\\g.exe"
        })
        .unwrap();
        assert_eq!(out, PathBuf::from("d:\\games\\g.exe"));
    }

    #[test]
    fn resolve_unix_absolute_errors_when_unreachable() {
        let base = Path::new("E:\\ReDive");
        let err = resolve_target_value("/nope/g.exe", base, &|_| false).unwrap_err();
        assert!(err.contains("Z:\\nope\\g.exe"), "got: {}", err);
        assert!(err.contains("redive.ini"), "got: {}", err);
    }

    #[test]
    fn resolve_relative_anchors_to_base_dir() {
        let base = Path::new("E:\\ReDive");
        let out = resolve_target_value("Game/g.exe", base, &|_| false).unwrap();
        assert_eq!(out, PathBuf::from("E:\\ReDive\\Game\\g.exe"));
    }

    #[test]
    fn resolve_empty_value_errors() {
        let base = Path::new("E:\\ReDive");
        assert!(resolve_target_value("  \"\"  ", base, &|_| false).is_err());
    }

    #[test]
    fn trailing_comment_only_after_whitespace() {
        assert_eq!(
            strip_trailing_comment("C:\\Games\\g.exe  # note"),
            "C:\\Games\\g.exe"
        );
        assert_eq!(
            strip_trailing_comment("C:\\Games\\g.exe ; note"),
            "C:\\Games\\g.exe"
        );
        // `#` inside a path (no preceding whitespace) is part of the value.
        assert_eq!(
            strip_trailing_comment("C:\\Games\\a#b\\g.exe"),
            "C:\\Games\\a#b\\g.exe"
        );
        assert_eq!(strip_trailing_comment("plain"), "plain");
    }

    #[test]
    fn main_ini_parsing_and_section_isolation() {
        let dir = scratch_dir("ini");
        let ini = dir.join("redive.ini");
        std::fs::write(
            &ini,
            "# comment line\n\
             [main]\n\
             target = \"E:\\Games\\MyGame\"\n\
             dmm = yes\n\
             launcher = E:\\Tools\\FastLauncher\\FastLauncher.exe\n\
             plugins = true\n\
             plugins_dir = \"E:\\Mods\\Plugins\"\n\
             senbei = E:\\Tools\\senbei.exe\n\
             \n\
             [OtherGame.exe]\n\
             dir = E:\\Games\\Other\n\
             target = ignored.exe\n",
        )
        .unwrap();

        let cfg = read_main_ini(&ini).unwrap();
        assert_eq!(cfg.target.as_deref(), Some("E:\\Games\\MyGame"));
        assert!(cfg.dmm);
        assert_eq!(
            cfg.launcher.as_deref(),
            Some("E:\\Tools\\FastLauncher\\FastLauncher.exe")
        );
        assert_eq!(cfg.plugins, Some(true));
        assert_eq!(cfg.plugins_dir.as_deref(), Some("E:\\Mods\\Plugins"));
        assert_eq!(cfg.senbei.as_deref(), Some("E:\\Tools\\senbei.exe"));
        // Keys inside a per-game section must not leak into [main].
        assert_ne!(cfg.target.as_deref(), Some("ignored.exe"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn main_ini_absent_or_sectionless() {
        let dir = scratch_dir("ini2");
        let missing = dir.join("redive.ini");
        assert!(read_main_ini(&missing).is_none());

        // No section header → treated as [main].
        let ini = dir.join("bare.ini");
        std::fs::write(&ini, "dmm = true\n").unwrap();
        let cfg = read_main_ini(&ini).unwrap();
        assert!(cfg.target.is_none());
        assert!(cfg.launcher.is_none());
        assert!(cfg.dmm);

        // Neither a key nor a section → nothing to launch from.
        let none = dir.join("none.ini");
        std::fs::write(&none, "# just a comment\n").unwrap();
        assert!(read_main_ini(&none).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exe_names_skip_boring_and_directories() {
        let dir = scratch_dir("names");
        for name in [
            "MyGame.exe",
            "UnityCrashHandler64.exe",
            "GameUninstaller.exe",
            "redive.exe",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        std::fs::create_dir_all(dir.join("sub.exe")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"").unwrap();

        assert_eq!(exe_names_in_dir(&dir, "redive.exe"), vec!["mygame.exe"]);
        // A directory named like an exe is not a process.
        assert!(!exe_names_in_dir(&dir, "redive.exe")
            .iter()
            .any(|n| n == "sub.exe"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_skips_boring_executables() {
        let dir = scratch_dir("scan");
        for name in [
            "redive.exe",
            "UnityCrashHandler64.exe",
            "GameUninstaller.exe",
            "SetupInstaller.exe",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        // Only .exe with a non-boring name counts; other extensions ignored.
        std::fs::write(dir.join("notes.txt"), b"").unwrap();

        assert!(scan_dir_for_exe(&dir, "redive.exe").is_none());

        std::fs::write(dir.join("Game.exe"), b"").unwrap();
        let found = scan_dir_for_exe(&dir, "redive.exe").unwrap();
        assert_eq!(found.file_name().unwrap(), "Game.exe");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
