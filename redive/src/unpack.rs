//! Unpack stage: run Senbei over the game tree, swap unpacked outputs into
//! place, and derive the hide manifest for the DMM launcher.
//!
//! Layout contract (matches the team's proven manual workflow):
//!   - `X` on disk     → the unpacked, runnable file (what executes)
//!   - `X.old` on disk → the untouched original (what DMM's verify pass
//!     must see; amulet redirects its reads here)
//!
//! The stage is self-describing: no state file. "Already unpacked" is
//! detected by hashing (`X` == Senbei's output), and "DMM updated the game"
//! is detected by `X` differing from the output while `X.old` exists —
//! which triggers a rotation (`X` → `X.old`, fresh output → `X`).

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Extra files the hide layer must protect even though Senbei never touched
/// them: the mod-loader boot chain and the optional metadata file.
pub const DEFAULT_SEEDS: &[&str] = &[
    // Optional IL2CPP metadata dump. Not required for ReDive to work (the
    // game runs without it), but when a user installs one, DMM must not
    // nuke it.
    "global-metadata.dat",
    // Unity Doorstop boot chain (either proxy variant may be installed).
    "dxgi.dll",
    "winhttp.dll",
    "doorstop_config.ini",
    ".doorstop_version",
    "changelog.txt",
    "BepInEx",
    "dotnet",
];

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// No existing target; unpacked file placed.
    Installed,
    /// Target existed as a (re-packed/updated) original; rotated to `.old`.
    Updated,
    /// Target already is the unpacked file; nothing touched.
    UpToDate,
}

/// SHA-256 of a file's contents, hex-encoded.
pub fn file_sha(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Rename, with copy+remove fallback for cross-volume moves (staging may
/// live on a different drive than the game).
fn move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(src, dst)?;
            std::fs::remove_file(src)
        }
    }
}

/// Run Senbei directly in-process over the target path (file or folder) into `staging`.
pub fn run_senbei_direct_target(target: &Path, staging: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging).map_err(|e| format!("cannot create staging: {}", e))?;

    if target.is_dir() {
        senbei_io::job::run_folder_opts(target, Some(staging), 0, false, true, false)
            .map_err(|e| format!("senbei unpack error: {:#}", e))?;
    } else if target.is_file() {
        senbei_io::job::run_file_v(target, Some(staging), 0, false, true)
            .map_err(|e| format!("senbei unpack error: {:#}", e))?;
    } else {
        return Err(format!("target does not exist: {}", target.display()));
    }
    Ok(())
}

/// Run Senbei directly in-process over the whole game tree into `staging`.
/// Folder scan only opens `.exe`, `.dll` and `global-metadata.dat` candidates,
/// so bulk assets are never read.
pub fn run_senbei_direct(game_dir: &Path, staging: &Path) -> Result<(), String> {
    run_senbei_direct_target(game_dir, staging)
}

/// Run an external Senbei binary on the target path into `staging`.
pub fn run_senbei_cli_target(senbei: &Path, target: &Path, staging: &Path) -> Result<(), String> {
    if !senbei.is_file() {
        return Err(format!("senbei binary not found: {}", senbei.display()));
    }
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging).map_err(|e| format!("cannot create staging: {}", e))?;

    let status = std::process::Command::new(senbei)
        .arg(target)
        .arg("--out")
        .arg(staging)
        .arg("--no-pause")
        .status()
        .map_err(|e| format!("failed to run senbei: {}", e))?;
    if !status.success() {
        return Err(format!("senbei exited with {}", status));
    }
    Ok(())
}

/// Run an external Senbei binary in folder-scan mode over the whole game tree into `staging`.
pub fn run_senbei(senbei: &Path, game_dir: &Path, staging: &Path) -> Result<(), String> {
    run_senbei_cli_target(senbei, game_dir, staging)
}

/// Recursively collect `X.unpack.<ext>` outputs in staging, paired with the
/// target path they belong to inside `game_dir` (`.unpack` removed, tree
/// structure preserved).
fn collect_unpack_outputs(staging: &Path, game_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut found = Vec::new();
    let mut stack = vec![staging.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.contains(".unpack.") {
                continue;
            }
            let target_name = name.replace(".unpack.", ".");
            let rel_parent = path
                .parent()
                .unwrap_or(staging)
                .strip_prefix(staging)
                .unwrap_or(Path::new(""));
            let target = game_dir.join(rel_parent).join(target_name);
            found.push((path, target));
        }
    }
    found.sort_by(|a, b| a.1.cmp(&b.1));
    found
}

