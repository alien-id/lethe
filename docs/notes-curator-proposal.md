# Proposal: Notes Curator (workspace notes deduplication)

**Status**: Design proposal. Not implemented in this PR.

## Problem

`{workspace}/notes/` accumulates duplicates because no process is responsible
for noticing. The existing `MemoryCurator` (`src/scheduler/curator.rs`)
operates on archival memory and message history; it never touches the notes
directory.

Without runtime enforcement, an autonomous loop can generate the same
conceptual document repeatedly on each heartbeat under filenames that
differ only in date suffixes or word ordering. Observed pattern in the
field: seven same-topic notes generated within an 8-hour stretch, each
written by a successive DMN heartbeat that didn't recognize the existing
file as the same logical document. The prompt-side rule against this
exists, but is ignored under heartbeat pressure.

A passive enforcement layer that runs at curator cadence and reports what
it cleaned (plus warnings on ambiguous cases) is the structural backstop.

## Proposed algorithm

### Inputs
Walk `{workspace}/notes/` (excluding any `_superseded/` subdir).

For each file, derive:
- `stem` — filename without extension and trailing date suffix
  (regexes for `_YYYY-MM-DD`, `_YYYYMMDD`, `_YYYY_MM_DD`)
- `topic_key` — normalized prefix of the stem (e.g. first 2–3 significant
  words after stop-word stripping)
- `date` — file mtime's calendar date
- `size`, `head_line` for canonicality scoring

### Bucketing
Group files into `(topic_key, date)` buckets.

### Action for each bucket of size ≥ 2
Pick the canonical file:
1. If exactly one file's first heading matches a canonicality regex
   (`(single|canonical|for principal|for review|final|approved)`), keep it.
2. Else: most recent mtime.
3. Tie-breaker: largest size.

Move every other file in the bucket to `{workspace}/notes/_superseded/`.
Append an entry to `_superseded/README.md` documenting the move
(timestamp, bucket key, canonical, list of superseded).

### Edge cases
- Single-file buckets (no peers): no action. Date-stamped lone files are
  fine.
- High-divergence content within a bucket: if two files share the bucket
  key but their headers diverge >70% (cheap edit-distance on title +
  first H2), log a warning instead of moving. Surface in DMN's output.
- In-flight protection: don't operate on files mtime'd in the last 15
  minutes (current heartbeat may still be writing).
- First-run backfill: on the first scheduled run after enabling the
  feature, report what *would* have been moved without acting. The
  principal grants one-shot approval (env flag / CLI), then enforce.

## Integration point

`scheduler/notes_curator.rs`, scheduled alongside the existing
`MemoryCurator` in `scheduler/mod.rs`. Same cadence
(`CURATOR_CADENCE_SECONDS` = 6h) seems reasonable for v1.

```rust
NotesCurator::new(notes_dir: PathBuf, state_path: PathBuf)
NotesCurator::run() -> NotesCuratorResult<NotesCuratorStats>
NotesCuratorState { last_run_at, total_moved, total_warnings }
```

State file: `{data}/notes_curator_state.json`, mirroring the existing
`curator_state.json` pattern.

## Why runtime, not just prompt

The prompt rule "update existing same-topic notes rather than creating
new" already exists in shipped prompts and is being bypassed under
heartbeat pressure. A passive enforcement layer running on schedule and
reporting cleanups gives the principal observability — they see the
cleanup happen in DMN/curator reports rather than discovering the mess.

This is a structural backstop, not a replacement for the prompt-side
rule. Both layers help.

## Out of scope

- Content-similarity dedup (would need embeddings; v2)
- Reaping `_superseded/` itself (separate slower cadence)
- Touching any other workspace subdirectory

## Decisions for review

1. Cadence — same as MemoryCurator (6h), or independent?
2. State directory — alongside `curator_state.json` or separate?
3. Bucketing heuristic — propose config-driven via a small TOML, or
   hardcoded heuristic?
4. First-run consent — env flag, CLI command, or just always run
   "report-only" on the first round?
