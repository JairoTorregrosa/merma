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

---

# v0.2 — the estimator rebuild (appended 2026-08-08)

v0.2.0 deletes four surfaces and rebuilds the product around "left on
the table". The raw-quartile + binary-UNSTABLE estimator of §4 is
replaced by an exact order-statistic estimator with admission gates,
quantization propagation, confidence tiers, and a decision layer.
Everything below was verified on this machine on 2026-08-08. Final
state: 71 tests pass; fmt and clippy clean.

## 10. Chosen methods, and the alternatives that lost

The full doctrine lives in DESIGN.md; this section records the
evidence behind each choice so it is not re-litigated without new data.

Chosen:

- **Center** — sample median of per-instance $/window rates, per
  `(window_id, window_minutes)`. Robust to the real outliers in this
  DB ($0.50-era rates next to $0.13-era rates).
- **Band** — the exact distribution-free order-statistic confidence
  interval for the median (`[x₁,xₙ]` for n ≤ 7, `[x₂,xₙ₋₁]` at n = 8).
  Exact finite-sample coverage; the only assumption is exchangeability.
  The achieved coverage prints exactly (§12 verifies the table).
- **Quantization** — worst-case ±1 outer bound on each instance's
  integer-quantized growth, with the lo/hi arrays sorted independently
  of the point rates (`c/(d±1)` is not monotone in `c/d`; the
  counterexample is a pinned test).
- **Influence** — max delete-1 median shift, threshold 0.25. Exact,
  deterministic, answers "does one instance control the number".
- **Censoring** — reject-and-report through four named admission gates
  (low growth, unpriced, usage gap, snapshot gap), each exclusion
  counted and surfaced (§14 shows the real effects).
- **Floor** — `bound = max(best achieved week scaled to the window,
  best 100%-instance dollars)`; composes by max with lower edges only.

Rejected, with the reason each lost:

- **Bootstrap** — even the fully-deterministic exhaustive bootstrap
  (126 multisets at n = 5) has no finite-sample coverage guarantee and
  undercovers badly at n < 10; at tiny n it degenerates onto the same
  order statistics the exact interval already uses.
- **Jackknife SE as headline spread** — inconsistent for the median
  (Miller 1974; Efron 1982). On the worked fixture it gives SE 1.36
  against an honest band of width 28 — a ~10× understatement. Retained
  only as the max-LOO-shift diagnostic.
- **Student-t on the mean** — the mean is exactly what the outlier
  corrupts (fixture: mean 29.2 vs median 25); normality uncertifiable
  on quantized, drift-contaminated n = 5.
- **Bayesian interval** — any prior on $/pct is a made-up number
  wearing a posterior; fails "every figure traceable".
- **Trend/drift regression** — drift is real in this DB (0.25–0.50 era
  → 0.13 era) but 2 parameters on n = 5 is noise wearing a trend;
  recency-limiting plus the wide exact band is the defensible
  treatment. Revisit past ~15 instances.
- **Kaplan–Meier / formal censoring models** — need dozens of
  observations; reject-and-count is honest at n ≤ 8.
- **Hodges–Lehmann + signed-rank CI** — slightly more efficient, but
  "median of your 5 instances" is head-auditable and "median of 15
  Walsh averages" is not.
- **Consecutive-snapshot regression** — already empirically dead in §4
  (R² < 0); stays dead.
- **Quadrature error composition** — needs independence and shape
  assumptions nobody can audit; the independent-sort outer bound is
  assumption-free.
- **Linear scaling of an uncalibrated floor for the live gap** —
  assumes the $/pct constancy the floor precisely lacks; the certified
  form `bound − extracted_in_window` needs no model.

## 11. Real-data run and re-derivation (2026-08-08)

