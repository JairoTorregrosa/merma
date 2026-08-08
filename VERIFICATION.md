# Verification

merma was verified against real local data before the engine was
written, on 2026-08-07, on a machine with Claude Max 20x and ChatGPT
Plus. Every assumption below was tested against real files. The design
followed the data. Two external review rounds followed; all confirmed
findings were applied.

## 1. Source inventory

- Claude: 789 transcript JSONL files, about 33 days deep. The default
  30-day cleanup had been active. The install raised retention to 365
  days.
- Codex: session files from 2025-11-24 to the present, in both
  `sessions/` and `archived_sessions/`. The two trees duplicate files by
  basename. merma dedups by basename; `sessions/` wins.
- Total size ~1.3 GB. A full scan takes 4.2 s and produces 28k+ usage
  events and 27k+ snapshots with 0 parse errors. An incremental rescan
  takes 0.08 s.

## 2. Claude transcript findings

- 57% of assistant entries are duplicates. The same `message.id` +
  `requestId` pair appears again across files and lines. Dedup is
  load-bearing: without it, every dollar figure roughly doubles.
- `costUSD` is absent under subscription auth. merma always prices
  tokens itself.
- `usage.input_tokens` EXCLUDES cache tokens. Codex INCLUDES them. The
  two providers need different normalization.
- Current-era files split cache writes into `ephemeral_5m` and
  `ephemeral_1h`. Older lines carry only the unsplit total. merma prices
  the unsplit total at the 5m rate and flags it approximate.
- merma skips `<synthetic>` model lines and zero-usage entries.

## 3. Codex rollout findings

- `total_token_usage` counters are cumulative per file. merma diffs
  consecutive values. A negative delta on the dominant counter means a
  baseline reset; the current value is then the delta.
- `input_tokens` includes cached tokens. merma normalizes:
  `input = raw − cached`.
- Six `rate_limits` schema variants exist across eras: no `limit_id` in
  2025-11, a nullable `secondary`, `plan_type` only in the current era,
  and drift in the `credits` object. Fixtures cover each variant.
- The primary window changed from 300 minutes to weekly on 2026-07-12.
  This is visible in the data. Window duration and identity must come
  from data, never from constants.
- The recorded plan history is: `plus` → `prolite` (2026-05-29) →
  `plus` (2026-06-30). Waste math must price each covered stretch at
  its own plan.
- `rate_limits` is null in `codex exec` output. merma handles it.
- A fresh file whose first counter already exceeds 500k tokens carries a
  replayed parent baseline, not usage (a ccusage finding). The local
  data does not show this. merma guards it anyway and warns.

## 4. The tokens-per-percent join

The core untested assumption was: dollars per utilization percent is
stable enough to extrapolate a window maximum.

- Consecutive-pair regression FAILS. `used_percent` is
  integer-quantized. Pairs give R² < 0. Rejected.
- Instance-level aggregation works. merma reconstructs window instances
  from `resets_at` moves (600 s tolerance against a fixed anchor),
  sudden percent drops above 5 points, regime changes, plan changes,
  and gaps longer than the window duration. It then joins Δpercent to
  dollar-weighted usage per instance.
- Only instances with ≥ 10% observed growth calibrate. Quantization
  noise dominates below that. merma uses the last ≤ 8 instances of the
  current regime of the operative window.
- Dispersion across instances is ~1.3–1.7× (P75/P25) on current data.
  The report prints the range. Above 1.5× it adds the UNSTABLE label.
- The achieved-best floor: at one point the join's median window max
  ($81) fell below an actually-achieved week ($158). In that case the
  report shows "period max ≥ (your best, scaled)" instead of a
  fabricated range. Banked resets make weekly instances 2–4 days long,
  so the floor scales by regime length.

## 5. Pricing cross-checks

- Hand check: one 30-day run of claude-fable-5 =
  1,194,676,817 cache-read tokens × $1/M + 5,227,100 output tokens ×
  $50/M + the remaining terms = $1,999.86. An independent Python
  recomputation matches to the cent.
- Credits coherence: 1878 Codex credits priced from the official credit
  card ≈ $75.12. The API-equivalent dollars computed independently from
  tokens for the same 30 days = $80.36. Two nearly independent paths
  agree within ~7%. This is the strongest signal that the token math is
  right.

## 6. Live cross-checks (merma doctor)

```
✓ claude oauth (live)      five_hour 20% · seven_day 20%
✓ codex wham (live)        primary 19%
✓ cross-check codex        primary rollout 19% vs live 19% (Δ0 ≤ 5)
```

The latest snapshot reconstructed from rollout files matches the live
endpoint exactly. The ingestion path and the reconstruction agree with
the official numbers.

