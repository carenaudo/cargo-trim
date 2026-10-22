use clap::Parser;
use rayon::prelude::*;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(
    name = "cargo-trim",
    author = clap::crate_authors!(", "),
    version = "0.1.0",
    about = "Recursively find and clean Cargo target build directories across nested subfolders."
)]
struct Args {
    /// Root directory to scan (defaults to current directory)
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Dry run: Only list discovered targets and calculate space without deleting
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Non-interactive: Delete targets without confirmation prompt
    #[arg(short = 'y', long)]
    yes: bool,

    /// Filter: Only clean targets inactive/unmodified for at least N days
    #[arg(short = 'd', long = "older", value_name = "DAYS")]
    older_days: Option<u64>,

    /// Smart mode: Keep only final binaries (.exe, .dll, .so, .dylib, .wasm, executables) and installer bundles; delete everything else (deps, build, incremental, .pdb, .rlib, tool caches, ...)
    #[arg(short = 'k', long = "keep-bin")]
    keep_bin: bool,

    /// Only include targets larger than this minimum size in MB
    #[arg(long = "min-mb", default_value = "0")]
    min_mb: u64,
}

#[derive(Debug, Clone)]
struct TargetInfo {
    path: PathBuf,
    size_bytes: u64,
    last_modified: SystemTime,
}

fn is_cargo_target(path: &Path) -> bool {
    // 1. Check for CACHEDIR.TAG (standard in Cargo targets)
    if path.join("CACHEDIR.TAG").exists() {
        return true;
    }
    // 2. Check if parent directory contains Cargo.toml
    if let Some(parent) = path.parent() {
        if parent.join("Cargo.toml").exists() {
            return true;
        }
    }
    // 3. Check for standard cargo subdirs/files
    if path.join(".rustc_info.json").exists()
        || path.join("debug").is_dir()
        || path.join("release").is_dir()
    {
        return true;
    }
    false
}

fn clean_path_display(path: &Path) -> String {
    let s = path.display().to_string();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        stripped.to_string()
    } else {
        s
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn format_age(last_mod: SystemTime) -> String {
    match SystemTime::now().duration_since(last_mod) {
        Ok(d) => {
            let secs = d.as_secs();
            let days = secs / 86400;
            let hours = (secs % 86400) / 3600;

            if days >= 365 {
                format!("{:.1} years ago", days as f64 / 365.0)
            } else if days >= 30 {
                format!("{} months ago", days / 30)
            } else if days >= 7 {
                format!("{} weeks ago", days / 7)
            } else if days > 0 {
                format!("{} days ago", days)
            } else if hours > 0 {
                format!("{} hours ago", hours)
            } else {
                "just now".to_string()
            }
        }
        Err(_) => "future/unknown".to_string(),
    }
}

fn calculate_target_stats(target_path: &Path) -> (u64, SystemTime) {
    let mut total_size = 0u64;
    let mut latest_mod = fs::metadata(target_path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);

    for entry in WalkDir::new(target_path).into_iter().filter_map(|e| e.ok()) {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total_size += meta.len();
                if let Ok(mod_time) = meta.modified() {
                    if mod_time > latest_mod {
                        latest_mod = mod_time;
                    }
                }
            }
        }
    }

    (total_size, latest_mod)
}

fn scan_for_targets(root: &Path) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    let mut it = WalkDir::new(root)
        .follow_links(false)
        .into_iter();

    while let Some(entry_res) = it.next() {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };

        let file_type = entry.file_type();
        if !file_type.is_dir() {
            continue;
        }

        let name = entry.file_name().to_string_lossy();

        // Skip folders that shouldn't be traversed
        if name == ".git" || name == ".venv" || name == "node_modules" {
            it.skip_current_dir();
            continue;
        }

        if name.eq_ignore_ascii_case("target") {
            let path = entry.path().to_path_buf();
            if is_cargo_target(&path) {
                targets.push(path);
                it.skip_current_dir();
            }
        }
    }

    targets
}

/// Top-level target entries kept as-is: installer/distribution output (cargo-wix, cargo-dist)
/// and the cache marker that lets us recognize the directory later.
const KEEP_TARGET_ENTRIES: [&str; 3] = ["wix", "distrib", "CACHEDIR.TAG"];

/// Top-level target entries produced by tools rather than builds; always removed even if
/// they contain profile-like subdirectories.
const TOOL_TARGET_DIRS: [&str; 7] = [
    "rust-analyzer",
    "llvm-cov-target",
    "nextest",
    "criterion",
    "tmp",
    "doc",
    "package",
];

/// Directories inside a profile that hold installer bundles (tauri, cargo-bundle, cargo-packager).
const KEEP_PROFILE_DIRS: [&str; 2] = ["bundle", "packager"];

/// A profile dir (debug, release, custom profiles) is recognized by Cargo's bookkeeping in it.
fn is_profile_dir(path: &Path) -> bool {
    [".fingerprint", "deps", "build", "incremental", ".cargo-lock"]
        .iter()
        .any(|m| path.join(m).exists())
}