`merma` (v0.2.0, installed binary) at capture time
(`generated_at = 1786224123`, 2026-08-08T21:22:03Z), 30-day period.
Both providers sit in the INSUFFICIENT tier on this machine — the
weekly regimes are young — so this run exercises the bound path, the
unlock formula, and the decision layer on live data; the CALIBRATED
band and MEASURED promotion arithmetic are pinned by the worked-example
fixtures (`tests/fixtures/brief_0_2_0.json` plus the Example A/C unit
tests, digit-for-digit).

Codex stanza:

```
left on table    ≥ $46.32   lower bound · your best achieved window is the bound · 30.0d measured of 30d (100%)
basis            INSUFFICIENT · 1 of 4 qualifying primary instances (needs 3 more with ≥10 pt growth) · 8 excluded: low growth ×4, usage gap in span ×4
open now         primary 0% used · ≥ $30.12 left ($30.12 best − $0.00 used this window) · resets in 6d 23h
decision         plan returned ×4.20 ($82.74 extracted / $19.71 plan cost · 30.0d) — keep (keep ≥ ×1.0)
```

Every number re-derived by hand from the same run's `--json`:

- Bound: best calendar week $30.12047 (week of 2026-07-20,
  `bound_source.start_ts = 1784505600`); weekly regime → no scaling.
- Period lower bound: `30.12047 × (2,591,491 / 604,800) − 82.74201 =
  30.12047 × 4.2848727 − 82.74201 = 46.3204` → **≥ $46.32**. ✓
- Live gap (certified form): `30.12047 − 0.00 = 30.12` → ≥ $30.12. ✓
- Plan cost over the measured span: `20 × 2,591,491 / 2,629,746 =
  19.70906` → $19.71; multiple `82.74201 / 19.70906 = 4.1982` →
  **×4.20 — keep**. ✓
- Unlock: the regime's first snapshot is `1783897559`
  (2026-07-12T23:05:59Z — the §3 regime-change date), so the raw
  structural formula gives `1783897559 + 4 × 604,800 = 1786316759`
  (~2026-08-09) — less than one regime away. One more qualifying
  instance cannot close before one more cycle elapses, so the clamp
  `max(raw, now + regime)` applies: `1786224123 + 604,800 =
  1786828923` → **~2026-08-15**. ✓

Claude stanza: 0 of 4 qualifying `seven_day` instances (the statusline
hook only began collecting utilization on 2026-08-08); no dollars
printed anywhere except the measured decision line:

- Plan cost: `200 × 2,592,000 / 2,629,746 = 197.12930` → $197.13;
  multiple `2985.87625 / 197.12930 = 15.1468` → **×15.15 — keep**. ✓
- Unlock (structural, no clamp needed): first snapshot
  `1786158505` (2026-08-08T03:08:25Z) `+ 4 × 604,800 = 1788577705` →
  **~2026-09-05**. ✓

## 12. Coverage-oracle spot check

Independent recomputation of the pinned coverage table:

```
$ python3 -c "import math
cov = lambda n,j,k: sum(math.comb(n,i) for i in range(j,k)) / 2**n
print([cov(n,1,n) for n in range(2,9)], cov(8,2,7))"
[0.5, 0.75, 0.875, 0.9375, 0.96875, 0.984375, 0.9921875] 0.9296875
```

Matches the `coverage_oracle` test constants exactly: outer band
`[x₁,xₙ]` for n = 2..8 and the inner `[x₂,x₇]` at n = 8 (92.9688%).

## 13. Quantization audit

The float dirt the snap rule was designed for exists in this DB.
Across 30,346 snapshots, 295 rows carry a non-integer `used_percent`;
the distinct dirty values are:

```
7.0000000000000009   14.000000000000002   28.000000000000004
28.999999999999996   55.000000000000007   56.000000000000007
56.999999999999993   57.999999999999993
```

