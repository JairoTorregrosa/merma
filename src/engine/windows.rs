//! Window-instance reconstruction from utilization snapshots.
//!
//! A "window instance" is one lifetime of a rate-limit window (e.g. one weekly
//! cycle). Instances are detected from data, never from calendar math:
//! - `resets_at` moving beyond tolerance → new instance (this also catches
//!   banked rate-limit resets, which move the weekly reset ~7 days out and can
//!   make instances much shorter than their nominal duration);
//! - a sudden used_percent drop with an unchanged/unknown resets_at → new
//!   instance (reset observed without metadata);
//! - gradual percent decay does NOT split (Claude windows are rolling — old
//!   usage exits the window smoothly).
//! - a regime change (window_minutes) always splits: utilization percentages
//!   from different regimes are not comparable denominators.

use crate::store::Snapshot;

/// Snapshots within this resets_at drift are the same instance (seconds).
pub const RESET_TOLERANCE_SECS: i64 = 600;
/// A point-to-point percent drop bigger than this is a reset, not decay.
pub const PCT_DROP_THRESHOLD: f64 = 5.0;

#[derive(Debug, Clone, serde::Serialize)]
pub struct WindowInstance {
    pub window_id: String,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<i64>,
    /// (ts, used_percent), merged across sources, ascending ts.
    pub points: Vec<(i64, f64)>,
    pub plan_type: Option<String>,
}

impl WindowInstance {
    pub fn first_ts(&self) -> i64 {
        self.points.first().map(|p| p.0).unwrap_or(0)
    }
    pub fn last_ts(&self) -> i64 {
        self.points.last().map(|p| p.0).unwrap_or(0)
    }
    pub fn covered_secs(&self) -> i64 {
        (self.last_ts() - self.first_ts()).max(0)
    }
    pub fn first_pct(&self) -> f64 {
        self.points.first().map(|p| p.1).unwrap_or(0.0)
    }
    pub fn peak_pct(&self) -> f64 {
        self.points.iter().map(|p| p.1).fold(0.0, f64::max)
    }
    /// Timestamp of the first observation of the peak percent. Usage after the
    /// peak produced no measured growth, so calibration must stop here.
    pub fn peak_ts(&self) -> i64 {
        let peak = self.peak_pct();
        self.points
            .iter()
            .find(|p| p.1 >= peak)
            .map(|p| p.0)
            .unwrap_or(0)
    }
    /// Observed percent growth: peak minus first observation.
    pub fn dpct(&self) -> f64 {
        (self.peak_pct() - self.first_pct()).max(0.0)
    }
}

