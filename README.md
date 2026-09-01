# cargo-trim

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org)
[![OS: Windows | Linux | macOS](https://img.shields.io/badge/OS-Windows%20%7C%20Linux%20%7C%20macOS-blue.svg)](https://github.com/carenaudoplq/cargo-trim)

A fast, lightweight, and cross-platform CLI tool & Cargo plugin to recursively find, analyze, and clean Cargo `target/` build directories across deeply nested project hierarchies. Runs seamlessly on **Windows, Linux, and macOS**.

```text
PS D:\programacion> cargo trim -n
🔍 Scanning for Cargo targets in: D:\programacion
📊 Found 5 potential targets. Analyzing sizes and age in parallel...

┌─ Found Cargo Targets ──────────────────────────────────────────────────────────────┐
│ Project Path                                                         │ Size         │ Last Active    │
├──────────────────────────────────────────────────────────────────────┼──────────────┼────────────────┤
│ D:\programacion\arena\desktop\target                                 │     12.22 GB │ 1 months ago   │
│ D:\programacion\short-com-PBE\solvers\rust\target                    │      7.38 GB │ 3 days ago     │
│ D:\programacion\horiReports\hori2rust\target                         │      1.76 GB │ 2 weeks ago    │
│ D:\programacion\cargo-trim\target                                    │     71.49 MB │ just now       │
│ D:\programacion\demo\plqdemos\rustplayground\hello_world\target      │      6.00 MB │ 3.3 years ago  │
└──────────────────────────────────────────────────────────────────────┴──────────────┴────────────────┘
Total Disk Usage: 21.44 GB across 5 projects

ℹ️  Dry-run active. No files were deleted.
```

---

## ⚖️ Comparison: `cargo-trim` vs. `kondo` vs. `cargo-sweep`

| Feature / Capability | `cargo-trim` | `kondo` | `cargo-sweep` |
| :--- | :---: | :---: | :---: |
| **Deep Nested Discovery** (nested crates, monorepos) | ✅ Yes (unlimited depth) | ❌ Skips subfolders if parent matches | ⚠️ Depends on Cargo structure |
| **Multi-Language Drive Resilience** | ✅ Won't get blocked by `.venv` / `node_modules` | ❌ Stops when parent project found | ✅ Rust-only |
| **Smart Cache Mode** (keep compiled `.exe` / binaries) | ✅ Yes (`-k` / `--keep-bin`) | ❌ No (all-or-nothing wipe) | ✅ Yes (`-i` / `--installed`) |
| **Age-Based Filtering** (`--older <days>`) | ✅ Yes (`-d`) | ⚠️ Timestamp based on dir | ✅ Yes (`-t`) |
| **Parallel Scanning & Deletion** | ✅ Multi-threaded (Rayon) | ⚠️ Partial | ❌ Single-threaded Cargo calls |
| **Unified Table Summary** | ✅ Single-glance summary of all targets | ⚠️ TUI (one-by-one) | ❌ Verbose log stream |
| **Dual Cargo Plugin & Standalone Binary** | ✅ `cargo trim` or `cargo-trim` | ❌ Standalone only | ⚠️ Cargo plugin only |
| **Cross-Platform** (Windows, Linux, macOS) | ✅ Full support | ✅ Full support | ✅ Full support |

### Key Differences in Short:
* **vs. `kondo`:**
  * **No missed nested targets:** `kondo` stops recursing once it matches a parent project (e.g. if a folder has Python `.venv` or Node `package.json`, it misses all nested Rust `target/` folders). `cargo-trim` finds all nested targets regardless of parent structure.
  * **Granular cache cleaning:** `kondo` only performs a complete directory wipe. `cargo-trim` supports smart cache pruning (`-k`) to keep your compiled binaries ready to execute.
* **vs. `cargo-sweep`:**
  * **Fast parallel filesystem scanning:** `cargo-sweep` relies on Cargo metadata queries which can be slow or fail across large multi-language drives. `cargo-trim` scans the filesystem directly with multi-threaded parallel walkers.
  * **Clear instant overview:** `cargo-trim` displays a neat, sorted summary table with project paths, human-readable sizes (GB/MB), and last-active dates before you decide to clean.

---

## ⚡ Why `cargo-trim`?

Rust's `target/` directories accumulate gigabytes of build artifacts, intermediate object files, incremental caches, and test binaries.

Existing tools often struggle with real-world developer workspaces:
* **Nested Projects & Monorepos:** General-purpose cleaners often stop descending into subfolders once they encounter a parent project marker (e.g. Python `.venv` or Node `package.json`), completely missing multi-gigabyte Rust targets nested deeper in the tree.
* **Granular Control:** Sometimes you want to clean **only stale projects** untouched for weeks, or clean **only intermediate cache files** while preserving your compiled executables.

`cargo-trim` solves this with deep parallel directory traversal and smart filtering.

---

## ✨ Features

* **Deep Recursive Traversal:** Scans arbitrary directory depths without stopping at parent boundaries.
* **Parallel Performance:** Multi-threaded size calculations, age detection, and file deletions powered by [Rayon](https://crates.io/crates/rayon).
* **Dual Execution Mode:** Runs directly as `cargo trim` or as a standalone binary `cargo-trim.exe`.
* **Safe Dry-Run Mode (`-n`):** Inspect targets and potential space savings before making any changes.
* **Age-Based Filtering (`-d` / `--older`):** Clean only projects that haven't been modified in $N$ days.
* **Smart Cache Trimming (`-k` / `--keep-bin`):** Deletes huge intermediate caches (`deps/`, `incremental/`, `build/`, `.fingerprint`) but keeps final compiled binaries (`.exe`, `.dll`, etc.) intact.
* **Self-Protection:** Automatically detects and avoids deleting the directory containing the currently running `cargo-trim` binary.

---

## 📦 Installation

### From Source (Local Repository)
```bash
git clone https://github.com/carenaudoplq/cargo-trim.git
cd cargo-trim
cargo install --path .
```

### Standalone Executable
Build an optimized, standalone binary (~580 KB):
```bash
cargo build --release
# The single binary is located at ./target/release/cargo-trim.exe
```

---

## 🚀 Usage

### 1. Preview / Dry Run
Analyze disk usage across the current folder or a specific directory:
```bash
# Current directory
cargo trim -n

# Specific path
cargo trim /path/to/projects -n
```

### 2. Interactive Cleanup
Scans all targets and prompts with `(y/N)` before deleting:
```bash
cargo trim
```

### 3. Automated / Non-Interactive Clean
Clean all found target directories immediately:
```bash
cargo trim -y
```

### 4. Clean Stale Projects Only
Clean targets for projects not modified in the last 14 days:
```bash
cargo trim -d 14
```

### 5. Smart Cache Clean (Keep Final Executables)
Reclaim disk space from intermediate caches while keeping your compiled binaries:
```bash
cargo trim -k
```

### 6. Filter by Minimum Size
Only list or clean targets that occupy more than a specific size in MB:
```bash
cargo trim --min-mb 100 -n
```

---

## 📋 CLI Options Reference

```text
Usage: cargo-trim.exe [OPTIONS] [PATH]

Arguments:
  [PATH]                 Root directory to scan (defaults to current directory) [default: .]

Options:
  -n, --dry-run          Dry run: Only list discovered targets and calculate space without deleting
  -y, --yes              Non-interactive: Delete targets without confirmation prompt
  -d, --older <DAYS>     Filter: Only clean targets inactive/unmodified for at least N days
  -k, --keep-bin         Smart mode: Delete intermediate caches (deps, incremental, build, .fingerprint)
                         while keeping final compiled binaries (.exe, .dll, etc.)
      --min-mb <MIN_MB>  Only include targets larger than this minimum size in MB [default: 0]
  -h, --help             Print help
  -V, --version          Print version
```

---

## 📄 License

This project is licensed under the [MIT License](LICENSE).