Every one is within 1e-6 of the integer grid (max distance 7.1e-15);
zero rows are genuinely fractional. The snap rule
(`PCT_SNAP_EPS = 1e-6`) absorbs all of them; the
fractional-`used_percent` doctor warning did not fire on this data
(`fractional_pct_observed: false` in the run's JSON).

±1 outer bound on the real qualifying instance: the one admitted codex
instance has snapped dpct 36, so its rate interval is
`[c/37, c/36, c/35] × 100` — max relative widening `36/35 − 1 =
0.028571`, exactly the `max_rel_widening` the run's JSON reports.

## 14. Gate effects on this machine

From the same run (`basis.excluded`, mirrored by `merma doctor`'s
calibration checks):

- **codex / primary (weekly)**: 9 closed candidates scanned; 1
  qualifies (dpct 36). 8 excluded: `low_growth` ×4 (observed growth
  below 10 points — quantization noise would dominate their rates),
  `usage_gap` ×4 (holes in the usage-event stream inside the
  attribution span longer than `max(0.25 × span, 30 min)`; dollars
  missing while percent grew would bias the rate low, so waste would
  be understated — rejected).
- **claude / seven_day**: 1 closed candidate, excluded by `usage_gap`;
  0 qualify. The hook began collecting on 2026-08-08, so this is the
  §9 "not yet" case, now with a printed unlock date instead of a
  shrug.

Doctor renders both as named calibration checks:

```
⚠ codex calibration    INSUFFICIENT · 1 qualifying primary instance(s) · 8 excluded: low growth ×4, usage gap in span ×4 · unlocks ~2026-08-15
⚠ claude calibration   INSUFFICIENT · 0 qualifying seven_day instance(s) · 1 excluded: usage gap in span · unlocks ~2026-09-05
✓ claude cross-window attribution   shorter-window attributions stay inside the long window (1 instance(s) checked)
```

## 15. Deletion evidence

Acceptance greps, run 2026-08-08 against the working tree:

```
$ grep -ri "wrapped\|heatmap" src README.md            → no matches
$ grep -rn "UNSTABLE\|OutputOnly\|codex_credits\|ratatui\|crossterm\|thiserror" src Cargo.toml
                                                        → no matches
$ grep -rn "allow(dead_code)" src                       → no matches
$ cargo tree | grep -ci "ratatui\|crossterm\|thiserror" → 0
```

Dependency diff, v0.1.0 → v0.2.0 (`Cargo.toml`):

```
- crossterm = "0.28"
- ratatui = "0.29"
- thiserror = "2"
```

No dependencies added. The v0.1 hits for "wrapped" in this file's own
§7 are history and stay.

## 16. Statusline hook and launchd agent, post-upgrade

The v0.2.0 binary was installed over v0.1.0 at
`~/.local/bin/merma`; the AGENTS.md postconditions were re-run:

- `merma --version` → `merma 0.2.0`, exit 0.
- `echo '{}' | merma statusline-hook` → exit 0, renders the usage
  context line.
- `~/.claude/settings.json` still parses; the `statusLine.command`
  entry is byte-identical to the pre-upgrade value
  (`/Users/jairo/.local/bin/merma statusline-hook`) — no settings diff.
- `launchctl list` shows `com.merma.collect` loaded with status 0; the
  plist mtime predates the upgrade (2026-08-07 21:09) — the agent was
  not touched. It runs `collect --quiet`, which exercises only
  collectors + store, none of the deleted code.

## 17. Credits chain removed

The Codex credits pricing chain (`[[codex_credits]]`,
`codex_event_credits`, `Constants.codex_credit_usd`) is deleted in
0.2.0. It served exactly one purpose: the §5 credits-coherence
cross-check (1878 credits ≈ $75.12 vs $80.36 token-priced, ~7%
agreement) that validated the token math before v0.1 shipped. That
recorded result stands as evidence; the chain fed no estimate and no
surface. An override price table no longer needs (and no longer
validates) a `[[codex_credits]]` section — the §8 note requiring it is
superseded.

## 18. JSON ↔ text 1:1 spot audit

From the capture-time pair (`merma --json` vs the rendered brief),
full-precision JSON left, rendered text right, under the fmt.rs rules:

| JSON value | Renders |
|---|---|
| `left_on_table_usd.low = 46.32036696793996` | `≥ $46.32` |
| `gap_usd.low = 30.12047000000002` | `≥ $30.12 left` |
| `decision.return_multiple = 4.198171547092022` | `×4.20` |
| `decision.plan_cost_measured_usd = 19.709059354021264` | `$19.71 plan cost` |
| `decision.extracted_usd = 2985.87624835001` (claude) | `$2,985.88 extracted` |
| `decision.return_multiple = 15.146790356083036` (claude) | `×15.15` |
| `insufficient.unlocks_at = 1788577705` | `~2026-09-05` |
| `measured.union_secs = 2591491` over a 30d request | `30.0d measured of 30d (100%)` |

Every dollar in the JSON is a tagged object (`kind: "floor"` here;
`kind: "band"` in the committed fixture); `left_on_table_usd` for the
uncalibrated provider is `null` with the sibling
`basis.insufficient` reason — never $0. The `text_json_equality` test
pins this equivalence for every `$` token on every run.

## 19. v0.2 audit fixes and re-derivation (2026-08-08)

An audit of the 2026-08-08 build found two blockers and a set of minor
defects. §11–§18 stay as recorded — they were true of that binary at
capture time. This section records the fixes and re-derives the changed
numbers from a fresh run of the fixed binary
(`generated_at = 1786227590`, 2026-08-08T22:19:50Z, 30-day period).

**Certified-epoch scaling (blocker).** `bound × period_windows` was
scaled over the full measured span, which for long periods includes the
prolite plan stretch (plan history 1780052984–1782535517) and the
pre-2026-07-12 non-weekly regime — spans over which the current-plan,
current-regime bound certifies nothing. The `≥` figure now scales only
over the certified epoch, `max(current-regime start, trailing
constant-plan-run start)` → measured end, and subtracts the extraction
inside that epoch (see DESIGN.md "the `≥` period scaling never leaves
the certified epoch"):

- Epoch start `1783897559` (2026-07-12T23:05:59Z, the §3 regime-change
  date; the plan run start is earlier so the regime start binds).
- Certified measured seconds `2,330,000` → `2,330,000 / 604,800 =
  3.8525132` windows; extraction inside the epoch `$54.016125`.
- `30.12047 × 3.8525132 − 54.016125 = 62.0234` → **≥ $62.02**, printed
  with its full arithmetic. ✓
- The same figure now prints for every period that contains the epoch:
  `--period all` shows `≥ $62.02 · 257.8d measured`, replacing the
  uncertified `≥ $407.23` (= 30.12047 × 36.83 all-history windows) the
  audit flagged. The 30-day figure moved too (was `≥ $46.32` =
  30.12047 × 4.2849 − 82.74201): the old form scaled 3 pre-regime days
  and subtracted extraction the epoch never contained.

**Unlock provenance (blocker).** The structural unlock renders
`max(first_snapshot + 4 regimes, now + 1 regime)`. When the clamp sets
the date, the note "(4 full windows from the first snapshot)" printed
arithmetic that does not reproduce it (1783897559 + 4 × 604,800 =
1786316759 ≈ 2026-08-09, but the brief said ~2026-08-15). The clamped
case now prints "(one full window from now — the next qualifying
instance cannot close sooner)": `1786227590 + 604,800 = 1786832390` =
the run's `insufficient.unlocks_at` → **~2026-08-15**. ✓ The unclamped
case (Claude, first snapshot 2026-08-08) keeps "(4 full windows from
the first snapshot)" → ~2026-09-05. ✓ JSON unchanged
(`path: "structural"`).

