//! The v0.2 estimator: order-statistic band, confidence tiers, floors, live
//! gap, capture pace, unlock dates, and the decision layer.
//!
//! Everything in this module is pure and deterministic — no store access, no
//! RNG, no clock reads. `waste.rs` adapts store data into [`EstimatorInput`]
//! and calls [`assemble`]. Every displayed digit is traceable to: instance
//! count, achieved coverage, the gates that excluded instances, and whichever
//! of floor / band / quantization dominates.
//!
//! Statistical doctrine (condensed; DESIGN.md carries the full version):
//! - Center = sample median of per-instance $/window rates.
//! - Sampling band = exact distribution-free order-statistic CI for the
//!   median; achieved coverage is printed exactly, never rounded.
//! - Quantization (±1 on integer-quantized percent endpoints) composes as a
//!   strict outer bound: lo/hi rate arrays are sorted INDEPENDENTLY of the
//!   point rates (c/(d±1) is not monotone in c/d across instances).
//! - The band covers the typical window, not the next window.
//! - Influence = max delete-1 median shift (the jackknife SE is inconsistent
//!   for the median and is not used).
//! - The floor/bound is a certified lower bound on window value; it composes
//!   by max with every lower edge and never tightens an upper edge, never
//!   affects the influence diagnostic, and never manufactures stability.

use serde::Serialize;

/// Mean Gregorian month, seconds (2,629,746).
pub const SECS_PER_MONTH: f64 = 30.436875 * 86_400.0;
/// Minimum qualifying instances for the CALIBRATED tier (== passes).
pub const N_MIN_CALIBRATED: usize = 4;
/// Recency window: at most this many newest qualifying instances calibrate.
pub const MAX_INSTANCES_FOR_ESTIMATE: usize = 8;
/// Growth gate = quantization gate (== passes): ±1 endpoint error ⇒ ≤10% rel.
pub const MIN_DPCT_FOR_ESTIMATE: f64 = 10.0;
/// Influence demotion threshold on the max delete-1 median shift (== passes).
pub const MAX_LOO_SHIFT: f64 = 0.25;
/// Usage-gap gate: fraction of the attribution span (== admits).
pub const G_USAGE_FRAC: f64 = 0.25;
/// Usage-gap gate floor, seconds.
pub const G_USAGE_FLOOR_SECS: f64 = 1800.0;
/// Snapshot-staleness gate: fraction of the regime duration (== admits).
pub const G_SNAP_FRAC: f64 = 0.10;
/// At this n the band switches from [x₁,xₙ] to [x₂,xₙ₋₁].
pub const INNER_BAND_MIN_N: usize = 8;
/// Capture-curve age checkpoints; the LARGEST ≤ current age is used.
pub const CURVE_CHECKPOINTS: [f64; 3] = [0.25, 0.50, 0.75];
/// Keep/downgrade decision cutoff, printed in the decision line.
pub const KEEP_THRESHOLD: f64 = 1.0;
/// Integer-grid snap tolerance for used_percent float dirt.
pub const PCT_SNAP_EPS: f64 = 1e-6;
/// A closed instance observed over at least this fraction of its regime
/// counts as a complete window for the unlock-rate formula. (The spec leaves
/// "complete" undefined; a banked-reset instance — much shorter than its
/// regime — must not count, per worked example B.)
pub const COMPLETE_WINDOW_MIN_FRAC: f64 = 0.75;

// ── exact order-statistic machinery ──

/// Exact C(n,k); u64-exact for n ≤ 16 (callers guarantee n ≤ 8).
/// n=64 exactness is NOT claimed — intermediate products overflow u64.
pub fn binom(n: u64, k: u64) -> u64 {
    debug_assert!(n <= 16);
    (1..=k).fold(1, |a, i| a * (n - i + 1) / i)
}

/// Exact coverage of [x_(j), x_(k)] (1-indexed) as a CI for the median:
/// P(x_(j) ≤ median ≤ x_(k)) = Σ_{i=j}^{k−1} C(n,i)·(1/2)^n.
/// The only assumption is exchangeability of instances.
pub fn median_ci_coverage(n: usize, j: usize, k: usize) -> f64 {
    debug_assert!(n <= 16 && j >= 1 && k <= n);
    let total: u64 = (j..k).map(|i| binom(n as u64, i as u64)).sum();
    total as f64 / (1u64 << n) as f64
}

/// Sample median of an ascending-sorted slice (even n: mean of central pair).
pub fn median_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    debug_assert!(n >= 1);
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Band order-statistic indices (1-indexed): [x₁,xₙ] below INNER_BAND_MIN_N,
/// [x₂,xₙ₋₁] at or above it.
pub fn band_indices(n: usize) -> (usize, usize) {
    if n >= INNER_BAND_MIN_N {
        (2, n - 1)
    } else {
        (1, n)
    }
}

/// Compose the sampling band with the quantization outer bound.
/// `points`, `lows`, `highs` each sorted ascending INDEPENDENTLY — never sort
/// lo/hi by the point rate; c/(d±1) is not monotone in c/d across instances.
/// Returns (band_lo, band_med, band_hi, achieved_coverage).
pub fn composed_band(points: &[f64], lows: &[f64], highs: &[f64]) -> (f64, f64, f64, f64) {
    let n = points.len();
    debug_assert!(n >= 2 && lows.len() == n && highs.len() == n);
    let med = median_sorted(points);
    let (j, k) = band_indices(n);
    (lows[j - 1], med, highs[k - 1], median_ci_coverage(n, j, k))
}

/// Max relative delete-1 median shift and the index (into `rates`, original
/// order) of the controlling observation. Returns (index, shift).
/// A zero median yields shift 0.0 (never NaN into a serialized field).
pub fn loo_controlling(rates: &[f64]) -> (usize, f64) {
    debug_assert!(rates.len() >= 2);
    let mut sorted = rates.to_vec();
    sorted.sort_by(f64::total_cmp);
    let med = median_sorted(&sorted);
    if med.abs() < f64::EPSILON {
        return (0, 0.0);
    }
    let mut best = (0usize, 0.0f64);
    for (i, r) in rates.iter().enumerate() {
        // Remove ONE occurrence of r from the sorted copy.
        let pos = sorted.partition_point(|x| x < r);
        let mut rest = Vec::with_capacity(sorted.len() - 1);
        rest.extend_from_slice(&sorted[..pos]);
        rest.extend_from_slice(&sorted[pos + 1..]);
        let shift = ((median_sorted(&rest) - med) / med).abs();
        if shift > best.1 {
            best = (i, shift);
        }
    }
    best
}

/// Max relative delete-1 median shift over an ascending-sorted rate slice.
pub fn max_loo_shift(points_sorted: &[f64]) -> f64 {
    loo_controlling(points_sorted).1
}

// ── quantization ──

/// Snap float-dirty used_percent to the integer grid. Returns the value to
/// use and whether the input was GENUINELY fractional (kept raw; the caller
/// surfaces a doctor warning — the quantization model may be wrong).
pub fn snap_pct(pct: f64) -> (f64, bool) {
    let q = pct.round();
    if (pct - q).abs() < PCT_SNAP_EPS {
        (q, false)
    } else {
        (pct, true)
    }
}

/// Per-instance rate with the worst-case ±1 quantization interval.
/// Both endpoints of d = peak − first are integer-quantized, so the true
/// growth lies in (d−1, d+1). Returns (lo, point, hi) in $/full-window.
pub fn rate_interval(attributed_usd: f64, dpct: f64) -> (f64, f64, f64) {
    debug_assert!(dpct > 1.0);
    (
        attributed_usd / (dpct + 1.0) * 100.0,
        attributed_usd / dpct * 100.0,
        attributed_usd / (dpct - 1.0) * 100.0,
    )
}

// ── admission pipeline ──

/// A closed window instance of the operative regime, prepared for admission.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Full snapshot series of the instance: (ts, used_percent), ascending.
    pub points: Vec<(i64, f64)>,
    /// First observation of the peak percent (attribution span end).
    pub peak_ts: i64,
    /// Full API-equivalent dollars attributed to (first_ts, peak_ts].
    pub attributed_usd: f64,
    /// True when the attribution span contains unpriced-model usage.
    pub unpriced: bool,
    /// True when era-approximate prices contributed to the dollars.
    pub approx: bool,
    /// Usage-event timestamps inside (first_ts, peak_ts], ascending.
    pub event_ts: Vec<i64>,
}

/// An instance that passed every gate.
#[derive(Debug, Clone)]
pub struct Qualified {
    pub first_ts: i64,
    pub last_ts: i64,
    /// Snapshot series with snapped percents (for the capture curve).
    pub points: Vec<(i64, f64)>,
    pub dpct: f64,
    pub rate: f64,
    pub rate_lo: f64,
    pub rate_hi: f64,
    pub approx: bool,
}

/// Exclusion counter for one named gate reason.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Exclusion {
    pub reason: &'static str,
    pub count: usize,
}

pub const REASON_LOW_GROWTH: &str = "low_growth";
pub const REASON_UNPRICED: &str = "unpriced";
pub const REASON_USAGE_GAP: &str = "usage_gap";
pub const REASON_SNAPSHOT_GAP: &str = "snapshot_gap";

/// Admission result: the qualifying set (newest-first) plus counted,
/// named exclusions. Excluded instances are evidence collection cadence, not
/// usage, is the bottleneck — they feed the unlock estimate.
#[derive(Debug, Clone, Default)]
pub struct Admission {
    pub qualifying: Vec<Qualified>,
    pub excluded: Vec<Exclusion>,
    /// A genuinely fractional used_percent was observed (doctor warning).
    pub fractional_pct: bool,
}

/// Largest gap between consecutive timestamps inside [bounds.0, bounds.1],
/// INCLUDING the edge gaps to both boundaries.
pub fn max_gap(bounds: (i64, i64), ts_ascending: &[i64]) -> i64 {
    let mut prev = bounds.0;
    let mut worst = 0i64;
    for &t in ts_ascending {
        worst = worst.max(t - prev);
        prev = t;
    }
    worst.max(bounds.1 - prev)
}

