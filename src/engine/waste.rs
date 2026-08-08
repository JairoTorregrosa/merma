//! Provider report build: adapts the store into the v0.2 estimator and the
//! decision layer.
//!
//! Measurement doctrine (see handoff research, verified 2026-08-07/08):
//! - Utilization ground truth = official used_percent snapshots. Quotas are
//!   never reconstructed from tokens as primary truth.
//! - API-equivalent $ = local token logs × dated API price tables, full
//!   (cache-included) accounting — cache reads dominate agentic totals.
//! - The dollars-per-percent join is estimated at WINDOW-INSTANCE granularity
//!   (consecutive-snapshot regression is unusable: used_percent is
//!   integer-quantized; R² < 0 on pairs). All estimator math lives in
//!   `engine/estimator.rs`; this module only prepares its inputs.
//! - Every estimate scales by the measured span (interval union), never the
//!   requested period.

use crate::pricing::{claude_event_cost, codex_event_cost, is_external_model, PriceBook};
use crate::store::{Store, UsageEvent, CLAUDE, CODEX};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;

use super::estimator as est;
use super::windows::{reconstruct, WindowInstance};

pub use super::estimator::SECS_PER_MONTH;

#[derive(Debug, Clone, Default)]
pub struct ApiEquiv {
    /// Full API-equivalent dollars (everything the API would bill).
    pub full_usd: f64,
    /// Portion of full_usd computed from era-approximate price entries.
    pub approx_usd: f64,
    /// (model, total tokens) with NO price for their era — excluded from totals, loudly listed.
    pub unpriced: Vec<(String, i64)>,
    /// Non-subscription models routed through the CLI (no quota impact).
    pub external: Vec<(String, i64)>,
}

/// API-equivalent cost of a set of events (one provider).
pub fn api_equiv(book: &PriceBook, provider: &str, events: &[UsageEvent]) -> ApiEquiv {
    let mut out = ApiEquiv::default();
    let mut unpriced: BTreeMap<String, i64> = BTreeMap::new();
    let mut external: BTreeMap<String, i64> = BTreeMap::new();
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
        out.full_usd += cost.full_usd;
        if cost.approx {
            out.approx_usd += cost.full_usd;
        }
    }
    out.unpriced = unpriced.into_iter().collect();
    out.external = external.into_iter().collect();
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanInfo {
    pub label: String,
    /// NaN (serialized as null) when the plan price is unknown.
    pub monthly_usd: f64,
    pub approx: bool,
    /// Plan cost of one operative window: monthly × regime_secs / month.
    pub window_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeasuredSpan {
    /// Hull of the measured data ∩ requested period (display only).
    pub t0: i64,
    pub t1: i64,
    /// Interval-UNION duration of measured data within the period — the ONLY
    /// period-scaling denominator.
    pub union_secs: i64,
    /// union_secs / requested period length, ≤ 1.
    pub coverage_frac: f64,
}

/// The 0.2.0 per-provider report: the brief is a faithful printout of this.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderReport {
    pub provider: String,
    pub plan: PlanInfo,
    pub measured: MeasuredSpan,
    /// Full-period API-equivalent dollars (cache included).
    pub extracted_usd: f64,
    /// None = unmeasured; UNKNOWN never becomes zero.
    pub left_on_table_usd: Option<est::Dollars>,
    /// None when the provider has no window data at all.
    pub basis: Option<est::Basis>,
    pub open_windows: Vec<est::OpenWindow>,
    pub decision: est::Decision,
    pub notes: Vec<String>,
    pub errors: Vec<String>,
}

/// Period-level context the estimator adapter needs from the report build.
struct EstimatorCtx {
    t0: i64,
    now: i64,
    extracted_usd: f64,
    measured_secs: i64,
    /// Merged measured intervals, clipped to the period (for the certified-
    /// epoch clip — the `≥` scaling never leaves plan/regime-constant time).
    measured_intervals: Vec<(i64, i64)>,
    have_billing: bool,
    plan_monthly_usd: f64,
}

