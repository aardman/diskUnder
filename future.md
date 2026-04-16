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

### `export` subcommand
Makes the index consumable by BI tools (Tableau, Power BI, Excel) and AI pipelines.

```
diskunder export /nas --format csv  > report.csv
diskunder export /nas --format json > report.json
```

---

## Medium value, moderate effort

### `watch` mode — continuous incremental updates
Replace the current 24-hour cutoff heuristic in `update` with OS-native filesystem events.
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