/// Swap every staging output into the game tree per the layout contract.
/// Returns `(relative target, action)` for each output, in path order.
pub fn apply_unpack(game_dir: &Path, staging: &Path) -> Result<Vec<(String, Action)>, String> {
    let mut results = Vec::new();
    for (unpacked, target) in collect_unpack_outputs(staging, game_dir) {
        let rel = target
            .strip_prefix(game_dir)
            .unwrap_or(&target)
            .to_string_lossy()
            .replace('/', "\\");

        let unpacked_sha =
            file_sha(&unpacked).map_err(|e| format!("{}: {}", unpacked.display(), e))?;

        if !target.exists() {
            move_file(&unpacked, &target)
                .map_err(|e| format!("cannot install {}: {}", target.display(), e))?;
            results.push((rel, Action::Installed));
            continue;
        }

        let target_sha = file_sha(&target).map_err(|e| format!("{}: {}", target.display(), e))?;
        if target_sha == unpacked_sha {
            results.push((rel, Action::UpToDate));
            continue;
        }

        // Target differs from the unpacked output: either first swap
        // (target is the protected original) or DMM re-packed/updated it.
        let old = target.with_extension({
            let ext = target.extension().map(|e| e.to_string_lossy().to_string());
            match ext {
                Some(e) => format!("{}.old", e),
                None => "old".to_string(),
            }
        });
        let was_stale = old.exists();
        if was_stale {
            // Rotation: the previous `.old` is stale — the current target is
            // now the authoritative original for DMM's verify pass.
            std::fs::remove_file(&old)
                .map_err(|e| format!("cannot remove stale {}: {}", old.display(), e))?;
        }
        std::fs::rename(&target, &old)
            .map_err(|e| format!("cannot move {} aside: {}", target.display(), e))?;
        move_file(&unpacked, &target)
            .map_err(|e| format!("cannot install {}: {}", target.display(), e))?;
        results.push((
            rel,
            if was_stale {
                Action::Updated
            } else {
                Action::Installed
            },
        ));
    }
    Ok(results)
}

/// Move all `.unpack.` files from `staging` flat into `out_dir`, regardless of
/// what subfolder they were in within `staging`.
/// Returns list of `(source_file_name, dest_path)`.
pub fn move_unpacked_flat(
    staging: &Path,
    out_dir: &Path,
) -> Result<Vec<(String, PathBuf)>, String> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("cannot create output dir {}: {}", out_dir.display(), e))?;

    let mut moved = Vec::new();
    let mut stack = vec![staging.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.contains(".unpack.") {
                continue;
            }
            let dest = out_dir.join(name);
            move_file(&path, &dest).map_err(|e| {
                format!(
                    "cannot move {} to {}: {}",
                    path.display(),
                    dest.display(),
                    e
                )
            })?;
            moved.push((name.to_string(), dest));
        }
    }

    moved.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(moved)
}