The fixed codex stanza, as rendered:

```
codex — ChatGPT Plus · $20.00/mo
  left on table    ≥ $62.02             lower bound · $30.12 best window × 3.85 current plan+regime windows (27.0d) −
                   $54.02 extracted in them · 30.0d measured of 30d (100%)
  basis            INSUFFICIENT · 1 of 4 qualifying primary instances (needs 3 more with ≥10 pt growth) · 8 excluded:
                   low growth ×4, usage gap in span ×4
                   calibrated estimate unlocks ~2026-08-15 at the earliest (one full window from now — the next
                   qualifying instance cannot close sooner)
  open now         primary    0% used · ≥ $30.12 left ($30.12 best − $0.00 used this window) · resets in 6d 22h
  decision         plan returned ×4.20 ($82.74 extracted / $19.71 plan cost · 30.0d) — keep (≥ ×1.0)
                   capture pace uncalibrated — unlocks with calibration
  ⚠ non-subscription models excluded from waste math: moonshotai/kimi-k3 (41218 tok)
```

JSON ↔ text for the new figures, same run, under the fmt.rs rules:

| JSON value | Renders |
|---|---|
| `left_on_table_usd.low = 62.02338369391553` | `≥ $62.02` |
| `basis.bound_usd_per_window = 30.12047000000002` | `$30.12 best window` |
| `basis.certified.windows = 3.8525132275132274` | `× 3.85 current plan+regime windows` |
| `basis.certified.secs = 2330000` | `(27.0d)` |
| `basis.certified.extracted_usd = 54.016125399999886` | `− $54.02 extracted in them` |
| `insufficient.unlocks_at = 1786832390` | `~2026-08-15` |

