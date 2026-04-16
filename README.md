# diskUnder

A command-line tool for indexing file metadata and detecting duplicate files/folders across a filesystem or network share — a CLI equivalent of [DiskOver](https://diskoverdata.com/). File metadata is stored in a local SQLite database, enabling fast queries for duplicates, large files, and directory summaries.

---

## Background

The project was developed in multiple languages, each addressing the limitations of the previous:

- **Bash (v1):** Initial proof-of-concept — simple but slow due to sequential per-file processing
- **Bash (v2):** Optimised with parallel `xargs`, deferred index creation, and SQL batching
- **Python:** More structured, with multiprocessing and bulk inserts — but ultimately too slow for large network shares
- **Rust v1** (`diskUnder.rust`): Best performance, but contained bugs around network share resilience and resumability
- **Rust v2** (`diskUnder2.rust`): All known bugs fixed — recommended for scripted/headless use
- **Rust v3** (`diskUnder3.rust`): Full TUI with live progress and duplicate detection — recommended for interactive use

See [changes.md](changes.md) for a detailed change log and [future.md](future.md) for planned features.

---

## How It Works

Each implementation walks a target directory, collects file metadata (size, dates, checksum), and writes it to a SQLite database. Multiple paths can be indexed into the same database — each path gets its own table named `files_<MD5(path)>`.

Two modes are supported:

- **`index`** — full traversal of the target path
- **`update`** — only files modified in the last 24 hours

### Database Schema

```sql
CREATE TABLE files_<MD5_HASH> (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    path          TEXT NOT NULL UNIQUE,
    filename      TEXT NOT NULL,
    size          INTEGER NOT NULL,
    modified_date TEXT,
    access_date   TEXT,
    change_date   TEXT,
    checksum      TEXT,       -- MD5 (Python/Bash) or BLAKE3 (Rust v2/v3)
    extension     TEXT,
    directory     TEXT,
    created_at    TEXT DEFAULT CURRENT_TIMESTAMP
);
```

Indexes are created on `size`, `filename`, `checksum`, `extension`, `directory`, `access_date`, and `modified_date`.

### SQLite Optimisations (all implementations)

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = 100000;
```

---

## Rust Implementations

### diskunder3 — TUI version (recommended for interactive use)

Built from `diskUnder3.rust`. Displays a live full-screen terminal UI with real-time progress,
duplicate detection, and an activity log. Falls back to plain text automatically when stdout
is not a terminal (cron, SSH, pipes).

**Build:**
```bash
cp diskUnder3.rust src/main.rs && cargo build --release
cp target/release/diskunder diskunder3
```

**Usage:**
```bash
./diskunder3 index <PATH> [OPTIONS]
./diskunder3 update <PATH> [OPTIONS]
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
./diskunder3 index /Volumes/NAS --db /data/index.db --threads 4

# Headless / scripted — auto plain text
./diskunder3 index /Volumes/NAS --db /data/index.db | tee run.log

# Explicit plain text in an interactive terminal
./diskunder3 --no-tui index /Volumes/NAS --db /data/index.db

# Update mode — re-index files changed in last 24 hours
./diskunder3 update /Volumes/NAS --db /data/index.db
```

**Plain text output format:**
```
index /Volumes/NAS
found 772087 files
  1000/772087 processed — 998 written, 2 errors/skipped
  2000/772087 processed — 1991 written, 9 errors/skipped
  ...
complete: 772087 files — 511597 written, 22683 errors/skipped, 14 duplicate groups
wasted (top 10): 3.2 GB
```

**Dependencies:** `blake3`, `chrono`, `crossterm`, `md5`, `rayon`, `ratatui`, `rusqlite`, `structopt`, `walkdir`

---

### diskunder2 — plain text version (recommended for scripted use)

Built from `diskUnder2.rust`. All resilience and database fixes from the original Rust v1,
with plain progress output to stderr. No TUI dependency.

**Build:**
```bash
cp diskUnder2.rust src/main.rs && cargo build --release
cp target/release/diskunder diskunder2
```

**Usage:**
```bash
./diskunder2 index <PATH> [OPTIONS]
./diskunder2 update <PATH> [OPTIONS]
```

**Options:**

| Flag | Default | Description |
|---|---|---|
| `--db <FILE>` | `fileindex.db` | SQLite database path (created if absent) |
| `--threads <N>` | `4` | Parallel hashing threads — reduce for network shares |
| `--verbose` / `-v` | off | Print each file path as it is hashed |

```bash
./diskunder2 index /Volumes/NAS --db /data/index.db --threads 4
./diskunder2 update /Volumes/NAS --db /data/index.db
./diskunder2 index /Volumes/NAS --db /data/index.db --verbose 2>run.log
```

**Dependencies:** `blake3`, `chrono`, `md5`, `rayon`, `rusqlite`, `structopt`, `walkdir`

---

### Rust v1 (`diskUnder.rust`) — original, for reference only

The original Rust implementation. Superseded by v2 and v3. Kept for reference. See
[changes.md](changes.md) for the full list of issues that were fixed.

---

## Other Implementations

### Python (`diskUnder.py`)

Uses `multiprocessing` for parallel file processing and batched SQLite inserts (100,000
records per flush). MD5 checksums. Works but is significantly slower than the Rust versions
on large shares.

**Requirements:** Python 3 (stdlib only — no external dependencies)

**Usage:**
```bash
python3 diskUnder.py index <PATH> [-v]
python3 diskUnder.py update <PATH> [-v]
```

---

### Bash v2 (`diskUnder02.sh`) — recommended Bash version

Uses `xargs -P $(nproc)` for parallel processing, deferred index creation, and SQL statement
buffering. BLAKE2b checksums via `b2sum`.

**Note:** Uses Linux-style `stat` — will not work on macOS without substituting `gstat`
from GNU coreutils.

**Usage:**
```bash
./diskUnder02.sh index <PATH> [-v]
./diskUnder02.sh update <PATH> [-v]
```

---

### Bash v1 (`diskUnder.sh`)

Original sequential implementation. Simple but slow on large directories.

**Usage:**
```bash
./diskUnder.sh index <PATH> [-v]
./diskUnder.sh update <PATH> [-v]
```

---

## Reporting

Two reporting scripts query the database and print summaries to stdout.

**`diskReport.sh <PATH>`** — three reports for the given path:
- Top 10 largest files
- Top 10 directories by total size with file counts
- Files with duplicate checksums (grouped by hash)

**`diskReport02.sh`** — same reports with improved formatting. Table name is hardcoded —
edit the variable at the top of the script before use.

```bash
./diskReport.sh /path/to/indexed/directory
```

---

## Utility Scripts

| Script | Purpose |
|---|---|
| `setupDB.sh` | Creates a single static `files` table (for single-directory use) |
| `DBtest.sh` | Drops and recreates the database, inserts a test record |
| `DBsize.sh` | Lists all `files_*` tables with record count and size in MB |

---

## Choosing the right binary

| Situation | Use |
|---|---|
| Interactive terminal, want live progress | `diskunder3` |
| Cron job, SSH, piped output | `diskunder3` (auto-detects) or `diskunder2` |
| Want to force plain text in a terminal | `diskunder3 --no-tui` |
| Scripting, need predictable stderr output | `diskunder2` |

---

## Comparison

| Feature | Python | Rust v1 | Rust v2 | Rust v3 (TUI) |
|---|---|---|---|---|
| Hashing | MD5 | BLAKE3 | BLAKE3 | BLAKE3 |
| Parallelisation | `multiprocessing` | Rayon | Rayon | Rayon |
| Batch transactions | Yes (100K) | No (single) | Yes (1K) | Yes (1K) |
| Resumable after crash | No | No | Yes | Yes |
| Network share resilience | Partial | Poor | Yes | Yes |
| TUI progress | No | No | No | Yes |
| Auto plain-text fallback | — | — | — | Yes |
| `--db` flag | No | No | Yes | Yes |
| `--threads` flag | No | No | Yes | Yes |

---

## Known Issues

- **`diskReport02.sh`:** Table name is hardcoded; must be manually updated before use.
- **Bash implementations:** Use Linux-style `stat -c` — will not work on macOS without
  modification (use `gstat` from GNU coreutils, or adapt the stat calls).
- **Rust v1 (`diskUnder.rust`):** Contains the original unresolved bugs — use v2 or v3 instead.

---

## Files

```
diskUnder/
├── src/main.rs        # Active Cargo source — currently diskUnder3.rust
├── diskUnder3.rust    # Rust v3 — TUI with live progress (primary)
├── diskunder3         # Compiled macOS ARM64 binary — rebuild if needed (see below)
├── Cargo.toml         # Rust project config
├── Cargo.lock         # Locked dependency versions — ensures reproducible builds
├── setupDB.sh         # Utility: initialise a single-directory database manually
├── changes.md         # Full change log (v1 → v2 → v3)
├── future.md          # Planned features
└── old/               # Superseded versions kept for reference
    ├── diskUnder.rust     # Rust v1 — original (known bugs, see changes.md)
    ├── diskUnder2.rust    # Rust v2 — resilience & DB fixes, no TUI
    ├── diskUnder.py       # Python implementation (too slow for large shares)
    ├── diskUnder.sh       # Bash v1 (sequential, slow)
    ├── diskUnder02.sh     # Bash v2 (parallel)
    ├── diskReport.sh      # Reporting queries
    ├── diskReport02.sh    # Reporting queries (formatted)
    ├── DBtest.sh          # Database test/reset
    └── DBsize.sh          # Table size summary
```

> **Note:** `fileindex.db` is excluded from version control (`.gitignore`) as it contains
> indexed file paths from your environment. Each user generates their own on first run.

### Rebuilding the binary

`diskunder3` is compiled for macOS ARM64. If you are on a different architecture or OS,
rebuild from source:

```bash
cargo build --release
cp target/release/diskunder diskunder3
```