/// Final artifacts worth keeping: executables and dynamic libraries / wasm modules.
fn is_final_artifact(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => matches!(
            ext.to_ascii_lowercase().as_str(),
            "exe" | "dll" | "so" | "dylib" | "wasm"
        ),
        None => is_unix_executable(path),
    }
}

#[cfg(unix)]
fn is_unix_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_unix_executable(_path: &Path) -> bool {
    false
}

/// Removes a file or directory, adding whatever was actually deleted to `freed`.
/// On failure, the bytes that did get removed still count and the first error is kept.
fn remove_entry(path: &Path, freed: &mut u64, first_err: &mut Option<io::Error>) {
    let is_dir = fs::symlink_metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    let (before, _) = calculate_target_stats(path);
    let res = if is_dir {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match res {
        Ok(_) => *freed += before,
        Err(e) => {
            let (after, _) = if path.exists() {
                calculate_target_stats(path)
            } else {
                (0, SystemTime::UNIX_EPOCH)
            };
            *freed += before.saturating_sub(after);
            if first_err.is_none() {
                *first_err = Some(io::Error::new(
                    e.kind(),
                    format!("{}: {}", clean_path_display(path), e),
                ));
            }
        }
    }
}

/// Inside a profile dir keep only top-level final artifacts and installer bundles.
/// Everything else (deps, build, incremental, .fingerprint, examples, .pdb, .d, .rlib, ...) goes.
fn trim_profile_dir(profile: &Path, freed: &mut u64, first_err: &mut Option<io::Error>) {
    let Ok(entries) = fs::read_dir(profile) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let keep = if is_dir {
            KEEP_PROFILE_DIRS.contains(&name.as_str())
        } else {
            is_final_artifact(&path)
        };
        if !keep {
            remove_entry(&path, freed, first_err);
        }
    }
}

fn clean_target_smart(target_path: &Path) -> (u64, Option<io::Error>) {
    let mut freed = 0u64;
    let mut first_err = None;

    let Ok(entries) = fs::read_dir(target_path) else {
        return (0, None);
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

        if KEEP_TARGET_ENTRIES.contains(&name.as_str()) {
            continue;
        }
        if !is_dir || TOOL_TARGET_DIRS.contains(&name.as_str()) || name.starts_with("flycheck") {
            remove_entry(&path, &mut freed, &mut first_err);
            continue;
        }
        if is_profile_dir(&path) {
            trim_profile_dir(&path, &mut freed, &mut first_err);
            continue;
        }

        // Cross-compilation dirs (target/<triple>/<profile>) hold profile dirs one level down.
        let sub_profiles: Vec<PathBuf> = fs::read_dir(&path)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_dir() && is_profile_dir(p))
                    .collect()
            })
            .unwrap_or_default();
        if sub_profiles.is_empty() {
            remove_entry(&path, &mut freed, &mut first_err);
            continue;
        }
        for sub in fs::read_dir(&path).into_iter().flatten().filter_map(|e| e.ok()) {
            let sub_path = sub.path();
            if sub_profiles.contains(&sub_path) {
                trim_profile_dir(&sub_path, &mut freed, &mut first_err);
            } else {
                remove_entry(&sub_path, &mut freed, &mut first_err);
            }
        }
    }

    (freed, first_err)
}