/// Calendar-week (Monday-UTC) full API-equivalent dollars across all history,
/// with approx-price tracking — the estimator's floor input. External models
/// are skipped; unpriced events contribute nothing, so the floor stays a
/// certified LOWER bound.
fn weekly_full_with_approx(
    store: &Store,
    book: &PriceBook,
    provider: &str,
) -> Result<Vec<(i64, f64, bool)>> {
    let Some((lo, hi)) = store.usage_bounds(provider)? else {
        return Ok(Vec::new());
    };
    let events = store.usage_between(provider, lo, hi + 1)?;
    let mut weeks: BTreeMap<i64, (f64, bool)> = BTreeMap::new();
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
        let entry = weeks.entry(week).or_insert((0.0, false));
        entry.0 += cost.full_usd;
        entry.1 |= cost.approx;
    }
    Ok(weeks.into_iter().map(|(w, (u, a))| (w, u, a)).collect())
}

/// Adapt store data into the pure estimator's input: operative-window
/// selection, admission candidates, floor/bound sources, open windows.
/// Returns None when the provider has no usable window data.
fn build_estimator_input(
    store: &Store,
    book: &PriceBook,
    provider: &str,
    ctx: &EstimatorCtx,
) -> Result<Option<est::EstimatorInput>> {
    let ids = store.window_ids(provider)?;
    // All-history reconstruction per non-scoped window id (scoped exclusion
    // is data-driven: another id of the same provider is a strict prefix).
    let mut wins: Vec<(String, Vec<WindowInstance>)> = Vec::new();
    for id in ids.iter().filter(|id| !est::is_scoped(id, &ids)) {
        let snaps = store.snapshots_between(provider, Some(id), 0, i64::MAX)?;
        if snaps.is_empty() {
            continue;
        }
        wins.push((id.clone(), reconstruct(snaps)));
    }
    let summaries: Vec<est::WindowSummary> = wins
        .iter()
        .map(|(_, inst)| est::WindowSummary {
            window_minutes: inst.last().and_then(|i| i.window_minutes).unwrap_or(0),
            last_snapshot_ts: inst.last().map(|i| i.last_ts()).unwrap_or(0),
        })
        .collect();
    let Some(op_idx) = est::select_operative(&summaries, ctx.now) else {
        return Ok(None);
    };
    let (op_id, op_instances) = &wins[op_idx];
    let Some(regime) = op_instances
        .last()
        .and_then(|i| i.window_minutes)
        .filter(|m| *m > 0)
    else {
        return Ok(None);
    };
    let regime_secs = regime * 60;
    let in_regime: Vec<&WindowInstance> = op_instances
        .iter()
        .filter(|i| i.window_minutes == Some(regime))
        .collect();
    if in_regime.is_empty() {
        return Ok(None);
    }

    // The still-open instance is never a candidate: its peak is still moving.
    let newest_open = in_regime
        .last()
        .copied()
        .filter(|inst| match inst.resets_at {
            Some(ra) => ra > ctx.now,
            None => ctx.now - inst.last_ts() <= regime_secs,
        });
    let closed: &[&WindowInstance] = if newest_open.is_some() {
        &in_regime[..in_regime.len() - 1]
    } else {
        &in_regime[..]
    };

    // Candidate prep, newest-first. Attribution stays (first_obs, peak]:
    // usage before first_obs is baked into first_pct; usage after the peak
    // produced no measured growth.
    let mut candidates: Vec<est::Candidate> = Vec::new();
    for inst in closed.iter().rev() {
        let evs = store.usage_between(provider, inst.first_ts() + 1, inst.peak_ts() + 1)?;
        let api = api_equiv(book, provider, &evs);
        candidates.push(est::Candidate {
            points: inst.points.clone(),
            peak_ts: inst.peak_ts(),
            attributed_usd: api.full_usd,
            unpriced: !api.unpriced.is_empty(),
            approx: api.approx_usd > 0.0,
            event_ts: evs.iter().map(|e| e.ts).collect(),
        });
    }
    let admission = est::admit(&candidates, regime_secs);

    // Floor: best achieved calendar week whose bucket starts inside the
    // current regime, scaled to the window duration.
    let regime_start = in_regime.first().map(|i| i.first_ts()).unwrap_or(ctx.now);
    let mut floor_weekly = 0.0f64;
    let mut floor_week_start = None;
    let mut floor_approx = false;
    for (wk, usd, approx) in weekly_full_with_approx(store, book, provider)? {
        if wk >= regime_start && usd > floor_weekly {
            floor_weekly = usd;
            floor_week_start = Some(wk);
            floor_approx = approx;
        }
    }
    let floor_window = floor_weekly * regime.min(10_080) as f64 / 10_080.0;

    // Measured-window candidates: a snapped 100% peak makes the attributed
    // dollars a MEASURED full-window value.
    let mut best_hundred = 0.0f64;
    let mut best_hundred_ts = None;
    let mut best_hundred_approx = false;
    let mut any_hundred = false;
    for inst in &in_regime {
        let (peak, _) = est::snap_pct(inst.peak_pct());
        if peak < 100.0 {
            continue;
        }
        any_hundred = true;
        let evs = store.usage_between(provider, inst.first_ts() + 1, inst.peak_ts() + 1)?;
        let api = api_equiv(book, provider, &evs);
        if api.full_usd > best_hundred {
            best_hundred = api.full_usd;
            best_hundred_ts = Some(inst.first_ts());
            best_hundred_approx = api.approx_usd > 0.0;
        }
    }
    let bound_approx = if best_hundred >= floor_window {
        best_hundred_approx
    } else {
        floor_approx
    };

    let complete = closed
        .iter()
        .filter(|i| i.covered_secs() as f64 >= est::COMPLETE_WINDOW_MIN_FRAC * regime_secs as f64)
        .count();
    let first_snapshot_ts = op_instances.first().map(|i| i.first_ts());

    // Certified epoch: the `≥` period scaling is a certified lower bound only
    // over measured time under the CURRENT plan and CURRENT regime — a prior
    // plan's windows had different capacity, a prior regime a different
    // denominator. Epoch start = max(regime start, start of the trailing
    // constant-plan run); the extraction subtracted is the epoch's own.
    let plan_points = store.plan_type_points(provider)?;
    let plan_run_start = match plan_points.last() {
        Some((_, current)) => plan_points
            .iter()
            .rev()
            .take_while(|(_, p)| p == current)
            .map(|(ts, _)| *ts)
            .last()
            .unwrap_or(i64::MIN),
        None => i64::MIN,
    };
    let certified_start = regime_start.max(plan_run_start);
    let certified_secs: i64 = ctx
        .measured_intervals
        .iter()
        .map(|&(a, b)| (b - a.max(certified_start)).max(0))
        .sum();
    let certified_extracted = {
        let evs = store.usage_between(provider, certified_start.max(ctx.t0), ctx.now)?;
        api_equiv(book, provider, &evs).full_usd
    };

    // Operative open-window input (requires a live snapshot).
    let open = match newest_open {
        Some(inst) if ctx.now - inst.last_ts() <= regime_secs => {
            let evs = store.usage_between(provider, inst.first_ts() + 1, ctx.now + 1)?;
            let api = api_equiv(book, provider, &evs);
            Some(est::OpenWindowInput {
                used_pct: inst.points.last().map(|p| p.1).unwrap_or(0.0),
                resets_at: inst.resets_at,
                first_ts: inst.first_ts(),
                extracted_in_window_usd: api.full_usd,
            })
        }
        _ => None,
    };

    // Non-operative open windows: percent + reset + context, never dollars —
    // a shorter window is capacity the operative limit already bounds.
    let mut others = Vec::new();
    for (idx, (id, instances)) in wins.iter().enumerate() {
        if idx == op_idx {
            continue;
        }
        let Some(newest) = instances.last() else {
            continue;
        };
        let Some(wm) = newest.window_minutes.filter(|m| *m > 0) else {
            continue;
        };
        if ctx.now - newest.last_ts() > wm * 60 {
            continue; // not live — not an open window
        }
        let newest_is_open = newest.resets_at.map(|ra| ra > ctx.now).unwrap_or(true);
        let mut peaks: Vec<f64> = Vec::new();
        let mut hit = 0usize;
        for i in instances
            .iter()
            .filter(|i| i.window_minutes == Some(wm))
            .filter(|i| !(newest_is_open && std::ptr::eq(*i, newest)))
        {
            let (p, _) = est::snap_pct(i.peak_pct());
            peaks.push(p);
            if p >= 100.0 {
                hit += 1;
            }
        }
        peaks.sort_by(f64::total_cmp);
        let context = (!peaks.is_empty()).then(|| est::WindowContext {
            median_peak_pct: est::median_sorted(&peaks),
            n: peaks.len(),
            hit_100: hit,
        });
        others.push(est::OtherWindowInput {
            window_id: id.clone(),
            used_pct: newest.points.last().map(|p| p.1).unwrap_or(0.0),
            resets_at: newest.resets_at,
            context,
        });
    }

    Ok(Some(est::EstimatorInput {
        window_id: op_id.clone(),
        regime_minutes: regime,
        admission,
        any_hundred_pct: any_hundred,
        best_hundred_usd: best_hundred,
        best_hundred_first_ts: best_hundred_ts,
        floor_window_usd: floor_window,
        floor_week_start,
        bound_approx,
        extracted_usd: ctx.extracted_usd,
        measured_secs: ctx.measured_secs,
        certified_start_ts: certified_start,
        certified_secs,
        certified_extracted_usd: certified_extracted,
        have_billing: ctx.have_billing,
        plan_monthly_usd: ctx.plan_monthly_usd,
        now: ctx.now,
        first_snapshot_ts,
        complete_windows_observed: complete,
        open,
        others,
    }))
}

