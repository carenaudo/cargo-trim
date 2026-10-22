use clap::Parser;
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
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

    /// Keep newest executables/shared libraries and installer bundles; delete other artifacts
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

/// A directory is deleted whole once it matches, so only evidence that Cargo itself wrote it
/// counts. A `debug/` or `release/` folder does not: other build systems create those too.
fn is_cargo_target(path: &Path) -> bool {
    // Cargo writes a CACHEDIR.TAG that names it; other tools write the same file with
    // their own name, so the file existing is not enough.
    if let Ok(tag) = fs::read_to_string(path.join("CACHEDIR.TAG")) {
        if tag.to_ascii_lowercase().contains("cargo") {
            return true;
        }
    }
    // Cargo caches the rustc version here; nothing else writes it.
    if path.join(".rustc_info.json").is_file() {
        return true;
    }
    // A `target/` next to a manifest, as a last resort for very old Cargo versions.
    path.parent()
        .is_some_and(|p| p.join("Cargo.toml").is_file())
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
    let mut it = WalkDir::new(root).follow_links(false).into_iter();

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

        // Cargo always names it `target` in lowercase; `Target/` is someone else's.
        if name == "target" {
            let path = entry.path().to_path_buf();
            if is_cargo_target(&path) {
                targets.push(path);
                it.skip_current_dir();
            }
        }
    }

    targets
}

#[derive(Debug)]
struct SmartPlan {
    keep: Vec<PathBuf>,
    remove: Vec<(PathBuf, u64)>,
    directories: Vec<PathBuf>,
}

fn is_shared_library(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    name.ends_with(".dll")
        || name.ends_with(".dylib")
        || name.ends_with(".so")
        || name.contains(".so.")
}

fn is_executable(path: &Path, meta: &fs::Metadata) -> bool {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return path.extension().is_none() && meta.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

fn is_installer(path: &Path) -> bool {
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    matches!(
        ext.as_str(),
        "msi"
            | "msix"
            | "msixbundle"
            | "appx"
            | "appxbundle"
            | "dmg"
            | "pkg"
            | "deb"
            | "rpm"
            | "appimage"
            | "apk"
            | "aab"
    )
}

fn is_cache_path(relative: &Path) -> bool {
    relative.components().any(|part| {
        let name = part.as_os_str().to_string_lossy();
        matches!(
            name.as_ref(),
            "deps"
                | "incremental"
                | "build"
                | ".fingerprint"
                | "examples"
                | "rust-analyzer"
                | "llvm-cov-target"
                | "nextest"
                | "criterion"
                | "tmp"
                | "doc"
                | "package"
        ) || name.starts_with("flycheck")
    })
}

fn plan_smart_cleanup(target: &Path) -> io::Result<SmartPlan> {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut newest = BTreeMap::<(PathBuf, std::ffi::OsString), (SystemTime, PathBuf)>::new();
    let mut libraries = Vec::new();
    let mut bundles = Vec::new();
    let mut walker = WalkDir::new(target)
        .follow_links(false)
        .min_depth(1)
        .into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry.map_err(io::Error::other)?;
        let path = entry.path().to_path_buf();
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let relative = path.strip_prefix(target).unwrap();
        let in_cache = is_cache_path(relative.parent().unwrap());
        if entry.file_type().is_dir()
            && !in_cache
            && (name == "bundle"
                || name == "packager"
                || (relative.components().count() == 1
                    && matches!(name.as_str(), "wix" | "distrib"))
                || name.ends_with(".app")
                || name.ends_with(".framework")
                || name.ends_with(".bundle"))
        {
            bundles.push(path);
            walker.skip_current_dir();
            continue;
        }
        // Fail closed: links may point outside the target or form library chains.
        if entry.path_is_symlink() {
            return Err(io::Error::other(format!(
                "Cannot safely trim target containing a symbolic link: {}",
                path.display()
            )));
        }
        if entry.file_type().is_dir() {
            directories.push(path);
            continue;
        }
        let meta = entry.metadata().map_err(io::Error::other)?;
        if !in_cache
            && (is_executable(&path, &meta)
                || is_shared_library(&path)
                || path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("wasm")))
        {
            let candidate = (meta.modified()?, path.clone());
            // Compare profiles within one target architecture, never across triples.
            // This also keeps independent nested output trees from competing.
            let scope = relative.parent().unwrap().parent().unwrap_or(Path::new(""));
            let key = (
                scope.to_path_buf(),
                path.file_name().unwrap().to_os_string(),
            );
            if !newest.get(&key).is_some_and(|old| candidate <= *old) {
                newest.insert(key, candidate);
            }
        }
        if is_shared_library(&path) {
            libraries.push(path.clone());
        }
        if !in_cache && is_installer(&path) {
            bundles.push(path.clone());
        }
        files.push((path, meta.len()));
    }
    let mut keep: HashSet<PathBuf> = newest.into_values().map(|(_, path)| path).collect();
    // Keep sibling libraries with retained programs: picking a DLL from another
    // profile can break the loader or introduce an incompatible ABI.
    let output_dirs: HashSet<PathBuf> = keep
        .iter()
        .map(|p| p.parent().unwrap().to_path_buf())
        .collect();
    keep.extend(
        libraries
            .into_iter()
            .filter(|p| output_dirs.contains(p.parent().unwrap())),
    );
    keep.extend(bundles);
    let remove = files
        .into_iter()
        .filter(|(p, _)| {
            !keep.contains(p)
                && p != &target.join("CACHEDIR.TAG")
                && p != &target.join(".rustc_info.json")
        })
        .collect();
    let mut keep: Vec<_> = keep.into_iter().collect();
    keep.sort();
    directories.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    Ok(SmartPlan {
        keep,
        remove,
        directories,
    })
}

