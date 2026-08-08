# VERIFICATION — empirical run against real local data

Executed 2026-08-07 on the primary machine (Claude Max 20x + ChatGPT Plus),
**before** the engine was written, per the handoff plan: every assumption below
was tested against real files, and the design followed the data.

## 1. Source inventory

- Claude: 789 transcript JSONL files, ~33 days deep (default 30-day cleanup had
  been active; retention raised to 365 during install).
- Codex: sessions from 2025-11-24 → today, both `sessions/` and
  `archived_sessions/` (basename-identical duplicates across the two — dedup by
  basename, `sessions/` wins).
- ~1.3 GB total; full scan 4.2 s, 0 parse errors, 28k+ events, 27k+ snapshots;
  incremental rescan 0.08 s.

## 2. Claude transcript findings

- **57% of assistant entries are duplicates** (same `message.id` +
  `requestId` re-emitted across files/lines). Dedup is load-bearing, not
  defensive — without it every dollar figure roughly doubles.
- `costUSD` absent under subscription auth → merma always prices tokens itself.
- `usage.input_tokens` EXCLUDES cache tokens (unlike Codex).
- `cache_creation.ephemeral_5m/1h` split present in current-era files; older
  lines only have the unsplit total → priced at the 5m rate and flagged approx.
- `<synthetic>` model lines and zero-usage entries skipped.

## 3. Codex rollout findings

- `total_token_usage` counters are **cumulative per file** → consecutive diff;
  a negative delta means a baseline reset → treat current as the delta.
- `input_tokens` INCLUDES cached tokens → normalized as `input = raw − cached`.
- **Six rate_limits schema variants** across eras (no `limit_id` in 2025-11,
  nullable `secondary`, `plan_type` only in current era, `credits` object drift).
- **Regime flip verified at 2026-07-12**: primary window 300 min → weekly-only
  (10080). Window duration/identity must come from data, never constants.
- plan_type history: `plus` → `prolite` (2026-05-29) → `plus` (2026-06-30) —
  utilization waste must price each covered stretch at its own plan.
