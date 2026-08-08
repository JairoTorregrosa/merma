//! `merma wrapped` — the shareable recap card.

use crate::engine::waste::{CacheMode, ProviderReport};
use crate::report::{date, usd};

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

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("MERMA WRAPPED · {label}"));
    lines.push(String::new());
    lines.push(format!("  extracted (API-equivalent)   {}", usd(extracted)));
    lines.push(format!(
        "  output-only floor            {}",
        usd(output_only)
    ));
    lines.push(format!(
        "  subscriptions cost           {}",
        usd(plan_total)
    ));
    if multiplier.is_finite() {
        lines.push(format!("  multiplier                   {multiplier:.1}×"));
    }
    lines.push(String::new());
    for r in reports {
        let name = match r.provider.as_str() {
            "codex" => "Codex ",
            _ => "Claude",
        };
        lines.push(format!(
            "  {name}  {}  ({})",
            usd(match r.cache_mode {
                CacheMode::Full => r.api.full_usd,
                CacheMode::OutputOnly => r.api.output_only_usd,
            }),
            r.plan_label
        ));
    }
    lines.push(String::new());
    if let Some((w, v)) = best {
        lines.push(format!("  best week   {} · {}", date(*w), usd(*v)));
    }
    if have_waste {
        let floored = reports
            .iter()
            .any(|r| r.max_extraction.as_ref().is_some_and(|m| m.floored));
        if (waste_hi - waste_lo).abs() < 0.005 || floored {
            lines.push(format!("  left on the table (est.)  ≥ {}", usd(waste_lo)));
        } else {
            lines.push(format!(
                "  left on the table (est.)  {} – {}",
                usd(waste_lo),
                usd(waste_hi)
            ));
        }
    }
    if !weeks.is_empty() {
        lines.push(format!("  weeks  {spark}"));
    }
    for r in reports {
        if let Some(u) = &r.utilization {
            lines.push(format!(
                "  {} peak utilization {:.0}% of plan",
                r.provider, u.weighted_peak_pct
            ));
        }
    }

    // Box it.
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) + 2;
    let mut out = String::new();
    out.push_str(&format!("╭{}╮\n", "─".repeat(width)));
    for l in &lines {
        let pad = width - l.chars().count() - 1;
        out.push_str(&format!("│ {}{}│\n", l, " ".repeat(pad)));
    }
    out.push_str(&format!("╰{}╯\n", "─".repeat(width)));
    out
}
