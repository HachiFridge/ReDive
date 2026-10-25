use std::io::Read;
use std::path::{Path, PathBuf};

const DETECT_PREFIX: u64 = 8 * 1024;

/// Run content probe and print findings for a single target file.
fn probe_file(path: &Path) {
    println!("[*] Probing file: {}", path.display());

    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[-] Error accessing file: {}", e);
            return;
        }
    };

    let file_len = metadata.len();
    println!(
        "    Size: {} bytes ({:.2} MB)",
        file_len,
        file_len as f64 / (1024.0 * 1024.0)
    );

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[-] Error opening file: {}", e);
            return;
        }
    };

    let mut buf = Vec::with_capacity(DETECT_PREFIX as usize);
    if let Err(e) = file.by_ref().take(DETECT_PREFIX).read_to_end(&mut buf) {
        eprintln!("[-] Error reading file header: {}", e);
        return;
    }

    // 1. Check if PE binary is protected
    if let Some(detected) = senbei_engine::detect(&buf) {
        println!("[+] Protected PE Binary Detected!");
        println!("    Classification : {:?}", detected.kind);
        println!("    Header Magic   : 0x{:08X}", detected.magic);

        // Check companion payload (e.g. Game.exe._)
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        let companion = path.with_file_name(format!("{}._", file_name));
        if companion.is_file() {
            if let Ok(c_meta) = std::fs::metadata(&companion) {
                println!(
                    "    Companion      : {} ({} bytes)",
                    companion.file_name().unwrap_or_default().to_string_lossy(),
                    c_meta.len()
                );
            }
        }
        return;
    }

    // 2. Check if global-metadata.dat
    if senbei_metadata::is_metadata(&buf) {
        println!("[+] Obfuscated IL2CPP Metadata Detected!");
        return;
    }

    // 3. Check if companion payload directly
    let name_str = path.file_name().unwrap_or_default().to_string_lossy();
    if let Some(base_name) = name_str.strip_suffix("._") {
        let base_path = path.with_file_name(base_name);
        if base_path.is_file() {
            println!("[+] Auxiliary Companion Payload Detected!");
            println!("    Associated Target: {}", base_path.display());
            return;
        }
    }

    println!("[-] Clean / Not Protected (standard file).");
}

/// Recursively count subdirectories and files in a directory for stats.
fn count_dir_entries(dir: &Path) -> (usize, usize) {
    let mut dirs: usize = 0;
    let mut files: usize = 0;
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| {
            // Skip previous unpack folders
            !e.file_name().eq_ignore_ascii_case("unpack")
        })
        .flatten()
    {
        if entry.file_type().is_dir() {
            dirs += 1;
        } else if entry.file_type().is_file() {
            files += 1;
        }
    }
    // Exclude the root dir from the subdirectories count
    (dirs.saturating_sub(1), files)
}

/// Run deep recursive directory scan across all subfolders.
fn scan_directory(dir: &Path, scan_all: bool) {
    println!(
        "[*] Recursively scanning across all subfolders in: {}",
        dir.display()
    );
    if scan_all {
        println!("    Mode: Deep scan (probing all candidates regardless of size)");
    } else {
        println!("    Mode: Standard fast scan (skips non-executable assets)");
    }

    let (subdirs_count, total_files) = count_dir_entries(dir);
    println!(
        "    Inspecting {} subfolder(s) ({} total files)...",
        subdirs_count, total_files
    );

    let scan = senbei_io::scan::find_targets_opts(dir, scan_all);

    let total_protected = scan.crackproof.len() + scan.metadata.len() + scan.android_so.len();

    println!("\n================ SCAN RESULTS ================");
    println!("Total Protected Targets Found: {}", total_protected);
    println!("----------------------------------------------");

    if !scan.crackproof.is_empty() {
        println!("[+] Protected Binaries ({}):", scan.crackproof.len());
        for p in &scan.crackproof {
            let rel = p.strip_prefix(dir).unwrap_or(p);
            println!("  • {}", rel.display());

            let name = p.file_name().unwrap_or_default().to_string_lossy();
            let companion = p.with_file_name(format!("{}._", name));
            if companion.is_file() {
                let comp_rel = companion.strip_prefix(dir).unwrap_or(&companion);
                println!("    └─ Auxiliary companion: {}", comp_rel.display());
            }
        }
        println!();
    }

    if !scan.metadata.is_empty() {
        println!("[+] Obfuscated Metadata ({}):", scan.metadata.len());
        for m in &scan.metadata {
            let rel = m.strip_prefix(dir).unwrap_or(m);
            println!("  • {}", rel.display());
        }
        println!();
    }

    if !scan.android_so.is_empty() {
        println!(
            "[+] Protected Shared Libraries ({}):",
            scan.android_so.len()
        );
        for s in &scan.android_so {
            let rel = s.strip_prefix(dir).unwrap_or(s);
            println!("  • {}", rel.display());
        }
        println!();
    }

    println!("[i] Traversal Summary:");
    println!("    Subfolders checked       : {}", subdirs_count);
    println!("    Probed clean / untouched : {}", scan.stats.skipped);
    if scan.stats.walk_errors > 0 || scan.stats.probe_errors > 0 {
        println!(
            "    Errors: {} unreadable entries, {} probe read errors",
            scan.stats.walk_errors, scan.stats.probe_errors
        );
    }

    if total_protected > 0 {
        println!(
            "\n[TIP] Run 'redive init \"{}\"' to unpack these files and configure amulet.",
            dir.display()
        );
    } else {
        println!("\n[✓] No protected targets found in this tree.");
    }
}

/// Entry point for `redive scan [path] [--all]`.
pub fn run(args: &[String]) {
    let mut target_path: Option<PathBuf> = None;
    let mut scan_all = false;

    for arg in args {
        match arg.as_str() {
            "--all" | "-a" => scan_all = true,
            s if !s.starts_with('-') && target_path.is_none() => {
                target_path = Some(PathBuf::from(s));
            }
            _ => {}
        }
    }

    let path = match target_path {
        Some(p) => p,
        None => {
            // Check redive.ini for default target
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("."));
            let ini = exe_dir.join("redive.ini");
            let cfg = crate::resolve::read_main_ini(&ini).unwrap_or_default();
            if let Some(t) = cfg.target {
                let p = PathBuf::from(t);
                if p.is_file() {
                    p.parent().map(Path::to_path_buf).unwrap_or(p)
                } else {
                    p
                }
            } else {
                PathBuf::from(".")
            }
        }
    };

    if !path.exists() {
        eprintln!("error: target path does not exist: {}", path.display());
        std::process::exit(1);
    }

    if path.is_file() {
        probe_file(&path);
    } else {
        scan_directory(&path, scan_all);
    }
}
