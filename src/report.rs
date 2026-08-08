//! Retrospective report rendering (text + JSON).

use crate::engine::waste::{build_provider_report, CacheMode, ProviderReport};
use crate::pricing::PriceBook;
use crate::store::{Store, CLAUDE, CODEX};
use anyhow::{bail, Context, Result};
use chrono::{TimeZone, Utc};

pub fn parse_period(period: &str, now: i64) -> Result<(i64, i64, String)> {
    let p = period.trim().to_lowercase();
    let (t0, label) = match p.as_str() {
        "week" | "7d" => (now - 7 * 86_400, "last 7 days".to_string()),
        "month" | "30d" => (now - 30 * 86_400, "last 30 days".to_string()),
        "90d" => (now - 90 * 86_400, "last 90 days".to_string()),
        "year" | "365d" => (now - 365 * 86_400, "last 365 days".to_string()),
        "all" => (0, "all history".to_string()),
        other => {
            if let Some(days) = other
                .strip_suffix('d')
                .and_then(|d| d.parse::<i64>().ok())
                .filter(|d| *d > 0)
            {
                (now - days * 86_400, format!("last {days} days"))
            } else {
                bail!("unknown period {other:?} — use 7d/30d/90d/365d/all or Nd")
            }
        }
    };
    Ok((t0, now, label))
}

pub fn parse_cache_mode(s: &str) -> Result<CacheMode> {
    match s.trim().to_lowercase().as_str() {
        "full" | "cache" | "cache-included" => Ok(CacheMode::Full),
        "output" | "output-only" | "out" => Ok(CacheMode::OutputOnly),
        other => bail!("unknown cache mode {other:?} — use full or output-only"),
    }
}

pub fn providers_from_flag(provider: Option<&str>) -> Result<Vec<&'static str>> {
    match provider.map(|p| p.to_lowercase()) {
        None => Ok(vec![CODEX, CLAUDE]),
        Some(p) if p == "claude" => Ok(vec![CLAUDE]),
        Some(p) if p == "codex" => Ok(vec![CODEX]),
        Some(p) if p == "both" || p == "all" => Ok(vec![CODEX, CLAUDE]),
        Some(other) => bail!("unknown provider {other:?} — use claude, codex or both"),
    }
}

