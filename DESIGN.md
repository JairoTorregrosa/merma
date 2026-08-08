# Design

This document lists the rules that govern merma. Changes must obey these
rules. [AGENTS.md](AGENTS.md) turns them into a checklist for coding
agents.

## Purpose

merma answers one question about your AI subscriptions: how many dollars
of the window capacity you pay for do you leave on the table. Every
number it prints supports that answer — the period estimate, the live
gap, the confidence basis, or the decision. A number that supports none
of them does not ship.

## Rules

### Measured data only

Every estimate scales by the measured span, never by the requested
period. The measured span is the interval union of the usage and
snapshot spans, clipped to the request — a gap between two measured
stretches is never scaled over. A `--period 365d` request over a source
with 30 days of data reports 30 days of coverage and says so.

### Absence is declared, not silent

- Coverage below 100% prints next to what it qualifies. Waste in
  unmeasured windows is UNKNOWN, never assumed zero, never rendered $0.
- Confidence is a stated tier — MEASURED, CALIBRATED, INSUFFICIENT —
  evaluated in that order, first match wins. There is no hidden second
  flag. INSUFFICIENT never prints a fabricated number: it prints what is
  missing and when it unlocks.
- Every approximate price (retired eras) carries a `~` that propagates
  into every figure it touches.
- Scan warnings and unparseable-line counts surface on every read path
  (brief, status, doctor), not only during `merma scan`.

### Estimator doctrine

- **Center**: the sample median of per-instance $/window rates, per
  `(window_id, window_minutes)`. Rates are never pooled across window
  ids or regimes (different denominators).
- **Band**: the exact distribution-free order-statistic confidence
  interval for the median — `[x₁,xₙ]` for n ≤ 7, `[x₂,xₙ₋₁]` at n = 8.
  The achieved coverage is printed exactly and is never rounded to a
  round number. The band is for the **typical window**; the words "next
  window" appear nowhere.
- **Quantization**: `used_percent` is integer-quantized, so each
  instance's growth carries a worst-case ±1 interval
  (`c/(d+1), c/(d−1)`). The lo/hi arrays are sorted INDEPENDENTLY of
  the point rates (`c/(d±1)` is not monotone in `c/d`; a counterexample
  is pinned in tests). This composes as a strict outer bound —
  quadrature is rejected (it needs independence and shape assumptions
  nobody can audit).
- **Influence**: the max delete-1 median shift, threshold 0.25. The
  jackknife SE is inconsistent for the median (Miller 1974; Efron 1982)
  and is not used.
- **Admission gates** (each exclusion counted and named): growth ≥ 10
  points; no unpriced usage in the attribution span; no usage-event gap
  above `max(0.25 × span, 30 min)`; no snapshot gap above
  `0.10 × regime`. Reject-and-report is the honest treatment of partial
  observation at n ≤ 8, not survival modeling.
- **Bound composition** (stated once, applied everywhere): the bound —
  `max(best achieved week scaled to the window, best 100%-instance
  dollars)` — is a certified lower bound on window value and composes
  **by max with every lower edge**. It never tightens an upper edge,
  never enters the influence diagnostic, and never manufactures
  stability. Truncating a confidence interval from below at a certified
  lower bound can only raise its coverage, so `display_lo =
  max(band_lo, bound)` preserves the achieved-coverage claim. When the
  bound climbs to or past the band median, the tier promotes to
  MEASURED and rendering collapses to one-sided `≥`.
- **The `≥` period scaling never leaves the certified epoch.**
  `bound × windows` is a certified lower bound only over measured time
  under the CURRENT plan and CURRENT regime: a prior plan's windows had
  different capacity, and a prior regime's windows a different
  denominator. The certified epoch starts at `max(current-regime start,
  start of the trailing constant-plan run)`; the `≥` figure is
  `bound × certified windows − extraction inside the certified epoch`,
  and the brief prints that full arithmetic so the reader can redo it.
  The uncertified remainder of the measured span still prints as
  coverage context, but never inflates the certified figure. (Known
  limitation, unreachable on this machine: a floor week or 100%
  instance achieved under a PRIOR plan inside the current regime would
  overstate the bound; the floor currently filters by regime start
  only.)
- `COMPLETE_WINDOW_MIN_FRAC = 0.75`: a closed instance counts as a
  complete window for the unlock-rate formula only when it was observed
  over at least 75% of its regime. The spec leaves "complete"
  undefined; a banked-reset instance — much shorter than its regime —
  must not count (worked Example B pins this). The constant is
  implementation-pinned because it decides the rate-vs-structural
  unlock path and therefore moves a displayed date.
- **Only the operative window carries dollars.** The operative window
  is the longest window with a live snapshot (scoped ids — those
  prefixed by another id — excluded, data-driven). Non-operative open
  windows print percent, reset countdown, and historical context, but
  never dollars: a 5-hour window is capacity the weekly limit already
  bounds, and a dollar extrapolation from it would double-count the
  same tokens.

Pinned constants (each with a regression test):

| Constant | Value |
|---|---|
| `N_MIN_CALIBRATED` | 4 |
| `MAX_INSTANCES_FOR_ESTIMATE` | 8 |
| `MIN_DPCT_FOR_ESTIMATE` | 10.0 |
| `MAX_LOO_SHIFT` | 0.25 |
| `G_USAGE_FRAC` / floor | 0.25 × span / 1800 s |
| `G_SNAP_FRAC` | 0.10 × regime |
| `INNER_BAND_MIN_N` | 8 |
| `CURVE_CHECKPOINTS` | 25% / 50% / 75% (largest ≤ age) |
| `KEEP_THRESHOLD` | 1.0 |
| `PCT_SNAP_EPS` | 1e-6 |
| `SECS_PER_MONTH` | 2,629,746 |
| `COMPLETE_WINDOW_MIN_FRAC` | 0.75 |

