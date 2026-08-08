//! `merma wrapped` — the shareable recap card.

use crate::engine::waste::{CacheMode, ProviderReport};
use crate::report::{date, usd};
use crate::theme::{bold, dim};

pub const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn sparkline(values: &[f64]) -> String {
    let max = values.iter().cloned().fold(0.0f64, f64::max);
    if max <= 0.0 {
        return "▁".repeat(values.len());
    }
    values
        .iter()
        .map(|v| SPARK[((v / max) * 7.0).round().clamp(0.0, 7.0) as usize])
        .collect()
}

pub fn render(reports: &[ProviderReport], label: &str) -> String {
    let extracted: f64 = reports.iter().map(|r| r.api.full_usd).sum();
    let output_only: f64 = reports.iter().map(|r| r.api.output_only_usd).sum();
    // Plan cost over MEASURED time only, integrated over plan history —
    // prolite-era stretches cost prolite prices, gaps cost nothing.
    let plan_total: f64 = reports
        .iter()
        .map(|r| {
            if r.plan_cost_effective_usd.is_nan() {
                0.0
            } else {
                r.plan_cost_effective_usd
            }
        })
        .sum();
    let multiplier = if plan_total > 0.0 {
        extracted / plan_total
    } else {
        f64::NAN
    };

    // Best week + weekly sparkline across providers (period-limited).
    let mut weeks: std::collections::BTreeMap<i64, f64> = Default::default();
    for r in reports {
        for (w, v) in &r.weekly_series {
            if *w >= r.period_t0 - 7 * 86_400 && *w < r.period_t1 {
                *weeks.entry(*w).or_default() += v;
            }
        }
    }
    let best = weeks.iter().max_by(|a, b| a.1.total_cmp(b.1));
    let spark = sparkline(&weeks.values().cloned().collect::<Vec<_>>());

    // Waste estimate range summed where present.
    let (mut waste_lo, mut waste_hi, mut have_waste) = (0.0, 0.0, false);
    for r in reports {
        if let Some(w) = &r.period_waste_usd {
            waste_lo += w.p25;
            waste_hi += w.p75;
            have_waste = true;
        }
    }

    // One row of the card: plain text for measuring, styled text for painting.
    // Plain and styled variants must be byte-identical modulo escapes.
    struct Row {
        l: String,
        r: String,
        ls: String,
        rs: String,
    }
    let row = |l: String, r: String, ls: String, rs: String| Row { l, r, ls, rs };
    let left = |l: String, ls: String| Row {
        l,
        r: String::new(),
        ls,
        rs: String::new(),
    };
    let mut groups: Vec<Vec<Row>> = Vec::new();

    groups.push(vec![left(
        format!("merma wrapped · {label}"),
        format!("{} {label}", dim("merma wrapped ·")),
    )]);

    let mut g = Vec::new();
    let v = usd(extracted);
    g.push(row(
        "extracted (API-equivalent)".into(),
        v.clone(),
        dim("extracted (API-equivalent)"),
        bold(&v),
    ));
    let v = usd(output_only);
    g.push(row(
        "output-only floor".into(),
        v.clone(),
        dim("output-only floor"),
        v.clone(),
    ));
    let v = usd(plan_total);
    g.push(row(
        "subscriptions cost".into(),
        v.clone(),
        dim("subscriptions cost"),
        v.clone(),
    ));
    if multiplier.is_finite() {
        let v = format!("{multiplier:.1}×");
        g.push(row(
            "multiplier".into(),
            v.clone(),
            dim("multiplier"),
            bold(&v),
        ));
    }
    groups.push(g);

    let mut g = Vec::new();
    for r in reports {
        let l = format!("{:<9}{}", r.provider, r.plan_label);
        let v = usd(match r.cache_mode {
            CacheMode::Full => r.api.full_usd,
            CacheMode::OutputOnly => r.api.output_only_usd,
        });
        g.push(row(l.clone(), v.clone(), dim(&l), v.clone()));
    }
    groups.push(g);

    let mut g = Vec::new();
    if let Some((w, v)) = best {
        let val = usd(*v);
        g.push(row(
            format!("best week   {}", date(*w)),
            val.clone(),
            format!("{}   {}", dim("best week"), date(*w)),
            val.clone(),
        ));
    }
    if have_waste {
        let floored = reports
            .iter()
            .any(|r| r.max_extraction.as_ref().is_some_and(|m| m.floored));
        let val = if (waste_hi - waste_lo).abs() < 0.005 || floored {
            format!("≥ {}", usd(waste_lo))
        } else {
            format!("{} – {}", usd(waste_lo), usd(waste_hi))
        };
        g.push(row(
            "left on the table (est.)".into(),
            val.clone(),
            dim("left on the table (est.)"),
            bold(&val),
        ));
    }
    if !weeks.is_empty() {
        g.push(left(
            format!("weeks   {spark}"),
            format!("{}   {spark}", dim("weeks")),
        ));
    }
    groups.push(g);

    let mut g = Vec::new();
    for r in reports {
        if let Some(u) = &r.utilization {
            let l = format!("{} peak utilization", r.provider);
            let v = format!("{:.0}% of plan", u.weighted_peak_pct);
            g.push(row(l.clone(), v.clone(), dim(&l), v.clone()));
        }
    }
    groups.push(g);

    // Geometry: inner content width fits every "label + 3-space gap + value"
    // pair, floored at 46; 3-cell interior side padding; values right-aligned.
    let inner = groups
        .iter()
        .flatten()
        .map(|r| {
            r.l.chars().count()
                + if r.r.is_empty() {
                    0
                } else {
                    3 + r.r.chars().count()
                }
        })
        .max()
        .unwrap_or(0)
        .max(46);

    let mut out = String::new();
    let rule = "─".repeat(inner + 6);
    let blank = format!("{}{}{}\n", dim("│"), " ".repeat(inner + 6), dim("│"));
    out.push_str(&format!("{}\n", dim(&format!("╭{rule}╮"))));
    out.push_str(&blank);
    for (i, g) in groups.iter().filter(|g| !g.is_empty()).enumerate() {
        if i > 0 {
            out.push_str(&blank);
        }
        for r in g {
            let content = if r.r.is_empty() {
                format!("{}{}", r.ls, " ".repeat(inner - r.l.chars().count()))
            } else {
                let gap = inner - r.l.chars().count() - r.r.chars().count();
                format!("{}{}{}", r.ls, " ".repeat(gap), r.rs)
            };
            out.push_str(&format!("{}   {content}   {}\n", dim("│"), dim("│")));
        }
    }
    out.push_str(&blank);
    out.push_str(&format!("{}\n", dim(&format!("╰{rule}╯"))));
    out
}
