# diskUnder — Future Feature Ideas

Inspired by feature parity with [Diskover](https://diskoverdata.com/). Items are grouped by effort and value. None of these are commitments — just ideas worth revisiting.

---

## High value, low effort

These build directly on the existing SQLite schema and require no new dependencies.

### `search` subcommand
Query the existing index without re-indexing. The DB already has indexes on name, size, extension, directory, and dates — it just needs a CLI query builder in front of it.

```
diskunder search /nas --name "*.log"
diskunder search /nas --older-than 90d --larger-than 500MB
diskunder search /nas --duplicate --larger-than 1GB
diskunder search /nas --extension iso,dmg,vhd
```

### `report` subcommand
A proper replacement for the basic `diskReport.sh` scripts. All data is already in the DB.

```
diskunder report /nas --top-files 20        # largest files
diskunder report /nas --by-type             # space consumed per extension
diskunder report /nas --stale 180d          # files not accessed in N days
diskunder report /nas --duplicates          # duplicate groups by wasted space
diskunder report /nas --cost 0.023          # apply a $/GB/month rate for cost overlay
diskunder report /nas --by-directory        # top directories by total size
```

#### Duplicate filtering and savings summary

The `--duplicates` report should support filters so large shares don't drown output
in thousands of trivial matches:

```
diskunder report /nas --duplicates --min-size 100MB
diskunder report /nas --duplicates --min-size 1GB --min-copies 3
diskunder report /nas --duplicates --extension iso,dmg,vhd
diskunder report /nas --duplicates --min-wasted 500MB   # filter by total wasted per group
```

Every duplicate report should close with a savings summary:

```
Duplicate groups found : 47
Files that are copies  : 183
Total wasted space     : 142.3 GB
Reclaimable (1 copy)   : 142.3 GB
Reclaimable (quarantine): 142.3 GB  ← what dedupe would free
Storage cost saving    : $3.27/month  (at $0.023/GB/month)
```

### `export` subcommand
Makes the index consumable by BI tools (Tableau, Power BI, Excel) and AI pipelines.

```
diskunder export /nas --format csv  > report.csv
diskunder export /nas --format json > report.json
```

### Jobs — named, saved runs
Save a run's settings under a name so it can be repeated without retyping paths and flags,
and keep a history of every run so it's easy to see when each job last ran and how it went.

```
diskunder jobs add nas-nightly update /Volumes/NAS --db /data/index.db --threads 4
diskunder jobs list                     # every job with its last run, status and duration
diskunder jobs show nas-nightly         # settings plus recent run history
diskunder jobs remove nas-nightly

diskunder run nas-nightly               # run a saved job by name (cron-friendly)
diskunder run                           # no name: pick a job from a menu
```

`jobs list` output:

```
NAME          MODE    PATH           LAST RUN           STATUS     DURATION  FILES    ERRORS
nas-nightly   update  /Volumes/NAS   2026-10-22 02:00   completed  14m 32s   772,087  22,683
projects      index   /Volumes/Proj  2026-10-19 18:45   failed     3m 10s    41,200   1
scratch       index   /tmp/scratch   never              —          —         —        —
```

**Menu:** `diskunder run` with no name opens a TUI list of saved jobs showing the same
columns. Arrow keys to choose, Enter to run, `q` to quit. In plain mode (no terminal), a
missing name is an error that lists the available jobs instead.

**Run history:** each run records start and end time, how it ended (completed, failed, or
interrupted), and the summary counts the tool already prints: found, skipped, written,
removed, errors, duplicate groups and wasted bytes. `jobs show` lists recent runs, and
`jobs list` shows the latest.

**Where jobs live:** a job names its own index database (`--db`), so job definitions and
history can't sit inside an index database. They belong in a small per-user database
(e.g. `~/.config/diskunder/jobs.db`), overridable with `--jobs-db` or an environment
variable for shared or service-account setups.

**Details worth deciding up front:**
- Running a job twice at once should be refused (a lock per job), since two runs against
  the same table would fight.
- Run-time flags such as `--no-tui` should still work on `run`; any setting given on the
  command line overrides the saved one for that run only.
- Job names are case-sensitive and limited to letters, digits, `-` and `_`, so they're safe
  in cron lines and shell scripts.

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

### Results browser TUI
A second TUI mode for exploring the index after indexing is complete — separate from
the indexing progress TUI in v3.0. Would allow keyboard navigation through duplicate
groups, directory trees, and search results without dropping to a SQL prompt.

```
diskunder browse /nas
```

---

## Safe deletion

Deletion is the most dangerous operation the tool could perform, especially on a network
share where mistakes are hard to reverse. All deletion features should be off by default
and require an explicit flag or confirmation step. The options below are roughly ordered
from safest to most aggressive.

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
- `jobs` and `job_runs` tables (in a per-user jobs database, not the index database) — saved
  job definitions, and one row per run with timings, outcome and summary counts
