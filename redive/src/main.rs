mod launch;
mod resolve;
mod scan;
mod unpack;

use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let first = args.get(1).map(|s| s.as_str());

    match first {
        None => {
            // Bare execution (e.g. double-click): run default launch chain
            launch::run(&[]);
        }
        Some("-h") | Some("--help") | Some("help") => {
            print_help();
        }
        Some("-V") | Some("--version") | Some("version") => {
            println!("ReDive {}", env!("CARGO_PKG_VERSION"));
        }
        Some("scan") | Some("probe") | Some("check") | Some("search") => scan::run(&args[2..]),
        Some("init") => cmd_init(&args[2..]),
        Some("unpack") => cmd_unpack(&args[2..]),
        Some("restore") => cmd_restore(&args[2..]),
        Some("launch") => launch::run(&args[2..]),
        Some(cmd) => {
            // Check if first argument is an explicit executable to launch (e.g. Steam launch options `redive Game.exe`)
            let trimmed = cmd.trim_matches('"');
            let is_exe = Path::new(trimmed)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
            let is_self = Path::new(trimmed)
                .file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("redive.exe"));

            if is_exe && !is_self {
                launch::run(&args[1..]);
            } else if is_self {
                eprintln!("error: cannot launch redive itself\nRun 'redive --help' for usage.");
                std::process::exit(2);
            } else {
                eprintln!("error: unrecognized command or invalid argument '{cmd}'\nRun 'redive --help' for usage.");
                std::process::exit(2);
            }
        }
    }
}

fn print_help() {
    println!(
        "ReDive {}\n\
         Static bypass, DMM mask, and mod loader for protected Windows games.\n\n\
         Usage:\n\
         \x20 redive [launch] [game_or_launcher.exe] [args...]\n\
         \x20 redive init [game_dir] [senbei_exe] [staging_dir]\n\
         \x20 redive unpack [path_or_dir] [out_dir] [senbei_exe]\n\
         \x20 redive restore [game_dir]\n\
         \x20 redive scan|search [path_or_file]\n\n\
         Commands:\n\
         \x20 launch       Spawn the game or launcher with amulet/prism (default action)\n\
         \x20 init         Unpack game binaries, swap into place (.old backups), and initialize amulet (amulet.conf)\n\
         \x20 unpack       Statically unpack binaries with Senbei into .unpack. files (defaults to \"out\" folder)\n\
         \x20 restore      Restore original protected binaries from .old backups and remove amulet.conf\n\
         \x20 scan|search  Probe and list protected binaries, companion files, and metadata\n\n\
         Configuration is read from 'redive.ini' beside redive.exe if present.",
        env!("CARGO_PKG_VERSION")
    );
}

/// Helper to check path existence.
fn path_exists(p: &str) -> bool {
    let path = Path::new(p);
    path.exists()
}

/// Locate `senbei` unpacker: explicit arg -> redive.ini -> next to redive.exe.
fn find_senbei(
    arg: Option<&String>,
    ini_senbei: Option<&String>,
    exe_dir: &Path,
) -> Option<PathBuf> {
    if let Some(s) = arg {
        return Some(PathBuf::from(s));
    }
    if let Some(s) = ini_senbei {
        if let Ok(resolved) = resolve::resolve_target_value(s, exe_dir, &path_exists) {
            if resolved.is_file() {
                return Some(resolved);
            }
        }
    }
    let beside = exe_dir.join("senbei.exe");
    if beside.is_file() {
        return Some(beside);
    }
    None
}

/// Locate game directory: explicit arg -> redive.ini target.
fn find_game_dir(
    arg: Option<&String>,
    ini_target: Option<&String>,
    exe_dir: &Path,
) -> Option<PathBuf> {
    if let Some(g) = arg {
        let p = PathBuf::from(g);
        if p.is_file() {
            return p.parent().map(Path::to_path_buf);
        }
        return Some(p);
    }
    if let Some(t) = ini_target {
        if let Ok(resolved) = resolve::resolve_target_value(t, exe_dir, &path_exists) {
            if resolved.is_dir() {
                return Some(resolved);
            } else if resolved.is_file() {
                return resolved.parent().map(Path::to_path_buf);
            }
        }
    }
    None
}

/// `redive init [game_dir] [senbei_exe] [staging_dir]`
///
/// Scans the game tree with Senbei, swaps unpacked files into place
/// (originals kept as `X.old`), and initializes the amulet hide manifest
/// (`amulet.conf`) next to this executable.
fn cmd_init(args: &[String]) {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let ini = exe_dir.join("redive.ini");
    let cfg = resolve::read_main_ini(&ini).unwrap_or_default();

    let game = match find_game_dir(args.first(), cfg.target.as_ref(), &exe_dir) {
        Some(g) => g,
        None => {
            eprintln!(
                "usage: redive init [game_dir] [senbei_exe] [staging_dir]\n\
                 error: game directory not specified and 'target' not found in redive.ini"
            );
            std::process::exit(2);
        }
    };

    let senbei = find_senbei(args.get(1), cfg.senbei.as_ref(), &exe_dir);

    let staging = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| exe_dir.join("staging"));

    let res = if let Some(ref s) = senbei {
        println!("using external unpacker: {}", s.display());
        unpack::sync_unpack(&game, &staging, |g, st| unpack::run_senbei(s, g, st))
    } else {
        println!("using integrated Senbei unpacker");
        unpack::sync_unpack(&game, &staging, unpack::run_senbei_direct)
    };

    match res {
        Ok(results) => {
            if results.is_empty() {
                println!(
                    "nothing to unpack — no protected files found under {}",
                    game.display()
                );
            }
            for (rel, action) in &results {
                println!("{:?}  {}", action, rel);
            }

            // Derive the manifest from persistent game-tree state (`X` +
            // `X.old` pairs), not this run's output — a re-run where Senbei
            // found nothing must still protect everything already swapped.
            let names = unpack::discover_unpacked(&game);
            let entries = unpack::hide_manifest(&names);
            let manifest_path = exe_dir.join("amulet.conf");
            match unpack::write_hide_manifest(&manifest_path, &entries) {
                Ok(()) => println!(
                    "hide manifest: {} ({} names)",
                    manifest_path.display(),
                    entries.len()
                ),
                Err(e) => eprintln!("warning: could not write hide manifest: {}", e),
            }
        }
        Err(e) => {
            eprintln!("init failed: {}", e);
            std::process::exit(1);
        }
    }
}