## 7. External review, round 1 (2026-08-07)

A correctness and design review by gpt-5.6-sol (read-only audit of all
sources) returned 20 findings. All confirmed-valid findings were applied:

- Scan problems reach every read path. Warnings and parse-error counts
  print for report, wrapped, status, and doctor — never only for
  `merma scan`.
- The effective period clamps BOTH ends to the measured span. A 365-day
  request over a source last used in June does not scale estimates into
  unmeasured months.
- Historical plan pricing: each covered stretch is priced at the plan
  its own snapshots record, with a loud note for unknown plan types.
- Billing constraint per point in time: greedy longest-window-first
  instance selection with overlap exclusion replaces the single-window
  pick. A 5-hour burst peak can never stand in for a weekly constraint.
- Join calibration de-biased: the numerator takes only events strictly
  after the baseline observation and no later than the first peak.
  Instances with unpriced-model usage are rejected and counted.
  Era-approximate prices propagate an `approx` flag.
- Floor discipline: achieved-best weeks count only when fully inside
  the current regime. The floor never mutates raw quartiles. A floor
  that dominates the median flips the rendering to one-sided `≥`.
- Window reconstruction hardened: same-second observations coalesce
  deterministically (max percent); drift is measured against a fixed
  instance anchor; a gap longer than the window always splits.
- Collector integrity: resume verifies a prefix hash plus a boundary
  newline; corrupt scan state triggers a loud full re-ingest; a
  rewritten Claude transcript triggers one full provider rescan so
  dedup winners surviving in other files are restored; Codex snapshots
  carry per-file provenance and are retracted on rewrite; a partial
  counter regression is clamped and warned, never recounted as fresh
  usage.
- Validation at trust boundaries: price tables validate at load; config
  prices must be finite and non-negative; percentages are rejected when
  non-finite or negative and are clamped at 100 for waste math.
- Dashboard: one long-lived poll connection; poll and rescan failures
  surface in the footer.

Consciously-partial applications, documented as trade-offs: scan
problems warn loudly rather than abort (one malformed line must not
brick the tool); the rewrite detector hashes the first 256 bytes plus
the resume boundary rather than the full content (session logs are not
adversarial); the unknown-plan fallback prices at the current plan WITH
a note rather than refusing the report.

## 8. External review, round 2 (same reviewer, post-fix)

A second static pass confirmed 11 findings fully fixed and flagged 5
defects introduced by the fixes. All five were addressed:

- Interval-union measurement replaced the convex hull. `measured_secs`
  is the union of the clipped usage and snapshot spans. Coverage,
  estimate scaling, and proration all use it. Gaps between disjoint
  stretches are never scaled over. A regression test pins this.
- Plan changes split window instances, and the subscription cost
  integrates over the recorded plan-type history. The all-history cost
  rose from $387 to $472 and the multiplier dropped from 8.6× to 7.1×.
  These are the honest numbers.
- Claude corrupt scan state now fails CLOSED (loud re-ingest), matching
  Codex.
- Legacy snapshots migrated: a one-time full Codex rescan rebuilt all
  27,224 pre-provenance snapshot rows with per-file sources. Verified
  live: 0 parse errors, cross-check still Δ0.
- Recovered poll errors clear from the dashboard footer. Initial-scan
  and periodic-rescan warnings surface there too.

Remaining partials applied in the same round: walk errors warn;
per-instance `usd_per_pct` uses the calibration attribution window; the
operative window tie-breaks toward the longer duration; live pollers
and the statusline ingester validate percentages; Claude transcripts
reject negative token counts; an override price table must include
`[[codex_credits]]`; unparseable-line counts persist and appear in
doctor as "parse loss".

## 9. Accepted limitations

These are conservative by construction:

- The greedy billing-overlap selection can drop the non-overlapping
  tail of a shorter-window instance. This undercounts *coverage*, which
  the report shows as UNKNOWN. It never overstates waste.
- The 256-byte prefix hash cannot detect a rewrite after byte 256 that
  preserves the size ordering and the boundary newline.
- A whole-vector Codex counter reset whose new baseline input exceeds
  the old one is clamped, with a warning, rather than recounted.
- Fast-mode multipliers and web-search surcharges are not visible in
  local logs. API-equivalent figures are floors in those cases.
- Claude max-extraction needs ≥ 2 window instances with ≥ 10% growth in
  the current regime. The report says "not yet" until the hook has
  collected them.

Final state at release: 27 tests pass; clippy is clean; `merma doctor`
is all green on live data; the all-history report shows $3,368 extracted
against $472 of subscription cost (7.1×).