/// Run the admission pipeline over ALL candidates of the regime, newest-first,
/// stopping once MAX_INSTANCES_FOR_ESTIMATE qualify (no candidate cap — the
/// scan continues past any number of exclusions). Each instance passes ALL
/// gates or is excluded with one counted reason (first failing gate, in the
/// order Q → P → U → S).
pub fn admit(candidates_newest_first: &[Candidate], regime_secs: i64) -> Admission {
    let mut out = Admission::default();
    let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for cand in candidates_newest_first {
        if out.qualifying.len() >= MAX_INSTANCES_FOR_ESTIMATE {
            break;
        }
        let Some(&(first_ts, first_raw)) = cand.points.first() else {
            continue;
        };
        let last_ts = cand.points.last().map(|p| p.0).unwrap_or(first_ts);
        let peak_raw = cand.points.iter().map(|p| p.1).fold(f64::MIN, f64::max);
        let (first_pct, f_frac) = snap_pct(first_raw);
        let (peak_pct, p_frac) = snap_pct(peak_raw);
        out.fractional_pct |= f_frac || p_frac;
        let dpct = (peak_pct - first_pct).max(0.0);
        // Gate Q — quantization/growth (== qualifies).
        if dpct < MIN_DPCT_FOR_ESTIMATE {
            *counts.entry(REASON_LOW_GROWTH).or_default() += 1;
            continue;
        }
        // Gate P — pricing: unpriced usage would silently deflate the rate.
        if cand.unpriced {
            *counts.entry(REASON_UNPRICED).or_default() += 1;
            continue;
        }
        // Gate U — numerator censoring: a transcript hole while percent grew
        // means dollars are missing → rate biased low → reject (== admits).
        let span = (cand.peak_ts - first_ts).max(0);
        let g_usage = (G_USAGE_FRAC * span as f64).max(G_USAGE_FLOOR_SECS);
        if max_gap((first_ts, cand.peak_ts), &cand.event_ts) as f64 > g_usage {
            *counts.entry(REASON_USAGE_GAP).or_default() += 1;
            continue;
        }
        // Gate S — denominator censoring: a peak that rose and decayed
        // between sparse snapshots corrupts dpct (== admits).
        let g_snap = G_SNAP_FRAC * regime_secs as f64;
        let snap_ts: Vec<i64> = cand
            .points
            .iter()
            .map(|p| p.0)
            .filter(|t| *t >= first_ts && *t <= cand.peak_ts)
            .collect();
        if max_gap((first_ts, cand.peak_ts), &snap_ts) as f64 > g_snap {
            *counts.entry(REASON_SNAPSHOT_GAP).or_default() += 1;
            continue;
        }
        let (lo, rate, hi) = rate_interval(cand.attributed_usd, dpct);
        let points = cand
            .points
            .iter()
            .map(|&(t, p)| {
                let (sp, frac) = snap_pct(p);
                out.fractional_pct |= frac;
                (t, sp)
            })
            .collect();
        out.qualifying.push(Qualified {
            first_ts,
            last_ts,
            points,
            dpct,
            rate,
            rate_lo: lo,
            rate_hi: hi,
            approx: cand.approx,
        });
    }
    out.excluded = counts
        .into_iter()
        .map(|(reason, count)| Exclusion { reason, count })
        .collect();
    out
}

// ── operative-window selection ──

/// True when `id` is scoped: another window id of the same provider is a
/// STRICT prefix of it (e.g. `seven_day` prefixes `seven_day_opus`).
/// Data-driven; survives future provider additions.
pub fn is_scoped(id: &str, all_ids: &[String]) -> bool {
    all_ids
        .iter()
        .any(|other| other.as_str() != id && id.starts_with(other.as_str()))
}

/// One non-scoped window of a provider, summarized for operative selection.
#[derive(Debug, Clone)]
pub struct WindowSummary {
    pub window_minutes: i64,
    pub last_snapshot_ts: i64,
}

/// The operative window = the LONGEST window with a live snapshot (within one
/// regime-duration of now); ties break to the most recent snapshot. If no
/// window is live (stale DB), fall back to v0.1's rule: latest snapshot,
/// ties to longer. The binding constraint of a subscription is its longest
/// concurrent limit. Returns an index into `windows`.
pub fn select_operative(windows: &[WindowSummary], now: i64) -> Option<usize> {
    let live = |w: &WindowSummary| {
        w.window_minutes > 0 && now - w.last_snapshot_ts <= w.window_minutes * 60
    };
    let among_live = windows
        .iter()
        .enumerate()
        .filter(|(_, w)| live(w))
        .max_by_key(|(_, w)| (w.window_minutes, w.last_snapshot_ts));
    if let Some((i, _)) = among_live {
        return Some(i);
    }
    windows
        .iter()
        .enumerate()
        .max_by_key(|(_, w)| (w.last_snapshot_ts, w.window_minutes))
        .map(|(i, _)| i)
}

// ── serialized output types ──