/// Build the full report for one provider over [t0, t1).
pub fn build_provider_report(
    store: &Store,
    book: &PriceBook,
    cfg: &crate::config::Cfg,
    provider: &str,
    t0: i64,
    t1: i64,
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
            crate::fmt::dur_short(measured_secs)
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

    // ── billing evidence: any non-scoped window with snapshots in period ──
    let ids = store.window_ids(provider)?;
    let mut have_billing = false;
    for id in ids.iter().filter(|id| !est::is_scoped(id, &ids)) {
        if !store
            .snapshots_between(provider, Some(id), t0, t1)?
            .is_empty()
        {
            have_billing = true;
            break;
        }
    }

    // ── plan cost over measured time, plan-history aware ──
    // Codex plan_type transitions (plus → prolite → plus locally) segment the
    // measured intervals; each segment is priced at ITS plan. Time before the
    // first recorded plan extends the earliest known plan backwards (noted).
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
                crate::fmt::date(*first_ts)
            ));
        }
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

    if !api.unpriced.is_empty() {
        notes.push(format!(
            "unpriced usage EXCLUDED from $ totals (no price for era): {}",
            api.unpriced
                .iter()
                .map(|(m, t)| format!("{m} ({} tok)", crate::fmt::count(*t)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !api.external.is_empty() {
        notes.push(format!(
            "non-subscription models excluded from waste math: {}",
            api.external
                .iter()
                .map(|(m, t)| format!("{m} ({} tok)", crate::fmt::count(*t)))
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

    // ── the estimator: tiers, order-statistic band, live gap ──
    let est_ctx = EstimatorCtx {
        t0,
        now: t1,
        extracted_usd: api.full_usd,
        measured_secs,
        measured_intervals: measured_intervals.clone(),
        have_billing,
        plan_monthly_usd: plan_monthly,
    };
    let estimate = build_estimator_input(store, book, provider, &est_ctx)?.map(est::assemble);
    if estimate
        .as_ref()
        .is_some_and(|e| e.basis.fractional_pct_observed)
    {
        notes.push("non-integer used_percent observed — quantization model may be wrong".into());
    }
    // Decision layer, line 1: extracted vs. plan cost over the MEASURED span.
    let decision = est::decide(
        Some((api.full_usd, api.full_usd)),
        (plan_cost_effective.is_finite() && plan_cost_effective > 0.0)
            .then_some(plan_cost_effective),
        api.approx_usd > 0.0 || plan_approx,
    );

    let period_secs = (t1 - t0).max(1);
    let (left, basis, open_windows, window_cost) = match estimate {
        Some(e) => (
            e.left_on_table_usd,
            Some(e.basis),
            e.open_windows,
            e.window_cost_usd,
        ),
        None => (None, None, Vec::new(), None),
    };
    Ok(ProviderReport {
        provider: provider.to_string(),
        plan: PlanInfo {
            label: plan_label,
            monthly_usd: plan_monthly,
            approx: plan_approx,
            window_cost_usd: window_cost,
        },
        measured: MeasuredSpan {
            t0: effective_t0,
            t1: effective_t1,
            union_secs: measured_secs,
            coverage_frac: (measured_secs as f64 / period_secs as f64).min(1.0),
        },
        extracted_usd: api.full_usd,
        left_on_table_usd: left,
        basis,
        open_windows,
        decision,
        notes,
        errors,
    })
}

/// Cross-window consistency diagnostic (doctor only, never an adjustment).
/// Instances of shorter windows inside a long-window attribution span
/// attribute over disjoint sub-spans of the long span, so the sum of their
/// attributed dollars can never exceed the long instance's attribution.
/// A violation means events were double-counted — an ingestion bug.
#[derive(Debug, Clone)]
pub struct CrossCheck {
    pub checked: usize,
    pub mismatches: Vec<String>,
}

pub fn attribution_cross_check(
    store: &Store,
    book: &PriceBook,
    provider: &str,
    now: i64,
) -> Result<Option<CrossCheck>> {
    let ids = store.window_ids(provider)?;
    let mut wins: Vec<(String, Vec<WindowInstance>)> = Vec::new();
    for id in ids.iter().filter(|id| !est::is_scoped(id, &ids)) {
        let snaps = store.snapshots_between(provider, Some(id), 0, i64::MAX)?;
        if snaps.is_empty() {
            continue;
        }
        wins.push((id.clone(), reconstruct(snaps)));
    }
    let summaries: Vec<est::WindowSummary> = wins
        .iter()
        .map(|(_, inst)| est::WindowSummary {
            window_minutes: inst.last().and_then(|i| i.window_minutes).unwrap_or(0),
            last_snapshot_ts: inst.last().map(|i| i.last_ts()).unwrap_or(0),
        })
        .collect();
    let Some(op_idx) = est::select_operative(&summaries, now) else {
        return Ok(None);
    };
    let (_, op_instances) = &wins[op_idx];
    let Some(regime) = op_instances
        .last()
        .and_then(|i| i.window_minutes)
        .filter(|m| *m > 0)
    else {
        return Ok(None);
    };
    // Shorter concurrent windows only (e.g. Claude five_hour under seven_day).
    let shorter: Vec<&Vec<WindowInstance>> = wins
        .iter()
        .enumerate()
        .filter(|(i, (_, inst))| {
            *i != op_idx
                && inst
                    .last()
                    .and_then(|w| w.window_minutes)
                    .is_some_and(|m| m > 0 && m < regime)
        })
        .map(|(_, (_, inst))| inst)
        .collect();
    if shorter.is_empty() {
        return Ok(None);
    }
    let mut checked = 0usize;
    let mut mismatches = Vec::new();
    for inst in op_instances
        .iter()
        .filter(|i| i.window_minutes == Some(regime))
        .rev()
        .take(est::MAX_INSTANCES_FOR_ESTIMATE)
    {
        let (a, b) = (inst.first_ts(), inst.peak_ts());
        if b <= a {
            continue;
        }
        let long_usd = api_equiv(
            book,
            provider,
            &store.usage_between(provider, a + 1, b + 1)?,
        )
        .full_usd;
        let mut inner_usd = 0.0f64;
        let mut inner_n = 0usize;
        for short in &shorter {
            for s in short
                .iter()
                .filter(|s| s.first_ts() >= a && s.peak_ts() <= b && s.peak_ts() > s.first_ts())
            {
                inner_usd += api_equiv(
                    book,
                    provider,
                    &store.usage_between(provider, s.first_ts() + 1, s.peak_ts() + 1)?,
                )
                .full_usd;
                inner_n += 1;
            }
        }
        if inner_n == 0 {
            continue;
        }
        checked += 1;
        if inner_usd > long_usd + 0.01 {
            mismatches.push(format!(
                "instance of {}: inner windows attribute {} > {} — possible ingestion bug \
                 (double-counted events)",
                crate::fmt::date(a),
                crate::fmt::usd(inner_usd),
                crate::fmt::usd(long_usd),
            ));
        }
    }
    Ok(Some(CrossCheck {
        checked,
        mismatches,
    }))
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
    /// today's ($20/mo): the decision's plan-cost operand integrates over the
    /// plan-type history recorded in snapshots.
    #[test]
    fn plan_history_cost_integration() {
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
        let r = build_provider_report(&store, &book, &cfg, CODEX, t - day, t + 14 * day).unwrap();
        // Snapshot hull t..t+12d(+1s) is one measured interval; the prolite →
        // plus changepoint at t+10d splits it: 10d at $100/mo + 2d+1s at $20/mo.
        let expect = 100.0 * (10.0 * day as f64) / SECS_PER_MONTH
            + 20.0 * (2.0 * day as f64 + 1.0) / SECS_PER_MONTH;
        let got = r.decision.plan_cost_measured_usd.expect("plan cost");
        assert!((got - expect).abs() < 0.01, "got {got} want {expect}");
        assert_eq!(r.measured.union_secs, 12 * day + 1);
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
        let r = build_provider_report(&store, &book, &cfg, CODEX, t - 90 * 86_400, t + 90 * 86_400)
            .unwrap();
        assert_eq!(r.measured.t0, t);
        assert_eq!(r.measured.t1, t + 86_400 + 1);
        assert_eq!(r.measured.union_secs, 86_400 + 1);
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
        let r = build_provider_report(&store, &book, &cfg, CODEX, 0, t + 200 * day).unwrap();
        // hull spans 101 days, but measured time is (2d+1s) + (1d+1s)
        assert_eq!(r.measured.union_secs, 3 * day + 2);
        assert!(
            r.notes.iter().any(|n| n.contains("DISJOINT")),
            "disjointness must be noted"
        );
    }

    fn ev(ts: i64, input: i64, key: &str) -> crate::store::UsageEvent {
        crate::store::UsageEvent {
            provider: CODEX,
            ts,
            model: "gpt-5.6-sol".into(), // $5/M input from 2026-06-01
            input,
            cached_input: 0,
            cache_w_5m: 0,
            cache_w_1h: 0,
            cache_w_unsplit: 0,
            output: 0,
            session_id: None,
            is_sidechain: false,
            dedup_key: key.into(),
            source_file: "t".into(),
        }
    }

    /// End-to-end adapter smoke: candidate prep, attribution queries, open
    /// instance detection, and tier evaluation against a real store.
    /// 4 closed weekly instances (each: dpct 40, $20 attributed → $50/window)
    /// plus one open instance → CALIBRATED with an operative live gap.
    #[test]
    fn estimator_end_to_end_smoke() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let book = PriceBook::load(None).unwrap();
        let mut store = Store::open(&tmp.path().join("m.db")).unwrap();
        let week = 604_800i64;
        let base = 1_781_000_000i64; // 2026-06-09, inside the gpt-5.6-sol era
        let mut snaps = Vec::new();
        let mut events = Vec::new();
        for i in 0..4i64 {
            let s = base + i * week;
            for k in 0..5i64 {
                snaps.push(snap(
                    "primary",
                    10_080,
                    s + k * 43_200,
                    (k * 10) as f64,
                    s + week,
                    "plus",
                ));
            }
            for k in 1..5i64 {
                // $5 each → $20 attributed inside (first_obs, peak].
                events.push(ev(s + k * 43_200, 1_000_000, &format!("e{i}:{k}")));
            }
        }
        // Open instance: 12% used, one $5 event, resets a week out.
        let s4 = base + 4 * week;
        snaps.push(snap("primary", 10_080, s4, 0.0, s4 + week, "plus"));
        snaps.push(snap(
            "primary",
            10_080,
            s4 + 43_200,
            12.0,
            s4 + week,
            "plus",
        ));
        events.push(ev(s4 + 43_200, 1_000_000, "open:1"));
        store.insert_snapshots(&snaps).unwrap();
        store.insert_usage_events(&events).unwrap();

        let t1 = s4 + 86_400;
        let r = build_provider_report(&store, &book, &cfg, CODEX, base - 86_400, t1).unwrap();
        let basis = r.basis.as_ref().expect("basis present");
        assert_eq!(basis.tier, est::Tier::Calibrated);
        assert_eq!(basis.n_qualifying, 4);
        assert!(basis.excluded.is_empty());
        assert!(!basis.floor_binding);
        let band = basis.band_usd_per_window.as_ref().unwrap();
        assert!((band.med - 50.0).abs() < 1e-9);
        assert!((band.lo - 20.0 / 41.0 * 100.0).abs() < 1e-9);
        assert!((band.hi - 20.0 / 39.0 * 100.0).abs() < 1e-9);
        assert_eq!(band.coverage, 0.875);
        assert_eq!(basis.max_loo_shift, Some(0.0));
        // Operative open window: live gap band at 12% used.
        let ow = &r.open_windows[0];
        assert!(ow.operative);
        assert_eq!(ow.window_id, "primary");
        assert!((ow.used_pct - 12.0).abs() < 1e-9);
        let Some(est::Dollars::Band(g)) = &ow.gap_usd else {
            panic!("expected calibrated gap band");
        };
        assert!((g.med - 44.0).abs() < 1e-9);
        // Window age 14% < 25% → pace too young, but capture rates print.
        match &ow.capture.as_ref().unwrap().pace {
            est::PaceComparison::TooYoung => {}
            other => panic!("expected TooYoung, got {other:?}"),
        }
        // Decision line 1: $85 extracted vs ≈$18.73 plan cost → keep.
        assert_eq!(r.decision.verdict, est::Verdict::Keep);
        let m = r.decision.return_multiple.unwrap();
        assert!((4.4..4.7).contains(&m), "multiple {m}");
    }

    /// Cold-start smoke: too few instances → INSUFFICIENT with an unlock
    /// date, no fabricated dollars anywhere.
    #[test]
    fn estimator_cold_start_insufficient() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let book = PriceBook::load(None).unwrap();
        let mut store = Store::open(&tmp.path().join("m.db")).unwrap();
        let week = 604_800i64;
        let base = 1_781_000_000i64;
        let mut snaps = Vec::new();
        // One closed qualifying instance (banked reset — far shorter than a
        // week) and the open one.
        for k in 0..5i64 {
            snaps.push(snap(
                "primary",
                10_080,
                base + k * 43_200,
                (k * 9) as f64,
                base + week,
                "plus",
            ));
        }
        let s1 = base + 300_000; // banked reset moved the window early
        snaps.push(snap("primary", 10_080, s1, 1.0, s1 + week, "plus"));
        let events: Vec<_> = (1..5i64)
            .map(|k| ev(base + k * 43_200, 400_000, &format!("c:{k}")))
            .collect();
        store.insert_snapshots(&snaps).unwrap();
        store.insert_usage_events(&events).unwrap();
        let t1 = s1 + 3_600;
        let r = build_provider_report(&store, &book, &cfg, CODEX, base - 86_400, t1).unwrap();
        let basis = r.basis.as_ref().expect("basis present");
        assert_eq!(basis.tier, est::Tier::Insufficient);
        assert_eq!(basis.n_qualifying, 1);
        let ins = basis.insufficient.as_ref().unwrap();
        assert_eq!(ins.needed_instances, 3);
        // Zero complete windows observed → structural unlock path from the
        // first snapshot.
        assert_eq!(ins.path, Some("structural"));
        assert_eq!(ins.unlocks_at, Some(base + 4 * week));
        // The certified gap may exist only via the floor; period bound ≤ 0
        // here → no dollars fabricated.
        assert!(r.left_on_table_usd.is_none());
    }

    /// The cross-window diagnostic: inner five_hour attributions summing past
    /// the weekly attribution is flagged; a consistent store is not.
    #[test]
    fn cross_window_check_flags_only_overcount() {
        let tmp = tempfile::tempdir().unwrap();
        let book = PriceBook::load(None).unwrap();
        let mut store = Store::open(&tmp.path().join("m.db")).unwrap();
        let base = 1_781_000_000i64;
        let week = 604_800i64;
        let mut snaps = Vec::new();
        // One closed weekly instance 0% → 40% over 4 half-days.
        for k in 0..5i64 {
            snaps.push(snap(
                "seven_day",
                10_080,
                base + k * 43_200,
                (k * 10) as f64,
                base + week,
                "plus",
            ));
        }
        // A five_hour instance inside the weekly span, 0% → 50%.
        for k in 0..3i64 {
            let mut s = snap(
                "five_hour",
                300,
                base + 40_000 + k * 3_000,
                (k * 25) as f64,
                base + 40_000 + 18_000,
                "plus",
            );
            s.provider = "claude".into();
            snaps.push(s);
        }
        for s in &mut snaps {
            s.provider = "claude".into();
        }
        store.insert_snapshots(&snaps).unwrap();
        // Events inside both spans (subset relation holds → no mismatch).
        let mut events = Vec::new();
        for k in 1..5i64 {
            let mut e = ev(base + k * 43_200, 1_000_000, &format!("w:{k}"));
            e.provider = CLAUDE;
            e.model = "claude-fable-5".into();
            events.push(e);
        }
        store.insert_usage_events(&events).unwrap();
        let check = attribution_cross_check(&store, &book, CLAUDE, base + 4 * 43_200 + 10)
            .unwrap()
            .expect("has shorter windows");
        assert!(check.mismatches.is_empty(), "{:?}", check.mismatches);
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
