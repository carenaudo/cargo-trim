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
    author = "Antigravity",
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

    /// Smart mode: Delete intermediate caches (deps, incremental, build, .fingerprint) but keep final compiled binaries (.exe, .dll, etc.)
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

fn clean_target_smart(target_path: &Path) -> io::Result<u64> {
    let mut freed_bytes = 0u64;

    // Subdirectories inside profiles to clean
    let cache_dirs = ["deps", "incremental", "build", ".fingerprint", "examples"];
    let profiles = ["debug", "release"];

    for profile in &profiles {
        let prof_dir = target_path.join(profile);
        if prof_dir.is_dir() {
            for cache in &cache_dirs {
                let cache_path = prof_dir.join(cache);
                if cache_path.exists() {
                    let (size, _) = calculate_target_stats(&cache_path);
                    if let Ok(_) = fs::remove_dir_all(&cache_path) {
                        freed_bytes += size;
                    }
                }
            }
        }
    }

    // Also clean doc and package directories
    for extra in &["doc", "package"] {
        let p = target_path.join(extra);
        if p.exists() {
            let (size, _) = calculate_target_stats(&p);
            if let Ok(_) = fs::remove_dir_all(&p) {
                freed_bytes += size;
            }
        }
    }

    Ok(freed_bytes)
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

    let results: Vec<(PathBuf, io::Result<u64>)> = target_infos
        .par_iter()
        .map(|info| {
            // Self protection: Don't delete target directory of currently executing binary
            if let Some(ref exe) = current_exe {
                if let Ok(canon_exe) = fs::canonicalize(exe) {
                    if let Ok(canon_target) = fs::canonicalize(&info.path) {
                        if canon_exe.starts_with(&canon_target) {
                            return (info.path.clone(), Err(io::Error::new(io::ErrorKind::Other, "Skipped: Target directory contains the currently running cargo-trim binary")));
                        }
                    }
                }
            }

            if args.keep_bin {
                let freed = clean_target_smart(&info.path);
                (info.path.clone(), freed)
            } else {
                match fs::remove_dir_all(&info.path) {
                    Ok(_) => (info.path.clone(), Ok(info.size_bytes)),
                    Err(e) => (info.path.clone(), Err(e)),
                }
            }
        })
        .collect();

    let mut total_freed = 0u64;
    let mut failed = 0;

    for (path, res) in results {
        let disp = clean_path_display(&path);
        match res {
            Ok(freed) => {
                println!("  ✅ Cleaned: {} (freed {})", disp, format_bytes(freed));
                total_freed += freed;
            }
            Err(e) => {
                println!("  ⚠️  Failed to clean {}: {}", disp, e);
                failed += 1;
            }
        }
    }

    println!("\n🎉 Finished! Successfully freed {} of disk space.", format_bytes(total_freed));
    if failed > 0 {
        println!("⚠️  {} targets could not be deleted (may be open in another program or IDE).", failed);
    }

    Ok(())
}