/// A dollar figure is always tagged — never a bare float.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Band {
    pub lo: f64,
    pub med: f64,
    pub hi: f64,
    /// Achieved coverage — exact, discrete, never a round 90%.
    pub coverage: f64,
    pub n: usize,
    pub approx: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Dollars {
    /// Two-sided calibrated band (`≈ lo – hi`, median printed beside).
    Band(Band),
    /// Certified one-sided lower bound (`≥ low`).
    Floor { low: f64, approx: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Tier {
    #[serde(rename = "MEASURED")]
    Measured,
    #[serde(rename = "CALIBRATED")]
    Calibrated,
    #[serde(rename = "INSUFFICIENT")]
    Insufficient,
}

/// What set the bound: an achieved calendar week or a 100%-peak instance.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BoundSource {
    Week { start_ts: i64 },
    Instance { first_ts: i64 },
}

/// The certified epoch the `≥` period scaling is allowed to cover: measured
/// time under the CURRENT plan and CURRENT regime, clipped to the period.
/// `bound × windows` is only a certified lower bound where plan and regime
/// are constant — a prior plan's windows had different capacity and a prior
/// regime's windows had a different denominator.
#[derive(Debug, Clone, Serialize)]
pub struct CertifiedScaling {
    /// Epoch start: max(current-regime start, current-plan-run start).
    pub start_ts: i64,
    /// Interval-union measured seconds inside the epoch, clipped to period.
    pub secs: i64,
    /// secs / regime_secs — the window count the `≥` figure scales over.
    pub windows: f64,
    /// Extracted dollars inside the epoch (clipped to the period).
    pub extracted_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Quantization {
    pub min_dpct: f64,
    /// Worst relative widening of the top edge: 1/(min_dpct − 1).
    pub max_rel_widening: f64,
}

/// The single instance controlling the estimate when LOO-demoted.
#[derive(Debug, Clone, Serialize)]
pub struct LooDemotion {
    pub first_ts: i64,
    pub rate: f64,
    pub shift: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Insufficient {
    /// k = N_MIN_CALIBRATED − n_qualifying (0 when demoted by influence).
    pub needed_instances: usize,
    pub missing: String,
    pub unlocks_at: Option<i64>,
    pub path: Option<&'static str>,
    pub demoted_by: Option<LooDemotion>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Basis {
    pub tier: Tier,
    pub window_id: String,
    pub regime_minutes: i64,
    pub n_qualifying: usize,
    pub excluded: Vec<Exclusion>,
    pub band_usd_per_window: Option<Band>,
    pub max_loo_shift: Option<f64>,
    pub quantization: Option<Quantization>,
    /// Achieved-best calendar week scaled to the window (weekly path only).
    pub floor_usd_per_window: Option<f64>,
    /// bound = max(floor_window, best 100%-instance dollars) — the only floor
    /// value the tiers, gaps, and period math consume.
    pub bound_usd_per_window: Option<f64>,
    pub bound_source: Option<BoundSource>,
    /// Present whenever a bound exists: the plan/regime-constant epoch that
    /// certifies the `≥` period scaling (and the mirror the renderer uses).
    pub certified: Option<CertifiedScaling>,
    pub floor_binding: bool,
    pub window_worth_multiple: Option<f64>,
    pub period_windows: f64,
    pub insufficient: Option<Insufficient>,
    /// Doctor warning: non-integer used_percent observed.
    pub fractional_pct_observed: bool,
}

/// Historical context for a non-operative window (never dollars — a shorter
/// window is capacity the operative limit already bounds; a dollar
/// extrapolation would double-count the same tokens).
#[derive(Debug, Clone, Serialize)]
pub struct WindowContext {
    pub median_peak_pct: f64,
    pub n: usize,
    pub hit_100: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Triple {
    pub lo: f64,
    pub med: f64,
    pub hi: f64,
}

/// Pace comparison at the age-conditional capture-curve checkpoint.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaceComparison {
    /// age < first checkpoint — "window too young for pace comparison".
    TooYoung,
    /// Fewer than N_MIN_CALIBRATED supporting instances at the checkpoint.
    Uncalibrated { checkpoint_pct: u32 },
    Calibrated {
        checkpoint_pct: u32,
        median_used_at_checkpoint: f64,
        curve_n: usize,
        curve_skipped: usize,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Capture {
    /// Dollars per DAY needed to capture the gap (renderers divide by 24 for
    /// /h when remaining < 24h).
    pub needed_per_day: Triple,
    pub remaining_secs: i64,
    pub age_frac: f64,
    pub pace: PaceComparison,
}

/// Operands of the certified (assumption-free) live gap, for rendering
/// "($X best − $Y used this window)".
#[derive(Debug, Clone, Serialize)]
pub struct CertifiedGap {
    pub bound: f64,
    pub extracted_in_window_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenWindow {
    pub window_id: String,
    pub operative: bool,
    pub used_pct: f64,
    pub resets_at: Option<i64>,
    /// None = gap unmeasured. UNKNOWN never becomes zero.
    pub gap_usd: Option<Dollars>,
    /// Present exactly when `gap_usd` is null: the sibling reason the JSON
    /// contract promises for every UNKNOWN dollar figure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap_reason: Option<&'static str>,
    pub certified: Option<CertifiedGap>,
    pub capture: Option<Capture>,
    pub context: Option<WindowContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    #[serde(rename = "KEEP")]
    Keep,
    #[serde(rename = "DOWNGRADE")]
    Downgrade,
    #[serde(rename = "UNKNOWN")]
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub verdict: Verdict,
    pub return_multiple: Option<f64>,
    pub extracted_usd: Option<f64>,
    pub plan_cost_measured_usd: Option<f64>,
    pub threshold_keep: f64,
    pub approx: bool,
    pub reason: Option<String>,
}

/// The per-provider estimate: headline, basis, open windows.
#[derive(Debug, Clone, Serialize)]
pub struct Estimate {
    pub left_on_table_usd: Option<Dollars>,
    pub basis: Basis,
    pub open_windows: Vec<OpenWindow>,
    /// Plan cost of one operative window: monthly × regime_secs / month.
    pub window_cost_usd: Option<f64>,
}

// ── assembly inputs ──

#[derive(Debug, Clone)]
pub struct OpenWindowInput {
    pub used_pct: f64,
    pub resets_at: Option<i64>,
    /// First observation of the open instance (age + attribution anchor).
    pub first_ts: i64,
    /// Attributed dollars since the open instance's first_ts.
    pub extracted_in_window_usd: f64,
}

#[derive(Debug, Clone)]
pub struct OtherWindowInput {
    pub window_id: String,
    pub used_pct: f64,
    pub resets_at: Option<i64>,
    pub context: Option<WindowContext>,
}

#[derive(Debug, Clone)]
pub struct EstimatorInput {
    pub window_id: String,
    pub regime_minutes: i64,
    pub admission: Admission,
    /// Any instance of the current regime hit a snapped 100% peak.
    pub any_hundred_pct: bool,
    /// Best attributed dollars among 100%-peak instances (0.0 when none).
    pub best_hundred_usd: f64,
    pub best_hundred_first_ts: Option<i64>,
    /// Achieved-best calendar week scaled to the window (0.0 when none).
    pub floor_window_usd: f64,
    pub floor_week_start: Option<i64>,
    /// Approx-price contamination of whichever source sets the bound.
    pub bound_approx: bool,
    /// Full-period extracted dollars (cache-included api_equiv).
    pub extracted_usd: f64,
    /// Interval-union measured seconds — the ONLY period-scaling denominator.
    pub measured_secs: i64,
    /// Start of the certified epoch: measured time under the CURRENT plan and
    /// CURRENT regime. Only this span certifies `bound × windows` scaling.
    pub certified_start_ts: i64,
    /// Interval-union measured seconds inside the certified epoch (≤
    /// measured_secs), clipped to the period.
    pub certified_secs: i64,
    /// Extracted dollars inside the certified epoch (clipped to the period).
    pub certified_extracted_usd: f64,
    pub have_billing: bool,
    /// May be NaN when the plan price is unknown.
    pub plan_monthly_usd: f64,
    pub now: i64,
    pub first_snapshot_ts: Option<i64>,
    /// Closed instances observed over ≥ COMPLETE_WINDOW_MIN_FRAC of a regime.
    pub complete_windows_observed: usize,
    pub open: Option<OpenWindowInput>,
    pub others: Vec<OtherWindowInput>,
}

// ── cold-start unlock ──

/// Deterministic unlock date for the CALIBRATED tier, re-evaluated per scan.
/// Returns (unlock_ts, path) — path is "rate" or "structural".
pub fn unlock_ts(
    now: i64,
    n_qualifying: usize,
    complete_windows_observed: usize,
    first_snapshot_ts: i64,
    regime_secs: i64,
) -> (i64, &'static str) {
    let k = N_MIN_CALIBRATED.saturating_sub(n_qualifying) as f64;
    if complete_windows_observed >= 1 {
        let q = n_qualifying as f64 / complete_windows_observed as f64;
        let cycles = (k / q.max(1e-9)).ceil() as i64;
        (now + cycles * regime_secs, "rate")
    } else {
        // Clamped to at least one regime from now: with a long but
        // never-fully-observed history, the raw formula can land in the past,
        // and a past "at the earliest" date is information-free. One more
        // qualifying instance cannot close before one more cycle elapses.
        (
            (first_snapshot_ts + N_MIN_CALIBRATED as i64 * regime_secs).max(now + regime_secs),
            "structural",
        )
    }
}

// ── capture curve ──

/// The LARGEST checkpoint ≤ the current age fraction ("nearest" is rejected —
/// it can select a checkpoint the window has not reached yet). None when the
/// window is too young for a pace comparison.
pub fn checkpoint_for_age(age_frac: f64) -> Option<f64> {
    CURVE_CHECKPOINTS
        .iter()
        .copied()
        .filter(|c| *c <= age_frac)
        .fold(None, |acc: Option<f64>, c| {
            Some(acc.map_or(c, |a| a.max(c)))
        })
}

/// Per qualifying instance, used_percent at the checkpoint instant
/// (first_ts + checkpoint × regime_secs): the last snapshot at or before it.
/// Instances whose observation ended before the instant are skipped and
/// counted. Returns (median, n, skipped) — None median when n == 0.
fn curve_at_checkpoint(
    qualifying: &[Qualified],
    checkpoint: f64,
    regime_secs: i64,
) -> (Option<f64>, usize, usize) {
    let mut vals = Vec::new();
    let mut skipped = 0usize;
    for q in qualifying {
        let instant = q.first_ts + (checkpoint * regime_secs as f64) as i64;
        if q.last_ts < instant {
            skipped += 1;
            continue;
        }
        if let Some(&(_, pct)) = q.points.iter().rev().find(|(t, _)| *t <= instant) {
            vals.push(pct);
        } else {
            skipped += 1;
        }
    }
    vals.sort_by(f64::total_cmp);
    let n = vals.len();
    ((n > 0).then(|| median_sorted(&vals)), n, skipped)
}

// ── decision layer ──

/// Line 1 — plan worth at achieved pace: extracted dollars vs. plan cost over
/// the measured span. Adds no statistics. The operand may be a range; a range
/// straddling the threshold yields UNKNOWN, never a coin flip.
pub fn decide(
    extracted: Option<(f64, f64)>,
    plan_cost_measured: Option<f64>,
    approx: bool,
) -> Decision {
    let plan = plan_cost_measured.filter(|p| p.is_finite() && *p > 0.0);
    let ex = extracted.filter(|(lo, hi)| lo.is_finite() && hi.is_finite() && lo <= hi);
    let base = Decision {
        verdict: Verdict::Unknown,
        return_multiple: None,
        extracted_usd: None,
        plan_cost_measured_usd: plan,
        threshold_keep: KEEP_THRESHOLD,
        approx,
        reason: None,
    };
    let Some((ex_lo, ex_hi)) = ex else {
        return Decision {
            reason: Some("extracted dollars over the measured span are unknown".into()),
            ..base
        };
    };
    let Some(plan) = plan else {
        return Decision {
            extracted_usd: (ex_lo == ex_hi).then_some(ex_lo),
            reason: Some(
                "plan cost over the measured span is unknown (no measured time or unknown plan price)"
                    .into(),
            ),
            ..base
        };
    };
    let (m_lo, m_hi) = (ex_lo / plan, ex_hi / plan);
    if m_lo < KEEP_THRESHOLD && m_hi >= KEEP_THRESHOLD {
        return Decision {
            extracted_usd: None,
            reason: Some(format!("straddles keep ≥ ×{KEEP_THRESHOLD:.1} — undecided")),
            ..base
        };
    }
    // Ranged, non-straddling operands: NEVER a fabricated midpoint. The
    // reported multiple is the CONSERVATIVE endpoint — a certified bound the
    // reader can redo: Keep → the low endpoint (returned ≥ ×m_lo ≥ keep);
    // Downgrade → the high endpoint (returned ≤ ×m_hi < keep) — and
    // extracted_usd carries the MATCHING endpoint so extracted / plan equals
    // the printed multiple exactly. Unreachable in the binary today
    // (waste.rs passes point operands); a caller that ships genuinely ranged
    // operands must first extend the renderer to print the range with its
    // one-sided symbol.
    let (m, ex) = if m_lo >= KEEP_THRESHOLD {
        (m_lo, ex_lo)
    } else {
        (m_hi, ex_hi)
    };
    Decision {
        verdict: if m_lo >= KEEP_THRESHOLD {
            Verdict::Keep
        } else {
            Verdict::Downgrade
        },
        return_multiple: Some(m),
        extracted_usd: Some(ex),
        plan_cost_measured_usd: Some(plan),
        threshold_keep: KEEP_THRESHOLD,
        approx,
        reason: (m_lo < KEEP_THRESHOLD)
            .then(|| format!("downgrade candidate (below keep ≥ ×{KEEP_THRESHOLD:.1})")),
    }
}

// ── assembly ──

fn date_utc(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::Utc
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".into())
}

/// Assemble the full estimate from prepared inputs. Pure and deterministic.
pub fn assemble(input: EstimatorInput) -> Estimate {
    let regime_secs = input.regime_minutes * 60;
    let adm = &input.admission;
    let n_q = adm.qualifying.len();
    let approx_any = adm.qualifying.iter().any(|q| q.approx);

    // Sampling band composed with the quantization outer bound — the three
    // arrays are sorted independently (strict outer bound, not quadrature).
    let mut points: Vec<f64> = adm.qualifying.iter().map(|q| q.rate).collect();
    let mut lows: Vec<f64> = adm.qualifying.iter().map(|q| q.rate_lo).collect();
    let mut highs: Vec<f64> = adm.qualifying.iter().map(|q| q.rate_hi).collect();
    points.sort_by(f64::total_cmp);
    lows.sort_by(f64::total_cmp);
    highs.sort_by(f64::total_cmp);
    let band = (n_q >= 2).then(|| {
        let (lo, med, hi, coverage) = composed_band(&points, &lows, &highs);
        Band {
            lo,
            med,
            hi,
            coverage,
            n: n_q,
            approx: approx_any,
        }
    });
    let rates_orig: Vec<f64> = adm.qualifying.iter().map(|q| q.rate).collect();
    let loo_shift = (n_q >= 2).then(|| max_loo_shift(&points));

    // The bound: certified lower bound on window value; composes by max with
    // every lower edge, never tightens an upper edge.
    let bound = input.floor_window_usd.max(input.best_hundred_usd);
    let bound_source = if bound <= 0.0 {
        None
    } else if input.best_hundred_usd >= input.floor_window_usd {
        input
            .best_hundred_first_ts
            .map(|t| BoundSource::Instance { first_ts: t })
    } else {
        input
            .floor_week_start
            .map(|t| BoundSource::Week { start_ts: t })
    };

    // Tiers, evaluated in order; first match wins. Equality promotes (bound
    // == band_med → MEASURED; n == 4 and shift == 0.25 → CALIBRATED).
    let tier = if input.any_hundred_pct
        || (bound > 0.0 && band.as_ref().is_some_and(|b| bound >= b.med))
    {
        Tier::Measured
    } else if n_q >= N_MIN_CALIBRATED && loo_shift.is_some_and(|s| s <= MAX_LOO_SHIFT) {
        Tier::Calibrated
    } else {
        Tier::Insufficient
    };

    let period_windows = if regime_secs > 0 {
        input.measured_secs as f64 / regime_secs as f64
    } else {
        0.0
    };

    // Certified epoch: `bound × windows` is a certified lower bound ONLY over
    // measured time where the plan and regime match the bound's — a prior
    // plan's windows had different capacity, a prior regime a different
    // denominator. The `≥` figure never scales past this epoch.
    let certified = (bound > 0.0 && regime_secs > 0).then(|| CertifiedScaling {
        start_ts: input.certified_start_ts,
        secs: input.certified_secs,
        windows: input.certified_secs as f64 / regime_secs as f64,
        extracted_usd: input.certified_extracted_usd,
    });

    // Period estimate — scales by the MEASURED span, never the requested
    // period. A lower bound that computes to $0 renders no dollars: there is
    // no measured shortfall yet, and "$0" would misread as "nothing wasted".
    let left_on_table = match tier {
        _ if !input.have_billing || period_windows <= 0.0 => None,
        Tier::Calibrated => band.as_ref().map(|b| {
            let display_lo = b.lo.max(bound);
            Dollars::Band(Band {
                lo: (display_lo * period_windows - input.extracted_usd).max(0.0),
                med: (b.med * period_windows - input.extracted_usd).max(0.0),
                hi: (b.hi * period_windows - input.extracted_usd).max(0.0),
                coverage: b.coverage,
                n: b.n,
                approx: b.approx,
            })
        }),
        Tier::Measured | Tier::Insufficient => certified.as_ref().and_then(|c| {
            let low = (bound * c.windows - c.extracted_usd).max(0.0);
            (low > 0.0).then_some(Dollars::Floor {
                low,
                approx: input.bound_approx,
            })
        }),
    };

    // Window-worth multiple (basis context, never the verdict operand).
    let window_cost = (input.plan_monthly_usd.is_finite() && regime_secs > 0)
        .then(|| input.plan_monthly_usd * regime_secs as f64 / SECS_PER_MONTH);
    let window_worth = window_cost.filter(|c| *c > 0.0).and_then(|c| match tier {
        Tier::Calibrated => band.as_ref().map(|b| b.med / c),
        Tier::Measured => (bound > 0.0).then_some(bound / c),
        Tier::Insufficient => None,
    });

    // INSUFFICIENT detail: exactly what is missing and when it unlocks.
    let insufficient = (tier == Tier::Insufficient).then(|| {
        if n_q < N_MIN_CALIBRATED {
            let k = N_MIN_CALIBRATED - n_q;
            let unlock = input.first_snapshot_ts.map(|first| {
                unlock_ts(
                    input.now,
                    n_q,
                    input.complete_windows_observed,
                    first,
                    regime_secs,
                )
            });
            Insufficient {
                needed_instances: k,
                missing: format!(
                    "{k} more qualifying {} instances with >={:.0} pt observed growth",
                    input.window_id, MIN_DPCT_FOR_ESTIMATE
                ),
                unlocks_at: unlock.map(|(ts, _)| ts),
                path: unlock.map(|(_, p)| p),
                demoted_by: None,
            }
        } else {
            // Demoted by influence: one instance controls the number.
            let (idx, shift) = loo_controlling(&rates_orig);
            let q = &adm.qualifying[idx];
            Insufficient {
                needed_instances: 0,
                missing: format!(
                    "one instance ({}, ${:.2}/window) moves the estimate {:.1}% — more instances \
                     needed to outvote it",
                    date_utc(q.first_ts),
                    q.rate,
                    shift * 100.0
                ),
                unlocks_at: None,
                path: None,
                demoted_by: Some(LooDemotion {
                    first_ts: q.first_ts,
                    rate: q.rate,
                    shift,
                }),
            }
        }
    });

    let quantization = (n_q > 0).then(|| {
        let min_dpct = adm
            .qualifying
            .iter()
            .map(|q| q.dpct)
            .fold(f64::INFINITY, f64::min);
        Quantization {
            min_dpct,
            max_rel_widening: 1.0 / (min_dpct - 1.0),
        }
    });

    // Open windows: only the operative window carries dollars.
    let mut open_windows = Vec::new();
    if let Some(open) = &input.open {
        let (used, _) = snap_pct(open.used_pct);
        let frac = ((100.0 - used) / 100.0).max(0.0);
        let (gap, certified, gap_reason) = match tier {
            Tier::Calibrated => {
                let gap = band.as_ref().map(|b| {
                    // The bound composes by max with the lower edge only.
                    let lo = (b.lo * frac).max(bound - open.extracted_in_window_usd);
                    Dollars::Band(Band {
                        lo: lo.max(0.0),
                        med: b.med * frac,
                        hi: b.hi * frac,
                        coverage: b.coverage,
                        n: b.n,
                        approx: b.approx,
                    })
                });
                (gap, None, None)
            }
            Tier::Measured | Tier::Insufficient => {
                // Certified form — no linearity assumption: linear scaling of
                // an uncalibrated floor assumes the $/pct constancy it lacks.
                let g = (bound - open.extracted_in_window_usd).max(0.0);
                if bound > 0.0 && g > 0.0 {
                    (
                        Some(Dollars::Floor {
                            low: g,
                            approx: input.bound_approx,
                        }),
                        Some(CertifiedGap {
                            bound,
                            extracted_in_window_usd: open.extracted_in_window_usd,
                        }),
                        None,
                    )
                } else if bound > 0.0 {
                    // gap unmeasured — never $0.
                    (
                        None,
                        None,
                        Some("extraction this window already covers the bound — no certified shortfall"),
                    )
                } else {
                    (
                        None,
                        None,
                        Some("uncalibrated and no achieved-window bound yet"),
                    )
                }
            }
        };
        // Capture pace: CALIBRATED only, and only with a known reset.
        let capture = match (tier, open.resets_at, &gap) {
            (Tier::Calibrated, Some(ra), Some(Dollars::Band(g))) if ra > input.now => {
                let remaining = ra - input.now;
                let days = remaining as f64 / 86_400.0;
                let age_frac = if regime_secs > 0 {
                    (input.now - open.first_ts).max(0) as f64 / regime_secs as f64
                } else {
                    0.0
                };
                let pace = match checkpoint_for_age(age_frac) {
                    None => PaceComparison::TooYoung,
                    Some(cp) => {
                        let (med, n, skipped) =
                            curve_at_checkpoint(&adm.qualifying, cp, regime_secs);
                        let checkpoint_pct = (cp * 100.0).round() as u32;
                        match med {
                            Some(m) if n >= N_MIN_CALIBRATED => PaceComparison::Calibrated {
                                checkpoint_pct,
                                median_used_at_checkpoint: m,
                                curve_n: n,
                                curve_skipped: skipped,
                            },
                            _ => PaceComparison::Uncalibrated { checkpoint_pct },
                        }
                    }
                };
                Some(Capture {
                    needed_per_day: Triple {
                        lo: g.lo / days,
                        med: g.med / days,
                        hi: g.hi / days,
                    },
                    remaining_secs: remaining,
                    age_frac,
                    pace,
                })
            }
            _ => None,
        };
        open_windows.push(OpenWindow {
            window_id: input.window_id.clone(),
            operative: true,
            used_pct: used,
            resets_at: open.resets_at,
            gap_usd: gap,
            gap_reason,
            certified,
            capture,
            context: None,
        });
    }
    for other in &input.others {
        let (used, _) = snap_pct(other.used_pct);
        open_windows.push(OpenWindow {
            window_id: other.window_id.clone(),
            operative: false,
            used_pct: used,
            resets_at: other.resets_at,
            gap_usd: None,
            gap_reason: Some("non-operative — the operative window carries the dollars"),
            certified: None,
            capture: None,
            context: other.context.clone(),
        });
    }

    Estimate {
        left_on_table_usd: left_on_table,
        basis: Basis {
            tier,
            window_id: input.window_id.clone(),
            regime_minutes: input.regime_minutes,
            n_qualifying: n_q,
            excluded: adm.excluded.clone(),
            band_usd_per_window: band,
            max_loo_shift: loo_shift,
            quantization,
            floor_usd_per_window: (input.floor_window_usd > 0.0).then_some(input.floor_window_usd),
            bound_usd_per_window: (bound > 0.0).then_some(bound),
            bound_source,
            certified,
            floor_binding: tier == Tier::Measured,
            window_worth_multiple: window_worth,
            period_windows,
            insufficient,
            fractional_pct_observed: adm.fractional_pct,
        },
        open_windows,
        window_cost_usd: window_cost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEEK: i64 = 604_800;

    /// Well-formed candidate: points every `step` seconds with the given
    /// integer percents, usage events on every step inside the attribution
    /// span. Passes gates U and S for a weekly regime when step ≤ 60 480 and
    /// step ≤ max(0.25 × span, 1800).
    fn mk_cand(first_ts: i64, step: i64, pcts: &[f64], usd: f64) -> Candidate {
        let points: Vec<(i64, f64)> = pcts
            .iter()
            .enumerate()
            .map(|(i, &p)| (first_ts + i as i64 * step, p))
            .collect();
        let peak = pcts.iter().copied().fold(f64::MIN, f64::max);
        let peak_idx = pcts.iter().position(|&p| p >= peak).unwrap();
        let event_ts: Vec<i64> = (1..=peak_idx).map(|i| first_ts + i as i64 * step).collect();
        Candidate {
            peak_ts: points[peak_idx].0,
            points,
            attributed_usd: usd,
            unpriced: false,
            approx: false,
            event_ts,
        }
    }

    /// Directly-constructed qualifying instance for boundary tests where the
    /// exact rate value matters more than the admission path.
    fn mk_q(first_ts: i64, rate: f64, dpct: f64) -> Qualified {
        Qualified {
            first_ts,
            last_ts: first_ts + WEEK,
            points: vec![(first_ts, 0.0), (first_ts + WEEK, dpct)],
            dpct,
            rate,
            rate_lo: rate * dpct / (dpct + 1.0),
            rate_hi: rate * dpct / (dpct - 1.0),
            approx: false,
        }
    }

    fn admission_of(qs: Vec<Qualified>) -> Admission {
        Admission {
            qualifying: qs,
            excluded: Vec::new(),
            fractional_pct: false,
        }
    }

    /// Minimal weekly input: no floor, no 100% instance, fully-measured span
    /// equal to one regime (period_windows = 1), extracted 0, plan $20/mo.
    fn base_input(adm: Admission) -> EstimatorInput {
        EstimatorInput {
            window_id: "primary".into(),
            regime_minutes: 10_080,
            admission: adm,
            any_hundred_pct: false,
            best_hundred_usd: 0.0,
            best_hundred_first_ts: None,
            floor_window_usd: 0.0,
            floor_week_start: None,
            bound_approx: false,
            extracted_usd: 0.0,
            measured_secs: WEEK,
            certified_start_ts: 1_780_000_000,
            certified_secs: WEEK,
            certified_extracted_usd: 0.0,
            have_billing: true,
            plan_monthly_usd: 20.0,
            now: 1_790_000_000,
            first_snapshot_ts: Some(1_780_000_000),
            complete_windows_observed: 0,
            open: None,
            others: Vec::new(),
        }
    }

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    // ── 1. coverage oracle (pinned table, exact dyadic rationals) ──
    #[test]
    fn coverage_oracle() {
        // outer [x₁,xₙ]
        assert_eq!(median_ci_coverage(2, 1, 2), 0.5);
        assert_eq!(median_ci_coverage(3, 1, 3), 0.75);
        assert_eq!(median_ci_coverage(4, 1, 4), 0.875);
        assert_eq!(median_ci_coverage(5, 1, 5), 0.9375);
        assert_eq!(median_ci_coverage(6, 1, 6), 0.96875);
        assert_eq!(median_ci_coverage(7, 1, 7), 0.984375);
        assert_eq!(median_ci_coverage(8, 1, 8), 0.9921875);
        // inner [x₂,xₙ₋₁]
        assert_eq!(median_ci_coverage(4, 2, 3), 0.375);
        assert_eq!(median_ci_coverage(5, 2, 4), 0.625);
        assert_eq!(median_ci_coverage(6, 2, 5), 0.78125);
        assert_eq!(median_ci_coverage(7, 2, 6), 0.875);
        assert_eq!(median_ci_coverage(8, 2, 7), 0.9296875);
    }

    // ── 2. band selection: n=7 outer, n=8 switches to inner ──
    #[test]
    fn band_selection() {
        assert_eq!(band_indices(7), (1, 7));
        assert_eq!(band_indices(8), (2, 7));
        let points: Vec<f64> = (1..=8).map(|i| i as f64).collect();
        let lows: Vec<f64> = points.iter().map(|p| p - 0.5).collect();
        let highs: Vec<f64> = points.iter().map(|p| p + 0.5).collect();
        let (lo, med, hi, cov) = composed_band(&points, &lows, &highs);
        assert_eq!(lo, 1.5); // lows[1] = x₂ − 0.5
        assert_eq!(med, 4.5);
        assert_eq!(hi, 7.5); // highs[6] = x₇ + 0.5
        assert_eq!(cov, 0.9296875);
    }

    // ── 3. binom exact through n=16 (u64-exact domain) ──
    #[test]
    fn binom_domain() {
        // Pascal-triangle cross-check, exact through n = 16.
        let mut row = vec![1u64];
        for n in 1..=16u64 {
            let mut next = vec![1u64];
            for i in 1..n as usize {
                next.push(row[i - 1] + row[i]);
            }
            next.push(1);
            row = next;
            for (k, &want) in row.iter().enumerate() {
                assert_eq!(binom(n, k as u64), want, "C({n},{k})");
            }
        }
        assert_eq!(binom(16, 8), 12_870);
    }

    // ── 4. median even/odd ──
    #[test]
    fn median_even_odd() {
        assert_eq!(median_sorted(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(median_sorted(&[1.0, 2.0, 3.0, 4.0, 5.0]), 3.0);
    }

    // ── 5. LOO shift fixture: R=[21,24,25,27,49] → max shift exactly 4% ──
    #[test]
    fn loo_shift_fixture() {
        let r = [21.0, 24.0, 25.0, 27.0, 49.0];
        // Delete-1 medians: {26, 26, 25.5, 24.5, 24.5}.
        assert!(close(max_loo_shift(&r), 0.04, 1e-12));
        // The $49 outlier does not control the number — the max shift comes
        // from a CENTRAL deletion, and robustness caps it at 4%.
        let (_, shift) = loo_controlling(&r);
        assert!(close(shift, 0.04, 1e-12));
    }

    // ── 6. quantization interval ──
    #[test]
    fn quantization_interval() {
        let (lo, point, hi) = rate_interval(5.88, 12.0);
        assert!(close(point, 49.0, 1e-9));
        assert!(close(lo, 5.88 / 13.0 * 100.0, 1e-9)); // 45.2307…
        assert!(close(hi, 5.88 / 11.0 * 100.0, 1e-9)); // 53.4545…
                                                       // d = 10 edge (the MIN_DPCT rationale): lo-side relative half-width
                                                       // = 1/(d+1) ≤ 1/d = 10%; hi-side = 1/(d−1).
        let (lo, point, hi) = rate_interval(1.0, 10.0);
        assert!((point - lo) / point <= 0.10);
        assert!(close((hi - point) / point, 1.0 / 9.0, 1e-12));
    }

    // ── 7. outer bound needs INDEPENDENT sorts (non-monotone counterexample) ──
    #[test]
    fn outer_bound_independent_sort() {
        // A = (c=2.88, d=12): point 24.0, hi 26.18…
        // B = (c=25.0, d=100): point 25.0, hi 25.25…
        // hi order inverts point order; sorting-by-point would return 25.25
        // as the band top and undercover.
        let (_, pa, ha) = rate_interval(2.88, 12.0);
        let (_, pb, hb) = rate_interval(25.0, 100.0);
        assert!(pa < pb && ha > hb);
        let points = [pa, pb];
        let lows = [rate_interval(2.88, 12.0).0, rate_interval(25.0, 100.0).0];
        let mut highs = [ha, hb];
        highs.sort_by(f64::total_cmp);
        let (_, _, hi, _) = composed_band(&points, &lows, &highs);
        assert!(close(hi, 2.88 / 11.0 * 100.0, 1e-9)); // 26.18, not 25.25
    }

    // ── 8 + 19. worked Example A, full pipeline to the cent ──
    fn example_a_input() -> EstimatorInput {
        let base = 1_780_000_000i64;
        let now = base + 10 * WEEK;
        // Qualifying (attributed $, dpct): the index-3 point (36 h in) is the
        // 25%-checkpoint value for the capture curve → median 22.
        let mut cands = vec![
            mk_cand(base + 5 * WEEK, 43_200, &[0.0, 9.0, 18.0, 22.0, 36.0], 8.64),
            mk_cand(
                base + 4 * WEEK,
                43_200,
                &[0.0, 12.0, 20.0, 23.0, 48.0],
                12.96,
            ),
            mk_cand(
                base + 3 * WEEK,
                43_200,
                &[0.0, 15.0, 20.0, 24.0, 62.0],
                13.02,
            ),
            mk_cand(base + 2 * WEEK, 43_200, &[0.0, 8.0, 15.0, 21.0, 25.0], 6.25),
            mk_cand(base + WEEK, 43_200, &[0.0, 3.0, 6.0, 9.0, 12.0], 5.88),
        ];
        // 6th candidate: dpct 27 observed across a 6-minute snapshot tail
        // after a coverage hole → Gate S rejects (usage events are dense
        // enough to pass Gate U, so the failure is denominator censoring).
        let t = base;
        let mut ev = Vec::new();
        let mut ts = t + 43_200;
        while ts <= t + 300_360 {
            ev.push(ts);
            ts += 43_200;
        }
        ev.push(t + 300_360);
        cands.push(Candidate {
            points: vec![(t, 10.0), (t + 300_000, 36.0), (t + 300_360, 37.0)],
            peak_ts: t + 300_360,
            attributed_usd: 4.0,
            unpriced: false,
            approx: false,
            event_ts: ev,
        });
        let admission = admit(&cands, WEEK);
        EstimatorInput {
            window_id: "primary".into(),
            regime_minutes: 10_080,
            admission,
            any_hundred_pct: false,
            best_hundred_usd: 0.0,
            best_hundred_first_ts: None,
            floor_window_usd: 18.60,
            floor_week_start: Some(base + WEEK),
            bound_approx: false,
            extracted_usd: 41.80,
            measured_secs: 2_592_000,
            certified_start_ts: base,
            certified_secs: 2_592_000,
            certified_extracted_usd: 41.80,
            have_billing: true,
            plan_monthly_usd: 20.0,
            now,
            first_snapshot_ts: Some(base),
            complete_windows_observed: 5,
            open: Some(OpenWindowInput {
                used_pct: 27.0,
                resets_at: Some(now + 385_200), // 4d 11h
                first_ts: now - 219_600,        // 61 h old
                extracted_in_window_usd: 5.0,
            }),
            others: Vec::new(),
        }
    }

    #[test]
    fn composed_band_example_a() {
        let e = assemble(example_a_input());
        assert_eq!(e.basis.tier, Tier::Calibrated);
        assert_eq!(e.basis.n_qualifying, 5);
        assert_eq!(
            e.basis.excluded,
            vec![Exclusion {
                reason: REASON_SNAPSHOT_GAP,
                count: 1
            }]
        );
        let b = e.basis.band_usd_per_window.as_ref().unwrap();
        assert!(close(b.lo, 20.67, 0.005), "band lo {}", b.lo);
        assert!(close(b.med, 25.00, 0.005));
        assert!(close(b.hi, 53.45, 0.005));
        assert_eq!(b.coverage, 0.9375);
        assert!(close(e.basis.max_loo_shift.unwrap(), 0.04, 1e-12));
        let q = e.basis.quantization.as_ref().unwrap();
        assert_eq!(q.min_dpct, 12.0);
        assert!(close(q.max_rel_widening, 1.0 / 11.0, 1e-12));
        // Floor $18.60 not binding; sampling spread dominates.
        assert!(!e.basis.floor_binding);
        assert_eq!(e.basis.floor_usd_per_window, Some(18.60));
        assert!(close(e.basis.period_windows, 2_592_000.0 / 604_800.0, 1e-9));
        // Period estimate: [$46.77, $65.34, $187.29].
        let Some(Dollars::Band(left)) = &e.left_on_table_usd else {
            panic!("expected calibrated band");
        };
        assert!(close(left.lo, 46.77, 0.005), "left lo {}", left.lo);
        assert!(close(left.med, 65.34, 0.005));
        assert!(close(left.hi, 187.29, 0.005));
        // Window worth ×5.4 its $4.60 cost.
        let cost = e.window_cost_usd.unwrap();
        assert!(close(cost, 4.60, 0.005));
        let w = e.basis.window_worth_multiple.unwrap();
        assert!(close(w, 25.0 / cost, 1e-9));
        assert!(close((w * 10.0).round() / 10.0, 5.4, 1e-9));
    }

    #[test]
    fn live_gap_calibrated() {
        let e = assemble(example_a_input());
        let ow = &e.open_windows[0];
        assert!(ow.operative);
        assert_eq!(ow.used_pct, 27.0);
        let Some(Dollars::Band(g)) = &ow.gap_usd else {
            panic!("expected calibrated gap band");
        };
        assert!(close(g.lo, 15.09, 0.005), "gap lo {}", g.lo);
        assert!(close(g.med, 18.25, 0.005));
        assert!(close(g.hi, 39.02, 0.005));
        // Capture: $3.38–$8.75/day (med $4.09) over 4.4583 days.
        let cap = ow.capture.as_ref().unwrap();
        assert_eq!(cap.remaining_secs, 385_200);
        assert!(close(cap.needed_per_day.lo, 3.38, 0.005));
        assert!(close(cap.needed_per_day.med, 4.09, 0.005));
        assert!(close(cap.needed_per_day.hi, 8.75, 0.005));
        assert!(close(cap.age_frac, 219_600.0 / 604_800.0, 1e-9)); // 36%
                                                                   // Age 36% → largest checkpoint ≤ age = 25%; historical median 22%.
        match &cap.pace {
            PaceComparison::Calibrated {
                checkpoint_pct,
                median_used_at_checkpoint,
                curve_n,
                curve_skipped,
            } => {
                assert_eq!(*checkpoint_pct, 25);
                assert_eq!(*median_used_at_checkpoint, 22.0);
                assert_eq!(*curve_n, 5);
                assert_eq!(*curve_skipped, 0);
            }
            other => panic!("expected calibrated pace, got {other:?}"),
        }
    }

    /// Certified term wins the live-gap lower-edge max when it exceeds
    /// band_lo × frac (composition by max, spec 1.8).
    #[test]
    fn live_gap_lower_edge_composes_by_max() {
        let mut input = example_a_input();
        input.floor_window_usd = 24.0; // < band_med 25 → still CALIBRATED
        input.open.as_mut().unwrap().extracted_in_window_usd = 2.0;
        let e = assemble(input);
        assert_eq!(e.basis.tier, Tier::Calibrated);
        let Some(Dollars::Band(g)) = &e.open_windows[0].gap_usd else {
            panic!("expected band");
        };
        // bound − in-window = 22.0 > band_lo × 0.73 = 15.09.
        assert!(close(g.lo, 22.0, 1e-9));
        assert!(close(g.med, 18.25, 0.005)); // med/hi untouched by the bound
    }

    // ── 9. tier boundary on n ──
    #[test]
    fn tier_n_boundary() {
        let e = assemble(base_input(admission_of(
            (0..3)
                .map(|i| mk_q(1_780_000_000 + i * WEEK, 10.0 + i as f64, 20.0))
                .collect(),
        )));
        assert_eq!(e.basis.tier, Tier::Insufficient);
        let ins = e.basis.insufficient.as_ref().unwrap();
        assert_eq!(ins.needed_instances, 1);
        assert!(ins.missing.contains("1 more qualifying primary instances"));
        let e = assemble(base_input(admission_of(
            (0..4)
                .map(|i| mk_q(1_780_000_000 + i * WEEK, 10.0 + i as f64, 20.0))
                .collect(),
        )));
        assert_eq!(e.basis.tier, Tier::Calibrated);
        assert!(e.basis.insufficient.is_none());
    }

    // ── 10. tier boundary on the influence diagnostic ──
    #[test]
    fn tier_loo_boundary() {
        // R = [1,6,10,20]: med 8, every delete-1 median ∈ {6,10} → shift
        // exactly 0.25 → equality passes.
        let rates = [1.0, 6.0, 10.0, 20.0];
        let e = assemble(base_input(admission_of(
            rates
                .iter()
                .enumerate()
                .map(|(i, &r)| mk_q(1_780_000_000 + i as i64 * WEEK, r, 20.0))
                .collect(),
        )));
        assert!(close(e.basis.max_loo_shift.unwrap(), 0.25, 1e-12));
        assert_eq!(e.basis.tier, Tier::Calibrated);
        // Push one central rate up: shift 2.05/8.05 > 0.25 → demoted, with
        // the outvote sentence naming the controlling instance.
        let rates = [1.0, 6.0, 10.1, 20.0];
        let e = assemble(base_input(admission_of(
            rates
                .iter()
                .enumerate()
                .map(|(i, &r)| mk_q(1_780_000_000 + i as i64 * WEEK, r, 20.0))
                .collect(),
        )));
        assert_eq!(e.basis.tier, Tier::Insufficient);
        let ins = e.basis.insufficient.as_ref().unwrap();
        assert_eq!(ins.needed_instances, 0);
        assert!(ins.missing.contains("moves the estimate"));
        assert!(ins.missing.contains("outvote"));
        assert!(ins.demoted_by.is_some());
        assert!(ins.unlocks_at.is_none());
    }

    // ── 11. floor promotion boundary: bound == band_med → MEASURED ──
    #[test]
    fn tier_floor_promotion() {
        let qs = |_: ()| -> Vec<Qualified> {
            [14.0, 16.0, 18.0, 19.0]
                .iter()
                .enumerate()
                .map(|(i, &r)| mk_q(1_780_000_000 + i as i64 * WEEK, r, 20.0))
                .collect()
        };
        let mut input = base_input(admission_of(qs(())));
        input.floor_window_usd = 17.0; // == band_med
        input.floor_week_start = Some(1_780_000_000);
        let e = assemble(input);
        assert_eq!(e.basis.tier, Tier::Measured);
        assert!(e.basis.floor_binding);
        // bound just below the median → CALIBRATED with display_lo composed
        // by max: left.lo comes from the bound, not band_lo.
        let mut input = base_input(admission_of(qs(())));
        input.floor_window_usd = 16.99;
        input.floor_week_start = Some(1_780_000_000);
        let e = assemble(input);
        assert_eq!(e.basis.tier, Tier::Calibrated);
        let Some(Dollars::Band(left)) = &e.left_on_table_usd else {
            panic!("expected band");
        };
        // period_windows = 1, extracted 0 → left.lo = max(band_lo, bound).
        assert!(close(left.lo, 16.99, 1e-9));
        assert!(close(left.med, 17.0, 1e-9));
    }

    // ── 12. 100% instance → MEASURED regardless of n; bound composition ──
    #[test]
    fn tier_hundred_pct_and_bound_composition() {
        let mut input = base_input(admission_of(Vec::new()));
        input.any_hundred_pct = true;
        input.best_hundred_usd = 30.0;
        input.best_hundred_first_ts = Some(1_780_000_000);
        input.floor_window_usd = 22.0; // best calendar week loses the max
        input.floor_week_start = Some(1_780_000_000);
        let e = assemble(input);
        assert_eq!(e.basis.tier, Tier::Measured);
        assert_eq!(e.basis.n_qualifying, 0);
        assert_eq!(e.basis.bound_usd_per_window, Some(30.0));
        assert_eq!(
            e.basis.bound_source,
            Some(BoundSource::Instance {
                first_ts: 1_780_000_000
            })
        );
        let Some(Dollars::Floor { low, .. }) = e.left_on_table_usd else {
            panic!("expected floor");
        };
        assert!(close(low, 30.0, 1e-9)); // period_windows 1, extracted 0
    }

    // ── 13. floor semantics: never stability, never the upper edge; $0 → none ──
    #[test]
    fn floor_semantics() {
        let qs: Vec<Qualified> = [14.0, 16.0, 18.0, 19.0]
            .iter()
            .enumerate()
            .map(|(i, &r)| mk_q(1_780_000_000 + i as i64 * WEEK, r, 20.0))
            .collect();
        let free = assemble(base_input(admission_of(qs.clone())));
        let mut input = base_input(admission_of(qs));
        input.floor_window_usd = 22.40;
        input.floor_week_start = Some(1_780_000_000);
        let bounded = assemble(input);
        // The bound promotes the tier but never tightens the upper edge and
        // never moves the influence diagnostic.
        assert_eq!(bounded.basis.tier, Tier::Measured);
        let (bf, bb) = (
            free.basis.band_usd_per_window.as_ref().unwrap(),
            bounded.basis.band_usd_per_window.as_ref().unwrap(),
        );
        assert_eq!(bf.hi, bb.hi);
        assert_eq!(free.basis.max_loo_shift, bounded.basis.max_loo_shift);
        // INSUFFICIENT + bound: ≥ only when the period lower bound is > $0.
        let mut input = base_input(admission_of(vec![mk_q(1_780_000_000, 12.0, 20.0)]));
        input.floor_window_usd = 10.0;
        input.floor_week_start = Some(1_780_000_000);
        input.extracted_usd = 5.0;
        input.certified_extracted_usd = 5.0;
        let e = assemble(input);
        assert_eq!(e.basis.tier, Tier::Insufficient);
        let Some(Dollars::Floor { low, .. }) = e.left_on_table_usd else {
            panic!("expected floor");
        };
        assert!(close(low, 5.0, 1e-9));
        // Bound-period == 0 → NO dollars (never "$0").
        let mut input = base_input(admission_of(vec![mk_q(1_780_000_000, 12.0, 20.0)]));
        input.floor_window_usd = 10.0;
        input.floor_week_start = Some(1_780_000_000);
        input.extracted_usd = 15.0;
        input.certified_extracted_usd = 15.0;
        let e = assemble(input);
        assert!(e.left_on_table_usd.is_none());
    }

    // ── 13b. the `≥` scaling never leaves the certified epoch ──
    /// `bound × windows` is certified only where plan and regime match the
    /// bound's. A measured span reaching into a prior plan or prior regime
    /// scales the `≥` figure over the CERTIFIED windows only, subtracting the
    /// extraction inside the same epoch — never `bound × full-span windows`.
    #[test]
    fn certified_epoch_clamps_floor_scaling() {
        // 4 measured weeks, but only 1 week under the current plan+regime
        // with $3 extracted inside it → ≥ 10 × 1 − 3 = $7, NOT 10 × 4 − 3.
        let mut input = base_input(admission_of(vec![mk_q(1_780_000_000, 12.0, 20.0)]));
        input.floor_window_usd = 10.0;
        input.floor_week_start = Some(1_789_000_000);
        input.measured_secs = 4 * WEEK;
        input.extracted_usd = 30.0; // full-span extraction (prior plan incl.)
        input.certified_start_ts = 1_790_000_000 - WEEK;
        input.certified_secs = WEEK;
        input.certified_extracted_usd = 3.0;
        let e = assemble(input);
        assert_eq!(e.basis.tier, Tier::Insufficient);
        let c = e.basis.certified.as_ref().unwrap();
        assert!(close(c.windows, 1.0, 1e-12));
        assert!(close(c.extracted_usd, 3.0, 1e-12));
        let Some(Dollars::Floor { low, .. }) = e.left_on_table_usd else {
            panic!("expected floor");
        };
        assert!(close(low, 7.0, 1e-9), "certified: 10×1−3, got {low}");
        // period_windows (coverage display, band arm) still spans measured.
        assert!(close(e.basis.period_windows, 4.0, 1e-9));
    }

    // ── 14. Gate U: usage gaps, edge gaps included, == admits ──
    #[test]
    fn gate_usage_gap() {
        // span 14 400 s → G_usage = max(0.25 × span, 1800) = 3600.
        let pcts = [0.0, 5.0, 10.0, 15.0, 20.0];
        let ok = mk_cand(0, 3600, &pcts, 2.0); // gaps exactly 3600 → admits
        let adm = admit(&[ok], WEEK);
        assert_eq!(adm.qualifying.len(), 1);
        // One interior gap of 3601 → rejected, reason usage_gap.
        let mut bad = mk_cand(0, 3600, &pcts, 2.0);
        bad.event_ts = vec![3600, 7201, 10_800, 14_400];
        let adm = admit(&[bad], WEEK);
        assert!(adm.qualifying.is_empty());
        assert_eq!(
            adm.excluded,
            vec![Exclusion {
                reason: REASON_USAGE_GAP,
                count: 1
            }]
        );
        // Edge gap to the span START also counts.
        let mut bad = mk_cand(0, 3600, &pcts, 2.0);
        bad.event_ts = vec![3601, 7200, 10_800, 14_400];
        let adm = admit(&[bad], WEEK);
        assert_eq!(adm.excluded[0].reason, REASON_USAGE_GAP);
        // Edge gap to the span END (peak) also counts.
        let mut bad = mk_cand(0, 3600, &pcts, 2.0);
        bad.event_ts = vec![3600, 7200, 10_799];
        let adm = admit(&[bad], WEEK);
        assert_eq!(adm.excluded[0].reason, REASON_USAGE_GAP);
    }

    // ── 15. Gate S: snapshot staleness, == admits ──
    #[test]
    fn gate_snapshot_gap() {
        // Weekly regime → G_snap = 0.10 × 604 800 = 60 480.
        let mk = |gap: i64| {
            let points = vec![(0, 0.0), (gap, 5.0), (gap + 43_200, 12.0)];
            let peak_ts = gap + 43_200;
            let mut ev = Vec::new();
            let mut ts = 20_000i64;
            while ts < peak_ts {
                ev.push(ts);
                ts += 20_000;
            }
            ev.push(peak_ts);
            Candidate {
                points,
                peak_ts,
                attributed_usd: 3.0,
                unpriced: false,
                approx: false,
                event_ts: ev,
            }
        };
        let adm = admit(&[mk(60_480)], WEEK);
        assert_eq!(adm.qualifying.len(), 1, "== admits");
        let adm = admit(&[mk(60_481)], WEEK);
        assert!(adm.qualifying.is_empty());
        assert_eq!(
            adm.excluded,
            vec![Exclusion {
                reason: REASON_SNAPSHOT_GAP,
                count: 1
            }]
        );
    }

    // ── 16. Gate Q + integer-grid snapping ──
    #[test]
    fn gate_dpct_and_snap() {
        assert_eq!(snap_pct(28.999_999_999_999_996), (29.0, false));
        assert_eq!(snap_pct(7.000_000_000_000_001), (7.0, false));
        let (v, frac) = snap_pct(36.5);
        assert_eq!(v, 36.5);
        assert!(frac, "genuinely fractional percent is kept raw");
        // dpct exactly 10 qualifies (== passes).
        let adm = admit(&[mk_cand(0, 3600, &[0.0, 3.0, 5.0, 8.0, 10.0], 1.0)], WEEK);
        assert_eq!(adm.qualifying.len(), 1);
        assert_eq!(adm.qualifying[0].dpct, 10.0);
        // Float dirt snapping to the grid still qualifies.
        let adm = admit(
            &[mk_cand(
                0,
                3600,
                &[0.0, 3.0, 5.0, 8.0, 10.000_000_000_1],
                1.0,
            )],
            WEEK,
        );
        assert_eq!(adm.qualifying.len(), 1);
        assert!(!adm.fractional_pct);
        // Genuinely fractional peak below the gate → low_growth + warning.
        let adm = admit(&[mk_cand(0, 3600, &[0.0, 3.0, 5.0, 8.0, 9.5], 1.0)], WEEK);
        assert!(adm.qualifying.is_empty());
        assert_eq!(adm.excluded[0].reason, REASON_LOW_GROWTH);
        assert!(adm.fractional_pct);
        // Genuinely fractional ABOVE the gate: kept raw, qualifies, flagged.
        let adm = admit(
            &[mk_cand(0, 3600, &[0.0, 9.0, 18.0, 27.0, 36.5], 4.0)],
            WEEK,
        );
        assert_eq!(adm.qualifying.len(), 1);
        assert_eq!(adm.qualifying[0].dpct, 36.5);
        assert!(adm.fractional_pct);
    }

    /// Tagged-dollar serialization shape: the JSON contract builds on it.
    #[test]
    fn dollars_serialize_tagged() {
        let band = serde_json::to_string(&Dollars::Band(Band {
            lo: 1.0,
            med: 2.0,
            hi: 3.0,
            coverage: 0.9375,
            n: 5,
            approx: false,
        }))
        .unwrap();
        assert!(band.contains("\"kind\":\"band\""), "{band}");
        assert!(band.contains("\"coverage\":0.9375"));
        let floor = serde_json::to_string(&Dollars::Floor {
            low: 43.7,
            approx: true,
        })
        .unwrap();
        assert!(floor.contains("\"kind\":\"floor\""), "{floor}");
    }

    // ── 17. no candidate cap: qualifying instances deep in history are found ──
    #[test]
    fn candidate_scan_no_cap() {
        let mut cands = Vec::new();
        for i in 0..16 {
            // newest 16: only 5 points of growth → low_growth
            cands.push(mk_cand(
                1_790_000_000 - i * WEEK,
                3600,
                &[0.0, 2.0, 3.0, 4.0, 5.0],
                1.0,
            ));
        }
        for i in 16..20 {
            cands.push(mk_cand(
                1_790_000_000 - i * WEEK,
                3600,
                &[0.0, 8.0, 12.0, 15.0, 20.0],
                2.0,
            ));
        }
        let adm = admit(&cands, WEEK);
        assert_eq!(
            adm.qualifying.len(),
            4,
            "v0.1's ×2 cap would have starved this"
        );
        assert_eq!(
            adm.excluded,
            vec![Exclusion {
                reason: REASON_LOW_GROWTH,
                count: 16
            }]
        );
    }

    // ── 18. period scaling clamps at 0 elementwise ──
    #[test]
    fn period_scaling_band() {
        let qs: Vec<Qualified> = [14.0, 16.0, 18.0, 19.0]
            .iter()
            .enumerate()
            .map(|(i, &r)| mk_q(1_780_000_000 + i as i64 * WEEK, r, 20.0))
            .collect();
        let mut input = base_input(admission_of(qs));
        input.extracted_usd = 15.0; // between band_lo×1 and med×1
        let e = assemble(input);
        let Some(Dollars::Band(left)) = &e.left_on_table_usd else {
            panic!("expected band");
        };
        assert_eq!(left.lo, 0.0, "lower edge clamps at 0");
        assert!(close(left.med, 2.0, 1e-9));
        assert!(left.hi > 0.0);
        // No billing instances → no period dollars at all.
        let qs: Vec<Qualified> = [14.0, 16.0, 18.0, 19.0]
            .iter()
            .enumerate()
            .map(|(i, &r)| mk_q(1_780_000_000 + i as i64 * WEEK, r, 20.0))
            .collect();
        let mut input = base_input(admission_of(qs));
        input.have_billing = false;
        assert!(assemble(input).left_on_table_usd.is_none());
    }

    // ── 20. certified live gap (Example C) ──
    #[test]
    fn live_gap_certified() {
        // Rates n=4: [14,16,18,19] from d ∈ {41,52,33,60} → med $17.00.
        let cands = vec![
            mk_cand(1_786_000_000, 43_200, &[0.0, 10.0, 20.0, 30.0, 41.0], 5.74),
            mk_cand(1_785_000_000, 43_200, &[0.0, 13.0, 26.0, 39.0, 52.0], 8.32),
            mk_cand(1_784_000_000, 43_200, &[0.0, 8.0, 16.0, 25.0, 33.0], 5.94),
            mk_cand(1_783_000_000, 43_200, &[0.0, 15.0, 30.0, 45.0, 60.0], 11.40),
        ];
        let admission = admit(&cands, WEEK);
        let now = 1_787_000_000i64;
        let input = EstimatorInput {
            window_id: "primary".into(),
            regime_minutes: 10_080,
            admission,
            any_hundred_pct: false,
            best_hundred_usd: 0.0,
            best_hundred_first_ts: None,
            floor_window_usd: 22.40, // best achieved week (no 100% instance)
            floor_week_start: Some(1_784_000_000),
            bound_approx: false,
            extracted_usd: 52.30,
            measured_secs: 2_592_000,
            certified_start_ts: 1_783_000_000,
            certified_secs: 2_592_000,
            certified_extracted_usd: 52.30,
            have_billing: true,
            plan_monthly_usd: 20.0,
            now,
            first_snapshot_ts: Some(1_783_000_000),
            complete_windows_observed: 4,
            open: Some(OpenWindowInput {
                used_pct: 31.0,
                resets_at: Some(now + 158_400), // 1d 20h
                first_ts: now - 100_000,
                extracted_in_window_usd: 4.10,
            }),
            others: Vec::new(),
        };
        let e = assemble(input);
        assert_eq!(e.basis.tier, Tier::Measured);
        let b = e.basis.band_usd_per_window.as_ref().unwrap();
        assert!(close(b.med, 17.0, 1e-9));
        assert!(close(b.lo, 14.0 * 41.0 / 42.0, 1e-9)); // 13.67
        assert!(close(b.hi, 19.0 * 60.0 / 59.0, 1e-9)); // 19.32
        assert_eq!(b.coverage, 0.875);
        // Period: ≥ max(22.40 × 4.2857 − 52.30, 0) = $43.70.
        let Some(Dollars::Floor { low, .. }) = e.left_on_table_usd else {
            panic!("expected floor");
        };
        assert!(close(low, 43.70, 0.005));
        // Certified live gap: ≥ 22.40 − 4.10 = 18.30, NOT a linear scaling
        // of the bound by remaining percent (that would be 22.40 × 0.69).
        let ow = &e.open_windows[0];
        let Some(Dollars::Floor { low: gap, .. }) = ow.gap_usd else {
            panic!("expected certified floor gap");
        };
        assert!(close(gap, 18.30, 1e-9));
        assert!(
            !close(gap, 22.40 * 0.69, 0.01),
            "linear floor scaling is rejected"
        );
        let cert = ow.certified.as_ref().unwrap();
        assert!(close(cert.bound, 22.40, 1e-9));
        assert!(close(cert.extracted_in_window_usd, 4.10, 1e-9));
        // MEASURED never claims a capture pace (CALIBRATED only).
        assert!(ow.capture.is_none());
        // Window worth ≥ ×4.9 its $4.60 cost.
        let w = e.basis.window_worth_multiple.unwrap();
        assert!(close((w * 10.0).round() / 10.0, 4.9, 1e-9));
        // Certified gap of 0 → gap unmeasured, never $0.
        let cands = vec![mk_cand(
            1_786_000_000,
            43_200,
            &[0.0, 10.0, 20.0, 30.0, 41.0],
            5.74,
        )];
        let admission = admit(&cands, WEEK);
        let mut input = base_input(admission);
        input.floor_window_usd = 10.0;
        input.floor_week_start = Some(1_784_000_000);
        input.open = Some(OpenWindowInput {
            used_pct: 36.0,
            resets_at: Some(input.now + 1000),
            first_ts: input.now - 1000,
            extracted_in_window_usd: 12.0, // ≥ bound → nothing certified yet
        });
        let e = assemble(input);
        assert!(
            e.open_windows[0].gap_usd.is_none(),
            "gap unmeasured, not $0"
        );
        // The JSON contract: an UNKNOWN dollar is null PLUS a sibling reason.
        assert_eq!(
            e.open_windows[0].gap_reason,
            Some("extraction this window already covers the bound — no certified shortfall")
        );
    }

    /// Invariant of the 0.2.0 JSON contract: `gap_usd: null` always carries a
    /// sibling `gap_reason`; a measured gap never does.
    #[test]
    fn null_gap_always_carries_reason() {
        let e = assemble(example_a_input()); // CALIBRATED operative + none other
        for ow in assemble(EstimatorInput {
            open: Some(OpenWindowInput {
                used_pct: 59.0,
                resets_at: None,
                first_ts: 1_786_200_000,
                extracted_in_window_usd: 0.0,
            }),
            ..base_input(admission_of(Vec::new()))
        })
        .open_windows
        .iter()
        .chain(e.open_windows.iter())
        {
            assert_eq!(
                ow.gap_usd.is_none(),
                ow.gap_reason.is_some(),
                "gap_usd null ⇔ gap_reason present ({})",
                ow.window_id
            );
        }
    }

    // ── 21. unlock formula, both paths ──
    #[test]
    fn unlock_structural_and_rate() {
        // Worked Example B: first snapshot ts 1786244905 is 2026-08-09T03:08:25Z
        // (the spec prose's "2026-08-08" day label is off by one for its own
        // pinned ts), n_q = 1, zero complete windows → structural: + 4 weeks
        // = 1788664105 = 2026-09-06.
        let (ts, path) = unlock_ts(1_786_600_000, 1, 0, 1_786_244_905, WEEK);
        assert_eq!(ts, 1_788_664_105);
        assert_eq!(path, "structural");
        // Rate path: n_q = 2, complete = 4 → q = 0.5, k = 2 → now + 4 cycles.
        let now = 1_786_600_000;
        let (ts, path) = unlock_ts(now, 2, 4, 1_780_000_000, WEEK);
        assert_eq!(ts, now + 4 * WEEK);
        assert_eq!(path, "rate");
        // Structural with an OLD first snapshot (long history, zero complete
        // windows) clamps to now + one regime — never a past unlock date.
        let (ts, path) = unlock_ts(now, 1, 0, now - 40 * WEEK, WEEK);
        assert_eq!(ts, now + WEEK);
        assert_eq!(path, "structural");
    }

    /// Worked Example B: Claude cold start, INSUFFICIENT end to end.
    #[test]
    fn cold_start_example_b() {
        let first_snap = 1_786_244_905i64;
        let now = first_snap + 354_240; // 4.1 d measured
        let cands = vec![mk_cand(
            first_snap,
            3600,
            &[0.0, 9.0, 18.0, 27.0, 36.0],
            30.0,
        )];
        let admission = admit(&cands, WEEK);
        let input = EstimatorInput {
            window_id: "seven_day".into(),
            regime_minutes: 10_080,
            admission,
            any_hundred_pct: false,
            best_hundred_usd: 0.0,
            best_hundred_first_ts: None,
            // The only calendar week is the in-progress one; everything
            // extracted so far sits inside the open window.
            floor_window_usd: 100.0,
            floor_week_start: Some(first_snap),
            bound_approx: false,
            extracted_usd: 138.40,
            measured_secs: 354_240,
            certified_start_ts: first_snap,
            certified_secs: 354_240,
            certified_extracted_usd: 138.40,
            have_billing: true,
            plan_monthly_usd: 200.0,
            now,
            first_snapshot_ts: Some(first_snap),
            complete_windows_observed: 0, // banked reset ≠ full observed cycle
            open: Some(OpenWindowInput {
                used_pct: 36.0,
                resets_at: Some(now + 248_400),
                first_ts: first_snap + 100_000,
                extracted_in_window_usd: 108.0, // ≥ bound → certified gap 0
            }),
            others: vec![OtherWindowInput {
                window_id: "five_hour".into(),
                used_pct: 62.0,
                resets_at: Some(now + 7_800),
                context: Some(WindowContext {
                    median_peak_pct: 41.0,
                    n: 3,
                    hit_100: 0,
                }),
            }],
        };
        let e = assemble(input);
        assert_eq!(e.basis.tier, Tier::Insufficient);
        assert_eq!(e.basis.n_qualifying, 1);
        assert!(e.basis.excluded.is_empty());
        let ins = e.basis.insufficient.as_ref().unwrap();
        assert_eq!(ins.needed_instances, 3);
        // 2026-09-06 for the spec's pinned ts (its "≈ 2026-09-05" prose label
        // is off by one day; real data, first snapshot 2026-08-08T03:08:25Z,
        // correctly prints ~2026-09-05).
        assert_eq!(ins.unlocks_at, Some(1_788_664_105));
        assert_eq!(ins.path, Some("structural"));
        // Period lower bound ≤ 0 → NO dollars printed.
        assert!(e.left_on_table_usd.is_none());
        // Certified live gap computes to 0 → gap unmeasured, with its reason.
        assert!(e.open_windows[0].gap_usd.is_none());
        assert!(e.open_windows[0].gap_reason.is_some());
        // Non-operative window: context, never dollars — and the null gap
        // carries its sibling reason.
        let five = &e.open_windows[1];
        assert!(!five.operative);
        assert!(five.gap_usd.is_none());
        assert_eq!(
            five.gap_reason,
            Some("non-operative — the operative window carries the dollars")
        );
        assert_eq!(five.context.as_ref().unwrap().n, 3);
        // Decision line 1 still prints (measured): ×5.14 — keep.
        let plan_cost = 200.0 * 354_240.0 / SECS_PER_MONTH;
        assert!(close(plan_cost, 26.94, 0.005));
        let d = decide(Some((138.40, 138.40)), Some(plan_cost), false);
        assert_eq!(d.verdict, Verdict::Keep);
        assert!(close(
            (d.return_multiple.unwrap() * 100.0).round() / 100.0,
            5.14,
            1e-9
        ));
    }

    // ── 22. decision verdicts, all boundaries ──
    #[test]
    fn decision_verdicts() {
        // ×1.0 exactly → KEEP (equality keeps).
        let d = decide(Some((10.0, 10.0)), Some(10.0), false);
        assert_eq!(d.verdict, Verdict::Keep);
        assert_eq!(d.return_multiple, Some(1.0));
        assert!(d.reason.is_none());
        // ×0.99 → DOWNGRADE, with the threshold named.
        let d = decide(Some((9.9, 9.9)), Some(10.0), false);
        assert_eq!(d.verdict, Verdict::Downgrade);
        assert!(d.reason.as_ref().unwrap().contains("downgrade candidate"));
        // Missing plan cost → UNKNOWN naming what is missing.
        let d = decide(Some((10.0, 10.0)), None, false);
        assert_eq!(d.verdict, Verdict::Unknown);
        assert!(d.reason.as_ref().unwrap().contains("plan cost"));
        // NaN plan cost is missing, not a number.
        let d = decide(Some((10.0, 10.0)), Some(f64::NAN), false);
        assert_eq!(d.verdict, Verdict::Unknown);
        // Missing extracted → UNKNOWN.
        let d = decide(None, Some(10.0), false);
        assert_eq!(d.verdict, Verdict::Unknown);
        assert!(d.reason.as_ref().unwrap().contains("extracted"));
        // Ranged operand straddling the threshold → UNKNOWN, never a coin flip.
        let d = decide(Some((9.0, 12.0)), Some(10.0), false);
        assert_eq!(d.verdict, Verdict::Unknown);
        assert!(d.reason.as_ref().unwrap().contains("straddles"));
        // Approx operand → the multiple carries ~.
        let d = decide(Some((20.0, 20.0)), Some(10.0), true);
        assert_eq!(d.verdict, Verdict::Keep);
        assert!(d.approx);
        // Ranged, non-straddling: the CONSERVATIVE endpoint, never a
        // fabricated midpoint, with extracted_usd on the matching endpoint so
        // extracted / plan equals the reported multiple exactly.
        let d = decide(Some((20.0, 30.0)), Some(10.0), false);
        assert_eq!(d.verdict, Verdict::Keep);
        assert_eq!(d.return_multiple, Some(2.0)); // low endpoint, not 2.5
        assert_eq!(d.extracted_usd, Some(20.0));
        let d = decide(Some((5.0, 8.0)), Some(10.0), false);
        assert_eq!(d.verdict, Verdict::Downgrade);
        assert_eq!(d.return_multiple, Some(0.8)); // high endpoint, not 0.65
        assert_eq!(d.extracted_usd, Some(8.0));
    }

    // ── 23. capture checkpoints: largest ≤ age, never nearest ──
    #[test]
    fn capture_checkpoints() {
        assert_eq!(checkpoint_for_age(0.20), None); // window too young
        assert_eq!(checkpoint_for_age(0.25), Some(0.25));
        assert_eq!(checkpoint_for_age(0.36), Some(0.25));
        assert_eq!(checkpoint_for_age(0.49), Some(0.25), "not nearest (0.50)");
        assert_eq!(checkpoint_for_age(0.50), Some(0.50));
        assert_eq!(checkpoint_for_age(0.80), Some(0.75));
        assert_eq!(checkpoint_for_age(1.20), Some(0.75));
        // Too-young window → pace omitted.
        let mut input = example_a_input();
        input.open.as_mut().unwrap().first_ts = input.now - (0.2 * WEEK as f64) as i64;
        let e = assemble(input);
        match &e.open_windows[0].capture.as_ref().unwrap().pace {
            PaceComparison::TooYoung => {}
            other => panic!("expected TooYoung, got {other:?}"),
        }
        // An instance whose observation ended before the checkpoint instant
        // is skipped and counted.
        let mut input = example_a_input();
        let short = mk_cand(1_780_000_000, 20_000, &[0.0, 5.0, 10.0, 15.0, 20.0], 4.0);
        input.admission = {
            let mut cands: Vec<Candidate> = vec![short.clone()];
            cands.extend((1..5).map(|i| {
                mk_cand(
                    1_780_000_000 + i * WEEK,
                    43_200,
                    &[0.0, 9.0, 18.0, 22.0, 36.0],
                    8.64,
                )
            }));
            admit(&cands, WEEK)
        };
        let e = assemble(input);
        match &e.open_windows[0].capture.as_ref().unwrap().pace {
            PaceComparison::Calibrated {
                curve_n,
                curve_skipped,
                ..
            } => {
                assert_eq!(*curve_n, 4);
                assert_eq!(*curve_skipped, 1);
            }
            other => panic!("expected calibrated pace, got {other:?}"),
        }
        // Fewer than N_MIN supporting instances → capture pace uncalibrated.
        let mut input = example_a_input();
        input.admission = {
            let mut cands: Vec<Candidate> = (0..2)
                .map(|i| {
                    let mut c = short.clone();
                    c.points = c.points.iter().map(|&(t, p)| (t + i * WEEK, p)).collect();
                    c.peak_ts += i * WEEK;
                    c.event_ts = c.event_ts.iter().map(|t| t + i * WEEK).collect();
                    c
                })
                .collect();
            cands.extend((2..5).map(|i| {
                mk_cand(
                    1_780_000_000 + i * WEEK,
                    43_200,
                    &[0.0, 9.0, 18.0, 22.0, 36.0],
                    8.64,
                )
            }));
            admit(&cands, WEEK)
        };
        let e = assemble(input);
        match &e.open_windows[0].capture.as_ref().unwrap().pace {
            PaceComparison::Uncalibrated { checkpoint_pct } => {
                assert_eq!(*checkpoint_pct, 25);
            }
            other => panic!("expected uncalibrated pace, got {other:?}"),
        }
    }

    // ── 24. operative-window selection ──
    #[test]
    fn operative_selection() {
        let now = 1_790_000_000i64;
        let weekly_live = WindowSummary {
            window_minutes: 10_080,
            last_snapshot_ts: now - 1_000,
        };
        let five_live = WindowSummary {
            window_minutes: 300,
            last_snapshot_ts: now - 100,
        };
        // Longest LIVE wins even with an older snapshot.
        assert_eq!(
            select_operative(&[weekly_live.clone(), five_live.clone()], now),
            Some(0)
        );
        // Stale weekly (> 1 regime old) falls back to the live five_hour.
        let weekly_stale = WindowSummary {
            window_minutes: 10_080,
            last_snapshot_ts: now - 700_000,
        };
        assert_eq!(
            select_operative(&[weekly_stale.clone(), five_live.clone()], now),
            Some(1)
        );
        // No live window → v0.1 rule: latest snapshot, ties to longer.
        let five_stale = WindowSummary {
            window_minutes: 300,
            last_snapshot_ts: now - 20_000,
        };
        assert_eq!(
            select_operative(&[weekly_stale.clone(), five_stale], now),
            Some(1)
        );
        let five_tied = WindowSummary {
            window_minutes: 300,
            last_snapshot_ts: now - 700_000,
        };
        assert_eq!(
            select_operative(&[five_tied, weekly_stale], now),
            Some(1),
            "equal-timestamp ties break to the longer window"
        );
        // Scoped-window exclusion is data-driven (strict prefix).
        let ids: Vec<String> = [
            "seven_day",
            "seven_day_opus",
            "seven_day_sonnet",
            "five_hour",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(!is_scoped("seven_day", &ids));
        assert!(is_scoped("seven_day_opus", &ids));
        assert!(is_scoped("seven_day_sonnet", &ids));
        assert!(!is_scoped("five_hour", &ids));
    }

    // ── pinned constants (each with a regression assertion) ──
    #[test]
    fn pinned_constants() {
        assert_eq!(N_MIN_CALIBRATED, 4);
        assert_eq!(MAX_INSTANCES_FOR_ESTIMATE, 8);
        assert_eq!(MIN_DPCT_FOR_ESTIMATE, 10.0);
        assert_eq!(MAX_LOO_SHIFT, 0.25);
        assert_eq!(G_USAGE_FRAC, 0.25);
        assert_eq!(G_USAGE_FLOOR_SECS, 1800.0);
        assert_eq!(G_SNAP_FRAC, 0.10);
        assert_eq!(INNER_BAND_MIN_N, 8);
        assert_eq!(CURVE_CHECKPOINTS, [0.25, 0.50, 0.75]);
        assert_eq!(KEEP_THRESHOLD, 1.0);
        assert_eq!(PCT_SNAP_EPS, 1e-6);
        assert_eq!(SECS_PER_MONTH, 2_629_746.0);
        // Implementation-pinned (absent from the spec's table, documented in
        // DESIGN.md): what "complete window" means for the unlock-rate path.
        // It decides rate-vs-structural and therefore moves a displayed date.
        assert_eq!(COMPLETE_WINDOW_MIN_FRAC, 0.75);
    }

    /// The admission scan caps the QUALIFYING set at 8, never the candidates.
    #[test]
    fn qualifying_capped_at_eight() {
        let cands: Vec<Candidate> = (0..12)
            .map(|i| {
                mk_cand(
                    1_790_000_000 - i * WEEK,
                    3600,
                    &[0.0, 8.0, 12.0, 15.0, 20.0],
                    2.0,
                )
            })
            .collect();
        let adm = admit(&cands, WEEK);
        assert_eq!(adm.qualifying.len(), MAX_INSTANCES_FOR_ESTIMATE);
    }
}
