# diskUnder v5 — plan

Search, report (with duplicate filtering), export, a run log, include/exclude filters for
file types and paths, saved jobs, and an interactive app — in six small releases.

**Out of scope for v5:** acting on duplicates. Quarantine comes first, then `dedupe` and its
dry run — both v6 (outline under [Deferred to v6](#deferred-to-v6--quarantine-then-dedupe)).
Also not in v5: `watch`, metadata enrichment. Cost/savings-in-money reporting is dropped.

---

## Summary

- **Mostly query work on data we already store.** Search, report and export need no new file
  columns. Timed on a synthetic 1,000,000-row table with the v4 schema and indexes (300 MB
  database, Apple Silicon laptop):

  | Query | Time |
  |---|---|
  | Name glob (`*.log`) | 0.03 s |
  | Size + modified-date filter | 0.18 s |
  | Top 20 largest files | < 0.01 s |
  | Totals by file type / by directory | 0.24 s / 0.25 s |
  | All duplicate groups, hard-link aware | 0.29 s |
  | Every path in every duplicate group (input for v6 `dedupe`) | 0.23 s |
  | Export every row to CSV (131 MB) | 2.3 s |

- **Schema changes are small:** store extensions lowercase, add three filter columns to
  `roots`, and start versioning migrations with `PRAGMA user_version`. Details in
  [Schema changes](#schema-changes).
- **Refactor first.** `src/main.rs` is 1,764 lines in one file; v5 adds four commands that
  share filtering and output code. Splitting it into modules is behaviour-neutral and the 29
  existing tests guard it.
- **Releases:** 5.0 groundwork + run log + search + export → 5.1 report → 5.2 file-type
  filters and `--dry-run` preview → 5.3 path patterns → 5.4 jobs → 5.5 interactive app.
  Each builds on the ones before: jobs carry the 5.2/5.3 filters and use `--dry-run` for
  "test"; the app is a front end to everything in 5.0–5.4.

---

## Shared design

Search, report and export all answer "which files match?" and print the result, so they
share one filter language and one output layer.

### Filters

Every query command accepts the same flags, translated to a SQL `WHERE` clause with bound
parameters (never string-built from user input):

| Flag | Matches | Notes |
|---|---|---|
| `--name GLOB` | file name | e.g. `"*.log"`; `--ignore-case` for case-insensitive |
| `--ext jpg,mov` | extension | case-insensitive; uses the extension index once stored lowercase |
| `--larger-than SIZE` / `--smaller-than SIZE` | size | `500MB`, `1.5GB`, `200K` — 1024-based, matching the tool's output |
| `--older-than AGE` / `--newer-than AGE` | modified date | `90d`, `6w`, `3m`, `1y`, or a date `2024-01-01` |
| `--not-accessed AGE` | access date | see the access-time caveat under `report --stale` |
| `--under PATH` | path prefix | restrict to a sub-folder of the indexed path |
| `--duplicate` | files with at least one other copy | hard links don't count as copies |

These query-time filters are separate from the indexing filters in 5.2 and 5.3, which decide
what gets indexed in the first place.

### Which index is queried

A path argument selects that path's table through `roots` (canonicalised, so any spelling
works). With no path, the command covers every recorded path through a temporary `UNION ALL`
view built at query time. That also gives **cross-share duplicates** — the same file on
`/nas` and `/projects` — without changing how data is stored.

### Output

- **A readable table by default — the same whether or not output is piped.** Output that
  changes when you add `| less` or `| tee` is surprising, and scripts should state their
  format explicitly anyway. When stdout isn't a terminal, only the terminal niceties change:
  no colour, and long paths aren't shortened to fit the window.
- **stdout carries only data; notes go to stderr.** stdout gets the header row and result
  rows. stderr gets "index as of …", "last run didn't finish", and "… 340 more — use
  `--limit`". So `| grep`, `| sort` and `> file` get clean rows, and the warnings still show
  in the terminal. This matches how indexing already sends progress to stderr.
- **`--limit`** defaults to 100 for the table; `--limit 0` means no limit.
- **`--format csv | tsv | json`** for machines. JSON is newline-delimited (one object per
  line) so million-row exports stream instead of building one giant array.
- **`--paths`** prints bare paths, one per line; `-0` separates with NUL for `xargs -0`.

| Use | Command |
|---|---|
| Reading in a terminal or `less` | `diskunder search /nas --ext mov` |
| Spreadsheet / BI | `diskunder export /nas > nas.csv` (CSV by default) |
| Scripts | `diskunder search /nas --ext mov --format tsv --limit 0` |
| `xargs` | `diskunder search /nas --ext tmp --paths -0 \| xargs -0 …` |

### Freshness

Queries read the index, not the disk. Human-readable output starts with
`index as of <last finished run>` from `roots`, and warns if the last run didn't finish.
Run `update` first for current answers.

---

## 5.0 — run log, `search` and `export` ✅

**Done.** As designed below, plus: a run that stops on a fatal error now exits with status 1.
See [changes.md](changes.md).

### Run log

A record of every command that changes a database — what was called, how it ended, a summary
of the results, and any errors with running it. **Not** a per-file log: per-file read errors
stay as counts, as they are in the summary today.

```text
2026-10-23T02:00:01+01:00 run=7f3a2c start  index /Volumes/NAS --db /data/index.db --threads 4  (user=svc-diskunder host=nas-util01 version=5.0)
2026-10-23T02:00:09+01:00 run=7f3a2c error  traversal: /Volumes/NAS/Finance — Permission denied (os error 13)
2026-10-23T02:14:34+01:00 run=7f3a2c end    completed in 14m33s — 772,087 found, 0 already indexed, 749,404 written, 0 removed, 22,684 errors (22,683 unreadable, 1 traversal/db), 1,408 duplicate groups, 3.2 GB wasted
2026-10-23T09:12:40+01:00 run=91be04 start  db drop /Volumes/OldShare --yes --db /data/index.db  (user=nathan.taylor host=DPTMac2607 version=5.0)
2026-10-23T09:12:40+01:00 run=91be04 end    completed in 0s — removed the index of /Volumes/OldShare (41,200 files)
```

**What's recorded:**

- **`start`:** timestamp, the full command line, user, host and tool version.
- **`error`:** problems with running the command — traversal and database errors, failed
  batches, migrations that stopped a run, fatal errors such as a missing path. These are the
  warnings plain mode already prints. Capped at 50 per run, then `… and N more`, so a share
  with a broken permission tree can't flood the log.
- **`end`:** outcome (`completed`, `failed`, `stopped by user` when `q` is pressed in the
  TUI), duration, exit code when non-zero, and a one-line summary: the same counts as the
  plain-text summary for `index`/`update`, what was done for `db drop`/`db vacuum`.
- **A `start` with no `end`** means the process was killed (Ctrl-C, crash, reboot). The run
  id ties the lines of one run together when two runs share a log.

**Where it goes:** next to the database, as `<database>.log` (`/data/index.db.log`,
`fileindex.db.log` by default). The log stays with the data it describes, cron and
interactive runs land in the same place, and a shared service-account setup needs no
per-user configuration. `--log FILE` writes elsewhere; `--no-log` turns it off.

**Which commands:** only those that change the database — `index`, `update`, `db drop` and
`db vacuum` (which rewrites the file). Read-only commands — `search`, `export`, `report`,
`db info`, `db check`, and the 5.2 `--dry-run` previews — aren't logged. Neither are commands
that fail before a database is involved (bad arguments, `--help`). A schema migration is
logged as part of the run that performs it.

**Details:**

- **Format:** plain text, one event per line, timestamps in ISO 8601 with the UTC offset so
  logs from different servers compare cleanly. Easy to read, and to `grep`/`tail`.
- **Safe with concurrent runs:** each line is a single append, so lines from two runs never
  interleave mid-line.
- **Rotation:** at 10 MB the log is renamed to `<database>.log.1` (replacing any older one)
  and a new log started. At typical use (a few runs a day) that's years of history.
- **Privacy:** the log contains paths, user and host names, like the database itself. Logs
  next to a repository-local database are covered by `.gitignore` (`*.db.log*` added in 5.0).
- **Logging never breaks a run:** if the log can't be written (read-only folder, disk full),
  the command carries on and prints one warning to stderr.
- **Viewing:** `tail`/`grep` the file. A `diskunder log` command (recent runs per database,
  filter by outcome) can follow if wanted; Jobs, when built, will keep structured run
  history per job and can link to these run ids.

**Assessment:** small. The summary counts and error messages already exist (they feed the
plain-text output), so the work is a logger that writes `start`, `error` and `end` lines,
hooks at each exit path (completed, fatal error, `q` in the TUI), rotation, the two flags,
and about 6 tests (start/end written, summary matches, errors capped, `--no-log`, rotation,
unwritable log doesn't fail the run, read-only commands write nothing). Roughly 150–200
lines. It's easier after the Phase 0 module split, so it comes straight after that in 5.0.

### `search`

```text
diskunder search /nas --name "*.log"
diskunder search /nas --older-than 1y --larger-than 500MB
diskunder search /nas --duplicate --larger-than 1GB
diskunder search --ext iso,dmg,vhd            # every indexed path
```

Columns: path, size, modified; `--long` adds checksum (short), access and birth dates.
**Size: S** once the shared filter and output code exists.

### `export`

```text
diskunder export /nas --format csv > nas.csv
diskunder export /nas --format json --ext mov,mp4 > video.ndjson
```

`search` with no row limit, every column plus the indexed root, and CSV as the default
format. Kept as its own command because "export" is what people look for. **Size: S.**

---

## 5.1 — `report` ✅

**Done.** As designed below, plus `--format csv|tsv|json` for single sections and a fuller
`--stale` (total, by folder, largest files). See [changes.md](changes.md).

```text
diskunder report /nas                   # overview: totals, top files, top types, duplicates summary
diskunder report /nas --top-files 20
diskunder report /nas --by-type
diskunder report /nas --by-directory --depth 2
diskunder report /nas --stale 180d
diskunder report /nas --duplicates
```

- **`--by-type`** groups by lowercase extension (see schema change 2).
- **`--by-directory --depth N`** rolls sizes up to N levels below the root.
- **`--stale AGE`** uses the **modified** date. Access times are unreliable on many file
  servers (`noatime`/`relatime` mounts, SMB), so access-based staleness is opt-in with
  `--stale-by access`, and the output says which was used.
- All reports accept the shared filters, e.g. `report --by-type --under /nas/projects`.

### Duplicate report and filtering

```text
diskunder report /nas --duplicates --min-size 100MB
diskunder report /nas --duplicates --min-size 1GB --min-copies 3
diskunder report /nas --duplicates --ext iso,dmg,vhd
diskunder report /nas --duplicates --min-wasted 500MB
diskunder report /nas --duplicates --under /nas/backup
```

| Flag | Keeps groups where… |
|---|---|
| `--min-size SIZE` | each copy is at least SIZE |
| `--min-copies N` | there are at least N distinct copies (hard links count once) |
| `--min-wasted SIZE` | size × (copies − 1) is at least SIZE |
| `--ext LIST` | the files have one of these extensions |
| `--under PATH` | at least one copy is under PATH |

Groups are sorted by wasted space. Each shows size × copies, wasted space, and its paths
(first 5; `--all-paths` for every one), with hard links marked. Every duplicate report ends
with a summary — **no cost figures**:

```text
Duplicate groups       : 47
Files that are copies  : 183
Total wasted space     : 142.3 GB
Largest group          : render.mov — 1.6 GB × 4 copies (4.8 GB wasted)
```

**Size: M.**

---

## 5.2 — Include / exclude file types, and `--dry-run` preview ✅

**Done**, in two parts: the dry run, then the filters. As designed below, plus `--all-types`
to clear stored filters. See [changes.md](changes.md).

```text
diskunder index /nas --include-ext jpg,jpeg,png,heic,mov,mp4
diskunder index /nas --exclude-ext tmp,log,ds_store
diskunder update /nas                      # reuses the stored filters
diskunder update /nas --exclude-ext tmp    # changes them, explicitly
```

### Assessment

**The filtering itself is easy.** One extension check per file during the walk, before any
stat or hash, so excluded files cost nothing — runs get faster on shares full of temporary or
log files.

**The work is in keeping it consistent.** Four parts:

1. **Remembering the filters.** If `update` ran without them it would hash everything that
   was excluded; if it ran with narrower ones, newly excluded files would be missing from the
   walk. So filters are stored per path in `roots` and reused automatically. Passing
   different filters replaces them, and the run says so.
2. **Deletion safety.** `update` removes rows for files not seen on disk. With filters, "not
   seen" must be split into "deleted" and "now excluded". Rows for newly excluded types are
   removed deliberately and reported separately: `removed 120 rows now excluded by filters`.
3. **Matching rules.** Case-insensitive. The extension is the text after the last dot
   (`archive.tar.gz` → `gz`). Files with no extension are matched with the token `none`. An
   include list is an allowlist; exclude is applied after include.
4. **Visibility.** `db info` shows each path's filters, and reports note when the index is
   filtered, so "no duplicates" can't be mistaken for "no duplicates in the files you didn't
   index".

**Schema:** two TEXT columns on `roots` (`include_ext`, `exclude_ext` — normalised lowercase,
sorted, comma-separated), added with `ALTER TABLE ADD COLUMN`.

### `index --dry-run` / `update --dry-run` ✅

**Done.**

Walk the tree without hashing or writing, and print how many files and bytes would be
indexed, by type — and, for `update`, how many rows would be removed as deleted or as newly
excluded. Cheap, and the way to check filters before a long run:

```text
diskunder index /nas --dry-run --include-ext jpg,png,mov
```

**Size: M** — roughly 250–350 lines plus about 10 integration tests (filters applied, stored
and reused, changed explicitly, newly-excluded rows removed and reported separately, `none`
token, case-insensitivity, `db info` display, dry run for `index` and `update`). Most of the
effort is parts 1–2 and their tests; 5.3 reuses all of it.

**Later:** named type groups (`--include-type images,video`), and Jobs carrying filters in
their definition.

---

## 5.3 — Path patterns (Ignore) ✅

**Done.** As designed below, with the two open questions settled: matching is
**case-sensitive** (indexes can live on Linux filesystems even when run from a Mac), and
**`--include-path`** makes exceptions to `--exclude-path` rather than acting as an allowlist.
Also: a trailing `/` matches folders only, and `--no-path-filters` clears stored path filters.
See [changes.md](changes.md).

Skip folders and files by path — snapshot folders, NAS thumbnail caches, recycle bins,
render caches.

```text
diskunder index /nas --exclude-path .snapshot --exclude-path @eaDir --exclude-path "#recycle"
diskunder index /nas --exclude-path "projects/*/render-cache" --exclude-path "**/node_modules"
diskunder update /nas                                   # reuses the stored patterns
diskunder index /nas --dry-run --exclude-path .snapshot # preview the effect
```

### Rules

Modelled on `.gitignore`, which most people already know:

- Patterns match the path **relative to the indexed root**.
- A pattern with no `/` matches a file or folder **name at any depth**: `.snapshot`, `*.tmp`,
  `Thumbs.db`.
- A pattern containing `/` is **anchored at the root**: `projects/*/render-cache`. `*`
  matches within one folder name; `**` matches any number of folders.
- Matching a folder **skips everything under it** — the walk never enters it. On NAS shares
  this is the big win: a `.snapshot` folder can multiply the apparent file count many times
  over.
- File-type filters and path patterns combine: a file is indexed if no path pattern excludes
  it and its type passes the 5.2 filters.

Common candidates:

| Pattern | What it is |
|---|---|
| `.snapshot`, `.zfs` | NetApp / ZFS snapshot folders |
| `@eaDir`, `#recycle`, `#snapshot` | Synology thumbnails, recycle bin, snapshots |
| `$RECYCLE.BIN`, `.Trash-*`, `.Trashes` | Windows, Linux and macOS trash |
| `.DS_Store`, `Thumbs.db`, `._*` | Desktop metadata files |

### Assessment

Built on 5.2's machinery — stored per path, reused by `update`, "excluded" kept separate from
"deleted", shown in `db info`, previewed with `--dry-run` — so the new work is:

- **Pattern matching** with the `globset` crate (the matcher behind ripgrep): one new,
  widely-used dependency.
- **Pruning folders during the walk** (WalkDir's `filter_entry`), so excluded trees are never
  listed.
- **Removing rows under newly excluded folders** in `update`, reported as excluded, not
  deleted — the 5.2 logic applied to path prefixes.

**Schema:** `roots.exclude_paths` (TEXT, one pattern per line).

**Size: S–M** — roughly 150 lines plus about 6 tests.

**Later:** a `--exclude-preset nas` bundle of the common candidates above, and an ignore file
(`.diskunderignore`) at the root of a share.

---

## 5.4 — Jobs ✅

**Done.** As designed below, plus: the store also honours `$XDG_CONFIG_HOME`; `jobs edit`
accepts `--mode` and `--path`; a run that fails before indexing is recorded with the reason;
and the database's folder is created on first run. See [changes.md](changes.md).

Named, saved runs — the design in [future.md](future.md#jobs--named-saved-runs), scheduled
here and extended for the app.

```text
diskunder jobs add nas-nightly update /Volumes/NAS --db /data/index.db --threads 4 --exclude-ext tmp,log
diskunder jobs list
diskunder jobs show nas-nightly
diskunder jobs edit nas-nightly --exclude-path .snapshot
diskunder jobs remove nas-nightly            # asks whether to also drop its index
diskunder run nas-nightly                    # cron-friendly
diskunder run nas-nightly --dry-run          # "test": what would be indexed / removed
```

- **A job is a saved command line:** mode, path, database, threads, file types and path
  patterns. Running it is exactly `index`/`update` with those settings, so the filters still
  end up stored in `roots`, the run log still records it, and everything from 5.0–5.3
  applies unchanged.
- **Test** is `run NAME --dry-run` — the 5.2 preview, using the job's settings.
- **`diskunder run` with no name** lists the jobs and their last result, and says to give a
  name. The interactive picker comes with the app in 5.5, so it isn't built twice.
- **Never two runs of one job at once:** a lock file per job, held with the standard
  library's file locking (no new dependency). A second run exits with "already running".
- **Overrides:** flags given to `run` (e.g. `--no-tui`) apply to that run only.
- **Names:** letters, digits, `-` and `_`, so they're safe in cron lines.

### Where jobs live

Jobs name their own index database, so they can't live inside one. They go in a **per-user
store**, `~/.config/diskunder/diskunder.db` (overridable with `--config-db` or the
`DISKUNDER_CONFIG_DB` environment variable, for service accounts and shared setups). The
same store holds the 5.5 settings.

| Table | Holds |
|---|---|
| `jobs` | name, mode, path, index database, threads, include/exclude types, path patterns, created, updated |
| `job_runs` | one row per run: run id (matching the run log), started, finished, outcome, exit code, and the summary counts — found, skipped, written, removed, errors by kind, duplicate groups, wasted bytes |
| `job_run_errors` | up to 50 error messages per run (same cap as the run log), so the app's **History** and **Errors** can show them for any past run |

**Size: M** — the CLI, the store, locking, and about 10 tests.

---

## 5.5 — Interactive app (TUI)

**5.5.1 done:** main window, Jobs menu, and Results' run views (Latest, History, Errors, Logs).
**5.5.2 done:** Search, Duplicates, Reports, Browse.
**5.5.3 done:** Database and Settings menus, the `settings` table and command, `@type` groups
in every type filter, configurable run-log folder and rotation, and `db backup`. **5.5 is
complete.**
**5.5.4 done:** Test job (sample data from the Jobs menu), Test renamed Dry run (`y`), and a
Host menu / `diskunder host` listing local drives and network mounts. **5.5.5:** the test
job's steps stay with you through its run and results; `m` goes back to the menu.
**5.5.6:** browse a mount's folders to pick one for a job; a clearer last-run strip.

`diskunder` with no subcommand, in a terminal, opens a full-screen app. With a subcommand,
piped or from cron, nothing changes. It's a front end to 5.0–5.4: every screen runs the same
code as a CLI command, and shows that command so it can be copied into a script, cron line
or job.

### Main window

Always visible beneath the menus: things to select on the left, information about the
selection on the right, and any run in progress along the bottom. Menus act on the
selection.

```text
┌ diskUnder v5 ─ /data/index.db ─────────────────────────────────────────────────────────┐
│ Jobs  Results  Database  Settings  Help                                                │
├─ Jobs ─────────────────────────────────┬─ nas-nightly ─────────────────────────────────┤
│ ▸ nas-nightly    ✓ 02:14 today         │ update /Volumes/NAS  ·  threads 4             │
│   projects       ✗ failed 19 Oct       │ types: all except tmp, log                    │
│   scratch        ● running 42%         │ last run  02:00 → 02:14 today, completed      │
│   photos         – never run           │   772,087 files · 749,404 written · 0 removed │
│                                        │   22,684 errors (22,683 unreadable, 1 other)  │
│ Indexed paths with no job              │ duplicates  1,408 groups · 3.2 GB wasted      │
│   /Volumes/Archive   index 3 Oct       │ previous    ✓ 02:13 yesterday · ✓ 02:15 Mon   │
├────────────────────────────────────────┴───────────────────────────────────────────────┤
│ scratch  ████████████░░░░░░░░░░░░░░  42%  4,120 / 9,844  data/renders/shot_041.exr     │
├────────────────────────────────────────────────────────────────────────────────────────┤
│ ⏎ run  t test  r results  n new job  ? help  q quit                                    │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

Indexed paths without a job are listed too, so nothing in the database is hidden. Runs
started from the app carry on in the bottom strip while you use the menus (the indexing
worker already runs on its own thread).

### Menus

```text
Jobs
├── Create…          name, path, mode, file types, path patterns, threads
├── Run ⏎            runs the selected job (progress in the bottom strip)
├── Test             dry run of the selected job: what would be indexed / removed, no hashing
├── Edit…
├── Status           all jobs: running now, last result, last run time
└── Remove…          job definition only — asks whether to also drop its index

Results              (for the selected job or path)
├── Latest           summary of the last run: counts, errors, duplicates, top files/types
├── Search…          filter form + live results
├── Duplicates       groups → paths, with filters (min size, copies, …)
├── Reports          top files · by type · by folder · stale
├── Browse           folder tree by size, drill in/out
├── History          every run of this job/path: when, outcome, counts
├── Errors           errors from the last run (or any run, from History)
└── Logs             run log viewer, filterable by job, outcome, date

Database
├── Info             every indexed path: files, size, duplicates, last run  (= db info)
├── Check            integrity check, tables with no recorded path         (= db check)
├── Vacuum…          compact the file                                      (= db vacuum)
├── Drop path…       remove an index                                       (= db drop)
├── Open…            switch to another database
└── Backup…          online copy of the database file

Settings
├── Start            database to open, what the main window lists first, auto-select last job
├── Defaults         threads, mode, file types and path patterns for new jobs
├── File types       named type groups (images, video, documents…) for filters
├── Deletion         dry run on by default · confirm before quarantine · grace period before purge   (v6)
└── Logging          log location, rotation size
```

**Keys everywhere:** `↑↓` `⏎` `Esc` to move, open and go back; `/` search; `f` filters for
the current list; `s` sort; `e` export the current list; `c` copy the equivalent CLI command;
`?` help; `q` quit (asks if a run is in progress).

### Notes

- **Browse** is new: a folder tree sorted by size, with file counts and wasted space per
  folder — "where did the space go?". It uses the `--by-directory` rollup from 5.1.
- **Settings** live in the per-user store from 5.4 (a `settings` table). Per-path filters
  stay in `roots`; a job's filters stay in the job.
- **Deletion settings** appear from the start but only take effect in v6. "Dry run on by
  default" can't be turned off without a confirmation.
- **Testing:** ratatui's test backend renders screens to text, so screens get snapshot tests
  alongside the CLI tests.

**Size: L** — the largest v5 release. It can ship in steps as 5.5.x: main window, Jobs and
Results (Latest, History, Errors, Logs) first; then Search, Duplicates, Reports and Browse;
then Database and Settings.

---

## Schema changes

| # | Change | Release | Why | Migration | Cost on an existing table |
|---|---|---|---|---|---|
| 1 | Number migrations with `PRAGMA user_version` | 5.0 | v4 grew several ad-hoc "does this column exist?" checks; v5 adds more. A version number runs each migration once, in order | Set on the first v5 run, after the existing v4 checks | None |
| 2 | Store `extension` lowercase | 5.0 | `--ext`, `--by-type` and the file-type filters treat `JPG`/`jpg` as one type and can use the extension index | `UPDATE … SET extension = lower(extension)` once per table; new rows written lowercase | 0.7 s per million rows |
| 3 | `roots.include_ext`, `roots.exclude_ext` | 5.2 | Remember file-type filters per path | `ALTER TABLE roots ADD COLUMN` | Instant |
| 4 | `roots.exclude_paths` | 5.3 | Remember path patterns per path | `ALTER TABLE roots ADD COLUMN` | Instant |
| 5 | Per-user store `~/.config/diskunder/diskunder.db`: `jobs`, `job_runs`, `job_run_errors` | 5.4 | Saved jobs and their run history — outside any index database | New file, created on first use | — |
| 6 | `settings` table in the per-user store | 5.5 | App settings (start-up, defaults, type groups, deletion, logging) | Created on first use | — |

**Deliberately not added in v5:**

- **`status`** (active / quarantined / deleted) — arrives with quarantine in v6.
- **`last_seen`** — would let reports exclude rows for files deleted since an `index` run
  (only `update` removes rows). Reports instead show the index's age and recommend `update`.
  Revisit if stale rows become a problem.
- **One combined `files` table** for every path — the temporary `UNION ALL` view covers
  cross-path queries without a disruptive migration.
- **Precomputed duplicate tables or extra indexes** — not needed at the speeds measured
  above; revisit beyond about 10 million rows per path.

---

## Phase 0 — groundwork (part of 5.0)

Before the new commands:

1. **Split `src/main.rs` into modules:** `cli`, `scan` (walk, hash, worker), `db` (schema,
   migrations, roots), `tui`, `query` (filters → SQL), `output` (table, CSV, JSON), and one
   module per command. Behaviour-neutral; the existing tests guard it.
2. **Migrations via `user_version`** (schema change 1), and the lowercase extension
   migration (change 2).
3. **Shared parsing** for sizes, ages and extension lists, with unit tests.
4. **Shared output layer** (table / CSV / TSV / JSON / paths).

**Size: M**, mostly mechanical.

---

## Releases and sizing

| Release | Contents | Size |
|---|---|---|
| 5.0 ✅ | Phase 0, run log, `search`, `export` | M |
| 5.1 ✅ | `report`, including duplicate filtering and the summary | M |
| 5.2 ✅ | Include / exclude file types; `index` / `update --dry-run` | M |
| 5.3 ✅ | Path patterns (Ignore) | S–M |
| 5.4 ✅ | Jobs: saved runs, `run NAME`, test (`--dry-run`), run history | M |
| 5.5 ✅ | Interactive app: main window, Jobs / Results / Database / Settings menus — shipped as 5.5.1–5.5.3 | L |

S is under a day of focused work, M is one to three, L is a week or more. These are rough. Each release ships
with integration tests in the style of `tests/cli.rs`, header checks for the CSV and JSON
formats, and its own changelog, README and comparison-table updates — the same tag, rebuild
and push routine as v4.1.

---

## Decisions made

- **Small releases**, as above.
- **`dedupe` moves to v6**, after quarantine.
- **`--stale` uses the modified date**; access-based is opt-in.
- **Path patterns are in v5**, as 5.3.
- **`dedupe` keeps the oldest modified copy by default** (recorded for v6).
- **Jobs are 5.4; the interactive app is 5.5**, with the menu structure above.
- **The run log records only commands that change the database** (see [Run log](#run-log)).
- **Piped output keeps the readable table**; machine formats are explicit, and notes go to
  stderr (see [Output](#output)).

- **Path patterns are case-sensitive**; **`--include-path` makes exceptions** to
  `--exclude-path` (settled when implementing 5.3).

---

## Deferred to v6 — quarantine, then dedupe

**Order:** quarantine first (move files to a dated trash folder, `purge` after a grace
period, `restore`, a manifest written before any move, and a `status` column on file rows),
then `dedupe` built on it. Both need the schema additions noted in
[future.md](future.md#schema-additions-to-consider): `status` and a `manifests` table.

The `dedupe` design worked out for this plan, kept for v6:

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
