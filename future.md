# diskUnder — Future Feature Ideas

Inspired by feature parity with [Diskover](https://diskoverdata.com/). Items are grouped by effort and value. None of these are commitments — just ideas worth revisiting.

**Done in v5** (small releases — see [changes.md](changes.md)): 5.0 run log, `search` and `export` (**done**) · 5.1 `report` with duplicate filtering (**done**) · 5.2
include/exclude file types and `--dry-run` preview (**done**) · 5.3 path patterns (Ignore) (**done**) · 5.4 Jobs (**done**) ·
5.5 interactive app (TUI) (**done**). **Planned for v6:** quarantine, then `dedupe` with its dry run. The sections below are the
original ideas.

### Plan for large estates (hundreds of TB, 10–20 servers)

Decided October 2026: **one shared index database**, runs one at a time (never two jobs on
the same mount at once). Comparing several databases at query time (SQLite `ATTACH`, about
10 at once by default) and runs at the same time are left until someone asks for them.

- **5.6 (done):** volume identity per indexed path, so one share reached through different
  mounts or machines — or overlapping indexed paths — isn't counted as duplicates of itself;
  one run per database (a lock with a clear message); all-paths Duplicates in the app.
- **5.7 (done) — read less:** only files whose size another file shares are read (across every
  share, which one database makes possible): a quick hash of the start and end first, a full
  hash only for those still matching. A file's checksum stays empty until another of its size
  appears; that run also reads the matching file in another path if it's reachable.
  `--hash-all` keeps the old behaviour. Linux opens files with `O_NOATIME` where allowed.
- **5.7 (done) — big databases:** measured on a synthetic 50-million-row index, then fixed: a
  duplicate summary table kept current by each run, capped search counts with a plan chosen
  by how many files match, and a fast `db info` (numbers in changes.md). Still to watch: a
  run checks its own unread files against every path (about a minute per 2.5 million files),
  and `report --by-directory` over every path takes about a minute at that size.
- **v6 — quarantine, then dedupe:** volume identity decides what's possible. Copies on the
  same filesystem can share blocks (Linux `FIDEDUPERANGE` on XFS / Btrfs, run on the server:
  the kernel checks the bytes match before sharing them, so a changed file is never touched)
  or be hard-linked; copies on different volumes can only be kept once and the rest
  quarantined. The reflink section below should say `FIDEDUPERANGE`, not `FICLONE`, for
  existing files.
- **Access times:** on Linux (`relatime`) reading a file sets its access time; since 5.7 runs
  open files with `O_NOATIME` where allowed (files the indexing user owns, or as root), and
  read far fewer files anyway. macOS local disks don't change; NFS / SMB depend on the server
  — being tested.

---

## Done — now in the README

The original "high value, low effort" ideas have all shipped; the README has the details and
examples, and [changes.md](changes.md) has each release:

- **`search`** and **`export`** (5.0) — [Searching and exporting](README.md#searching-and-exporting)
- **`report`** (5.1), with duplicate filtering (`--min-size`, `--min-copies`, `--min-wasted`,
  `--ext`) and the closing summary — [Reports](README.md#reports)
- **Include / exclude file types** (5.2) and **path patterns** (Ignore, 5.3), with
  `--dry-run` — [Usage](README.md#usage)
- **Jobs** — named, saved runs with history (5.4) — [Jobs](README.md#jobs)

---

## Medium value, moderate effort

### `watch` mode — continuous incremental updates
Replace scheduled `update` runs (which walk the whole tree) with OS-native filesystem events.
Uses the `notify` crate (wraps FSEvents on macOS, inotify on Linux) to respond to actual
file changes rather than polling on a schedule.

```
diskunder watch /nas --interval 15m
```

### Rules / automation engine
Define rules that are evaluated against the index and trigger an action. Rules stored in
a separate table in the same SQLite DB.

```
diskunder rules add --path /nas/tmp --older-than 7d --action flag
diskunder rules add --path /nas --duplicate --larger-than 1GB --action report
diskunder rules run
```

Possible actions: `flag` (mark in DB), `report` (print to stdout), `delete` (with
explicit confirmation prompt — never silent).

### Interactive app (TUI)
**Done in 5.5 (5.5.1–5.5.6)**. Replaces the earlier "results
browser TUI" idea with a full app: `diskunder` with no subcommand opens a main window listing
jobs and indexed paths (with status and last-run details), and menus for **Jobs** (create,
run, dry run, edit, status, remove, test job), **Results** (latest, search, duplicates, reports, browse,
history, errors, logs), **Database** (info, check, vacuum, drop, open, backup), **Host** (drives & mounts, host info) and
**Settings** (start-up, defaults, file types, deletion, logging). Every screen shows the
equivalent CLI command. It also absorbs the dedupe "TUI selection" idea below for v6.

---

## Safe deletion

**Planned for v6, in this order:** quarantine (with manifest, `purge`, `restore` and a
`status` column), then `dedupe` built on it, with `--dry-run`. Default copy to keep: the
oldest modified. The `dedupe` design is [below](#dedupe-command--design).

Deletion is the most dangerous operation the tool could perform, especially on a network
share where mistakes are hard to reverse. All deletion features should be off by default
and require an explicit flag or confirmation step. The options below are roughly ordered
from safest to most aggressive.

### `dedupe` command — design

Worked out during v5, for v6:

```text
diskunder dedupe /nas --dry-run
diskunder dedupe /nas --dry-run --min-size 500MB --remove-under /nas/backup
diskunder dedupe /nas --dry-run --keep newest --plan dedupe-plan.csv
```

```text
render.mov — 1.6 GB × 3 copies
  KEEP    /nas/projects/2024/final/render.mov     modified 2024-11-01
  REMOVE  /nas/backup/render.mov                  modified 2023-06-14
  REMOVE  /nas/temp/render.mov                    modified 2023-06-14

Would remove 2 files, freeing 3.2 GB (dry run — nothing was changed)
```

- **Choosing the copy to keep:** `--keep oldest` (**default** — earliest modified, usually
  the original) `| newest | shortest-path | first-path`; `--keep-under PATH` prefers copies
  there.
- **Limiting removals:** `--remove-under PATH` only ever proposes removing copies under PATH
  (e.g. backup folders); the 5.1 duplicate filters apply too.
- **Hard links:** paths sharing a device and inode are one copy. A hard link to the kept copy
  is shown as `LINKED` (removing it frees nothing) and is never proposed for removal.
- **`--plan FILE`** writes a CSV (group, action, path, size, checksum, modified, device,
  inode) to review or edit; `dedupe --apply FILE` acts on a reviewed plan, re-checking each
  file before touching it.
- **`--verify`** checks each path still exists with the indexed size and modified time
  (stat only, no hashing) and flags anything that's changed since indexing.

### Dry run — always available, never touches files

```
diskunder dedupe /nas --dry-run
diskunder dedupe /nas --min-size 500MB --dry-run
```

Shows exactly which copy would be kept, which would be removed, and the total space
reclaimed — without touching anything. Should arguably be required before any real action
is allowed.

### Duplicate filtering for targeted deletion

Avoid acting on trivial matches by filtering before any deletion step:

```
diskunder dedupe /nas --min-size 100MB          # ignore duplicates smaller than 100 MB
diskunder dedupe /nas --min-copies 3            # only act on files with 3+ copies
diskunder dedupe /nas --min-wasted 500MB        # only groups wasting more than 500 MB total
diskunder dedupe /nas --extension iso,dmg,vhd   # restrict to specific file types
diskunder dedupe /nas --path /nas/backup        # only remove copies under this path
```

Filters compose — e.g. `--min-size 1GB --min-copies 3 --path /nas/backup` targets only
large, heavily duplicated files whose redundant copies live under the backup path.

### Default action: quarantine folder

Move duplicates to a dated staging directory rather than deleting in-place. All three
paths still appear in the DB (tagged `quarantined`) so the quarantine can be reversed.

```
diskunder dedupe /nas --min-size 500MB
# default action: quarantine

# Staged copies land at:
/nas/.diskunder_trash/2026-04-16/nas/backup/video.mp4
/nas/.diskunder_trash/2026-04-16/nas/temp/video.mp4

# Purge after a grace period (irreversible):
diskunder purge --older-than 7d
diskunder purge --older-than 7d --dry-run    # preview first
```

### Rename to `.bak` (lighter alternative to quarantine)

Renames the redundant copy in-place — no files move, no space is reclaimed, but the
original name is freed and the data is safe.

```
diskunder dedupe /nas --min-size 500MB --action bak
# /nas/backup/video.mp4  →  /nas/backup/video.mp4.bak
```

Useful when the goal is to free a filename rather than reclaim space.

### Deletion manifest

Written before any file is touched, regardless of action chosen. Records the original
path, destination (for quarantine/bak), and checksum so the operation can be audited
or reversed.

```
# diskunder deletion manifest — 2026-04-16 10:32:01
# Restore with: diskunder restore --manifest manifest-2026-04-16.log
QUARANTINE  /nas/backup/video.mp4  →  /nas/.diskunder_trash/2026-04-16/...  blake3:9df4c2…
QUARANTINE  /nas/temp/video.mp4    →  /nas/.diskunder_trash/2026-04-16/...  blake3:9df4c2…
```

### Permanent delete — explicit opt-in only

```
diskunder dedupe /nas --min-size 500MB --action delete
```

Requires an additional `--confirm` flag (or interactive prompt) on top of `--action delete`.
Never the default. Always writes a manifest first.

### Reflinks (copy-on-write clones) — best option for BTRFS / ZFS / APFS

A reflink creates a new file that shares the underlying data blocks with the original.
Both files appear full-size in `ls` but occupy almost no extra disk space. Unlike a hard
link, the two files are completely independent — writing to one only duplicates the
changed blocks (copy-on-write), so editing or deleting one has no effect on the other.
This makes reflinks safer and more flexible than hard links for deduplication.

```
diskunder dedupe /nas --action reflink
```

**Filesystem support:**
| Filesystem | Support | Mechanism |
|---|---|---|
| XFS | Yes (kernel 4.16, 2018) | `FICLONE` ioctl |
| BTRFS | Yes (stable) | `FICLONE` ioctl |
| ZFS / OpenZFS 2.2+ | Yes (2023+) | `FICLONE` ioctl |
| APFS (macOS) | Yes | `clonefile()` syscall |
| ext4, NTFS, FAT | No | Falls back to regular copy |

XFS is the default filesystem on RHEL, Rocky, and most enterprise Linux distributions,
so reflink support is available on the majority of production Linux file servers without
any filesystem migration.

**Over Samba — partially viable:**
SMB3 has a protocol-level equivalent (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`, also called
server-side copy / ODX). Samba 4.x exposes this when the server filesystem supports
reflinks — configured with `vfs objects = btrfs` in `smb.conf`. With this in place,
the reflink operation happens entirely on the server; the data never travels over the
network. macOS Finder already uses this transparently when copying files on compatible
SMB3 shares. Requires the Samba server to be explicitly configured — most default
installations won't have it enabled.

**Implementation:**
The `reflink-copy` Rust crate handles all platforms with graceful fallback:
```toml
reflink-copy = "0.1"
```
Tries `FICLONE` on Linux, `clonefile()` on macOS, and falls back to a regular file
copy if the filesystem doesn't support reflinks — meaning the same code path works
everywhere without crashing.

---

### Hard links — lossless space reclaim (local filesystem only)

Replace redundant copies with hard links to a single on-disk inode. All paths continue
to work; only one copy exists on disk. No data is ever at risk.

```
diskunder dedupe /nas --action hardlink
```

**Limitations:**
- Only works within the same filesystem/volume — won't help across different shares or
  mount points.
- **Will not work over Samba/SMB.** Hard links are a filesystem-level operation; the SMB
  protocol does not reliably expose them to network clients. macOS's SMB client returns
  `Operation not supported` for hard link calls on network mounts. Linux CIFS mounts can
  support them only if the server has `unix extensions = yes` in `smb.conf`, which most
  corporate Samba configurations disable for Windows client compatibility.
- **Only viable if diskunder runs directly on the server** — i.e. as a native binary or
  package on the NAS itself (e.g. Synology, QNAP), bypassing SMB and operating on the
  local filesystem directly. In that case, hard links work normally on ext4, XFS, Btrfs,
  and ZFS.

### TUI selection — choose which copy to keep interactively

Rather than auto-deciding based on rules, surface each duplicate group in the browser TUI
and let the user pick:

```
┌─ Duplicate group — 1.6 GB × 3 copies ──────────────────────────────┐
│                                                                      │
│  [KEEP]   /nas/projects/2024/final/render.mov   modified 2024-11-01 │
│  [REMOVE] /nas/backup/render.mov                modified 2023-06-14 │
│  [REMOVE] /nas/temp/render.mov                  modified 2023-06-14 │
│                                                                      │
│  Space reclaimed: 3.2 GB     Action: quarantine                     │
│                                                                      │
│  [↑↓] navigate  [Space] toggle  [K] keep all  [A] action  [Enter] apply │
└──────────────────────────────────────────────────────────────────────┘
```

Space to toggle keep/remove per file. `[A]` cycles through quarantine / bak / delete /
hardlink. `[Enter]` applies to this group and advances to the next.

### Summary of deletion options

| Option | Space reclaimed | Reversible | Notes |
|---|---|---|---|
| `--dry-run` | No | — | Should always be available |
| `--action bak` | No | Yes — rename back | Simple, in-place |
| `--action quarantine` *(default)* | After `purge` | Yes — restore command | Best default |
| `--action reflink` | Yes, immediately | No | BTRFS/ZFS/APFS; partial SMB3 support |
| `--action hardlink` | Yes, immediately | No | Local filesystem only — not over SMB |
| `--action delete` | Yes, immediately | No | Requires `--confirm` |
| Manifest | — | Enables restore | Written before any action |
| TUI selection | Depends | Depends on action | Best UX for interactive use |

---

## Useful but more involved

### Metadata enrichment
Collect richer per-file metadata for specific types and store it in extension columns.

| File type | Extra metadata | Approach |
|---|---|---|
| Images (JPEG, RAW, TIFF) | Resolution, camera, GPS, date taken | `kamadak-exif` crate |
| Video (MP4, MOV, MKV) | Duration, codec, resolution, bitrate | `ffprobe` subprocess |
| Audio (MP3, FLAC, WAV) | Duration, bitrate, artist, album | `ffprobe` or `id3` crate |

Enrichment would run as an optional pass after indexing, adding columns to the table
on first encounter.

### `ask` — natural language queries via Claude API
Translate plain English questions into SQL against the index schema, run the query,
and format the result in plain language. The schema is simple and consistent enough
for this to work well.

```
diskunder ask "what file types are wasting the most space in /nas/projects?"
diskunder ask "which directories haven't been touched in over a year?"
diskunder ask "show me all duplicate videos larger than 2GB"
```

Would require an `ANTHROPIC_API_KEY` environment variable and the `anthropic` Rust crate
(or an HTTP client hitting the Claude API directly).

---

## Probably not worth it for a CLI tool

- **Web UI** — embedding an HTTP server adds significant complexity; `export` + an
  external tool (Grafana, Metabase, Excel) is more practical
- **Distributed / multi-node indexing** — SQLite doesn't scale to that level; would
  require migrating to Postgres or Elasticsearch
- **User / permission management** — Diskover's enterprise auth layer is out of scope
  for a single-user CLI tool

---

## Schema additions to consider

Some features above would benefit from schema changes. Notes for when that work starts:

- `include_ext` / `exclude_ext` on `roots` — stored file-type filters (done in 5.2)
- `exclude_paths` on `roots` — stored path patterns (done in 5.3)
- Lowercase `extension` and `PRAGMA user_version` migrations — done in 5.2
- ~~Per-path run metadata~~ — done in v4.1 as the `roots` table (path, first/last run, mode,
  whether the last run finished); `diskunder db info` reads it
- `last_seen` column — timestamp of last successful index pass; enables true stale
  detection and orphan identification (files present in DB but not seen in latest scan)
- `tags` column — free-text or JSON array; supports the rules engine and manual flagging
- `owner` column — file owner username; enables per-user storage reporting
- `enriched_at` column — timestamp of last metadata enrichment pass
- Separate `rules` table — stores rule definitions for the automation engine
- Separate `tags` table — normalised tag storage if tag querying becomes important
- `status` column — `active` / `quarantined` / `deleted`; updated by dedupe actions so
  reports can exclude already-handled files
- Separate `manifests` table — records every dedupe action for audit and restore support
- `jobs`, `job_runs`, `job_run_errors` and `settings` tables (in the per-user store, not the
  index database) — saved jobs, one row per run with timings, outcome, summary counts and up
  to 50 errors, and app settings (done in 5.4 / 5.5)
