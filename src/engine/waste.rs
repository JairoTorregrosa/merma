//! Waste computation: what you extracted vs. what the plan allowed.
//!
//! Measurement doctrine (see handoff research, verified 2026-08-07/08):
//! - Utilization ground truth = official used_percent snapshots. Quotas are
//!   never reconstructed from tokens as primary truth.
//! - API-equivalent $ = local token logs × dated API price tables. Two
//!   variants always: cache-included ("full") and output-only — cache reads
//!   dominate agentic totals and the difference is the honesty lever.
//! - The tokens-per-percent join is estimated at WINDOW-INSTANCE granularity.
//!   Consecutive-snapshot regression is unusable: used_percent is
//!   integer-quantized (verified empirically: R² < 0 on pairs, while instance
//!   aggregates are meaningful). Instances disperse ~2.5× on this machine's
//!   data, so the extrapolated max is a labeled RANGE (P25/median/P75), and
//!   `stable` is false when dispersion exceeds 1.5× — consumers must present
//!   it as an estimate, never as a fact.

use crate::pricing::{
    claude_event_cost, codex_event_cost, codex_event_credits, is_external_model, PriceBook,
};
use crate::store::{Store, UsageEvent, CLAUDE, CODEX};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;

use super::windows::{reconstruct, WindowInstance};

pub const SECS_PER_MONTH: f64 = 30.436875 * 86_400.0; // mean Gregorian month
/// An instance must show at least this much percent growth to calibrate $/pct.
pub const MIN_DPCT_FOR_ESTIMATE: f64 = 10.0;
/// Use at most this many most-recent qualifying instances per regime.
pub const MAX_INSTANCES_FOR_ESTIMATE: usize = 8;
/// P75/P25 above this ⇒ the join is unstable ⇒ range presented as rough estimate.
pub const STABLE_DISPERSION: f64 = 1.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CacheMode {
    /// Full API-equivalent: everything the API would bill, incl. cache traffic.
    Full,
    /// Output tokens only: the cache-skeptic value floor.
    OutputOnly,
}