Smaller fixes, verified in the same run:

- **Wrap at 120 columns.** The brief self-wraps at a fixed 120 print
  columns (word boundaries only, continuations indented to the value
  column); previously lines reached 159 columns and relied on the
  terminal. Fixed-width by decision — terminal detection would need a
  new dependency. The golden line budget counts non-diagnostic lines;
  `⚠`/`✗` lines are exempt (they may never be dropped to fit), which
  resolves the `--period all` stanza the audit flagged at 11 raw lines:
  7 content lines + 4 exempt notes.
- **Decision wording.** `keep (keep ≥ ×1.0)` → `keep (≥ ×1.0)`; the
  verdict word is the threshold's name and printed once. Downgrade and
  straddle branches keep the full name (`below keep ≥ ×1.0`).
- **decide() never fabricates a midpoint.** A ranged, non-straddling
  operand (unreachable in the binary; pinned by test) now reports the
  conservative endpoint with the matching extracted operand, so
  extracted / plan equals the printed multiple exactly.
- **`COMPLETE_WINDOW_MIN_FRAC = 0.75`** joined the pinned-constants
  table and the `pinned_constants` regression test.
- **Doctor label column** widened to 31 cells so
  `claude cross-window attribution` no longer pushes its detail out of
  the shared column.
- **Poll-cadence orphans deleted.** `oauth_poll_secs` /
  `wham_poll_secs` lost their only consumer with the v0.1 dashboard;
  the keys, their README rows, and the minimum-cadence prohibition are
  gone. `merma collect` polls once per invocation; the launchd
  schedule (15 min) is the cadence, and AGENTS.md now says exactly
  that. Existing configs with the old keys still parse.
- **Teal retired by decision** (DESIGN.md): it was graphic ink only and
  v0.2 deleted every graphic surface.
- **README status example** regenerated from real output (the old one
  showed a CALIBRATED band this machine has never produced);
  `assets/report.svg` renamed to `assets/brief.svg`; "one screen per
  provider" corrected to one screen with a stanza per provider.
- Worked Example B's pinned ts `1786244905` is 2026-08-09T03:08:25Z, so
  its structural unlock `1788664105` is **2026-09-06**; the spec
  prose's "≈ 2026-09-05" day label is off by one for its own pinned
  timestamp. Fixture assertions were already correct; the stray
  comments inheriting the label were fixed. Real data (first snapshot
  2026-08-08T03:08:25Z) correctly prints ~2026-09-05, matching §11.

