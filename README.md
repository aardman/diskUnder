# diskUnder

A command-line tool for indexing file metadata and detecting duplicate files/folders across a filesystem or network share — a CLI equivalent of [DiskOver](https://diskoverdata.com/). File metadata is stored in a local SQLite database, enabling fast queries for duplicates, large files, and directory summaries.

---

## Background

The project was developed in multiple languages, each addressing the limitations of the previous:

- **Bash (v1):** Initial proof-of-concept — simple but slow due to sequential per-file processing
- **Bash (v2):** Optimised with parallel `xargs`, deferred index creation, and SQL batching
- **Python:** More structured, with multiprocessing and bulk inserts — but ultimately too slow for large network shares
- **Rust v1:** Best performance, but contained bugs around network share resilience and resumability
- **Rust v2:** All known bugs fixed, plain-text output only
- **Rust v3:** Full TUI with live progress and duplicate detection, with automatic plain-text fallback
- **Rust v4** (current, `src/main.rs`): Fixes — cheap resume, change detection in `update`, duplicate tracking that scales to large shares
- **Rust v5+** (planned): Features — see [future.md](future.md)

Only the current Rust version is kept in this repository; the earlier versions were not committed.

See [changes.md](changes.md) for a detailed change log and [future.md](future.md) for planned features.

---

## How It Works

diskUnder walks a target directory, collects file metadata (size, dates, BLAKE3 checksum), and writes it to a SQLite database. Multiple paths can be indexed into the same database — each path gets its own table named `files_<MD5(path)>`.

Two modes are supported:

- **`index`** — full traversal of the target path. Files already in the database are skipped without being re-hashed, so an interrupted run can simply be restarted
- **`update`** — only files modified in the last 24 hours. Of those, files whose size and modified time still match the database are skipped without being re-hashed, so frequent runs (e.g. hourly cron) stay cheap

### Database Schema

```sql
CREATE TABLE files_<MD5_HASH> (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    path          TEXT NOT NULL UNIQUE,
    filename      TEXT NOT NULL,
    size          INTEGER NOT NULL,
    modified_date TEXT,
    access_date   TEXT,
    change_date   TEXT,       -- file birth time where supported, else empty
    checksum      TEXT,       -- BLAKE3
    extension     TEXT,
    directory     TEXT,
    created_at    TEXT DEFAULT CURRENT_TIMESTAMP
);
```

Indexes are created on `size`, `filename`, `checksum`, `extension`, `directory`, `access_date`, and `modified_date`.

### SQLite Optimisations

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = 100000;
```

---

## Usage

Displays a live full-screen terminal UI with real-time progress, duplicate detection, and an
activity log. Falls back to plain text automatically when stdout is not a terminal (cron, SSH,
pipes).

Use `diskunder-macos` (Apple Silicon) or `diskunder-linux` (x86_64, static) — shown below as `diskunder`.

```bash
./diskunder index <PATH> [OPTIONS]
./diskunder update <PATH> [OPTIONS]
```

**Options:**

| Flag | Default | Description |
|---|---|---|
| `--db <FILE>` | `fileindex.db` | SQLite database path (created if absent) |
| `--threads <N>` | `4` | Parallel hashing threads — reduce for network shares |
| `--no-tui` | off | Disable TUI and print plain progress lines to stderr |

**`--no-tui` / auto-detection:**
The TUI is skipped automatically when stdout is not an interactive terminal, so no flag is
needed for cron jobs, piped output, or SSH sessions without a TTY. Use `--no-tui` to force
plain mode explicitly in an interactive terminal.

```bash
# Interactive — full TUI
./diskunder index /Volumes/NAS --db /data/index.db --threads 4

# Headless / scripted — auto plain text (progress is written to stderr)
./diskunder index /Volumes/NAS --db /data/index.db 2>&1 | tee run.log

# Explicit plain text in an interactive terminal
./diskunder --no-tui index /Volumes/NAS --db /data/index.db

# Update mode — re-index files changed in last 24 hours
./diskunder update /Volumes/NAS --db /data/index.db
```

**Plain text output format:**
```
index /Volumes/NAS
  1000/772087 processed — 998 written, 2 errors
  2000/772087 processed — 1991 written, 9 errors
  ...
complete: 772087 files — 0 already indexed, 749404 written, 22683 errors (22683 unreadable), 1408 duplicate groups
wasted: 3.2 GB
```

**Dependencies:** `blake3`, `chrono`, `crossterm`, `md5`, `rayon`, `ratatui`, `rusqlite`, `structopt`, `walkdir`

---

## Comparison with earlier versions

| Feature | Python | Rust v1 | Rust v2 | Rust v3 (TUI) | Rust v4 (fixes) | Rust v5+ (planned) |
|---|---|---|---|---|---|---|
| Hashing | MD5 | BLAKE3 | BLAKE3 | BLAKE3 | BLAKE3 | BLAKE3 |
| Parallelisation | `multiprocessing` | Rayon | Rayon | Rayon | Rayon | Rayon |
| Batch transactions | Yes (100K) | No (single) | Yes (1K) | Yes (1K) | Yes (1K) | Yes (1K) |
| Resumable after crash | No | No | Yes | Yes | Yes | Yes |
| Resume skips already-indexed files | — | — | No | No | Yes | Yes |
| `update` skips unchanged files | — | — | No | No | Yes | Yes |
| Duplicate tracking scales to large runs | — | — | — | No | Yes (empty files excluded) | Yes |
| Duplicate totals cover all groups (not just the top 10) | — | — | — | No | Yes | Yes |
| Error breakdown (unreadable / not written / traversal) | No | No | No | No | Yes | Yes |
| Network share resilience | Partial | Poor | Yes | Yes | Yes | Yes |
| TUI progress | No | No | No | Yes | Yes | Yes |
| Auto plain-text fallback | — | — | — | Yes | Yes | Yes |
| `--db` flag | No | No | Yes | Yes | Yes | Yes |
| `--threads` flag | No | No | Yes | Yes | Yes | Yes |
| `search` — query the index by name, size, age, type | No | No | No | No | No | Planned |
| `report` — top files/dirs, by type, stale, duplicates | Scripts | Scripts | Scripts | Scripts | No | Planned |
| Duplicate filtering — `--min-size`, `--min-copies`, `--min-wasted`, `--extension` | No | No | No | No | No | Planned |
| `export` — CSV / JSON for BI tools | No | No | No | No | No | Planned |
| Dry run — preview dedupe actions without touching files | No | No | No | No | No | Planned |
| Ignore — exclude paths/patterns from indexing | No | No | No | No | No | Planned |
| Quarantine — move duplicates to a dated trash folder, with `purge` | No | No | No | No | No | Planned |
| Backup — rename redundant copies to `.bak` in place | No | No | No | No | No | Planned |

---

## Files

```
diskunder/
├── src/main.rs        # Rust v4 source
├── diskunder-macos    # Compiled macOS binary (arm64)
├── diskunder-linux    # Compiled Linux x86_64 binary (statically linked, musl)
├── Makefile           # Build targets: make, make mac, make linux, make clean
├── .cargo/config.toml # Cargo cross-compilation config (musl linker)
├── Cargo.toml         # Rust project config
├── Cargo.lock         # Locked dependency versions — ensures reproducible builds
├── setupDB.sh         # Utility: create a single static `files` table manually
├── changes.md         # Full change log (v1 → v4)
└── future.md          # Planned features
```

> **Note:** `*.db` files are excluded from version control (`.gitignore`) as they contain
> indexed file paths from your environment. Each user generates their own on first run.

### Building the binaries

Pre-built binaries are included for macOS and Linux x86_64. To rebuild:

```bash
make        # builds both diskunder-macos and diskunder-linux
make mac    # macOS only
make linux  # Linux only
make clean  # remove build artifacts and binaries
```

**Prerequisites for Linux cross-compilation (macOS host):**
```bash
brew install FiloSottile/musl-cross/musl-cross
rustup target add x86_64-unknown-linux-musl
```

The Linux binary is statically linked against musl libc — no system dependencies required on the target machine.