impl CacheMode {
    pub fn pick(&self, cost: &crate::pricing::EventCost) -> f64 {
        match self {
            CacheMode::Full => cost.full_usd,
            CacheMode::OutputOnly => cost.output_only_usd,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCost {
    pub model: String,
    pub events: usize,
    pub input: i64,
    pub cached_input: i64,
    pub cache_writes: i64,
    pub output: i64,
    pub full_usd: f64,
    pub output_only_usd: f64,
    pub credits: Option<f64>,
    pub approx: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ApiEquiv {
    pub full_usd: f64,
    pub output_only_usd: f64,
    /// Portion of full_usd computed from era-approximate price entries.
    pub approx_usd: f64,
    pub credits: Option<f64>,
    /// Credits valued at the per-credit street price.
    pub credits_usd_approx: Option<f64>,
    /// True when the per-credit price itself is flagged approximate.
    pub credits_price_approx: bool,
    pub by_model: Vec<ModelCost>,
    /// (model, total tokens) with NO price for their era — excluded from totals, loudly listed.
    pub unpriced: Vec<(String, i64)>,
    /// Non-subscription models routed through the CLI (no quota impact).
    pub external: Vec<(String, i64)>,
}

/// API-equivalent cost of a set of events (one provider).
pub fn api_equiv(book: &PriceBook, provider: &str, events: &[UsageEvent]) -> ApiEquiv {
    let mut agg: BTreeMap<String, ModelCost> = BTreeMap::new();
    let mut out = ApiEquiv::default();
    let mut unpriced: BTreeMap<String, i64> = BTreeMap::new();
    let mut external: BTreeMap<String, i64> = BTreeMap::new();
    let mut credits_total = 0.0f64;
    let mut any_credits = false;
    for e in events {
        let total_tokens =
            e.input + e.cached_input + e.cache_w_5m + e.cache_w_1h + e.cache_w_unsplit + e.output;
        if is_external_model(provider, &e.model) {
            *external.entry(e.model.clone()).or_default() += total_tokens;
            continue;
        }
        let cost = match provider {
            CLAUDE => claude_event_cost(book, e),
            _ => codex_event_cost(book, e),
        };
        let Some(cost) = cost else {
            *unpriced.entry(e.model.clone()).or_default() += total_tokens;
            continue;
        };
        let m = agg.entry(e.model.clone()).or_insert_with(|| ModelCost {
            model: e.model.clone(),
            events: 0,
            input: 0,
            cached_input: 0,
            cache_writes: 0,
            output: 0,
            full_usd: 0.0,
            output_only_usd: 0.0,
            credits: None,
            approx: false,
        });
        m.events += 1;
        m.input += e.input;
        m.cached_input += e.cached_input;
        m.cache_writes += e.cache_w_5m + e.cache_w_1h + e.cache_w_unsplit;
        m.output += e.output;
        m.full_usd += cost.full_usd;
        m.output_only_usd += cost.output_only_usd;
        m.approx |= cost.approx;
        out.full_usd += cost.full_usd;
        out.output_only_usd += cost.output_only_usd;
        if cost.approx {
            out.approx_usd += cost.full_usd;
        }
        if provider == CODEX {
            if let Some(c) = codex_event_credits(book, e) {
                credits_total += c;
                any_credits = true;
                *m.credits.get_or_insert(0.0) += c;
            }
        }
    }
    out.credits = any_credits.then_some(credits_total);
    out.credits_usd_approx = any_credits.then_some(credits_total * book.constants.codex_credit_usd);
    out.credits_price_approx = book.constants.codex_credit_usd_approx;
    let mut models: Vec<ModelCost> = agg.into_values().collect();
    models.sort_by(|a, b| b.full_usd.total_cmp(&a.full_usd));
    out.by_model = models;
    out.unpriced = unpriced.into_iter().collect();
    out.external = external.into_iter().collect();
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct InstanceStat {
    pub first_ts: i64,
    pub last_ts: i64,
    pub resets_at: Option<i64>,
    pub peak_pct: f64,
    pub dpct: f64,
    pub extracted_usd: f64,
    pub usd_per_pct: Option<f64>,
    pub plan_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowReport {
    pub window_id: String,
    pub window_minutes: Option<i64>,
    pub n_instances: usize,
    pub covered_secs: i64,
    /// Duration-weighted mean of instance peak utilization (0–100).
    pub weighted_peak_pct: f64,
    pub instances: Vec<InstanceStat>,
    /// Latest snapshot in period: (ts, pct, resets_at).
    pub current: Option<(i64, f64, Option<i64>)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Quartiles {
    pub p25: f64,
    pub med: f64,
    pub p75: f64,
}

pub fn quartiles(sorted: &[f64]) -> Option<Quartiles> {
    if sorted.is_empty() {
        return None;
    }
    let q = |f: f64| -> f64 {
        let idx = f * (sorted.len() - 1) as f64;
        let lo = idx.floor() as usize;
        let hi = idx.ceil() as usize;
        if lo == hi {
            sorted[lo]
        } else {
            sorted[lo] + (sorted[hi] - sorted[lo]) * (idx - lo as f64)
        }
    };
    Some(Quartiles {
        p25: q(0.25),
        med: q(0.5),
        p75: q(0.75),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct MaxExtraction {
    pub window_id: String,
    pub regime_minutes: i64,
    pub n_instances_used: usize,
    /// $ per full window at 100% utilization, from instance-level joins.
    pub window_max_usd: Quartiles,
    /// Raw P75/P25 of the instance joins; None when P25 is zero (undefined).
    pub dispersion: Option<f64>,
    pub stable: bool,
    /// True when the achieved-best floor dominates the estimate: the value is
    /// then a LOWER BOUND on the ceiling, not a two-sided range.
    pub floored: bool,
    /// True when any calibration instance used era-approximate prices.
    pub approx: bool,
    pub cache_mode: CacheMode,
}

#[derive(Debug, Clone, Serialize)]
pub struct UtilizationWaste {
    /// Plan cost attributable to covered time in the period.
    pub covered_plan_usd: f64,
    /// Portion of that left unused: covered_plan_usd × (1 − weighted peak).
    pub waste_usd: f64,
    pub weighted_peak_pct: f64,
    pub coverage_frac: f64,
    pub window_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Denominator {
    pub name: String,
    pub weekly_usd: f64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderReport {
    pub provider: String,
    pub plan_label: String,
    pub plan_monthly_usd: f64,
    pub plan_approx: bool,
    pub period_t0: i64,
    pub period_t1: i64,
    /// Hull of the measured data ∩ requested period (display only).
    pub effective_t0: i64,
    pub effective_t1: i64,
    /// Interval-UNION duration of measured data within the period — every
    /// extrapolation and proration uses THIS, so `--period all` never scales
    /// estimates over unmeasured years OR gaps between disjoint stretches.
    pub measured_secs: i64,
    /// Plan cost of the measured time, integrated over plan-type history
    /// (prolite-era stretches cost prolite prices, not today's).
    pub plan_cost_effective_usd: f64,
    pub cache_mode: CacheMode,
    pub api: ApiEquiv,
    pub windows: Vec<WindowReport>,
    pub utilization: Option<UtilizationWaste>,
    pub max_extraction: Option<MaxExtraction>,
    /// Headline: estimated max extractable over the period (low/mid/high) and waste.
    pub period_max_usd: Option<Quartiles>,
    pub period_waste_usd: Option<Quartiles>,
    pub denominators: Vec<Denominator>,
    pub weekly_series: Vec<(i64, f64)>,
    pub notes: Vec<String>,
    pub errors: Vec<String>,
}

fn instance_stats(
    inst: &[WindowInstance],
    events: &[UsageEvent],
    book: &PriceBook,
    provider: &str,
    mode: CacheMode,
) -> Vec<InstanceStat> {
    inst.iter()
        .map(|w| {
            let evs: Vec<UsageEvent> = events
                .iter()
                .filter(|e| e.ts >= w.first_ts() && e.ts <= w.last_ts())
                .cloned()
                .collect();
            let api = api_equiv(book, provider, &evs);
            let extracted = match mode {
                CacheMode::Full => api.full_usd,
                CacheMode::OutputOnly => api.output_only_usd,
            };
            // $/percent uses the SAME attribution window as calibration:
            // strictly after the baseline observation, no later than the peak.
            let calib_evs: Vec<UsageEvent> = events
                .iter()
                .filter(|e| e.ts > w.first_ts() && e.ts <= w.peak_ts())
                .cloned()
                .collect();
            let calib_api = api_equiv(book, provider, &calib_evs);
            let calib_extracted = match mode {
                CacheMode::Full => calib_api.full_usd,
                CacheMode::OutputOnly => calib_api.output_only_usd,
            };
            let dpct = w.dpct();
            InstanceStat {
                first_ts: w.first_ts(),
                last_ts: w.last_ts(),
                resets_at: w.resets_at,
                peak_pct: w.peak_pct(),
                dpct,
                extracted_usd: extracted,
                usd_per_pct: (dpct >= MIN_DPCT_FOR_ESTIMATE).then_some(calib_extracted / dpct),
                plan_type: w.plan_type.clone(),
            }
        })
        .collect()
}

/// Build the full report for one provider over [t0, t1).
pub fn build_provider_report(
    store: &Store,
    book: &PriceBook,
    cfg: &crate::config::Cfg,
    provider: &str,
    t0: i64,
    t1: i64,
    mode: CacheMode,
) -> Result<ProviderReport> {
    let events = store
        .usage_between(provider, t0, t1)
        .context("loading usage events")?;
    let api = api_equiv(book, provider, &events);
    let mut notes = Vec::new();
    let mut errors = Vec::new();

    // Measured intervals = the usage-event span and the snapshot span, each
    // clipped to the requested period, merged when they overlap. Every
    // extrapolation and proration uses the interval-UNION duration — never the
    // hull — so usage in January plus snapshots in July does not fabricate a
    // measured spring in between.
    let mut measured_intervals: Vec<(i64, i64)> = [
        store.usage_bounds(provider)?,
        store.snapshot_bounds(provider)?,
    ]
    .into_iter()
    .flatten()
    .filter_map(|(lo, hi)| {
        let a = lo.max(t0);
        let b = (hi + 1).min(t1);
        (a < b).then_some((a, b))
    })
    .collect();
    measured_intervals.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (a, b) in measured_intervals {
        match merged.last_mut() {
            Some((_, pb)) if a <= *pb => *pb = (*pb).max(b),
            _ => merged.push((a, b)),
        }
    }
    let measured_intervals = merged;
    let measured_secs: i64 = measured_intervals.iter().map(|(a, b)| b - a).sum();
    // Hull, for display/labels only — durations always come from measured_secs.
    let (effective_t0, effective_t1) = match (measured_intervals.first(), measured_intervals.last())
    {
        (Some(&(lo, _)), Some(&(_, hi))) => (lo, hi),
        _ => (t0, t0), // nothing measured: zero-length effective span
    };
    if measured_intervals.len() > 1 {
        notes.push(format!(
            "measured data is DISJOINT ({} stretches, {} total) — estimates cover only the \
             measured time, not the gaps",
            measured_intervals.len(),
            crate::report::dur_short(measured_secs)
        ));
    }

    // ── plan ──
    let (plan_label, plan_monthly, plan_approx) = match provider {
        CLAUDE => match cfg.claude_plan(book) {
            Ok(p) => p,
            Err(e) => {
                errors.push(format!("{e:#}"));
                ("Claude (plan unknown)".into(), f64::NAN, true)
            }
        },
        _ => match store.latest_plan_type(CODEX)? {
            Some(pt) => match cfg.codex_plan(book, &pt) {
                Ok(p) => p,
                Err(e) => {
                    errors.push(format!("{e:#}"));
                    (format!("Codex ({pt})"), f64::NAN, true)
                }
            },
            None => {
                errors.push(
                    "no Codex plan_type in any snapshot — run `merma scan` first; \
                     or set codex_monthly_usd in config"
                        .into(),
                );
                ("Codex (plan unknown)".into(), f64::NAN, true)
            }
        },
    };

    // ── windows ──
    let mut window_reports = Vec::new();
    let mut all_instances: Vec<WindowInstance> = Vec::new();
    for wid in store.window_ids(provider)? {
        let snaps = store.snapshots_between(provider, Some(&wid), t0, t1)?;
        if snaps.is_empty() {
            continue;
        }
        let instances = reconstruct(snaps);
        let stats = instance_stats(&instances, &events, book, provider, mode);
        let covered: i64 = instances.iter().map(|w| w.covered_secs()).sum();
        let weighted_peak = if covered > 0 {
            instances
                .iter()
                .map(|w| w.peak_pct() * w.covered_secs() as f64)
                .sum::<f64>()
                / covered as f64
        } else {
            instances.iter().map(|w| w.peak_pct()).sum::<f64>() / instances.len().max(1) as f64
        };
        let current = instances
            .last()
            .and_then(|w| w.points.last().map(|&(ts, pct)| (ts, pct, w.resets_at)));
        window_reports.push(WindowReport {
            window_id: wid.clone(),
            window_minutes: instances.last().and_then(|w| w.window_minutes),
            n_instances: instances.len(),
            covered_secs: covered,
            weighted_peak_pct: weighted_peak,
            instances: stats,
            current,
        });
        if wid != "seven_day_opus" && wid != "seven_day_sonnet" {
            all_instances.extend(instances);
        }
    }

    // ── operative billing constraint, per point in time ──
    // The subscription's real constraint is the LONGEST window observed at any
    // moment (weekly beats 5-hour burst limits). Regimes changed over history
    // (Codex: 300-min primary → weekly-only, Jul 2026), so selection is per
    // instance, greedy longest-window-first with time-overlap exclusion —
    // never one window_id for the whole period, and never a 5-hour peak
    // standing in for a weekly constraint that was measured at the same time.
    all_instances.sort_by(|a, b| {
        b.window_minutes
            .unwrap_or(0)
            .cmp(&a.window_minutes.unwrap_or(0))
            .then(b.covered_secs().cmp(&a.covered_secs()))
    });
    let mut billing_insts: Vec<&WindowInstance> = Vec::new();
    for inst in &all_instances {
        let overlaps = billing_insts
            .iter()
            .any(|b| inst.first_ts() < b.last_ts() && b.first_ts() < inst.last_ts());
        if !overlaps {
            billing_insts.push(inst);
        }
    }
    billing_insts.sort_by_key(|w| w.first_ts());

    // ── utilization-based waste (defensible metric) ──
    // Each covered stretch is priced at the plan that was ACTIVE during it
    // (snapshots carry plan_type; Codex history includes plus → prolite →
    // plus). Stretches with no recorded plan fall back to the current plan.
    let mut plan_cache: BTreeMap<String, Option<f64>> = BTreeMap::new();
    let mut monthly_for = |pt: Option<&str>| -> f64 {
        match pt {
            Some(pt) if provider == CODEX => *plan_cache
                .entry(pt.to_string())
                .or_insert_with(|| cfg.codex_plan(book, pt).ok().map(|(_, usd, _)| usd))
                .as_ref()
                .unwrap_or(&plan_monthly),
            _ => plan_monthly,
        }
    };

    // ── plan cost over measured time, plan-history aware ──
    // Codex plan_type transitions (plus → prolite → plus locally) segment the
    // measured intervals; each segment is priced at ITS plan. Time before the
    // first recorded plan extends the earliest known plan backwards (noted).
    let changepoints = if provider == CODEX {
        store.plan_type_points(CODEX)?
    } else {
        Vec::new()
    };
    let mut plan_cost_effective = 0.0f64;
    for &(a, b) in &measured_intervals {
        let mut cursor = a;
        while cursor < b {
            let (plan, boundary) = if changepoints.is_empty() {
                (None, i64::MAX)
            } else {
                let idx = changepoints.partition_point(|(ts, _)| *ts <= cursor);
                let eff = idx.saturating_sub(1);
                (
                    Some(changepoints[eff].1.as_str()),
                    changepoints
                        .get(eff + 1)
                        .map(|(ts, _)| *ts)
                        .unwrap_or(i64::MAX),
                )
            };
            let seg_end = b.min(boundary.max(cursor + 1));
            plan_cost_effective += monthly_for(plan) * ((seg_end - cursor) as f64 / SECS_PER_MONTH);
            cursor = seg_end;
        }
    }
    if let Some((first_ts, first_plan)) = changepoints.first() {
        if effective_t0 + 86_400 < *first_ts {
            notes.push(format!(
                "plan history unknown before {} — earlier measured time priced at the \
                 earliest known plan ({first_plan})",
                crate::report::date(*first_ts)
            ));
        }
    }

    let mut unplanned_covered: i64 = 0;
    let utilization = (!billing_insts.is_empty()).then(|| {
        let period_secs = measured_secs.max(1);
        let covered: i64 = billing_insts.iter().map(|w| w.covered_secs()).sum();
        let mut covered_plan_usd = 0.0f64;
        let mut waste_usd = 0.0f64;
        let mut peak_weighted = 0.0f64;
        for w in &billing_insts {
            if w.plan_type.is_none() && provider == CODEX {
                unplanned_covered += w.covered_secs();
            }
            let monthly = monthly_for(w.plan_type.as_deref());
            let plan_part = monthly * (w.covered_secs() as f64 / SECS_PER_MONTH);
            let peak = w.peak_pct().min(100.0);
            covered_plan_usd += plan_part;
            waste_usd += (plan_part * (1.0 - peak / 100.0)).max(0.0);
            peak_weighted += peak * w.covered_secs() as f64;
        }
        let weighted_peak_pct = if covered > 0 {
            peak_weighted / covered as f64
        } else {
            billing_insts
                .iter()
                .map(|w| w.peak_pct().min(100.0))
                .sum::<f64>()
                / billing_insts.len() as f64
        };
        let mut ids: Vec<&str> = billing_insts.iter().map(|w| w.window_id.as_str()).collect();
        ids.dedup();
        UtilizationWaste {
            covered_plan_usd,
            waste_usd,
            weighted_peak_pct,
            coverage_frac: (covered as f64 / period_secs as f64).min(1.0),
            window_id: {
                let mut uniq: Vec<&str> = Vec::new();
                for id in ids {
                    if !uniq.contains(&id) {
                        uniq.push(id);
                    }
                }
                uniq.join("+")
            },
        }
    });
    if let Some(u) = &utilization {
        if u.coverage_frac < 0.9 {
            notes.push(format!(
                "utilization coverage is {:.0}% of the period — waste outside measured windows is \
                 UNKNOWN, not zero",
                u.coverage_frac * 100.0
            ));
        }
    }
    let have_billing = !billing_insts.is_empty();
    drop(billing_insts);
    if unplanned_covered > 86_400 {
        notes.push(format!(
            "{} of covered time predates plan_type reporting — priced at the current plan",
            crate::report::dur_short(unplanned_covered)
        ));
    }
    let unknown_plans: Vec<String> = plan_cache
        .iter()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.clone())
        .collect();
    if !unknown_plans.is_empty() {
        notes.push(format!(
            "no price for historical plan type(s) {} — those stretches are priced at the \
             CURRENT plan; add them to prices.toml [[plan]] for accuracy",
            unknown_plans.join(", ")
        ));
    }

    // ── tokens-per-percent join → max extraction estimate ──
    // Calibrated on ALL history of the CURRENT regime of the billing window
    // (never just the report period): the join needs every qualifying instance
    // it can get, and instances from a different window regime are not
    // comparable denominators.
    let weekly = weekly_series(store, book, provider, mode)?;
    let mut floor_note: Option<String> = None;
    // Extrapolation calibrates on the CURRENTLY OPERATIVE window — the one with
    // the most recent snapshot — regardless of which window carried historical
    // utilization (Codex flipped secondary→primary as billing window Jul 2026).
    let operative = window_reports
        .iter()
        .filter(|w| w.window_id != "seven_day_opus" && w.window_id != "seven_day_sonnet")
        .max_by_key(|w| {
            // Latest snapshot wins; equal-timestamp ties break toward the
            // LONGER (billing) window, never by iteration order.
            (
                w.current.map(|c| c.0).unwrap_or(0),
                w.window_minutes.unwrap_or(0),
            )
        });
    let max_extraction = match operative {
        Some(w) => {
            let all_snaps = store.snapshots_between(provider, Some(&w.window_id), 0, i64::MAX)?;
            let all_instances = reconstruct(all_snaps);
            let regime = all_instances
                .last()
                .and_then(|i| i.window_minutes)
                .filter(|m| *m > 0);
            match regime {
                Some(regime) => {
                    let in_regime: Vec<&WindowInstance> = all_instances
                        .iter()
                        .filter(|i| i.window_minutes == Some(regime))
                        .collect();
                    let regime_start = in_regime.first().map(|i| i.first_ts()).unwrap_or(t1);
                    let mut rates: Vec<f64> = Vec::new();
                    let mut calib_approx = false;
                    let mut skipped_unpriced = 0usize;
                    for inst in in_regime.iter().rev().take(MAX_INSTANCES_FOR_ESTIMATE * 2) {
                        let dpct = inst.dpct();
                        if dpct < MIN_DPCT_FOR_ESTIMATE {
                            continue;
                        }
                        // Attribution window: strictly AFTER the baseline
                        // observation (its own request is already inside
                        // first_pct) and no later than the first peak (usage
                        // after the peak produced no measured growth). Both
                        // exclusions keep $/percent from inflating.
                        let evs = store.usage_between(
                            provider,
                            inst.first_ts() + 1,
                            inst.peak_ts() + 1,
                        )?;
                        let api = api_equiv(book, provider, &evs);
                        // An instance whose growth partly came from unpriced
                        // models would silently DEFLATE the rate — reject it.
                        if !api.unpriced.is_empty() {
                            skipped_unpriced += 1;
                            continue;
                        }
                        calib_approx |= api.approx_usd > 0.0;
                        let extracted = match mode {
                            CacheMode::Full => api.full_usd,
                            CacheMode::OutputOnly => api.output_only_usd,
                        };
                        rates.push(extracted / dpct * 100.0);
                        if rates.len() >= MAX_INSTANCES_FOR_ESTIMATE {
                            break;
                        }
                    }
                    if skipped_unpriced > 0 {
                        notes.push(format!(
                            "{skipped_unpriced} calibration instance(s) skipped: they contain \
                             unpriced-model usage that would distort the $/percent join"
                        ));
                    }
                    rates.sort_by(f64::total_cmp);
                    if rates.len() < 2 {
                        None
                    } else {
                        let q = quartiles(&rates).expect("nonempty");
                        // Raw dispersion only: a floor must never manufacture
                        // apparent stability.
                        let dispersion = (q.p25 > 0.0).then_some(q.p75 / q.p25);
                        let stable = dispersion.is_some_and(|d| d <= STABLE_DISPERSION);
                        // Achieved-week FLOOR: a week you actually extracted in
                        // THIS regime is a lower bound on the window max. Weeks
                        // straddling the regime boundary are excluded — usage
                        // from the previous regime must not pose as achieved
                        // under this one.
                        let floor_weekly = weekly
                            .iter()
                            .filter(|(wk, _)| *wk >= regime_start)
                            .map(|(_, v)| *v)
                            .fold(0.0f64, f64::max);
                        let floor_window = floor_weekly * regime.min(10_080) as f64 / 10_080.0;
                        let floored = floor_window > q.med;
                        let q = if floored {
                            floor_note = Some(format!(
                                "join estimate lifted by your achieved best ({} per window) — \
                                 the true ceiling is AT LEAST that; how much higher is unknown",
                                crate::report::usd(floor_window)
                            ));
                            Quartiles {
                                p25: floor_window,
                                med: floor_window,
                                p75: q.p75.max(floor_window),
                            }
                        } else {
                            q
                        };
                        Some(MaxExtraction {
                            window_id: w.window_id.clone(),
                            regime_minutes: regime,
                            n_instances_used: rates.len(),
                            window_max_usd: q,
                            dispersion,
                            // A floor-dominated estimate is a one-sided bound, never "stable".
                            stable: stable && !floored,
                            floored,
                            approx: calib_approx,
                            cache_mode: mode,
                        })
                    }
                }
                None => None,
            }
        }
        None => None,
    };
    if let Some(n) = floor_note {
        notes.push(n);
    }
    match &max_extraction {
        Some(m) if !m.stable && !m.floored => notes.push(format!(
            "tokens-per-percent join is UNSTABLE on this data (P75/P25 = {}) — the \
             extrapolated max is a rough range, not a measurement",
            m.dispersion
                .map(|d| format!("{d:.1}×"))
                .unwrap_or_else(|| "undefined".into())
        )),
        None => notes.push(
            "no max-extraction estimate: need ≥2 window instances with ≥10% observed growth \
             in the current regime (Claude accrues these only after `merma install`)"
                .into(),
        ),
        _ => {}
    }
    if max_extraction.as_ref().is_some_and(|m| m.approx) {
        notes.push(
            "max-extraction calibration includes era-approximate prices — the estimate \
             inherits that approximation"
                .into(),
        );
    }

    // ── period-scaled headline ──
    let extracted = match mode {
        CacheMode::Full => api.full_usd,
        CacheMode::OutputOnly => api.output_only_usd,
    };
    let (period_max, period_waste) = match (&max_extraction, have_billing) {
        (Some(m), true) => {
            let period_windows = measured_secs as f64 / (m.regime_minutes as f64 * 60.0);
            let scale = |x: f64| x * period_windows;
            let pm = Quartiles {
                p25: scale(m.window_max_usd.p25),
                med: scale(m.window_max_usd.med),
                p75: scale(m.window_max_usd.p75),
            };
            let pw = Quartiles {
                p25: (pm.p25 - extracted).max(0.0),
                med: (pm.med - extracted).max(0.0),
                p75: (pm.p75 - extracted).max(0.0),
            };
            (Some(pm), Some(pw))
        }
        _ => (None, None),
    };

    // ── alternative denominators (weekly $) ──
    let mut denominators = Vec::new();
    if let Some(m) = &max_extraction {
        let per_week = 7.0 * 86_400.0 / (m.regime_minutes as f64 * 60.0);
        denominators.push(Denominator {
            name: "official".into(),
            weekly_usd: m.window_max_usd.med * per_week,
            note: "extrapolated from official used_percent (median of instance joins)".into(),
        });
    }
    let mut vals: Vec<f64> = weekly.iter().map(|w| w.1).filter(|v| *v > 0.0).collect();
    vals.sort_by(f64::total_cmp);
    if let Some(&best) = vals.last() {
        denominators.push(Denominator {
            name: "personal-best".into(),
            weekly_usd: best,
            note: "your highest measured week (ccusage --token-limit max precedent)".into(),
        });
    }
    if vals.len() >= 3 {
        let idx = ((vals.len() - 1) as f64 * 0.9).round() as usize;
        denominators.push(Denominator {
            name: "p90".into(),
            weekly_usd: vals[idx.min(vals.len() - 1)],
            note: "90th percentile of your ACTIVE (nonzero) weeks — idle weeks excluded".into(),
        });
    }

    if !api.unpriced.is_empty() {
        notes.push(format!(
            "unpriced usage EXCLUDED from $ totals (no price for era): {}",
            api.unpriced
                .iter()
                .map(|(m, t)| format!("{m} ({t} tok)"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !api.external.is_empty() {
        notes.push(format!(
            "non-subscription models excluded from waste math: {}",
            api.external
                .iter()
                .map(|(m, t)| format!("{m} ({t} tok)"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if api.approx_usd > 0.005 {
        notes.push(format!(
            "${:.2} of the API-equivalent total uses era-approximate prices (see prices.toml \
             `approx` entries)",
            api.approx_usd
        ));
    }

    Ok(ProviderReport {
        provider: provider.to_string(),
        plan_label,
        plan_monthly_usd: plan_monthly,
        plan_approx,
        period_t0: t0,
        period_t1: t1,
        effective_t0,
        effective_t1,
        measured_secs,
        plan_cost_effective_usd: plan_cost_effective,
        cache_mode: mode,
        api,
        windows: window_reports,
        utilization,
        max_extraction,
        period_max_usd: period_max,
        period_waste_usd: period_waste,
        denominators,
        weekly_series: weekly,
        notes,
        errors,
    })
}

/// Calendar-week (UTC, Monday-start) series of API-equivalent $ across ALL history.
pub fn weekly_series(
    store: &Store,
    book: &PriceBook,
    provider: &str,
    mode: CacheMode,
) -> Result<Vec<(i64, f64)>> {
    let Some((lo, hi)) = store.usage_bounds(provider)? else {
        return Ok(Vec::new());
    };
    let events = store.usage_between(provider, lo, hi + 1)?;
    let mut weeks: BTreeMap<i64, f64> = BTreeMap::new();
    for e in &events {
        if is_external_model(provider, &e.model) {
            continue;
        }
        let cost = match provider {
            CLAUDE => claude_event_cost(book, e),
            _ => codex_event_cost(book, e),
        };
        let Some(cost) = cost else { continue };
        // Monday 00:00 UTC of the event's week (epoch day 0 = Thursday; +3 shifts to Monday).
        let week = (e.ts / 86_400 + 3).div_euclid(7) * 7 * 86_400 - 3 * 86_400;
        *weeks.entry(week).or_default() += mode.pick(&cost);
    }
    // Materialize zero weeks between the first and last active bucket so
    // inactive stretches are visible data, not silently absent calendar time.
    if let (Some(&first), Some(&last)) = (
        weeks.keys().next().copied().as_ref(),
        weeks.keys().next_back().copied().as_ref(),
    ) {
        let mut wk = first;
        while wk < last {
            weeks.entry(wk).or_insert(0.0);
            wk += 7 * 86_400;
        }
    }
    Ok(weeks.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Snapshot;

    fn test_cfg(dir: &std::path::Path) -> crate::config::Cfg {
        crate::config::Cfg {
            merma_home: dir.join("merma"),
            claude_home: dir.join("claude"),
            codex_home: dir.join("codex"),
            file: Default::default(),
        }
    }

    fn snap(wid: &str, wm: i64, ts: i64, pct: f64, resets: i64, plan: &str) -> Snapshot {
        Snapshot {
            provider: "codex".into(),
            window_id: wid.into(),
            window_minutes: Some(wm),
            used_percent: pct,
            resets_at: Some(resets),
            ts,
            source: "test".into(),
            plan_type: Some(plan.into()),
        }
    }

    /// Historical stretches are priced at THEIR plan (prolite $100/mo), not
    /// today's ($20/mo), and the longest window wins overlaps: an overlapping
    /// 5-hour burst instance must not double-count covered time.
    #[test]
    fn per_instance_plan_pricing_and_overlap_exclusion() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let book = PriceBook::load(None).unwrap();
        let mut store = Store::open(&tmp.path().join("m.db")).unwrap();
        let day = 86_400;
        let t = 1_780_000_000i64; // mid-2026
        store
            .insert_snapshots(&[
                // weekly instance under prolite: 2 days covered, peak 60
                snap("secondary", 10_080, t, 10.0, t + 7 * day, "prolite"),
                snap(
                    "secondary",
                    10_080,
                    t + 2 * day,
                    60.0,
                    t + 7 * day,
                    "prolite",
                ),
                // overlapping 5h burst instance — must be EXCLUDED from billing
                snap("primary", 300, t + day, 5.0, t + day + 18_000, "prolite"),
                snap(
                    "primary",
                    300,
                    t + day + 9_000,
                    90.0,
                    t + day + 18_000,
                    "prolite",
                ),
                // later weekly instance under plus: 2 days covered, peak 40
                snap("secondary", 10_080, t + 10 * day, 4.0, t + 17 * day, "plus"),
                snap(
                    "secondary",
                    10_080,
                    t + 12 * day,
                    40.0,
                    t + 17 * day,
                    "plus",
                ),
            ])
            .unwrap();
        let r = build_provider_report(
            &store,
            &book,
            &cfg,
            CODEX,
            t - day,
            t + 14 * day,
            CacheMode::Full,
        )
        .unwrap();
        let u = r.utilization.expect("has utilization");
        let part = |monthly: f64| monthly * (2.0 * day as f64) / SECS_PER_MONTH;
        let expect_covered = part(100.0) + part(20.0);
        assert!(
            (u.covered_plan_usd - expect_covered).abs() < 0.01,
            "covered {} vs expected {expect_covered}",
            u.covered_plan_usd
        );
        let expect_waste = part(100.0) * 0.4 + part(20.0) * 0.6;
        assert!((u.waste_usd - expect_waste).abs() < 0.01);
        // 4 days covered out of the EFFECTIVE (measured-clamped) 12-day span,
        // burst instance excluded
        assert!((u.coverage_frac - 4.0 / 12.0).abs() < 0.01);
        assert_eq!(u.window_id, "secondary");
    }

    /// A report period reaching far past the last measured data must clamp its
    /// effective end to the data, not scale estimates over unmeasured months.
    #[test]
    fn effective_period_clamps_both_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let book = PriceBook::load(None).unwrap();
        let mut store = Store::open(&tmp.path().join("m.db")).unwrap();
        let t = 1_780_000_000i64;
        store
            .insert_snapshots(&[
                snap("primary", 10_080, t, 10.0, t + 7 * 86_400, "plus"),
                snap("primary", 10_080, t + 86_400, 30.0, t + 7 * 86_400, "plus"),
            ])
            .unwrap();
        let r = build_provider_report(
            &store,
            &book,
            &cfg,
            CODEX,
            t - 90 * 86_400,
            t + 90 * 86_400,
            CacheMode::Full,
        )
        .unwrap();
        assert_eq!(r.effective_t0, t);
        assert_eq!(r.effective_t1, t + 86_400 + 1);
        assert_eq!(r.measured_secs, 86_400 + 1);
    }

    /// Disjoint measurement stretches must NOT fabricate measured time across
    /// the gap between them (convex-hull regression).
    #[test]
    fn disjoint_measured_spans_use_interval_union_not_hull() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let book = PriceBook::load(None).unwrap();
        let mut store = Store::open(&tmp.path().join("m.db")).unwrap();
        let day = 86_400;
        let t = 1_780_000_000i64;
        // usage: 2 days starting at t; snapshots: 1 day starting 100 days later
        store
            .insert_usage_events(&[
                crate::store::UsageEvent {
                    provider: CODEX,
                    ts: t,
                    model: "gpt-5.6-sol".into(),
                    input: 1000,
                    cached_input: 0,
                    cache_w_5m: 0,
                    cache_w_1h: 0,
                    cache_w_unsplit: 0,
                    output: 100,
                    session_id: None,
                    is_sidechain: false,
                    dedup_key: "a:1".into(),
                    source_file: "a".into(),
                },
                crate::store::UsageEvent {
                    provider: CODEX,
                    ts: t + 2 * day,
                    model: "gpt-5.6-sol".into(),
                    input: 1000,
                    cached_input: 0,
                    cache_w_5m: 0,
                    cache_w_1h: 0,
                    cache_w_unsplit: 0,
                    output: 100,
                    session_id: None,
                    is_sidechain: false,
                    dedup_key: "a:2".into(),
                    source_file: "a".into(),
                },
            ])
            .unwrap();
        store
            .insert_snapshots(&[
                snap(
                    "primary",
                    10_080,
                    t + 100 * day,
                    10.0,
                    t + 107 * day,
                    "plus",
                ),
                snap(
                    "primary",
                    10_080,
                    t + 101 * day,
                    30.0,
                    t + 107 * day,
                    "plus",
                ),
            ])
            .unwrap();
        let r = build_provider_report(
            &store,
            &book,
            &cfg,
            CODEX,
            0,
            t + 200 * day,
            CacheMode::Full,
        )
        .unwrap();
        // hull spans 101 days, but measured time is (2d+1s) + (1d+1s)
        assert_eq!(r.measured_secs, 3 * day + 2);
        assert!(
            r.notes.iter().any(|n| n.contains("DISJOINT")),
            "disjointness must be noted"
        );
    }

    #[test]
    fn quartiles_basics() {
        let q = quartiles(&[1.0, 2.0, 3.0, 4.0, 5.0]).unwrap();
        assert!((q.med - 3.0).abs() < 1e-9);
        assert!((q.p25 - 2.0).abs() < 1e-9);
        assert!((q.p75 - 4.0).abs() < 1e-9);
        assert!(quartiles(&[]).is_none());
    }

    #[test]
    fn week_bucket_is_monday_utc() {
        // 2026-08-03 is a Monday.
        let monday = chrono::DateTime::parse_from_rfc3339("2026-08-03T00:00:00Z")
            .unwrap()
            .timestamp();
        let wednesday = chrono::DateTime::parse_from_rfc3339("2026-08-05T13:00:00Z")
            .unwrap()
            .timestamp();
        let bucket = (wednesday / 86_400 + 3).div_euclid(7) * 7 * 86_400 - 3 * 86_400;
        assert_eq!(bucket, monday);
    }
}
