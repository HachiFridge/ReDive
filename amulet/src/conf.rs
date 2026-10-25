//! `amulet.conf` — one file basename per line, `#` comment lines, written by
//! `redive unpack`. Matching is by basename (`PathStripPathW`
//! behavior), so entries work no matter which directory they live in.

use std::path::{Path, PathBuf};

pub const CONF_NAME: &str = "amulet.conf";

/// Trimmed non-empty lines with `#` comments dropped; duplicate names
/// (case-insensitive) collapse to one.
pub fn parse(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !names.iter().any(|n| n.eq_ignore_ascii_case(line)) {
            names.push(line.to_string());
        }
    }
    names
}

/// Search order: next to the amulet DLL, next to the host process, then the
/// working directory. First hit wins.
pub fn find(dll_dir: Option<&Path>, exe_dir: Option<&Path>) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(d) = dll_dir {
        candidates.push(d.join(CONF_NAME));
    }
    if let Some(d) = exe_dir {
        candidates.push(d.join(CONF_NAME));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(CONF_NAME));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Everything after the last `\` or `/`, mirroring `PathStripPathW`.
pub fn basename(path: &str) -> &str {
    match path.rfind(['\\', '/']) {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// True when the path's basename is on the hidden list.
pub fn is_hidden(names: &[String], path: &str) -> bool {
    let base = basename(path);
    names.iter().any(|n| n.eq_ignore_ascii_case(base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_blanks_and_crlf() {
        let text = "# ReDive hide manifest\r\nGameAssembly.dll\r\n\r\n  BepInEx  \r\n";
        assert_eq!(parse(text), vec!["GameAssembly.dll", "BepInEx"]);
    }

    #[test]
    fn parse_dedupes_case_insensitively() {
        let text = "gameassembly.DLL\ngameassembly.dll\n";
        assert_eq!(parse(text), vec!["gameassembly.DLL"]);
    }

    #[test]
    fn basename_handles_both_separators() {
        assert_eq!(basename(r"C:\g\GameAssembly.dll"), "GameAssembly.dll");
        assert_eq!(basename("C:/g/GameAssembly.dll"), "GameAssembly.dll");
        assert_eq!(basename("GameAssembly.dll"), "GameAssembly.dll");
        assert_eq!(basename(r"C:\g\"), "");
    }

    #[test]
    fn is_hidden_matches_basename_only() {
        let names = vec!["GameAssembly.dll".to_string()];
        assert!(is_hidden(&names, r"C:\g\GameAssembly.dll"));
        assert!(is_hidden(&names, r"C:\g\sub/GameAssembly.dll"));
        assert!(is_hidden(&names, "gameassembly.DLL"));
        // Suffixes and other files in the same directory do not match.
        assert!(!is_hidden(&names, r"C:\g\GameAssembly.dll.old"));
        assert!(!is_hidden(&names, r"C:\g\other.dll"));
    }
}