pub fn build_reports(
    store: &Store,
    book: &PriceBook,
    cfg: &crate::config::Cfg,
    providers: &[&'static str],
    t0: i64,
    t1: i64,
    mode: CacheMode,
) -> Result<Vec<ProviderReport>> {
    providers
        .iter()
        .map(|p| {
            build_provider_report(store, book, cfg, p, t0, t1, mode)
                .with_context(|| format!("building {p} report"))
        })
        .collect()
}

pub fn usd(x: f64) -> String {
    if x.is_nan() {
        return "$?".into();
    }
    let neg = x < 0.0;
    let cents = (x.abs() * 100.0).round() as i64;
    let (int, frac) = (cents / 100, cents % 100);
    let mut s = int.to_string();
    let mut grouped = String::new();
    while s.len() > 3 {
        let tail = s.split_off(s.len() - 3);
        grouped = format!(",{tail}{grouped}");
    }
    format!(
        "{}${}{}.{:02}",
        if neg { "-" } else { "" },
        s,
        grouped,
        frac
    )
}

pub fn date(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".into())
}

pub fn dur_short(secs: i64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

pub fn render_text(reports: &[ProviderReport], label: &str) -> String {
    use crate::theme::{bold, dim, err_glyph, warn, warn_glyph};
    let mut out = String::new();
    let push = |out: &mut String, s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    // Label gutter: 2-cell margin + 17-cell label field → values at col 20.
    let lab = |l: &str| dim(&format!("  {l:<17}"));
    // Continuation rows align under the value column.
    let ind = " ".repeat(19);
    // Primary values pad to a 21-cell field so annotations align (min 1 gap).
    let gap = |v: &str| " ".repeat(21usize.saturating_sub(v.chars().count()).max(1));
    push(&mut out, format!("{} {label}", dim("merma report ·")));
    for r in reports {
        push(&mut out, String::new());
        let mut head = bold(&r.provider);
        head.push_str(&dim(&format!(
            " — {} · {}/mo",
            r.plan_label,
            usd(r.plan_monthly_usd)
        )));
        if r.plan_approx {
            head.push_str(&dim(" (price approx)"));
        }
        push(&mut out, head);
        for e in &r.errors {
            push(&mut out, format!("  {} {e}", err_glyph()));
        }
        let (full, floor) = (usd(r.api.full_usd), usd(r.api.output_only_usd));
        let extracted = match r.cache_mode {
            CacheMode::Full => format!(
                "{}{}{}{}{floor}",
                lab("extracted"),
                bold(&full),
                gap(&full),
                dim("full API-equivalent, cache included · output-only floor ")
            ),
            CacheMode::OutputOnly => format!(
                "{}{}{}{}{full}",
                lab("extracted"),
                bold(&floor),
                gap(&floor),
                dim("output-only · full API-equivalent ")
            ),
        };
        push(&mut out, extracted);
        if let Some(c) = r.api.credits {
            let count = format!("{c:.0}");
            let mut line = format!("{}{count}", lab("credits"));
            if let Some(u) = r.api.credits_usd_approx {
                line.push_str(&gap(&count));
                line.push_str(&dim(&format!("≈ {}", usd(u))));
                if r.api.credits_price_approx {
                    line.push_str(&dim(" ~"));
                }
            }
            push(&mut out, line);
        }
        if let Some(u) = &r.utilization {
            push(
                &mut out,
                format!(
                    "{}weighted peak {:.0}% · coverage {:.0}%{}",
                    lab("utilization"),
                    u.weighted_peak_pct,
                    u.coverage_frac * 100.0,
                    dim(&format!(" · {}", u.window_id))
                ),
            );
            push(
                &mut out,
                format!(
                    "{}{} of {} covered plan cost",
                    lab("waste"),
                    usd(u.waste_usd),
                    usd(u.covered_plan_usd)
                ),
            );
        }
        match (&r.period_max_usd, &r.period_waste_usd, &r.max_extraction) {
            (Some(pm), Some(pw), Some(m)) if m.floored => {
                push(
                    &mut out,
                    format!(
                        "{}≥ {}{}",
                        lab("period max"),
                        usd(pm.med),
                        dim(&format!(
                            "    lower bound — from your achieved best; \
                             join gave less · {} instances",
                            m.n_instances_used
                        ))
                    ),
                );
                push(
                    &mut out,
                    format!(
                        "{}{}",
                        lab("left on table"),
                        bold(&format!("≥ {}", usd(pw.med)))
                    ),
                );
            }
            (Some(pm), Some(pw), Some(m)) => {
                push(
                    &mut out,
                    format!(
                        "{}≈ {} – {}{}",
                        lab("period max"),
                        usd(pm.p25),
                        usd(pm.p75),
                        dim(&format!("    median {}", usd(pm.med)))
                    ),
                );
                push(
                    &mut out,
                    format!(
                        "{}{}{}",
                        lab("left on table"),
                        bold(&format!("≈ {} – {}", usd(pw.p25), usd(pw.p75))),
                        dim(&format!("    median {}", usd(pw.med)))
                    ),
                );
                let quality = if m.stable {
                    dim("stable")
                } else {
                    warn("UNSTABLE")
                };
                push(
                    &mut out,
                    format!(
                        "{ind}{quality}{}",
                        dim(&format!(
                            " join · P75/P25 = {} · {} instances",
                            m.dispersion
                                .map(|d| format!("{d:.1}×"))
                                .unwrap_or_else(|| "undefined".into()),
                            m.n_instances_used
                        ))
                    ),
                );
            }
            _ => {}
        }
        if !r.denominators.is_empty() {
            let entries: Vec<String> = r
                .denominators
                .iter()
                .map(|d| format!("{} {}/wk", d.name, usd(d.weekly_usd)))
                .collect();
            push(
                &mut out,
                format!("{}{}", lab("denominators"), entries.join(&dim(" · "))),
            );
        }
        if !r.api.by_model.is_empty() {
            push(
                &mut out,
                format!(
                    "{}{}",
                    lab("by model"),
                    dim(&format!(
                        "{:<28} {:>7} {:>14} {:>14} {:>12} {:>10} {:>10}",
                        "model", "calls", "input", "cache-read", "output", "full$", "out$"
                    ))
                ),
            );
            for m in &r.api.by_model {
                push(
                    &mut out,
                    format!(
                        "{ind}{:<28} {:>7} {:>14} {:>14} {:>12} {:>10} {:>10}{}",
                        m.model,
                        m.events,
                        m.input,
                        m.cached_input,
                        m.output,
                        usd(m.full_usd),
                        usd(m.output_only_usd),
                        if m.approx { " ~" } else { "" }
                    ),
                );
            }
        }
        for n in &r.notes {
            push(&mut out, format!("  {} {n}", warn_glyph()));
        }
    }
    // Combined footer when both providers present and healthy.
    let both: Vec<&ProviderReport> = reports.iter().filter(|r| r.errors.is_empty()).collect();
    if both.len() > 1 {
        let extracted: f64 = both
            .iter()
            .map(|r| match r.cache_mode {
                CacheMode::Full => r.api.full_usd,
                CacheMode::OutputOnly => r.api.output_only_usd,
            })
            .sum();
        let plan: f64 = both.iter().map(|r| r.plan_monthly_usd).sum();
        push(&mut out, String::new());
        push(
            &mut out,
            format!(
                "{}{}{}{}{}",
                dim("total   extracted "),
                bold(&usd(extracted)),
                dim(" across subscriptions worth "),
                usd(plan),
                dim("/mo")
            ),
        );
    }
    out
}