### Rejected alternatives (do not re-litigate without new evidence)

- **Bootstrap** — the percentile bootstrap has no finite-sample coverage
  guarantee and undercovers badly at n < 10; the order-statistic
  interval achieves exact coverage from the same order statistics.
- **Jackknife SE as headline spread** — inconsistent for the median;
  on the worked fixture it understates the honest band width ~10×.
  Retained only as the max-LOO-shift influence diagnostic.
- **Student-t on the mean** — the mean is exactly what the outlier
  corrupts; normality uncertifiable on quantized, drift-contaminated
  tiny samples.
- **Bayesian interval** — any prior on $/pct is a made-up number wearing
  a posterior; fails "every figure traceable".
- **Trend/drift regression** — 2 parameters on n = 5 is noise wearing a
  trend; recency-limiting plus the wide exact band is the defensible
  treatment. Revisit only if MAX_INSTANCES grows past ~15.
- **Kaplan–Meier / formal censoring** — needs dozens of observations;
  reject-and-count is honest at n ≤ 8.
- **Hodges–Lehmann + signed-rank CI** — slightly more efficient, but
  "median of your 5 instances" is head-auditable and "median of 15
  Walsh averages" is not.
- **Consecutive-snapshot regression** — empirically dead (R² < 0).
- **Quadrature error composition** — requires independence/shape
  assumptions; the outer bound is assumption-free.
- **Linear scaling of an uncalibrated floor for the live gap** — assumes
  the $/pct constancy the floor precisely lacks; the certified form
  `bound − extracted_in_window` needs no model.

### The decision layer adds no statistics

The keep/downgrade verdict operand is the achieved-pace comparison:
extracted dollars vs. plan cost over the measured span, against the
printed `keep ≥ ×1.0` threshold. The per-window comparison (window
worth vs. window cost) prints in the basis as context only — it answers
"is the plan worth it *if* captured", not "as used", and is
decision-inert. Grammar: `verdict — comparison (operand / operand ·
span) — threshold`; every operand prints with unit and span so the
reader can redo the arithmetic. A ranged operand straddling the
threshold yields UNKNOWN, never a coin flip. Present indicative, no
imperatives, no second person.

### Rendering and JSON invariants

- Every estimated dollar figure in `--json` (`left_on_table_usd`,
  `gap_usd`) is a tagged object (`band` or `floor`), never a bare
  float; exact measured operands (extracted dollars, plan cost, the
  basis floors and bounds) are plain numbers — they carry no
  uncertainty to tag. An unknown estimated dollar is `null` plus a
  sibling reason (`gap_reason`, `basis.insufficient`). Serialized
  fields may only grow within a schema_version.
- Every number in the text exists in the JSON with the same value; the
  text formats it under the fmt.rs rendering rules (a test pins
  formatted equality).
- The brief self-wraps at a fixed 120 print columns, breaking only at
  word boundaries, with continuations indented to the value column.
  Fixed, not detected: terminal-size detection would need a new
  dependency (banned), and a fixed width keeps output deterministic and
  captures reproducible. 120 is the narrowest width at which the mockup
  rows stay whole. Narrower terminals still hard-wrap, but every break
  merma makes lands on a word boundary. An operator word (`−`, `×`,
  `≥`, `≈`, `–`, `-`) binds to its right operand and never ends a
  line; `·` and `—` are separators and may.
- Line budget (golden-tested, at the golden width): a provider stanza
  is at most 9 rendered lines including its header; the default
  two-provider brief is at most 22 lines. Diagnostic lines (`⚠` notes,
  `✗` errors) are EXEMPT from the budget: every warning must reach
  every read path and is never dropped to fit a layout — the budget
  governs the designed surface, not the diagnostics riding on it.
- TTY-gated styling, NO_COLOR honored, piped output byte-plain, width
  math on plain strings. `--json` never touches theme.rs.
- The v0.1 teal accent is retired by decision, not omission: teal was
  graphic ink only, v0.2 deleted every graphic surface (dashboard,
  heatmap, wrapped card), so no surface remains that may carry it. The
  palette is bold/dim plus status colors (green ✓, yellow ⚠, red ✗)
  strictly on status. If a graphic surface ever returns, teal returns
  with it.

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
polls once per `merma collect` (the launchd agent runs it every 15
minutes; nothing else polls automatically), and turns off with one
config line (`claude_oauth_enabled = false`). merma degrades cleanly
without it.

### Data sources

| Source | Provides | Cadence |
|---|---|---|
| Claude transcript JSONL | tokens per call, models | incremental scan |
| Codex rollout files | tokens, snapshots, plan type | incremental scan |
| statusline hook spool | Claude utilization snapshots | each render |
| Claude OAuth endpoint (opt-out) | live utilization | one poll per `merma collect` |
| Codex wham endpoint | live utilization | one poll per `merma collect` |

### Tests are first-class

Every parser handles a fixture from real data. Edge cases have tests:
era boundaries, counter resets, archived duplicates, partial tail lines,
same-second snapshots, disjoint measured spans, every tier boundary at
equality, the coverage oracle, the independent-sort counterexample, the
JSON contract snapshot, the line budget. CI enforces
`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
`cargo test` on Linux and macOS.