#[derive(Default, Debug)]
struct CleanupResult {
    removed_bytes: u64,
    error: Option<io::Error>,
}

impl CleanupResult {
    fn record(&mut self, path: &Path, result: io::Result<()>) {
        if let Err(error) = result {
            if self.error.is_none() {
                self.error = Some(io::Error::new(
                    error.kind(),
                    format!("{}: {}", path.display(), error),
                ));
            }
        }
    }

    fn remove_file(&mut self, path: &Path) {
        let result = (|| {
            // Use the current size, not the potentially stale dry-run estimate.
            let meta = fs::symlink_metadata(path)?;
            fs::remove_file(path)?;
            if meta.is_file() {
                self.removed_bytes += meta.len();
            }
            Ok(())
        })();
        self.record(path, result);
    }
}

fn clean_target_smart(plan: &SmartPlan) -> CleanupResult {
    let mut result = CleanupResult::default();
    for (path, _) in &plan.remove {
        result.remove_file(path);
    }
    for path in &plan.directories {
        let removal = (|| {
            if fs::read_dir(path)?.next().transpose()?.is_none() {
                fs::remove_dir(path)?;
            }
            Ok(())
        })();
        result.record(path, removal);
    }
    result
}

fn clean_target_all(target: &Path) -> CleanupResult {
    let mut result = CleanupResult::default();
    for entry in WalkDir::new(target)
        .follow_links(false)
        .contents_first(true)
    {
        match entry {
            Ok(entry) if entry.file_type().is_dir() => {
                result.record(entry.path(), fs::remove_dir(entry.path()));
            }
            Ok(entry) => result.remove_file(entry.path()),
            Err(error) => result.record(target, Err(io::Error::other(error))),
        }
    }
    result
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

    println!(
        "🔍 Scanning for Cargo targets in: {}",
        clean_path_display(&root)
    );
    let raw_targets = scan_for_targets(&root);

    if raw_targets.is_empty() {
        println!("✨ No Cargo target directories found.");
        return Ok(());
    }

    println!(
        "📊 Found {} potential targets. Analyzing sizes and age in parallel...",
        raw_targets.len()
    );

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
    target_infos.sort_by_key(|info| std::cmp::Reverse(info.size_bytes));

    let total_size: u64 = target_infos.iter().map(|t| t.size_bytes).sum();

    println!(
        "\n┌─ Found Cargo Targets ──────────────────────────────────────────────────────────────┐"
    );
    println!(
        "│ {:<68} │ {:<12} │ {:<14} │",
        "Project Path", "Size", "Last Active"
    );
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

    let mut smart_plans = BTreeMap::new();
    if args.keep_bin {
        let mut reclaimable = 0;
        for info in &target_infos {
            let plan = plan_smart_cleanup(&info.path)?;
            println!("Keep in {}:", clean_path_display(&info.path));
            if plan.keep.is_empty() {
                println!("  No executable or shared library found.");
            }
            for path in &plan.keep {
                println!("  {}", clean_path_display(path));
            }
            reclaimable += plan.remove.iter().map(|(_, size)| size).sum::<u64>();
            smart_plans.insert(info.path.clone(), plan);
        }
        println!(
            "Estimated reclaimable file size: {}",
            format_bytes(reclaimable)
        );
        println!("Other artifacts, including debug symbols, static libraries and captures, will be deleted. External runtime assets are not inferred.");
    }

    if args.dry_run {
        println!("ℹ️  Dry-run active. No files were deleted.");
        return Ok(());
    }

    // Check confirmation if not -y
    if !args.yes {
        let mode_desc = if args.keep_bin {
            "Delete other artifacts; keep newest executables and shared libraries listed above"
        } else {
            "Completely delete target directories"
        };
        print!(
            "❓ Clean these {} targets [{}]? (y/N): ",
            target_infos.len(),
            mode_desc
        );
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

    let results: Vec<(PathBuf, CleanupResult)> = target_infos
        .par_iter()
        .map(|info| {
            // Self protection: Don't delete target directory of currently executing binary
            if let Some(ref exe) = current_exe {
                if let Ok(canon_exe) = fs::canonicalize(exe) {
                    if let Ok(canon_target) = fs::canonicalize(&info.path) {
                        if canon_exe.starts_with(&canon_target) {
                            return (info.path.clone(), CleanupResult { removed_bytes: 0, error: Some(io::Error::other("Skipped: Target directory contains the currently running cargo-trim binary")) });
                        }
                    }
                }
            }

            if args.keep_bin {
                let freed = clean_target_smart(&smart_plans[&info.path]);
                (info.path.clone(), freed)
            } else {
                (info.path.clone(), clean_target_all(&info.path))
            }
        })
        .collect();

    let mut total_freed = 0u64;
    let mut failed = 0;

    for (path, res) in results {
        let disp = clean_path_display(&path);
        total_freed += res.removed_bytes;
        match res.error {
            None => {
                println!(
                    "  ✅ Cleaned: {} (removed {})",
                    disp,
                    format_bytes(res.removed_bytes)
                );
            }
            Some(e) => {
                println!(
                    "  ⚠️  Could not fully clean {} (removed {}): {}",
                    disp,
                    format_bytes(res.removed_bytes),
                    e
                );
                failed += 1;
            }
        }
    }

    println!(
        "\n🎉 Finished! Removed {} of files (logical size; disk space reclaimed may differ).",
        format_bytes(total_freed)
    );
    if failed > 0 {
        println!(
            "⚠️  {} targets could not be deleted (may be open in another program or IDE).",
            failed
        );
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

    fn fixture(root: &Path, path: &str, bytes: &[u8], age: u64) -> PathBuf {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(age)),
            )
            .unwrap();
        path
    }

    #[test]
    fn smart_removes_debug_and_nested_artifacts_but_keeps_newest_outputs() {
        let root = scratch("smart");
        let old = fixture(&root, "release/game.exe", b"old", 1);
        let latest = fixture(&root, "debug/game.exe", b"new", 2);
        let tool = fixture(&root, "release/tool.exe", b"tool", 1);
        let library = fixture(&root, "custom/libplugin.so", b"library", 2);
        fixture(&root, "release/libplugin.so", b"older", 1);
        let dll = fixture(&root, "debug/runtime.dll", b"runtime", 1);
        for path in [
            "debug/game.pdb",
            "debug/lib.rlib",
            "nested/release/deps/cache.rmeta",
            "debug/deps/game-hash.exe",
            "performance/frame.rgba",
        ] {
            fixture(&root, path, b"junk", 3);
        }
        let plan = plan_smart_cleanup(&root).unwrap();
        let expected: u64 = plan.remove.iter().map(|(_, size)| size).sum();
        let before = calculate_target_stats(&root).0;
        let result = clean_target_smart(&plan);
        assert!(result.error.is_none());
        assert_eq!(result.removed_bytes, expected);
        assert_eq!(before - calculate_target_stats(&root).0, expected);
        assert!(!old.exists());
        for path in [latest, tool, library, dll] {
            assert!(path.exists());
        }
        assert!(!root.join("performance").exists());
        assert!(!root.join("debug/deps").exists());
        assert!(!root.join("debug/game.pdb").exists());
        assert!(plan_smart_cleanup(&root).unwrap().remove.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn smart_keeps_installers_and_complete_bundles() {
        let root = scratch("bundles");
        let paths = [
            "release/bundle/nsis/setup.exe",
            "release/bundle/resources/data.bin",
            "debug/Game.app/Contents/Resources/icon.png",
            "packages/game.msi",
            "packages/game.AppImage",
            "packages/game.deb",
            "release/plugin.dylib",
        ];
        for path in paths {
            fixture(&root, path, b"keep", 1);
        }
        fixture(&root, "debug/cache.pdb", b"remove", 1);
        let plan = plan_smart_cleanup(&root).unwrap();
        assert!(root.join("debug/cache.pdb").exists()); // Planning is read-only.
        assert!(clean_target_smart(&plan).error.is_none());
        for path in paths {
            assert!(root.join(path).exists(), "{path}");
        }
        assert!(!root.join("debug/cache.pdb").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn smart_reports_deletion_failures() {
        let root = scratch("failure");
        let path = fixture(&root, "debug/cache.pdb", b"junk", 1);
        let plan = plan_smart_cleanup(&root).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(clean_target_smart(&plan).error.is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn smart_handles_cross_targets_packaging_and_tool_caches_twice() {
        let root = scratch("layouts");
        let kept = [
            "CACHEDIR.TAG",
            ".rustc_info.json",
            "release/app.exe",
            "custom/plugin.dll",
            "x86_64-pc-windows-msvc/release/app.exe",
            "aarch64-pc-windows-msvc/release/app.exe",
            "wasm32-unknown-unknown/release/app.wasm",
            "release/packager/resources/config.json",
            "wix/source.wxs",
            "distrib/app.tar.gz",
            "release/libplugin.so.1",
            "release/Game.app/Contents/Info.plist",
        ];
        let removed = [
            "debug/app.exe",
            "release/.cargo-lock",
            "custom/deps/plugin.dll",
            "release/app.pdb",
            "release/app.d",
            "release/libapp.a",
            "release/examples/demo.exe",
            "release/build/pkg/out/setup.msi",
            "release/build/pkg/out/bundle/data.bin",
            "release/deps/Plugin.framework/Resources/data.bin",
            "rust-analyzer/debug/app.exe",
            "flycheck0/debug/app.exe",
            "llvm-cov-target/release/app.exe",
            "nextest/app.exe",
            "criterion/app.exe",
            "tmp/app.exe",
            "doc/demo.wasm",
            "package/demo/target/release/app.exe",
        ];
        for path in kept {
            fixture(&root, path, b"keep", 2);
        }
        for path in removed {
            fixture(&root, path, b"delete", 1);
        }
        let plan = plan_smart_cleanup(&root).unwrap();
        assert_eq!(
            plan.remove.iter().map(|(_, size)| size).sum::<u64>(),
            removed.len() as u64 * 6
        );
        let result = clean_target_smart(&plan);
        assert!(result.error.is_none());
        assert_eq!(result.removed_bytes, removed.len() as u64 * 6);
        for path in kept {
            assert!(root.join(path).exists(), "{path}");
        }
        for path in removed {
            assert!(!root.join(path).exists(), "{path}");
        }
        // No cache markers are left to identify custom or cross-target profiles.
        let again = plan_smart_cleanup(&root).unwrap();
        assert!(again.remove.is_empty());
        assert!(clean_target_smart(&again).error.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_cleanup_counts_successes_and_current_sizes() {
        let root = scratch("partial");
        let first = fixture(&root, "first.pdb", b"old", 1);
        let missing = root.join("missing.pdb");
        let last = fixture(&root, "last.pdb", b"last", 1);
        let plan = SmartPlan {
            keep: vec![],
            remove: vec![(first.clone(), 3), (missing, 100), (last.clone(), 4)],
            directories: vec![],
        };
        fs::write(&first, b"changed size").unwrap();
        let result = clean_target_smart(&plan);
        assert!(result.error.is_some());
        assert_eq!(result.removed_bytes, 16);
        assert!(!first.exists());
        assert!(!last.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_cleanup_counts_files_and_reports_missing_target() {
        let root = scratch("full");
        fixture(&root, "debug/deps/file.rlib", b"data", 1);
        fixture(&root, "release/app.exe", b"program", 1);
        let result = clean_target_all(&root);
        assert!(result.error.is_none());
        assert_eq!(result.removed_bytes, 11);
        assert!(!root.exists());
        let result = clean_target_all(&root);
        assert!(result.error.is_some());
        assert_eq!(result.removed_bytes, 0);
    }

    #[cfg(windows)]
    #[test]
    fn full_cleanup_counts_partial_success_with_locked_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = scratch("locked");
        let locked = fixture(&root, "locked.pdb", b"locked", 1);
        fixture(&root, "other.pdb", b"other", 1);
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .unwrap();
        let result = clean_target_all(&root);
        assert!(result.error.is_some());
        assert_eq!(result.removed_bytes, 5);
        assert!(locked.exists());
        drop(handle);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn smart_keeps_unix_executables_and_rejects_external_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = scratch("unix");
        let executable = fixture(&root, "release/app", b"program", 1);
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        fixture(&root, "release/cache.pdb", b"cache", 1);
        assert!(plan_smart_cleanup(&root)
            .unwrap()
            .keep
            .contains(&executable));
        symlink(&executable, root.join("release/link")).unwrap();
        assert!(plan_smart_cleanup(&root).is_err());
        assert!(root.join("release/cache.pdb").exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cargo-trim-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn recognises_targets_cargo_wrote() {
        let root = scratch("cargo");

        let tagged = root.join("tagged/target");
        fs::create_dir_all(&tagged).unwrap();
        fs::write(
            tagged.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n",
        )
        .unwrap();

        let rustc_info = root.join("info/target");
        fs::create_dir_all(&rustc_info).unwrap();
        fs::write(rustc_info.join(".rustc_info.json"), "{}").unwrap();

        let manifest = root.join("manifest");
        fs::create_dir_all(manifest.join("target")).unwrap();
        fs::write(manifest.join("Cargo.toml"), "[package]\n").unwrap();

        assert!(is_cargo_target(&tagged));
        assert!(is_cargo_target(&rustc_info));
        assert!(is_cargo_target(&manifest.join("target")));
        let mut found = scan_for_targets(&root);
        found.sort();
        assert_eq!(found.len(), 3);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ignores_targets_other_tools_wrote() {
        let root = scratch("other");

        // Another build system's output with the usual profile folders.
        let other = root.join("cmake-project/target");
        fs::create_dir_all(other.join("release")).unwrap();
        fs::create_dir_all(other.join("debug")).unwrap();

        // Another tool's cache tag.
        let tagged = root.join("tagged/target");
        fs::create_dir_all(&tagged).unwrap();
        fs::write(
            tagged.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# created by some-other-tool\n",
        )
        .unwrap();

        // Capitalised, even with Cargo's marker inside.
        let capital = root.join("capital/Target");
        fs::create_dir_all(&capital).unwrap();
        fs::write(capital.join(".rustc_info.json"), "{}").unwrap();

        assert!(!is_cargo_target(&other));
        assert!(!is_cargo_target(&tagged));
        assert!(scan_for_targets(&root).is_empty());

        fs::remove_dir_all(&root).unwrap();
    }
}