/// Basenames of everything the unpack stage has swapped, discovered from the
/// game tree itself: any `X.old` sibling proves its pair `X` is the unpacked
/// file. Discovering state (instead of reporting this run's results) keeps
/// the manifest stable across re-runs where Senbei finds nothing new.
pub fn discover_unpacked(game_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut stack = vec![game_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(base) = name.strip_suffix(".old") {
                if !base.is_empty() {
                    names.push(base.to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Basenames for the hide manifest: every unpacked target (amulet matches
/// by basename, mirroring `PathStripPathW`) plus the default seeds.
pub fn hide_manifest(unpacked_basenames: &[String]) -> Vec<String> {
    let mut names: Vec<String> = unpacked_basenames.to_vec();
    for seed in DEFAULT_SEEDS {
        let s = (*seed).to_string();
        if !names.contains(&s) {
            names.push(s);
        }
    }
    names
}

/// One name per line, `#` comments allowed — the format amulet.conf reads.
pub fn write_hide_manifest(path: &Path, names: &[String]) -> std::io::Result<()> {
    let mut out =
        String::from("# ReDive hide manifest — files DMM Game Player must not rewrite.\n");
    for name in names {
        out.push_str(name);
        out.push('\n');
    }
    std::fs::write(path, out)
}

/// Restore all `.old` originals across the game tree back to their active locations,
/// removing the unpacked copies. Returns list of `(old_path, restored_target)`.
pub fn restore_game_tree(game_dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut restored = Vec::new();
    let mut stack = vec![game_dir.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("cannot read directory {}: {}", dir.display(), e))?;

        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }

            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if let Some(orig_name) = name.strip_suffix(".old") {
                if orig_name.is_empty() {
                    continue;
                }
                let target = path.with_file_name(orig_name);
                if target.exists() {
                    std::fs::remove_file(&target).map_err(|e| {
                        format!("cannot remove unpacked file {}: {}", target.display(), e)
                    })?;
                }
                move_file(&path, &target).map_err(|e| {
                    format!(
                        "cannot restore {} to {}: {}",
                        path.display(),
                        target.display(),
                        e
                    )
                })?;
                restored.push((path, target));
            }
        }
    }

    restored.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(restored)
}

/// Full stage: clean staging → Senbei → swap → manifest entries.
/// The runner is injected so tests exercise the flow without Senbei.
pub fn sync_unpack<F>(
    game_dir: &Path,
    staging: &Path,
    runner: F,
) -> Result<Vec<(String, Action)>, String>
where
    F: FnOnce(&Path, &Path) -> Result<(), String>,
{
    runner(game_dir, staging)?;
    let res = apply_unpack(game_dir, staging);
    if res.is_ok() && staging.exists() {
        let _ = std::fs::remove_dir_all(staging);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_ID: AtomicU32 = AtomicU32::new(0);

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "redive-unpack-test-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::SeqCst)
        ));
        let dir = dir.join(tag);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn put(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn installs_and_keeps_original_as_old() {
        let g = scratch("install");
        put(&g.join("GameAssembly.dll"), "packed-v1");
        put(&g.join("staging/GameAssembly.unpack.dll"), "unpacked-v1");

        let results = apply_unpack(&g, &g.join("staging")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, Action::Installed);
        assert_eq!(read(&g.join("GameAssembly.dll")), "unpacked-v1");
        assert_eq!(read(&g.join("GameAssembly.dll.old")), "packed-v1");

        let _ = std::fs::remove_dir_all(&g);
    }

    #[test]
    fn second_run_is_up_to_date() {
        let g = scratch("idempotent");
        put(&g.join("GameAssembly.dll"), "packed-v1");
        put(&g.join("staging/GameAssembly.unpack.dll"), "unpacked-v1");
        apply_unpack(&g, &g.join("staging")).unwrap();

        // Real second run: Senbei skips the already-unpacked target, so
        // staging is empty — nothing to do, `.old` untouched.
        let results = apply_unpack(&g, &g.join("staging")).unwrap();
        assert!(results.is_empty());
        assert_eq!(read(&g.join("GameAssembly.dll")), "unpacked-v1");
        assert_eq!(read(&g.join("GameAssembly.dll.old")), "packed-v1");

        // Defensive branch: if a staging copy does match the target hash,
        // it is reported UpToDate and nothing is moved.
        put(&g.join("staging/GameAssembly.unpack.dll"), "unpacked-v1");
        let results = apply_unpack(&g, &g.join("staging")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, Action::UpToDate);
        assert_eq!(read(&g.join("GameAssembly.dll")), "unpacked-v1");
        assert_eq!(read(&g.join("GameAssembly.dll.old")), "packed-v1");

        let _ = std::fs::remove_dir_all(&g);
    }

    #[test]
    fn dmm_update_rotates_old_and_reinstalls() {
        let g = scratch("rotate");
        put(&g.join("GameAssembly.dll"), "packed-v1");
        put(&g.join("staging/GameAssembly.unpack.dll"), "unpacked-v1");
        apply_unpack(&g, &g.join("staging")).unwrap();

        // DMM pushes an update: new packed target, fresh Senbei output.
        put(&g.join("GameAssembly.dll"), "packed-v2");
        put(&g.join("staging/GameAssembly.unpack.dll"), "unpacked-v2");
        let results = apply_unpack(&g, &g.join("staging")).unwrap();
        assert_eq!(results[0].1, Action::Updated);
        assert_eq!(read(&g.join("GameAssembly.dll")), "unpacked-v2");
        // `.old` is now the *current* original so DMM's verify still matches.
        assert_eq!(read(&g.join("GameAssembly.dll.old")), "packed-v2");

        let _ = std::fs::remove_dir_all(&g);
    }

    #[test]
    fn missing_target_installs_without_old() {
        let g = scratch("fresh");
        put(&g.join("staging/dropped.dll"), "data");
        // Non-unpack files in staging are ignored.
        put(&g.join("staging/senbei.log"), "log");

        let results = apply_unpack(&g, &g.join("staging")).unwrap();
        assert!(results.is_empty());
        assert!(!g.join("dropped.dll").exists());

        let _ = std::fs::remove_dir_all(&g);
    }

    #[test]
    fn subdirectory_structure_is_preserved() {
        let g = scratch("subdir");
        put(&g.join("x86_64/coneshell.dll"), "packed-coneshell");
        put(
            &g.join("staging/x86_64/coneshell.unpack.dll"),
            "unpacked-coneshell",
        );

        let results = apply_unpack(&g, &g.join("staging")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, Action::Installed);
        // Target mirrors the staging tree relative to the scan root.
        assert_eq!(read(&g.join("x86_64/coneshell.dll")), "unpacked-coneshell");
        assert_eq!(
            read(&g.join("x86_64/coneshell.dll.old")),
            "packed-coneshell"
        );

        let _ = std::fs::remove_dir_all(&g);
    }

    #[test]
    fn discovery_finds_swapped_pairs_recursively() {
        let g = scratch("discover");
        put(&g.join("GameAssembly.dll"), "unpacked");
        put(&g.join("GameAssembly.dll.old"), "packed");
        put(&g.join("x86_64/coneshell.dll"), "unpacked");
        put(&g.join("x86_64/coneshell.dll.old"), "packed");
        // Unrelated `.old`-suffixed and plain files are ignored / mapped.
        put(&g.join("notes.txt"), "n");
        put(&g.join("UnityPlayer.dll"), "clean");

        let mut names = discover_unpacked(&g);
        names.sort();
        assert_eq!(names, vec!["GameAssembly.dll", "coneshell.dll"]);

        let _ = std::fs::remove_dir_all(&g);
    }

    #[test]
    fn manifest_combines_unpacks_and_seeds_without_duplicates() {
        let names = hide_manifest(&["GameAssembly.dll".into(), "dxgi.dll".into()]);
        assert!(names.contains(&"GameAssembly.dll".to_string()));
        assert!(names.contains(&"BepInEx".to_string()));
        // Stays protected even though it is not required for ReDive to work.
        assert!(names.contains(&"global-metadata.dat".to_string()));
        // Seed already present from unpacks isn't duplicated.
        assert_eq!(names.iter().filter(|n| *n == "dxgi.dll").count(), 1);
    }

    #[test]
    fn manifest_writes_line_per_name() {
        let g = scratch("manifest");
        let path = g.join("amulet.conf");
        write_hide_manifest(&path, &["a.dll".into(), "BepInEx".into()]).unwrap();
        let text = read(&path);
        assert!(text.starts_with('#'));
        assert!(text.contains("a.dll\n"));
        assert!(text.contains("BepInEx\n"));
        let _ = std::fs::remove_dir_all(&g);
    }

    #[test]
    fn sync_runs_runner_before_apply() {
        let g = scratch("sync");
        put(&g.join("GameAssembly.dll"), "packed");

        let ran = std::rc::Rc::new(std::cell::Cell::new(false));
        let ran2 = ran.clone();
        let expected_game = g.clone();
        let staging = g.join("staging");
        let results = sync_unpack(&g, &staging, move |game, staging| {
            ran2.set(true);
            assert_eq!(game, expected_game.as_path());
            put(&staging.join("GameAssembly.unpack.dll"), "unpacked");
            Ok(())
        })
        .unwrap();
        assert!(ran.get());
        assert_eq!(results[0].1, Action::Installed);

        let _ = std::fs::remove_dir_all(&g);
    }

    #[test]
    fn restore_moves_old_back_to_target() {
        let g = scratch("restore");
        put(&g.join("GameAssembly.dll"), "unpacked-content");
        put(&g.join("GameAssembly.dll.old"), "original-content");
        put(&g.join("sub/Plugin.dll"), "unpacked-plugin");
        put(&g.join("sub/Plugin.dll.old"), "original-plugin");

        let restored = restore_game_tree(&g).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(read(&g.join("GameAssembly.dll")), "original-content");
        assert!(!g.join("GameAssembly.dll.old").exists());
        assert_eq!(read(&g.join("sub/Plugin.dll")), "original-plugin");
        assert!(!g.join("sub/Plugin.dll.old").exists());

        let _ = std::fs::remove_dir_all(&g);
    }

    #[test]
    fn move_unpacked_flat_moves_nested_unpacked_to_flat_out() {
        let g = scratch("flat_unpack");
        let staging = g.join("staging");
        let out = g.join("out");

        put(&staging.join("GameAssembly.unpack.dll"), "unpacked-ga");
        put(
            &staging.join("nested/dir/plugin.unpack.dll"),
            "unpacked-plugin",
        );
        put(&staging.join("nested/other.txt"), "ignored-text");

        let moved = move_unpacked_flat(&staging, &out).unwrap();
        assert_eq!(moved.len(), 2);
        assert_eq!(read(&out.join("GameAssembly.unpack.dll")), "unpacked-ga");
        assert_eq!(read(&out.join("plugin.unpack.dll")), "unpacked-plugin");
        assert!(!out.join("other.txt").exists());

        let _ = std::fs::remove_dir_all(&g);
    }
}