- `rate_limits` is null in `codex exec` output — handled.
- Inherited-baseline guard (from ccusage's MultiAgent V2 finding): a fresh
  file whose first counter is already >500k tokens is a replayed parent
  baseline, not usage. Empirically absent in local data (checked the 60 largest
  files) but guarded anyway with a warning.

## 4. The tokens-per-percent join (the core untested assumption)

- **Consecutive-pair regression FAILS**: `used_percent` is integer-quantized;
  pairs give R² < 0. Rejected.
- **Instance-level aggregation works**: reconstruct window instances from
  `resets_at` moves (600 s tolerance), sudden >5-point percent drops, and
  regime changes; then join Δpercent to $-weighted usage per instance.
- Only instances with ≥10% observed growth calibrate (quantization noise
  dominates below that); last ≤8 instances of the **current regime** of the
  operative window.
- Dispersion across instances is ~1.3× (P75/P25) on current data — reported
  with the range, marked UNSTABLE above 1.5×.
- **Achieved-best floor**: the join's median max ($81/window at one point) fell
  below an actually-achieved week ($158). When that happens the report shows
  "period max ≥ (your best, scaled)" instead of a fabricated range. Banked
  resets make weekly instances 2–4 days, so the floor scales by regime length.

## 5. Pricing cross-checks

- Hand-verified claude-fable-5 arithmetic: one 30-day run,
  1,194,676,817 cache-read tokens × $1/M + 5,227,100 out × $50/M + … =
  $1,999.86 — matches an independent Python recomputation to the cent.
- **Credits coherence**: Codex credits are priced from the official credit rate
  card; multiplying total credits by the ~$0.04 street price lands within ~7%
  of the API-equivalent dollars computed independently from tokens
  (1878 credits ≈ $75.12 vs $80.36 for the same 30 days). Two nearly
  independent paths agreeing is the strongest signal the token math is right.

## 6. Live cross-checks (merma doctor, post-install)

```
✓ claude oauth (live)      five_hour 20% · seven_day 20%
✓ codex wham (live)        primary 19%
✓ cross-check codex        primary rollout 19% vs live 19% (Δ0 ≤ 5)
```

The latest snapshot reconstructed from rollout files matches the live endpoint
exactly — the ingestion path and the reconstruction are consistent with the
official numbers.

## 7. End-to-end sanity (2026-08-07)

- 30 days: Codex $80.36 extracted vs an estimated $161–213 max; Claude
  $2,596 extracted over 33 days of history.
- All history: $3,298 extracted across subscriptions that cost $387 over the
  measured span → 8.5× multiplier; left on the table ≥ $681 (Codex side only;
  Claude accrues window instances only now that the statusline hook is live).
- Extrapolations and prorations clamp to the **measured data span** — a
  `--period all` request does not scale over unmeasured time.
- Dashboard smoke-tested in a sized pty: all four tabs render, cache-mode
  toggle and quit work, terminal restored cleanly.
- 16 unit/fixture tests: pricing edges (era boundaries, prefix matching,
  external models), window reconstruction (reset moves, percent drops, regime
  splits, gradual decay), both Codex rollout eras + archived dedup + inherited
  baseline + incremental resume, Claude dedup within/across files + partial
  tail-line resume.

## 8. External expert review (2026-08-07)

A full correctness/design review by gpt-5.6-sol (read-only audit of all
sources) returned 20 findings; all confirmed-valid ones were applied and
re-verified:

- **Scan problems now reach every read path** — warnings and parse-error
  counts print on stderr for report/wrapped/status/doctor, never only on
  `merma scan` (silent-incompleteness fix).
- **Effective period clamps BOTH ends** to the measured span (usage ∪
  snapshot bounds); a 365-day request over a source last used in June no
  longer scales estimates into unmeasured months.
- **Historical plan pricing**: each covered stretch is priced at the plan
  recorded on its own snapshots (plus → prolite $100/mo → plus history),
  with a loud note for unknown plan types instead of silent current-plan
  inheritance.
- **Billing constraint per point in time**: greedy longest-window-first
  instance selection with overlap exclusion replaces the single-window pick,
  so pre-flip weekly `secondary` and post-flip weekly `primary` combine and
  a 5-hour burst peak can never stand in for a weekly constraint.
- **Join calibration de-biased**: numerator restricted to events strictly
  after the baseline observation and no later than the first peak; instances
  containing unpriced-model usage are rejected (counted + noted);
  era-approximate prices propagate an `approx` flag into the estimate.
- **Floor discipline**: achieved-best weeks only count if fully inside the
  current regime; the floor never mutates the raw quartiles or manufactured
  dispersion — dispersion is always raw (Option, undefined when P25=0), and
  a floor that dominates the median flips to the one-sided "≥" rendering.
- **Window reconstruction hardened**: same-second observations from
  different sources coalesce deterministically (max percent); resets_at
  drift is measured against a fixed instance anchor, not the previous point;
  a gap longer than the window duration always splits (a reset must have
  happened unobserved).
- **Collector integrity**: resume is verified with a prefix hash + boundary
  newline check (same-size rewrites detected); corrupt Codex scan state
  triggers a loud full re-ingest instead of silently becoming an empty
  baseline; a rewritten Claude transcript triggers one full provider rescan
  so deduplicated events surviving in other files are restored; Codex
  snapshots carry per-file provenance and are retracted on rewrite; a
  partial counter regression (e.g. only cache_write resets) is clamped and
  warned, never recounted as fresh usage; the inherited-baseline threshold
  no longer double-counts cached tokens.
- **Validation at trust boundaries**: price tables are validated at load
  (date sanity, overlapping-era ambiguity, negative/non-finite rates,
  duplicate plans); config prices must be finite and non-negative;
  percentages are rejected when non-finite/negative and clamped at 100 for
  waste math.
- **Dashboard**: single long-lived poll connection; poll/rescan failures
  surface in the footer instead of being swallowed.

Post-fix verification: 25 unit/fixture tests pass (including regression
tests for same-size rewrites, dup-winner restoration, partial counter
regression, per-plan pricing, overlap exclusion, and effective-span
clamping); real-data reports re-ran coherently — the 30-day Codex estimate
is now an honest UNSTABLE 2.2× range ($96–211 period max) instead of a
floor-contaminated bound, and all-history covered plan cost correctly rose
to $89.69 with prolite-era stretches priced at $100/mo.

Consciously-partial applications (documented trade-offs): scan problems warn
loudly rather than abort (one malformed line must not brick the tool); the
rewrite detector hashes the first 256 bytes + resume boundary rather than
full content (session logs are not adversarial); the unknown-plan fallback
prices at the current plan WITH a note rather than refusing the report.

### Confirmation round (same reviewer, post-fix)

A second static pass confirmed 11 findings fully fixed and flagged 5 defects
introduced by the fixes; all were addressed:

- **Interval-union measurement** replaced the convex hull: `measured_secs`
  is the union of the (clipped) usage and snapshot spans, used for coverage,
  estimate scaling and proration; disjoint stretches are noted, gaps between
  them are never scaled over. Regression test included.
- **Plan changes split window instances** (a prolite→plus flip can no longer
  be priced wholesale at the last plan), and wrapped/plan cost now integrate
  over the recorded plan-type history (`plan_cost_effective_usd`), extending
  the earliest known plan backwards with a note. The all-history wrapped
  cost rose $387 → $472 and the multiplier dropped 8.6× → 7.1× — the honest
  numbers.
- **Claude corrupt scan state now fails CLOSED** (loud re-ingest, mirroring
  Codex) instead of silently disabling rewrite detection.
- **Legacy snapshots migrated**: a one-time full Codex rescan rebuilt all
  27,224 pre-provenance snapshot rows with per-file sources (verified live,
  0 parse errors, cross-check still Δ0).
- **Recovered poll errors clear from the dashboard footer**; initial-scan
  and periodic-rescan warnings surface in the footer too.
- Remaining partials applied: walk errors are warned; per-instance
  `usd_per_pct` uses the calibration attribution window; the operative
  window tie-breaks toward the longer duration; live pollers and the
  statusline ingester validate percentages; Claude transcripts reject
  negative token counts; an override price table must include
  `[[codex_credits]]`; unparseable-line counts persist to meta and appear in
  doctor ("parse loss").

Accepted limitations from the confirmation pass (conservative by
construction): the greedy billing-overlap selection can drop the
non-overlapping tail of a shorter-window instance (undercounts *coverage*,
which is reported as UNKNOWN — never overstates waste); the 256-byte prefix
hash cannot detect a rewrite located after byte 256 and before the resume
offset that preserves size ordering and the boundary newline; a
whole-vector Codex counter reset whose new baseline input exceeds the old
one is clamped (with warning) rather than recounted.

Final state: 27 tests pass; clippy clean; live doctor all green.

## Known limitations (honest, by design)

- Fast-mode multipliers and web-search surcharges are not visible in local
  logs — API-equivalent figures are floors in those cases.
- Claude-side max-extraction needs ≥2 window instances with ≥10% growth in the
  current regime; it reports "not yet" until the hook has collected them.
- Utilization coverage below 100% is printed with every waste figure; waste in
  unmeasured windows is UNKNOWN, never assumed zero.
- The per-credit $0.04 valuation and a handful of retired-era prices are
  flagged `approx` and propagate a `~` marker into every figure they touch.
