# Design

This document lists the rules that govern merma. Changes must obey these
rules. [AGENTS.md](AGENTS.md) turns them into a checklist for coding
agents.

## Purpose

merma answers two questions about your AI subscriptions: how much value
did you extract, and how much did you leave on the table. Every number it
prints supports one of those two answers. A number that supports neither
does not ship.

## Rules

### Measured data only

Every estimate scales by the measured span, never by the requested
period. The measured span is the interval union of the usage and
snapshot spans, clipped to the request — a gap between two measured
stretches is never scaled over. A `--period 365d` request over a source
with 30 days of data reports 30 days of coverage and says so.

### Absence is declared, not silent

- Utilization coverage below 100% prints next to every waste figure.
  Waste in unmeasured windows is UNKNOWN, never assumed zero.
- An unstable tokens-per-percent join (P75/P25 above 1.5×) is labeled
  UNSTABLE and rendered as a range, not a point.
- When the achieved-best floor dominates the estimate, the report shows
  `≥` and the floor, not a fabricated range.
- Every approximate price (retired eras, the per-credit valuation)
  carries a `~` that propagates into every figure it touches.
- Scan warnings and unparseable-line counts surface on every read path
  (report, wrapped, status, doctor, dashboard footer), not only during
  `merma scan`.

### Windows come from data, never constants

Billing-window duration and identity are reconstructed from `resets_at`
moves, percent drops, and plan changes recorded in snapshots. Codex
changed its primary window from 300 minutes to weekly on 2026-07-12;
merma detected that from data. A hardcoded window constant is a bug.

### Dedup is load-bearing

Claude transcripts re-emit about 57% of assistant entries across files;
merma dedups by `message.id` + `requestId`. Codex rollout counters are
cumulative per file; merma diffs consecutive values. Without either rule
the dollar figures roughly double. Tests pin both.

### Each covered stretch is priced at its own plan

Plan changes recorded in snapshots split window instances and segment
the subscription cost. A `prolite` month is priced at the `prolite`
rate even when the current plan is `plus`. An unknown plan type falls
back to the current plan and prints a loud note.

### Trust boundaries validate

- Price tables are validated at load: date sanity, no overlapping eras
  for one prefix, no negative or non-finite rates, no duplicate plans.
- A corrupt scan state fails closed: loud full re-ingest, never a silent
  empty baseline.
- File rewrites are detected with a prefix hash and a resume-boundary
  check; a rewritten Claude transcript triggers a full provider rescan
  so deduplicated events surviving in other files are restored.
- Live percentages are rejected when non-finite or negative.

### The sanctioned path is the default

The statusline hook and the local transcripts are the sanctioned data
path. The Claude OAuth poller is unofficial: it is documented as such,
polls at a respectful cadence, and turns off with one config line
(`claude_oauth_enabled = false`). merma degrades cleanly without it.

### Data sources

| Source | Provides | Cadence |
|---|---|---|
| Claude transcript JSONL | tokens per call, models | incremental scan |
| Codex rollout files | tokens, snapshots, plan type | incremental scan |
| statusline hook spool | Claude utilization snapshots | each render |
| Claude OAuth endpoint (opt-out) | live utilization | poll ≥ 120 s |
| Codex wham endpoint | live utilization, credits | poll ≥ 60 s |

### Tests are first-class

Every parser handles a fixture from real data. Edge cases have tests:
era boundaries, counter resets, archived duplicates, partial tail lines,
same-second snapshots, disjoint measured spans. CI enforces
`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
`cargo test` on Linux and macOS.