Gates at the end of the audit-fix pass: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test` (77 tests)
all green; assets re-captured with freeze from the fixed binary.

## 20. Second audit pass: contract truth, grep gate, reinstall (2026-08-08)

A second v0.2 audit found the README `--json` claims overstated, the
handoff's literal grep gate failing on word-wrap vocabulary, a stale
installed binary, and three taste defects. §11–§19 stay as recorded —
true of their binaries at their capture times. Fixes, verified against
a fresh run (`generated_at = 1786229952`):

**`--json` claims scoped and completed.** The README claimed "every
dollar figure is a tagged object"; live JSON serializes exact measured
operands (`extracted_usd`, `plan.monthly_usd`, `plan.window_cost_usd`,
`basis.floor_usd_per_window`, `basis.bound_usd_per_window`,
`decision.plan_cost_measured_usd`, `basis.certified.extracted_usd`) as
plain floats. The claim now reads "every **estimated** dollar figure"
— only `left_on_table_usd` and `gap_usd` carry uncertainty to tag;
measured operands have none. And the "UNKNOWN is null plus a sibling
reason" claim was false for `open_windows[].gap_usd` (bare `null`,
demonstrated on claude `seven_day`): `OpenWindow` gains `gap_reason`,
present exactly when `gap_usd` is null (within-schema field growth) —
this run emits `"uncalibrated and no achieved-window bound yet"`
(operative, no bound) and `"non-operative — the operative window
carries the dollars"` (`five_hour`); the third arm, extraction
covering the bound, is pinned by test. Invariant test:
`null_gap_always_carries_reason`.

**Grep gate.** §15 recorded `grep -ri "wrapped\|heatmap" src README.md
→ no matches` while 9 word-wrap-vocabulary hits lived in src/brief.rs
(`push_wrapped`, "Wrapped continuations", "unwrapped render") — not
the deleted feature, but the gate is literal. Renamed to `push_folded`
/ "folded continuations" / "unfolded render"; the gate now passes
literally: `grep -ri "wrapped\|heatmap" src/ README.md` → exit 1.

**Reinstall.** `~/.local/bin/merma` was a stale earlier v0.2 build
(sha256 9a9a8f81… at this pass's start; the audit had caught 17b4728d…
printing the `keep (keep ≥ ×1.0)` stutter and the uncertified
`≥ $46.32`). Reinstalled: installed sha256 244284b0… ==
`target/release/merma` byte-identical. Postconditions re-run:
`merma --version` → 0.2.0 exit 0; `echo '{}' | merma statusline-hook`
exit 0; `~/.claude/settings.json` parses; doctor `✓ store`,
`✓ claude transcripts`; `launchctl list` shows `com.merma.collect`
status 0. The machine's statusline now runs the fixed build.

**Taste.**

- The `≥` headline wrapped with a widowed `−` at end of line
  ("…windows (27.0d) − ⏎ $54.02 extracted…"). The fold rule now binds
  an operator word (`−`, `×`, `≥`, `≈`, `–`, `-`) to its right operand
  — a break lands before the operator, never after; `·` and `—` remain
  legitimate line-end separators. Pinned by
  `fold_never_orphans_an_operator`; this run renders "…windows
  (27.0d) ⏎ − $54.02 extracted in them". The bind requires exactly one
  space, so the value-column pad never fuses.
- Token counts group thousands like the dollars beside them:
  `41218 tok` → `41,218 tok` (`fmt::count`, one numeral rule per
  surface).
- `⚠` note sentences render dim (glyph-only yellow), restoring the
  v0.1 brightness hierarchy; `✗` errors stay full-bright.
- Doctor's value column keeps a ≥ 2-space gutter (label field 31 → 32
  cells) so `claude cross-window attribution` no longer touches its
  detail.

Assets re-captured from the reinstalled binary:
`freeze --execute "merma" --window -o assets/brief.svg` (and
`"merma doctor"` → assets/doctor.svg; same flags for the PNG spot
captures). Gates: `cargo fmt --check`, `cargo clippy --all-targets --
-D warnings`, `cargo test` (80 tests) all green.