/// Reconstruct instances from snapshots of ONE (provider, window_id), any mix
/// of sources. Input need not be sorted.
pub fn reconstruct(mut snaps: Vec<Snapshot>) -> Vec<WindowInstance> {
    // Deterministic order, then coalesce same-second observations from
    // different sources (rollout vs live poll can disagree by measurement lag;
    // SQLite tie order is arbitrary, so without this the instance count would
    // depend on insertion order). Keep the highest percent of the second —
    // conservative: a higher observed peak claims LESS waste.
    snaps.sort_by(|a, b| {
        a.ts.cmp(&b.ts)
            .then(a.used_percent.total_cmp(&b.used_percent))
    });
    let mut merged: Vec<Snapshot> = Vec::with_capacity(snaps.len());
    for s in snaps {
        match merged.last_mut() {
            Some(prev) if prev.ts == s.ts => {
                // s sorts >= prev on used_percent: adopt it, keep any metadata
                // the winning record lacks.
                let keep_reset = s.resets_at.or(prev.resets_at);
                let keep_wm = s.window_minutes.or(prev.window_minutes);
                let keep_plan = s.plan_type.clone().or_else(|| prev.plan_type.clone());
                *prev = s;
                prev.resets_at = keep_reset;
                prev.window_minutes = keep_wm;
                prev.plan_type = keep_plan;
            }
            _ => merged.push(s),
        }
    }
    let mut out: Vec<WindowInstance> = Vec::new();
    // resets_at anchor of the instance being built: drift is measured against
    // this fixed point, not the previous observation, so slow cumulative drift
    // cannot smuggle an arbitrarily large reset move inside tolerance.
    let mut anchor: Option<i64> = None;
    for s in merged {
        let new_instance = match out.last() {
            None => true,
            Some(cur) => {
                let regime_changed = cur.window_minutes.is_some()
                    && s.window_minutes.is_some()
                    && cur.window_minutes != s.window_minutes;
                let reset_moved = match (anchor, s.resets_at) {
                    (Some(a), Some(b)) => (a - b).abs() > RESET_TOLERANCE_SECS,
                    _ => false,
                };
                let pct_reset = s.used_percent
                    < cur.points.last().map(|p| p.1).unwrap_or(0.0) - PCT_DROP_THRESHOLD;
                // A whole window elapsed unobserved ⇒ at least one reset
                // happened in the gap; gluing the endpoints together would
                // fabricate coverage and merge distinct instances.
                let gap_expired = cur
                    .window_minutes
                    .map(|wm| s.ts - cur.last_ts() > wm * 60)
                    .unwrap_or(false);
                // A plan change resets quotas AND changes what the covered
                // time costs — one instance must never span two plans.
                let plan_changed = cur.plan_type.is_some()
                    && s.plan_type.is_some()
                    && cur.plan_type != s.plan_type;
                regime_changed || reset_moved || pct_reset || gap_expired || plan_changed
            }
        };
        if new_instance {
            anchor = s.resets_at;
            out.push(WindowInstance {
                window_id: s.window_id.clone(),
                window_minutes: s.window_minutes,
                resets_at: s.resets_at,
                points: vec![(s.ts, s.used_percent)],
                plan_type: s.plan_type.clone(),
            });
        } else if let Some(cur) = out.last_mut() {
            cur.points.push((s.ts, s.used_percent));
            if cur.window_minutes.is_none() {
                cur.window_minutes = s.window_minutes;
            }
            if s.resets_at.is_some() {
                if anchor.is_none() {
                    anchor = s.resets_at;
                }
                cur.resets_at = s.resets_at;
            }
            if s.plan_type.is_some() {
                cur.plan_type = s.plan_type.clone();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Snapshot;

    fn snap(ts: i64, pct: f64, resets_at: Option<i64>, wm: Option<i64>) -> Snapshot {
        Snapshot {
            provider: "codex".into(),
            window_id: "primary".into(),
            window_minutes: wm,
            used_percent: pct,
            resets_at,
            ts,
            source: "rollout".into(),
            plan_type: None,
        }
    }

    #[test]
    fn splits_on_reset_move() {
        let s = vec![
            snap(100, 10.0, Some(10_000), Some(10_080)),
            snap(200, 20.0, Some(10_000), Some(10_080)),
            snap(300, 1.0, Some(700_000), Some(10_080)), // banked reset moved resets_at
            snap(400, 3.0, Some(700_000), Some(10_080)),
        ];
        let inst = reconstruct(s);
        assert_eq!(inst.len(), 2);
        assert_eq!(inst[0].points.len(), 2);
        assert!((inst[0].peak_pct() - 20.0).abs() < 1e-9);
        assert!((inst[1].dpct() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn splits_on_pct_drop_without_reset_info() {
        let s = vec![
            snap(100, 40.0, None, Some(300)),
            snap(200, 41.0, None, Some(300)),
            snap(300, 2.0, None, Some(300)),
        ];
        let inst = reconstruct(s);
        assert_eq!(inst.len(), 2);
    }

    #[test]
    fn gradual_decay_does_not_split() {
        // Rolling window: 40 → 38 → 36 is decay, not reset.
        let s = vec![
            snap(100, 40.0, None, Some(300)),
            snap(200, 38.0, None, Some(300)),
            snap(300, 36.0, None, Some(300)),
        ];
        assert_eq!(reconstruct(s).len(), 1);
    }

    #[test]
    fn same_second_disagreement_coalesces_deterministically() {
        // rollout says 40, live poll says 30 in the same second: without
        // coalescing, visit order decides whether a phantom reset splits here.
        let a = vec![
            snap(100, 39.0, None, Some(300)),
            snap(200, 40.0, None, Some(300)),
            snap(200, 30.0, None, Some(300)),
            snap(300, 41.0, None, Some(300)),
        ];
        let inst = reconstruct(a);
        assert_eq!(inst.len(), 1);
        assert!((inst[0].peak_pct() - 41.0).abs() < 1e-9);
        assert_eq!(inst[0].points.len(), 3); // 200 coalesced to the 40.0 obs
    }

    #[test]
    fn cumulative_reset_drift_splits_against_anchor() {
        // Each step drifts 500s (< tolerance) but the total is 1500s: one
        // instance under prev-comparison, two against the anchor.
        let s = vec![
            snap(100, 10.0, Some(10_000), Some(10_080)),
            snap(200, 11.0, Some(10_500), Some(10_080)),
            snap(300, 12.0, Some(11_000), Some(10_080)),
            snap(400, 13.0, Some(11_500), Some(10_080)),
        ];
        assert_eq!(reconstruct(s).len(), 2);
    }

    #[test]
    fn unobserved_full_window_gap_splits() {
        // 300-min window, next observation 6h later: a reset MUST have
        // happened in the gap even though pct grew and resets_at is unknown.
        let s = vec![
            snap(1_000, 10.0, None, Some(300)),
            snap(2_000, 12.0, None, Some(300)),
            snap(2_000 + 6 * 3600, 14.0, None, Some(300)),
        ];
        let inst = reconstruct(s);
        assert_eq!(inst.len(), 2);
        assert_eq!(inst[1].points.len(), 1);
    }

    #[test]
    fn plan_change_splits_instance() {
        let mut a = snap(100, 10.0, None, Some(10_080));
        a.plan_type = Some("prolite".into());
        let mut b = snap(200, 12.0, None, Some(10_080));
        b.plan_type = Some("prolite".into());
        let mut c = snap(300, 2.0, None, Some(10_080));
        c.plan_type = Some("plus".into());
        let inst = reconstruct(vec![a, b, c]);
        assert_eq!(inst.len(), 2);
        assert_eq!(inst[0].plan_type.as_deref(), Some("prolite"));
        assert_eq!(inst[1].plan_type.as_deref(), Some("plus"));
    }

    #[test]
    fn peak_ts_is_first_peak_observation() {
        let s = vec![
            snap(100, 10.0, None, Some(300)),
            snap(200, 40.0, None, Some(300)),
            snap(300, 40.0, None, Some(300)),
            snap(400, 38.0, None, Some(300)),
        ];
        let inst = reconstruct(s);
        assert_eq!(inst.len(), 1);
        assert_eq!(inst[0].peak_ts(), 200);
    }

    #[test]
    fn regime_change_always_splits() {
        // Jul→Aug 2026: primary flipped 300-min → 10080-min.
        let s = vec![
            snap(100, 10.0, Some(5_000), Some(300)),
            snap(200, 11.0, Some(5_000), Some(300)),
            snap(300, 11.0, Some(5_000), Some(10_080)),
        ];
        let inst = reconstruct(s);
        assert_eq!(inst.len(), 2);
        assert_eq!(inst[1].window_minutes, Some(10_080));
    }
}