/// `redive unpack [path_or_dir] [out_dir] [senbei_exe]`
///
/// Statically unpacks protected game binaries or files with Senbei into
/// `.unpack.` files in the target directory (defaults to "out" under the
/// working directory), without swapping or modifying game files.
fn cmd_unpack(args: &[String]) {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let ini = exe_dir.join("redive.ini");
    let cfg = resolve::read_main_ini(&ini).unwrap_or_default();

    let target = match args.first() {
        Some(s) => PathBuf::from(s),
        None => {
            if let Some(ref t) = cfg.target {
                match resolve::resolve_target_value(t, &exe_dir, &path_exists) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("error resolving target from redive.ini: {}", e);
                        std::process::exit(2);
                    }
                }
            } else {
                eprintln!(
                    "usage: redive unpack [path_or_dir] [out_dir] [senbei_exe]\n\
                     error: target path not specified and 'target' not found in redive.ini"
                );
                std::process::exit(2);
            }
        }
    };

    if !target.exists() {
        eprintln!("error: target path does not exist: {}", target.display());
        std::process::exit(2);
    }

    // Working directory where ReDive is being pointed: target itself if dir, or parent if file.
    let work_dir = if target.is_dir() {
        target.clone()
    } else {
        target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };

    // Determine out_dir and senbei_exe from arguments.
    let mut out_dir: Option<PathBuf> = None;
    let mut explicit_senbei: Option<&String> = None;

    if let Some(arg1) = args.get(1) {
        let is_senbei = arg1.to_lowercase().ends_with("senbei.exe")
            || Path::new(arg1)
                .file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("senbei.exe"));
        if is_senbei {
            explicit_senbei = Some(arg1);
        } else {
            out_dir = Some(PathBuf::from(arg1));
            explicit_senbei = args.get(2);
        }
    }

    let out_dir = out_dir.unwrap_or_else(|| work_dir.join("out"));
    let senbei = find_senbei(explicit_senbei, cfg.senbei.as_ref(), &exe_dir);
    let staging = exe_dir.join("staging");

    let res = if let Some(ref s) = senbei {
        println!("using external unpacker: {}", s.display());
        unpack::run_senbei_cli_target(s, &target, &staging)
    } else {
        println!("using integrated Senbei unpacker");
        unpack::run_senbei_direct_target(&target, &staging)
    };

    if let Err(e) = res {
        eprintln!("unpack failed: {}", e);
        std::process::exit(1);
    }

    match unpack::move_unpacked_flat(&staging, &out_dir) {
        Ok(unpacked) => {
            let _ = std::fs::remove_dir_all(&staging);
            if unpacked.is_empty() {
                println!(
                    "nothing to unpack — no protected files found under {}",
                    target.display()
                );
            } else {
                println!(
                    "\nUnpacked {} file(s) -> {}",
                    unpacked.len(),
                    out_dir.display()
                );
                for (name, _) in &unpacked {
                    println!("  • {}", name);
                }
            }
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            eprintln!("failed to move unpacked files: {}", e);
            std::process::exit(1);
        }
    }
}

/// `redive restore [game_dir]`
///
/// Restores original protected binaries from their `.old` backups across the
/// game tree, removes the unpacked files, and removes `amulet.conf`.
fn cmd_restore(args: &[String]) {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let ini = exe_dir.join("redive.ini");
    let cfg = resolve::read_main_ini(&ini).unwrap_or_default();

    let game = match find_game_dir(args.first(), cfg.target.as_ref(), &exe_dir) {
        Some(g) => g,
        None => {
            eprintln!(
                "usage: redive restore [game_dir]\n\
                 error: game directory not specified and 'target' not found in redive.ini"
            );
            std::process::exit(2);
        }
    };

    println!("restoring originals under {}", game.display());
    match unpack::restore_game_tree(&game) {
        Ok(restored) => {
            if restored.is_empty() {
                println!(
                    "nothing to restore — no .old backup files found under {}",
                    game.display()
                );
            } else {
                for (_old_path, target_path) in &restored {
                    let rel = target_path
                        .strip_prefix(&game)
                        .unwrap_or(target_path)
                        .to_string_lossy();
                    println!("restored: {}", rel);
                }
                println!(
                    "successfully restored {} file(s) to original state.",
                    restored.len()
                );
            }

            // Remove or clean up amulet.conf
            let manifest_path = exe_dir.join("amulet.conf");
            if manifest_path.exists() {
                if let Err(e) = std::fs::remove_file(&manifest_path) {
                    eprintln!("warning: could not remove amulet.conf: {}", e);
                } else {
                    println!("removed hide manifest: {}", manifest_path.display());
                }
            }
        }
        Err(e) => {
            eprintln!("restore failed: {}", e);
            std::process::exit(1);
        }
    }
}