fn main() -> io::Result<()> {
    let mut raw_args: Vec<String> = std::env::args().collect();
    // If invoked as `cargo trim`, cargo passes "trim" as the first argument after the binary
    if raw_args.len() > 1 && raw_args[1] == "trim" {
        raw_args.remove(1);
    }
    let args = Args::parse_from(raw_args);
    let root = fs::canonicalize(&args.path).unwrap_or_else(|_| args.path.clone());
    let current_exe = std::env::current_exe().ok();

    println!("🔍 Scanning for Cargo targets in: {}", clean_path_display(&root));
    let raw_targets = scan_for_targets(&root);

    if raw_targets.is_empty() {
        println!("✨ No Cargo target directories found.");
        return Ok(());
    }

    println!("📊 Found {} potential targets. Analyzing sizes and age in parallel...", raw_targets.len());

    let mut target_infos: Vec<TargetInfo> = raw_targets
        .into_par_iter()
        .map(|path| {
            let (size_bytes, last_modified) = calculate_target_stats(&path);
            TargetInfo {
                path,
                size_bytes,
                last_modified,
            }
        })
        .collect();

    // Filter by min size
    if args.min_mb > 0 {
        let min_bytes = args.min_mb * 1024 * 1024;
        target_infos.retain(|t| t.size_bytes >= min_bytes);
    }

    // Filter by age
    if let Some(older_days) = args.older_days {
        let min_duration = Duration::from_secs(older_days * 86400);
        let now = SystemTime::now();
        target_infos.retain(|t| match now.duration_since(t.last_modified) {
            Ok(age) => age >= min_duration,
            Err(_) => false,
        });
    }

    if target_infos.is_empty() {
        println!("✨ No Cargo target directories matched your filter criteria.");
        return Ok(());
    }

    // Sort by size descending
    target_infos.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

    let total_size: u64 = target_infos.iter().map(|t| t.size_bytes).sum();

    println!("\n┌─ Found Cargo Targets ──────────────────────────────────────────────────────────────┐");
    println!("│ {:<68} │ {:<12} │ {:<14} │", "Project Path", "Size", "Last Active");
    println!("├──────────────────────────────────────────────────────────────────────┼──────────────┼────────────────┤");

    for info in &target_infos {
        let display_path = clean_path_display(&info.path);
        let truncated_path = if display_path.len() > 68 {
            format!("...{}", &display_path[display_path.len() - 65..])
        } else {
            display_path
        };

        println!(
            "│ {:<68} │ {:>12} │ {:<14} │",
            truncated_path,
            format_bytes(info.size_bytes),
            format_age(info.last_modified)
        );
    }
    println!("└──────────────────────────────────────────────────────────────────────┴──────────────┴────────────────┘");
    println!(
        "Total Disk Usage: {} across {} projects\n",
        format_bytes(total_size),
        target_infos.len()
    );

    if args.dry_run {
        println!("ℹ️  Dry-run active. No files were deleted.");
        return Ok(());
    }

    // Check confirmation if not -y
    if !args.yes {
        let mode_desc = if args.keep_bin {
            "Trim intermediate caches (keep final binaries)"
        } else {
            "Completely delete target directories"
        };
        print!("❓ Clean these {} targets [{}]? (y/N): ", target_infos.len(), mode_desc);
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            println!("❌ Operation cancelled by user.");
            return Ok(());
        }
    }

    println!("\n🧹 Cleaning targets...");

    let results: Vec<(PathBuf, u64, Option<io::Error>)> = target_infos
        .par_iter()
        .map(|info| {
            // Self protection: Don't delete target directory of currently executing binary
            if let Some(ref exe) = current_exe {
                if let Ok(canon_exe) = fs::canonicalize(exe) {
                    if let Ok(canon_target) = fs::canonicalize(&info.path) {
                        if canon_exe.starts_with(&canon_target) {
                            return (info.path.clone(), 0, Some(io::Error::new(io::ErrorKind::Other, "Skipped: Target directory contains the currently running cargo-trim binary")));
                        }
                    }
                }
            }

            if args.keep_bin {
                let (freed, err) = clean_target_smart(&info.path);
                (info.path.clone(), freed, err)
            } else {
                let mut freed = 0u64;
                let mut err = None;
                remove_entry(&info.path, &mut freed, &mut err);
                (info.path.clone(), freed, err)
            }
        })
        .collect();

    let mut total_freed = 0u64;
    let mut failed = 0;

    for (path, freed, err) in results {
        let disp = clean_path_display(&path);
        total_freed += freed;
        match err {
            None => println!("  ✅ Cleaned: {} (freed {})", disp, format_bytes(freed)),
            Some(e) => {
                println!("  ⚠️  Partially cleaned {} (freed {}): {}", disp, format_bytes(freed), e);
                failed += 1;
            }
        }
    }

    println!("\n🎉 Finished! Successfully freed {} of disk space.", format_bytes(total_freed));
    if failed > 0 {
        println!("⚠️  {} targets could not be fully cleaned (files may be open in another program or IDE).", failed);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn author_comes_from_cargo_toml() {
        assert_eq!(Args::command().get_author(), Some("carenaudo, Antigravity"));
    }

    #[test]
    fn smart_clean_keeps_only_binaries_and_bundles() {
        let root = std::env::temp_dir().join(format!("cargo-trim-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let t = root.join("target");
        let files = [
            "CACHEDIR.TAG",
            ".rustc_info.json",
            "debug/.cargo-lock",
            "debug/app.exe",
            "debug/app.pdb",
            "debug/app.d",
            "debug/libapp.rlib",
            "debug/deps/app-123.exe",
            "debug/.fingerprint/x/y",
            "debug/examples/ex.exe",
            "release/.fingerprint/x/y",
            "release/core.dll",
            "release/bundle/msi/app.msi",
            "release-lto/deps/z.rlib",
            "release-lto/app.exe",
            "wasm32-unknown-unknown/release/deps/a.rlib",
            "wasm32-unknown-unknown/release/app.wasm",
            "rust-analyzer/flycheck0/debug/deps/a.rlib",
            "doc/index.html",
            "wix/app.msi",
        ];
        for f in files {
            let p = t.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"data").unwrap();
        }

        let (freed, err) = clean_target_smart(&t);
        assert!(err.is_none());

        let mut left: Vec<String> = WalkDir::new(&t)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().strip_prefix(&t).unwrap().to_string_lossy().replace('\\', "/"))
            .collect();
        left.sort();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(
            left,
            [
                "CACHEDIR.TAG",
                "debug/app.exe",
                "release-lto/app.exe",
                "release/bundle/msi/app.msi",
                "release/core.dll",
                "wasm32-unknown-unknown/release/app.wasm",
                "wix/app.msi",
            ]
        );
        assert_eq!(freed, 4 * (files.len() as u64 - left.len() as u64));
    }
}
