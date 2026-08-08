//! The brief — merma's single surface — plus the 0.2.0 `--json` contract and
//! the one-line status.
//!
//! The text is a faithful printout of the JSON: every number in the text
//! exists in the JSON with the same value, formatted under the fmt.rs
//! rendering rules. Symbol discipline: `≥` bound/measured (one-sided) ·
//! `≈ a – b` calibrated band · `~` approximate-price propagation ·
//! absence/`unmeasured` is never rendered as $0.

use crate::engine::estimator as est;
use crate::engine::waste::{build_provider_report, ProviderReport};
use crate::fmt::{coverage_pct, date, dur_short, usd, usd_whole};
use crate::pricing::PriceBook;
use crate::store::{Store, CLAUDE, CODEX};
use anyhow::{bail, Context, Result};
use serde::Serialize;

/// `--json` schema version. Breaking change from the 0.1.x report shape,
/// sanctioned and documented in the README; fields may only grow WITHIN a
/// schema_version.
pub const SCHEMA_VERSION: &str = "0.2.0";

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
) -> Result<Vec<ProviderReport>> {
    providers
        .iter()
        .map(|p| {
            build_provider_report(store, book, cfg, p, t0, t1)
                .with_context(|| format!("building {p} report"))
        })
        .collect()
}

// ── JSON contract ──

#[derive(Debug, Serialize)]
pub struct PeriodJson {
    pub requested: String,
    pub t0: i64,
    pub t1: i64,
}

#[derive(Debug, Serialize)]
pub struct BriefJson {
    pub schema_version: &'static str,
    pub generated_at: i64,
    pub period: PeriodJson,
    pub providers: Vec<ProviderReport>,
}

pub fn brief_json(reports: Vec<ProviderReport>, requested: &str, t0: i64, t1: i64) -> BriefJson {
    BriefJson {
        schema_version: SCHEMA_VERSION,
        generated_at: t1,
        period: PeriodJson {
            requested: requested.trim().to_lowercase(),
            t0,
            t1,
        },
        providers: reports,
    }
}

// ── shared wording ──

/// Human phrase for a gate-exclusion reason.
pub fn reason_phrase(reason: &str) -> &'static str {
    match reason {
        est::REASON_LOW_GROWTH => "low growth",
        est::REASON_UNPRICED => "unpriced usage",
        est::REASON_USAGE_GAP => "usage gap in span",
        est::REASON_SNAPSHOT_GAP => "snapshot gap in span",
        _ => "other",
    }
}

/// Exclusion summary: `5 excluded: usage gap in span ×4, low growth`.
pub fn excluded_phrase(excluded: &[est::Exclusion]) -> Option<String> {
    if excluded.is_empty() {
        return None;
    }
    let total: usize = excluded.iter().map(|e| e.count).sum();
    let parts: Vec<String> = excluded
        .iter()
        .map(|e| {
            if e.count > 1 {
                format!("{} ×{}", reason_phrase(e.reason), e.count)
            } else {
                reason_phrase(e.reason).to_string()
            }
        })
        .collect();
    Some(format!("{total} excluded: {}", parts.join(", ")))
}

/// Tier name as printed (matches the serialized rename).
pub fn tier_name(tier: est::Tier) -> &'static str {
    match tier {
        est::Tier::Measured => "MEASURED",
        est::Tier::Calibrated => "CALIBRATED",
        est::Tier::Insufficient => "INSUFFICIENT",
    }
}

/// Word for a regime duration: 10 080 min → `weekly`, 300 → `5h`.
fn regime_word(minutes: i64) -> String {
    if minutes == 10_080 {
        "weekly".into()
    } else if minutes % 1440 == 0 {
        format!("{}d", minutes / 1440)
    } else if minutes % 60 == 0 {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

fn measured_days(r: &ProviderReport) -> String {
    format!("{:.1}d", r.measured.union_secs as f64 / 86_400.0)
}

/// `30.0d measured of 30d (100%)`; for `all`, coverage of the requested span
/// is meaningless, so only the measured duration prints.
fn measured_ann(r: &ProviderReport, requested: &str) -> String {
    if requested == "all" {
        format!("{} measured", measured_days(r))
    } else {
        format!(
            "{} measured of {requested} ({:.0}%)",
            measured_days(r),
            r.measured.coverage_frac * 100.0
        )
    }
}

// ── wrapping ──

/// The brief self-wraps at this many print columns. Fixed, not detected:
/// terminal-size detection would need a new dependency (banned), and a fixed
/// width keeps the output deterministic and the captures reproducible. 120 is
/// the narrowest width at which the DESIGN.md mockup rows stay whole.
pub const BRIEF_WRAP_WIDTH: usize = 120;
/// Folded continuations indent to the value column (2 + 17).
const CONT_INDENT: usize = 19;

/// Styling role of one segment of a logical brief line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seg {
    Plain,
    Dim,
    Bold,
}

fn paint_seg(text: &str, seg: Seg) -> String {
    use crate::theme::{bold, dim};
    match seg {
        Seg::Plain => text.to_string(),
        Seg::Dim => dim(text),
        Seg::Bold => bold(text),
    }
}

/// Greedy word-fold of one logical line into physical lines of at most
/// `width` print columns. All width math runs on PLAIN text; styles are
/// painted per run only after the breaks are chosen. Interior space runs
/// (label gutter, value-column pad) are preserved verbatim; a space run is
/// dropped when it lands at a break; continuation lines indent `indent`
/// columns; trailing spaces never survive to the output. A single word wider
/// than the remaining width is emitted whole — never split mid-token. An
/// operator word (`−`, `×`, `≥`, `≈`, `–`, `-`) binds to the word after it,
/// so a break never orphans an operator at the end of a line.
fn push_folded(out: &mut String, segs: &[(String, Seg)], width: usize, indent: usize) {
    // Atomize into maximal space / non-space runs, then group consecutive
    // non-space atoms (even across segment boundaries) into unbreakable words.
    #[derive(Debug)]
    struct Item {
        runs: Vec<(String, Seg)>,
        is_space: bool,
        width: usize,
    }
    let mut items: Vec<Item> = Vec::new();
    for (text, seg) in segs {
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let is_space = rest.starts_with(' ');
            let end = rest
                .find(|c: char| (c == ' ') != is_space)
                .unwrap_or(rest.len());
            let (atom, tail) = rest.split_at(end);
            let w = atom.chars().count();
            match items.last_mut() {
                Some(it) if !it.is_space && !is_space => {
                    it.runs.push((atom.to_string(), *seg));
                    it.width += w;
                }
                _ => items.push(Item {
                    runs: vec![(atom.to_string(), *seg)],
                    is_space,
                    width: w,
                }),
            }
            rest = tail;
        }
    }
    // Typographic binding: a lone operator word never ends a line — it fuses
    // with the single space after it and its right operand into one
    // unbreakable word. `·` and `—` are separators and MAY end a line; `~`
    // trails its value and never starts a bind.
    const BINDING_OPS: [&str; 6] = ["−", "×", "≥", "≈", "–", "-"];
    let mut fused: Vec<Item> = Vec::with_capacity(items.len());
    let mut i = 0;
    while i < items.len() {
        let binds = !items[i].is_space
            && BINDING_OPS.contains(
                &items[i]
                    .runs
                    .iter()
                    .map(|(t, _)| t.as_str())
                    .collect::<String>()
                    .as_str(),
            )
            && items.get(i + 1).is_some_and(|s| s.is_space && s.width == 1)
            && items.get(i + 2).is_some_and(|w| !w.is_space);
        let take = if binds { 3 } else { 1 };
        let mut it = Item {
            runs: Vec::new(),
            is_space: items[i].is_space,
            width: 0,
        };
        for part in items.iter_mut().skip(i).take(take) {
            it.width += part.width;
            it.runs.append(&mut part.runs);
        }
        fused.push(it);
        i += take;
    }
    let items = fused;
    let flush = |out: &mut String, line: &mut Vec<(String, Seg)>| {
        while line
            .last()
            .is_some_and(|(t, _)| t.chars().all(|c| c == ' '))
        {
            line.pop();
        }
        // Merge adjacent same-style runs, then paint each run once.
        for (text, seg) in line
            .drain(..)
            .fold(Vec::<(String, Seg)>::new(), |mut acc, (t, s)| {
                match acc.last_mut() {
                    Some((pt, ps)) if *ps == s => pt.push_str(&t),
                    _ => acc.push((t, s)),
                }
                acc
            })
        {
            out.push_str(&paint_seg(&text, seg));
        }
        out.push('\n');
    };
    // Greedy fill. A break is only taken before a word, never before a space
    // run, and a continuation always STARTS with the word that overflowed —
    // the space run that landed on the break is stripped by `flush`. Leading
    // gutter spaces of the first line pass through untouched.
    let mut line: Vec<(String, Seg)> = Vec::new();
    let mut line_w = 0usize;
    let mut any_word = false; // never break before the line's first word
    let mut emitted = false;
    for it in items {
        if !it.is_space && any_word && line_w + it.width > width {
            flush(out, &mut line);
            emitted = true;
            line_w = 0;
            if indent > 0 {
                line.push((" ".repeat(indent), Seg::Plain));
                line_w = indent;
            }
        }
        line_w += it.width;
        line.extend(it.runs);
        any_word |= !it.is_space;
    }
    if !line.is_empty() || !emitted {
        flush(out, &mut line);
    }
}

// ── the brief ──

/// Render the plain/TTY brief at `width` print columns (the default surface
/// passes [`BRIEF_WRAP_WIDTH`]). `now` = the period end (reset countdowns).
pub fn render_brief(
    reports: &[ProviderReport],
    label: &str,
    requested: &str,
    now: i64,
    width: usize,
) -> String {
    use crate::theme::{err_glyph, warn_glyph};
    let mut out = String::new();
    let push = |out: &mut String, s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    let pushw = |out: &mut String, segs: Vec<(String, Seg)>| {
        push_folded(out, &segs, width, CONT_INDENT);
    };
    // Label gutter: 2-cell margin + 17-cell label field → values at col 20.
    let lab = |l: &str| (format!("  {l:<17}"), Seg::Dim);
    // Continuation rows align under the value column.
    let ind_seg = || (" ".repeat(CONT_INDENT), Seg::Plain);
    // Primary values pad to a 21-cell field so annotations align (min 1 gap).
    let gap = |v: &str| " ".repeat(21usize.saturating_sub(v.chars().count()).max(1));
    pushw(
        &mut out,
        vec![
            ("merma ·".into(), Seg::Dim),
            (format!(" {label}"), Seg::Plain),
        ],
    );

    for r in reports {
        push(&mut out, String::new());
        let mut head = vec![
            (r.provider.clone(), Seg::Bold),
            (
                format!(" — {} · {}/mo", r.plan.label, usd(r.plan.monthly_usd)),
                Seg::Dim,
            ),
        ];
        if r.plan.approx {
            head.push((" (price approx)".into(), Seg::Dim));
        }
        pushw(&mut out, head);
        for e in &r.errors {
            push(&mut out, format!("  {} {e}", err_glyph()));
        }

        // ── left on table ──
        let tier = r.basis.as_ref().map(|b| b.tier);
        match (&r.left_on_table_usd, tier) {
            (Some(est::Dollars::Band(b)), _) => {
                let mut value = format!("≈ {} – {}", usd(b.lo), usd(b.hi));
                if b.approx {
                    value.push_str(" ~");
                }
                let pad = gap(&value);
                pushw(
                    &mut out,
                    vec![
                        lab("left on table"),
                        (value, Seg::Bold),
                        (pad, Seg::Plain),
                        (
                            format!(
                                "median {} · {} band · {}",
                                usd(b.med),
                                coverage_pct(b.coverage),
                                measured_ann(r, requested)
                            ),
                            Seg::Dim,
                        ),
                    ],
                );
            }
            (Some(est::Dollars::Floor { low, approx }), _) => {
                let mut value = format!("≥ {}", usd(*low));
                if *approx {
                    value.push_str(" ~");
                }
                // The certified arithmetic prints in full so the reader can
                // redo it: bound × windows of the certified (current
                // plan+regime) epoch − the extraction inside that epoch.
                let cert = r.basis.as_ref().and_then(|b| {
                    let c = b.certified.as_ref()?;
                    let bound = b.bound_usd_per_window?;
                    Some(format!(
                        "{} best window × {:.2} current plan+regime windows ({:.1}d) − {} \
                         extracted in them",
                        usd(bound),
                        c.windows,
                        c.secs as f64 / 86_400.0,
                        usd(c.extracted_usd)
                    ))
                });
                let ann = match cert {
                    Some(cert) => format!("lower bound · {cert} · {}", measured_ann(r, requested)),
                    None => format!(
                        "lower bound · your best achieved window is the bound · {}",
                        measured_ann(r, requested)
                    ),
                };
                let pad = gap(&value);
                pushw(
                    &mut out,
                    vec![
                        lab("left on table"),
                        (value, Seg::Bold),
                        (pad, Seg::Plain),
                        (ann, Seg::Dim),
                    ],
                );
            }
            (None, tier) => {
                // A bound exists but the period figure computed to nothing:
                // extraction covered the certified windows' bound, so there is
                // no measured shortfall. This is NOT the cold-start case — the
                // same mirror of the estimator arithmetic
                // (bound × certified windows − certified extracted ≤ 0)
                // discriminates it from "no estimate yet". Never $0 either way.
                let no_shortfall = matches!(
                    tier,
                    Some(est::Tier::Measured) | Some(est::Tier::Insufficient)
                ) && r.basis.as_ref().is_some_and(|b| {
                    b.period_windows > 0.0
                        && b.certified
                            .as_ref()
                            .zip(b.bound_usd_per_window)
                            .is_some_and(|(c, bd)| bd * c.windows - c.extracted_usd <= 0.0)
                });
                if no_shortfall {
                    let value = "no measured shortfall";
                    pushw(
                        &mut out,
                        vec![
                            lab("left on table"),
                            (value.into(), Seg::Bold),
                            (gap(value), Seg::Plain),
                            (
                                format!(
                                    "extraction covered the certified windows' bound · {}",
                                    measured_ann(r, requested)
                                ),
                                Seg::Dim,
                            ),
                        ],
                    );
                } else if matches!(tier, Some(est::Tier::Insufficient) | None) {
                    pushw(
                        &mut out,
                        vec![
                            lab("left on table"),
                            ("calibrating — no estimate yet".into(), Seg::Bold),
                        ],
                    );
                } else {
                    // Measured/calibrated tier with no billing evidence in the
                    // period (or a zero-length measured span).
                    pushw(
                        &mut out,
                        vec![
                            lab("left on table"),
                            ("unmeasured".into(), Seg::Bold),
                            (gap("unmeasured"), Seg::Plain),
                            (measured_ann(r, requested), Seg::Dim),
                        ],
                    );
                }
            }
        }

        // ── basis ──
        match &r.basis {
            None => pushw(
                &mut out,
                vec![
                    lab("basis"),
                    (
                        "no utilization history yet — `merma install` starts collection".into(),
                        Seg::Plain,
                    ),
                ],
            ),
            Some(b) => {
                let word = regime_word(b.regime_minutes);
                match b.tier {
                    est::Tier::Calibrated => {
                        let band = b.band_usd_per_window.as_ref().expect("calibrated has band");
                        let display_lo = band.lo.max(b.bound_usd_per_window.unwrap_or(0.0));
                        pushw(
                            &mut out,
                            vec![
                                lab("basis"),
                                (
                                    format!(
                                        "{} · {} {} ({}) instances · window worth {}–{} (median {})",
                                        tier_name(b.tier),
                                        b.n_qualifying,
                                        word,
                                        b.window_id,
                                        usd(display_lo),
                                        usd(band.hi),
                                        usd(band.med)
                                    ),
                                    Seg::Plain,
                                ),
                            ],
                        );
                        if let (Some(q), Some(shift)) = (&b.quantization, b.max_loo_shift) {
                            pushw(
                                &mut out,
                                vec![
                                    ind_seg(),
                                    (
                                        format!(
                                            "quantization ≤ +{:.1}% (min growth {:.0} pts) · \
                                             max single-instance pull {:.1}%",
                                            q.max_rel_widening * 100.0,
                                            q.min_dpct,
                                            shift * 100.0
                                        ),
                                        Seg::Dim,
                                    ),
                                ],
                            );
                        }
                        let mut parts: Vec<String> = Vec::new();
                        if let Some(f) = b.floor_usd_per_window {
                            parts.push(format!("floor {}/window not binding", usd(f)));
                        }
                        if let (Some(w), Some(c)) =
                            (b.window_worth_multiple, r.plan.window_cost_usd)
                        {
                            parts.push(format!("window worth ×{w:.1} its {} cost", usd(c)));
                        }
                        if let Some(x) = excluded_phrase(&b.excluded) {
                            parts.push(x);
                        }
                        if !parts.is_empty() {
                            pushw(&mut out, vec![ind_seg(), (parts.join(" · "), Seg::Dim)]);
                        }
                    }
                    est::Tier::Measured => {
                        let bound = b.bound_usd_per_window.unwrap_or(0.0);
                        let bound_desc = match &b.bound_source {
                            Some(est::BoundSource::Week { start_ts }) => format!(
                                "best week {}/window (week of {})",
                                usd(bound),
                                date(*start_ts)
                            ),
                            Some(est::BoundSource::Instance { first_ts }) => format!(
                                "100% window {} (instance of {})",
                                usd(bound),
                                date(*first_ts)
                            ),
                            None => "a window reached 100%".to_string(),
                        };
                        let vs_band = match &b.band_usd_per_window {
                            Some(band) => format!(
                                " ≥ calibrated median {} ({} instances)",
                                usd(band.med),
                                b.n_qualifying
                            ),
                            None => String::new(),
                        };
                        pushw(
                            &mut out,
                            vec![
                                lab("basis"),
                                (
                                    format!("{} · {bound_desc}{vs_band}", tier_name(b.tier)),
                                    Seg::Plain,
                                ),
                            ],
                        );
                        let mut parts = vec!["measurement supersedes the band".to_string()];
                        if let (Some(w), Some(c)) =
                            (b.window_worth_multiple, r.plan.window_cost_usd)
                        {
                            parts.push(format!("window worth ≥ ×{w:.1} its {} cost", usd(c)));
                        }
                        parts.push("the bound never claims stability".into());
                        if let Some(x) = excluded_phrase(&b.excluded) {
                            parts.push(x);
                        }
                        pushw(&mut out, vec![ind_seg(), (parts.join(" · "), Seg::Dim)]);
                    }
                    est::Tier::Insufficient => {
                        let ins = b.insufficient.as_ref();
                        let mut line1 = match ins {
                            Some(i) if i.needed_instances > 0 => format!(
                                "{} · {} of {} qualifying {} instances (needs {} more with \
                                 ≥{:.0} pt growth)",
                                tier_name(b.tier),
                                b.n_qualifying,
                                est::N_MIN_CALIBRATED,
                                b.window_id,
                                i.needed_instances,
                                est::MIN_DPCT_FOR_ESTIMATE
                            ),
                            _ => format!(
                                "{} · {} {} instances, but one controls the number",
                                tier_name(b.tier),
                                b.n_qualifying,
                                b.window_id
                            ),
                        };
                        if let Some(x) = excluded_phrase(&b.excluded) {
                            line1.push_str(&format!(" · {x}"));
                        }
                        pushw(&mut out, vec![lab("basis"), (line1, Seg::Plain)]);
                        match ins {
                            Some(i) if i.unlocks_at.is_some() => {
                                // The structural date is max(first snapshot +
                                // 4 regimes, now + 1 regime). When the clamp
                                // set it, saying "4 full windows from the
                                // first snapshot" would print arithmetic that
                                // does NOT reproduce the date — name the
                                // clamped basis instead.
                                let clamped = i.unlocks_at == Some(now + b.regime_minutes * 60);
                                let path_note = match i.path {
                                    Some("rate") => "at your current qualifying rate",
                                    _ if clamped => {
                                        "one full window from now — the next qualifying \
                                         instance cannot close sooner"
                                    }
                                    _ => "4 full windows from the first snapshot",
                                };
                                pushw(
                                    &mut out,
                                    vec![
                                        ind_seg(),
                                        (
                                            format!(
                                                "calibrated estimate unlocks ~{} at the earliest \
                                                 ({path_note})",
                                                date(i.unlocks_at.unwrap_or(0))
                                            ),
                                            Seg::Dim,
                                        ),
                                    ],
                                );
                            }
                            Some(i) if i.demoted_by.is_some() => {
                                pushw(&mut out, vec![ind_seg(), (i.missing.clone(), Seg::Dim)]);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // ── open now ──
        let op_word = r
            .basis
            .as_ref()
            .map(|b| regime_word(b.regime_minutes))
            .unwrap_or_else(|| "operative window".into());
        for (i, w) in r.open_windows.iter().enumerate() {
            let head = if i == 0 { lab("open now") } else { ind_seg() };
            let mut fields = vec![format!("{:<10} {:.0}% used", w.window_id, w.used_pct)];
            if w.operative {
                match &w.gap_usd {
                    Some(est::Dollars::Band(g)) => {
                        let mut v = format!("≈ {} – {} left", usd(g.lo), usd(g.hi));
                        if g.approx {
                            v.push_str(" ~");
                        }
                        fields.push(v);
                    }
                    Some(est::Dollars::Floor { low, approx }) => {
                        let mut v = format!("≥ {} left", usd(*low));
                        if let Some(c) = &w.certified {
                            v.push_str(&format!(
                                " ({} best − {} used this window)",
                                usd(c.bound),
                                usd(c.extracted_in_window_usd)
                            ));
                        }
                        if *approx {
                            v.push_str(" ~");
                        }
                        fields.push(v);
                    }
                    None => fields.push("gap unmeasured".into()),
                }
            }
            if let Some(ra) = w.resets_at {
                fields.push(format!("resets in {}", dur_short((ra - now).max(0))));
            }
            if let Some(c) = &w.context {
                fields.push(format!(
                    "context — median peak {:.0}%, {} of {} hit 100% ({op_word} carries \
                     the dollars)",
                    c.median_peak_pct, c.hit_100, c.n
                ));
            }
            pushw(&mut out, vec![head, (fields.join(" · "), Seg::Plain)]);
        }

        // ── decision ──
        let d = &r.decision;
        let line1 = match (d.return_multiple, d.extracted_usd, d.plan_cost_measured_usd) {
            (Some(m), Some(ex), Some(plan)) => {
                let mult = if d.approx {
                    format!("~×{m:.2}")
                } else {
                    format!("×{m:.2}")
                };
                let span = if r.measured.coverage_frac < 0.995 {
                    format!("{} measured", measured_days(r))
                } else {
                    measured_days(r)
                };
                let verdict = match d.verdict {
                    // The verdict word IS the threshold's name — don't print
                    // "keep (keep ≥ …)"; the constant alone suffices here.
                    est::Verdict::Keep => format!("keep (≥ ×{:.1})", d.threshold_keep),
                    est::Verdict::Downgrade => {
                        format!(
                            "downgrade candidate (below keep ≥ ×{:.1})",
                            d.threshold_keep
                        )
                    }
                    est::Verdict::Unknown => {
                        format!("unknown — {}", d.reason.as_deref().unwrap_or("undecided"))
                    }
                };
                format!(
                    "plan returned {mult} ({} extracted / {} plan cost · {span}) — {verdict}",
                    usd(ex),
                    usd(plan)
                )
            }
            _ => format!(
                "plan return unknown — {}",
                d.reason.as_deref().unwrap_or("operands unmeasured")
            ),
        };
        pushw(&mut out, vec![lab("decision"), (line1, Seg::Plain)]);
        if let Some(line2) = decision_line2(r, now) {
            pushw(&mut out, vec![ind_seg(), (line2, Seg::Dim)]);
        }

        for n in &r.notes {
            // Only the ⚠ glyph carries yellow; the sentence recedes to dim so
            // diagnostics sit below the values in the brightness hierarchy.
            push(
                &mut out,
                format!("  {} {}", warn_glyph(), crate::theme::dim(n)),
            );
        }
    }
    out
}

/// Decision line 2: gap capture (CALIBRATED), or the uncalibrated sentence
/// (INSUFFICIENT). MEASURED adds nothing.
fn decision_line2(r: &ProviderReport, now: i64) -> Option<String> {
    let basis = r.basis.as_ref()?;
    match basis.tier {
        est::Tier::Insufficient => {
            Some("capture pace uncalibrated — unlocks with calibration".into())
        }
        est::Tier::Measured => None,
        est::Tier::Calibrated => {
            let w = r.open_windows.iter().find(|w| w.operative)?;
            let cap = w.capture.as_ref()?;
            let (unit, div) = if cap.remaining_secs < 86_400 {
                ("h", 24.0)
            } else {
                ("day", 1.0)
            };
            let mut line = format!(
                "capturing {} needs {}–{}/{unit} for {}",
                w.window_id,
                usd(cap.needed_per_day.lo / div),
                usd(cap.needed_per_day.hi / div),
                dur_short((w.resets_at.unwrap_or(now) - now).max(0))
            );
            match &cap.pace {
                est::PaceComparison::TooYoung => {
                    line.push_str(" · window too young for pace comparison");
                }
                est::PaceComparison::Uncalibrated { .. } => {
                    line.push_str(" · capture pace uncalibrated");
                }
                est::PaceComparison::Calibrated {
                    checkpoint_pct,
                    median_used_at_checkpoint,
                    ..
                } => {
                    line.push_str(&format!(
                        " · at the {checkpoint_pct}% checkpoint your median window sat at \
                         {median_used_at_checkpoint:.0}% — you are at {:.0}%",
                        w.used_pct
                    ));
                }
            }
            Some(line)
        }
    }
}

// ── status ──

/// One line, entries joined by ` | `, fields by ` · `, whole dollars.
pub fn render_status(reports: &[ProviderReport], now: i64) -> String {
    let mut entries = Vec::new();
    for r in reports {
        for w in &r.open_windows {
            let mut fields = vec![format!("{} {} {:.0}%", r.provider, w.window_id, w.used_pct)];
            if w.operative {
                match &w.gap_usd {
                    Some(est::Dollars::Band(g)) => {
                        fields.push(format!("≈{}–{} left", usd_whole(g.lo), usd_whole(g.hi)));
                    }
                    Some(est::Dollars::Floor { low, .. }) => {
                        fields.push(format!("≥{} left", usd_whole(*low)));
                    }
                    None => fields.push("gap unmeasured".into()),
                }
            }
            if let Some(ra) = w.resets_at {
                fields.push(format!("resets {}", dur_short((ra - now).max(0))));
            }
            entries.push(fields.join(" · "));
        }
    }
    entries.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::waste::{MeasuredSpan, PlanInfo};

    const WEEK: i64 = 604_800;
    /// Shared fixture clock: 2026-08-09T13:32:25Z.
    const NOW: i64 = 1_786_599_145;
    const PERIOD: i64 = 2_592_000; // 30d

    /// Well-formed candidate (mirrors the estimator's worked Example A).
    fn mk_cand(first_ts: i64, step: i64, pcts: &[f64], usd: f64) -> est::Candidate {
        let points: Vec<(i64, f64)> = pcts
            .iter()
            .enumerate()
            .map(|(i, &p)| (first_ts + i as i64 * step, p))
            .collect();
        let peak = pcts.iter().copied().fold(f64::MIN, f64::max);
        let peak_idx = pcts.iter().position(|&p| p >= peak).unwrap();
        let event_ts: Vec<i64> = (1..=peak_idx).map(|i| first_ts + i as i64 * step).collect();
        est::Candidate {
            peak_ts: points[peak_idx].0,
            points,
            attributed_usd: usd,
            unpriced: false,
            approx: false,
            event_ts,
        }
    }

    /// Worked Example A (Codex, CALIBRATED, 5 weekly instances), anchored so
    /// the open window is live at NOW.
    fn fixture_codex() -> ProviderReport {
        let base = NOW - 10 * WEEK;
        let cands = vec![
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
        let admission = est::admit(&cands, WEEK);
        let input = est::EstimatorInput {
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
            measured_secs: PERIOD,
            certified_start_ts: base,
            certified_secs: PERIOD,
            certified_extracted_usd: 41.80,
            have_billing: true,
            plan_monthly_usd: 20.0,
            now: NOW,
            first_snapshot_ts: Some(base),
            complete_windows_observed: 5,
            open: Some(est::OpenWindowInput {
                used_pct: 27.0,
                resets_at: Some(NOW + 385_200), // 4d 11h
                first_ts: NOW - 219_600,        // 61 h old → 25% checkpoint
                extracted_in_window_usd: 5.0,
            }),
            others: Vec::new(),
        };
        let e = est::assemble(input);
        let plan_cost = 20.0 * PERIOD as f64 / est::SECS_PER_MONTH;
        ProviderReport {
            provider: "codex".into(),
            plan: PlanInfo {
                label: "plus".into(),
                monthly_usd: 20.0,
                approx: false,
                window_cost_usd: e.window_cost_usd,
            },
            measured: MeasuredSpan {
                t0: NOW - PERIOD,
                t1: NOW,
                union_secs: PERIOD,
                coverage_frac: 1.0,
            },
            extracted_usd: 41.80,
            left_on_table_usd: e.left_on_table_usd,
            basis: Some(e.basis),
            open_windows: e.open_windows,
            decision: est::decide(Some((41.80, 41.80)), Some(plan_cost), false),
            notes: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Worked Example B (Claude cold start, INSUFFICIENT, structural unlock
    /// pinned to 1788664105 = 2026-09-06 — the spec prose's "≈ 2026-09-05"
    /// day label is off by one for its own pinned ts).
    fn fixture_claude() -> ProviderReport {
        let first = 1_786_244_905i64; // 2026-08-08T03:08:25Z
        let measured = NOW - first; // 354 240 s = 4.1 d
        let q = est::Qualified {
            first_ts: first,
            last_ts: first + 300_000,
            points: vec![(first, 0.0), (first + 300_000, 36.0)],
            dpct: 36.0,
            rate: 100.0,
            rate_lo: 100.0 * 36.0 / 37.0,
            rate_hi: 100.0 * 36.0 / 35.0,
            approx: false,
        };
        let input = est::EstimatorInput {
            window_id: "seven_day".into(),
            regime_minutes: 10_080,
            admission: est::Admission {
                qualifying: vec![q],
                excluded: Vec::new(),
                fractional_pct: false,
            },
            any_hundred_pct: false,
            best_hundred_usd: 0.0,
            best_hundred_first_ts: None,
            floor_window_usd: 0.0,
            floor_week_start: None,
            bound_approx: false,
            extracted_usd: 138.40,
            measured_secs: measured,
            certified_start_ts: first,
            certified_secs: measured,
            certified_extracted_usd: 138.40,
            have_billing: true,
            plan_monthly_usd: 200.0,
            now: NOW,
            first_snapshot_ts: Some(first),
            complete_windows_observed: 0,
            open: Some(est::OpenWindowInput {
                used_pct: 36.0,
                resets_at: Some(NOW + 248_400), // 2d 21h
                first_ts: first,
                extracted_in_window_usd: 0.0,
            }),
            others: vec![est::OtherWindowInput {
                window_id: "five_hour".into(),
                used_pct: 62.0,
                resets_at: Some(NOW + 7_800), // 2h 10m
                context: Some(est::WindowContext {
                    median_peak_pct: 41.0,
                    n: 3,
                    hit_100: 0,
                }),
            }],
        };
        let e = est::assemble(input);
        let plan_cost = 200.0 * measured as f64 / est::SECS_PER_MONTH;
        ProviderReport {
            provider: "claude".into(),
            plan: PlanInfo {
                label: "max_20x".into(),
                monthly_usd: 200.0,
                approx: false,
                window_cost_usd: e.window_cost_usd,
            },
            measured: MeasuredSpan {
                t0: first,
                t1: NOW,
                union_secs: measured,
                coverage_frac: measured as f64 / PERIOD as f64,
            },
            extracted_usd: 138.40,
            left_on_table_usd: e.left_on_table_usd,
            basis: Some(e.basis),
            open_windows: e.open_windows,
            decision: est::decide(Some((138.40, 138.40)), Some(plan_cost), false),
            notes: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn fixture_reports() -> Vec<ProviderReport> {
        vec![fixture_codex(), fixture_claude()]
    }

    fn fixture_text() -> String {
        render_brief(
            &fixture_reports(),
            "last 30 days",
            "30d",
            NOW,
            BRIEF_WRAP_WIDTH,
        )
    }

    /// Unfolded render for LOGICAL-content assertions (substrings that may
    /// legitimately straddle a wrap break at the golden width). Physical
    /// layout at BRIEF_WRAP_WIDTH is pinned by `wrap_layout` / `line_budget`.
    fn fixture_text_wide() -> String {
        render_brief(&fixture_reports(), "last 30 days", "30d", NOW, usize::MAX)
    }

    #[test]
    fn parse_period_cases() {
        let now = 1_786_500_000i64;
        let (t0, t1, label) = parse_period("7d", now).unwrap();
        assert_eq!((t0, t1), (now - 7 * 86_400, now));
        assert_eq!(label, "last 7 days");
        assert_eq!(parse_period("30d", now).unwrap().0, now - 30 * 86_400);
        assert_eq!(parse_period("90d", now).unwrap().0, now - 90 * 86_400);
        assert_eq!(parse_period("365d", now).unwrap().0, now - 365 * 86_400);
        assert_eq!(parse_period("all", now).unwrap().0, 0);
        let (t0, _, label) = parse_period("45d", now).unwrap();
        assert_eq!(t0, now - 45 * 86_400);
        assert_eq!(label, "last 45 days");
        assert!(parse_period("0d", now).is_err());
        assert!(parse_period("-3d", now).is_err());
        assert!(parse_period("fortnight", now).is_err());
    }

    #[test]
    fn providers_flag_cases() {
        assert_eq!(providers_from_flag(None).unwrap(), vec![CODEX, CLAUDE]);
        assert_eq!(providers_from_flag(Some("claude")).unwrap(), vec![CLAUDE]);
        assert_eq!(providers_from_flag(Some("codex")).unwrap(), vec![CODEX]);
        assert_eq!(
            providers_from_flag(Some("both")).unwrap(),
            vec![CODEX, CLAUDE]
        );
        assert!(providers_from_flag(Some("gemini")).is_err());
    }

    /// The 0.2.0 JSON contract, byte-exact against the committed fixture.
    #[test]
    fn json_contract_snapshot() {
        let json = brief_json(fixture_reports(), "30d", NOW - PERIOD, NOW);
        let got = format!("{}\n", serde_json::to_string_pretty(&json).unwrap());
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/brief_0_2_0.json"
        );
        if std::env::var_os("MERMA_BLESS").is_some() {
            std::fs::write(path, &got).expect("write fixture");
        }
        let want = std::fs::read_to_string(path).expect("fixture exists");
        assert_eq!(got, want, "0.2.0 JSON contract drifted from the fixture");
    }

    /// Every dollar figure in the text exists in the JSON with the same value
    /// under the rendering rules (text == format(json_value)).
    #[test]
    fn text_json_equality() {
        let text = fixture_text();
        let json =
            serde_json::to_value(brief_json(fixture_reports(), "30d", NOW - PERIOD, NOW)).unwrap();
        // Collect every number in the JSON, formatted as dollars.
        fn numbers(v: &serde_json::Value, out: &mut Vec<f64>) {
            match v {
                serde_json::Value::Number(n) => out.extend(n.as_f64()),
                serde_json::Value::Array(a) => a.iter().for_each(|x| numbers(x, out)),
                serde_json::Value::Object(o) => o.values().for_each(|x| numbers(x, out)),
                _ => {}
            }
        }
        let mut nums = Vec::new();
        numbers(&json, &mut nums);
        let dollar_set: std::collections::BTreeSet<String> = nums.iter().map(|x| usd(*x)).collect();
        // Scan the plain text for dollar tokens.
        let mut found = 0usize;
        for (i, _) in text.match_indices('$') {
            let tail: String = text[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
                .collect();
            let token = format!("${}", tail.trim_end_matches(['.', ',']));
            assert!(
                dollar_set.contains(&token),
                "text dollar {token} not found in the JSON value set"
            );
            found += 1;
        }
        assert!(found >= 15, "expected many dollar figures, found {found}");
        // Targeted equalities for non-dollar figures.
        assert!(text.contains("93.75% band"), "coverage: {text}");
        assert!(text.contains("×2.12"), "return multiple: {text}");
        assert!(text.contains("×5.4 its $4.60 cost"), "window worth: {text}");
        assert!(text.contains("max single-instance pull 4.0%"));
        assert!(text.contains("quantization ≤ +9.1% (min growth 12 pts)"));
        assert!(text.contains("~2026-09-06 at the earliest"));
        assert!(text.contains("×5.14"), "claude multiple: {text}");
        assert!(text.contains("$26.94 plan cost · 4.1d measured"));
    }

    /// The mockup-derived shapes hold: headline forms, gap lines, decision.
    /// (Logical content — asserted on the unfolded render; the physical
    /// wrap at the golden width is pinned separately.)
    #[test]
    fn brief_shapes() {
        let text = fixture_text_wide();
        assert!(text.contains("merma · last 30 days"));
        assert!(text.contains("codex — plus · $20.00/mo"));
        assert!(text.contains("≈ $46.77 – $187.29"));
        assert!(text.contains("median $65.34"));
        assert!(text.contains("30.0d measured of 30d (100%)"));
        assert!(text.contains("CALIBRATED · 5 weekly (primary) instances"));
        assert!(text.contains("window worth $20.67–$53.45 (median $25.00)"));
        assert!(text.contains("floor $18.60/window not binding"));
        assert!(text.contains("primary    27% used · ≈ $15.09 – $39.02 left · resets in 4d 11h"));
        assert!(text.contains(
            "plan returned ×2.12 ($41.80 extracted / $19.71 plan cost · 30.0d) — keep (≥ ×1.0)"
        ));
        assert!(text.contains("capturing primary needs $3.38–$8.75/day for 4d 11h"));
        assert!(
            text.contains("at the 25% checkpoint your median window sat at 22% — you are at 27%")
        );
        // Claude cold start: no dollars except the measured decision line.
        assert!(text.contains("calibrating — no estimate yet"));
        assert!(text.contains("INSUFFICIENT · 1 of 4 qualifying seven_day instances"));
        assert!(text.contains("needs 3 more with ≥10 pt growth"));
        assert!(text.contains("seven_day  36% used · gap unmeasured · resets in 2d 21h"));
        assert!(text.contains(
            "five_hour  62% used · resets in 2h 10m · context — median peak 41%, 0 of 3 hit \
             100% (weekly carries the dollars)"
        ));
        assert!(text.contains("capture pace uncalibrated — unlocks with calibration"));
    }

    /// INSUFFICIENT with a bound whose measured shortfall computed to ≤ 0 is
    /// NOT the cold-start case: the same data must not read "calibrating — no
    /// estimate yet" at one period and `≥ $X` at another. The render mirrors
    /// the estimator arithmetic (bound × period_windows − extracted ≤ 0).
    #[test]
    fn covered_bound_prints_no_shortfall_not_calibrating() {
        let mut r = fixture_claude();
        assert!(r.left_on_table_usd.is_none());
        let (m_t0, m_secs) = (r.measured.t0, r.measured.union_secs);
        let b = r.basis.as_mut().unwrap();
        assert!(b.period_windows > 0.0);
        // A certified $10/window bound: 10 × 0.586 windows ≤ $138.40 extracted.
        b.bound_usd_per_window = Some(10.0);
        b.certified = Some(est::CertifiedScaling {
            start_ts: m_t0,
            secs: m_secs,
            windows: m_secs as f64 / (7.0 * 86_400.0),
            extracted_usd: 138.40,
        });
        let text = render_brief(&[r], "last 30 days", "30d", NOW, BRIEF_WRAP_WIDTH);
        assert!(text.contains("no measured shortfall"), "{text}");
        assert!(
            text.contains("extraction covered the certified windows' bound"),
            "{text}"
        );
        assert!(!text.contains("calibrating — no estimate yet"), "{text}");
    }

    /// The structural unlock note must reproduce the printed date. When the
    /// clamp (now + one regime) set the date, "4 full windows from the first
    /// snapshot" would be a FALSE provenance — a reader redoing that
    /// arithmetic gets a different day — so the clamped basis prints instead.
    #[test]
    fn clamped_structural_unlock_names_its_basis() {
        let mut r = fixture_claude();
        let ins = r.basis.as_mut().unwrap().insufficient.as_mut().unwrap();
        ins.unlocks_at = Some(NOW + 7 * 86_400); // the clamp arm won
        let text = render_brief(&[r], "last 30 days", "30d", NOW, usize::MAX);
        assert!(
            text.contains(
                "one full window from now — the next qualifying instance cannot close sooner"
            ),
            "{text}"
        );
        assert!(
            !text.contains("4 full windows from the first snapshot"),
            "{text}"
        );
        // The unclamped fixture keeps the snapshot-anchored provenance.
        let text = render_brief(&[fixture_claude()], "last 30 days", "30d", NOW, usize::MAX);
        assert!(text.contains("4 full windows from the first snapshot"));
    }

    /// The `≥` headline names its full certified arithmetic — bound ×
    /// certified (current plan+regime) windows − the extraction inside them —
    /// so every digit is redoable; it never claims the whole measured span.
    #[test]
    fn floor_headline_prints_certified_arithmetic() {
        let mut r = fixture_claude();
        r.left_on_table_usd = Some(est::Dollars::Floor {
            low: 7.0,
            approx: false,
        });
        let b = r.basis.as_mut().unwrap();
        b.bound_usd_per_window = Some(10.0);
        b.certified = Some(est::CertifiedScaling {
            start_ts: 1_786_000_000,
            secs: 604_800,
            windows: 1.0,
            extracted_usd: 3.0,
        });
        let text = render_brief(&[r], "last 30 days", "30d", NOW, usize::MAX);
        assert!(text.contains("≥ $7.00"), "{text}");
        assert!(
            text.contains(
                "$10.00 best window × 1.00 current plan+regime windows (7.0d) − $3.00 \
                 extracted in them"
            ),
            "{text}"
        );
    }

    /// Piped output is byte-plain: no ANSI escapes when styling is off.
    #[test]
    fn piped_plain() {
        let text = fixture_text();
        assert!(!text.contains('\x1b'), "ANSI escapes in piped brief");
        let status = render_status(&fixture_reports(), NOW);
        assert!(!status.contains('\x1b'), "ANSI escapes in status");
    }

    /// Diagnostic lines (⚠ notes, ✗ errors) are exempt from the golden
    /// budget: every warning must reach every read path and may never be
    /// dropped to fit a layout, so the budget governs the DESIGNED surface
    /// (a stanza's content lines), not the diagnostics riding on it.
    fn is_diag(l: &str) -> bool {
        let t = l.trim_start();
        t.starts_with('⚠') || t.starts_with('✗')
    }

    /// Golden line budget at the golden width: ≤ 9 rendered non-diagnostic
    /// lines per stanza (incl. its header), ≤ 22 for the two-provider brief.
    #[test]
    fn line_budget() {
        let text = fixture_text();
        let lines: Vec<&str> = text.lines().filter(|l| !is_diag(l)).collect();
        assert!(lines.len() <= 22, "brief is {} lines:\n{text}", lines.len());
        let mut stanza = 0usize;
        let mut worst = 0usize;
        for l in &lines[1..] {
            if l.is_empty() {
                worst = worst.max(stanza);
                stanza = 0;
            } else {
                stanza += 1;
            }
        }
        worst = worst.max(stanza);
        assert!(worst <= 9, "stanza is {worst} lines:\n{text}");
    }

    /// A stanza carrying required ⚠ notes (the `--period all` shape: 4 notes
    /// on the codex stanza) still meets the CONTENT budget — the notes render
    /// in full and are exempt, not dropped.
    #[test]
    fn line_budget_exempts_required_notes() {
        let mut reports = fixture_reports();
        reports[0].notes = vec![
            "plan history unknown before 2026-03-07 — earlier measured time priced at the \
             earliest known plan (plus)"
                .into(),
            "unpriced usage EXCLUDED from $ totals (no price for era): UNKNOWN (358,288 tok)"
                .into(),
            "non-subscription models excluded from waste math: moonshotai/kimi-k3 (41,218 tok)"
                .into(),
            "$16.18 of the API-equivalent total uses era-approximate prices (see prices.toml \
             `approx` entries)"
                .into(),
        ];
        let text = render_brief(&reports, "all history", "all", NOW, BRIEF_WRAP_WIDTH);
        for n in &reports[0].notes {
            assert!(text.contains(n), "note dropped: {n}");
        }
        let mut stanza = 0usize;
        let mut worst_content = 0usize;
        let mut worst_raw = 0usize;
        let mut raw = 0usize;
        for l in text.lines().skip(1) {
            if l.is_empty() {
                worst_content = worst_content.max(stanza);
                worst_raw = worst_raw.max(raw);
                stanza = 0;
                raw = 0;
            } else {
                raw += 1;
                if !is_diag(l) {
                    stanza += 1;
                }
            }
        }
        worst_content = worst_content.max(stanza);
        worst_raw = worst_raw.max(raw);
        assert!(worst_content <= 9, "content lines {worst_content}:\n{text}");
        assert!(worst_raw > 9, "fixture should exceed the budget with notes");
    }

    /// The wrap helper itself: interior space runs (label gutter, value pad)
    /// preserved, breaks at word boundaries only, continuation indent, the
    /// break-point space run dropped, trailing spaces stripped, words that
    /// span segment boundaries never split.
    #[test]
    fn wrap_mechanics() {
        let seg = |t: &str, s: Seg| (t.to_string(), s);
        let mut out = String::new();
        push_folded(
            &mut out,
            &[seg("  label   ", Seg::Dim), seg("aaa bbb ccc", Seg::Plain)],
            14,
            4,
        );
        assert_eq!(out, "  label   aaa\n    bbb ccc\n");
        // A word assembled across segment boundaries is unbreakable; a word
        // wider than the width overflows whole — never split mid-token.
        let mut out = String::new();
        push_folded(
            &mut out,
            &[seg("foo", Seg::Bold), seg("bar baz", Seg::Plain)],
            5,
            0,
        );
        assert_eq!(out, "foobar\nbaz\n");
        // A pad landing exactly on a break never survives as trailing space.
        let mut out = String::new();
        push_folded(
            &mut out,
            &[
                seg("ab", Seg::Plain),
                seg("     ", Seg::Plain),
                seg("cd", Seg::Plain),
            ],
            4,
            0,
        );
        assert_eq!(out, "ab\ncd\n");
    }

    /// An operator word never ends a line: it binds to its right operand, so
    /// the break lands BEFORE the operator, never after it.
    #[test]
    fn fold_never_orphans_an_operator() {
        let seg = |t: &str, s: Seg| (t.to_string(), s);
        for op in ["−", "×", "≥", "≈", "–", "-"] {
            let mut out = String::new();
            push_folded(
                &mut out,
                &[seg(&format!("aaaa bbbb {op} $9.99"), Seg::Plain)],
                12,
                0,
            );
            assert_eq!(out, format!("aaaa bbbb\n{op} $9.99\n"), "op {op:?}");
        }
        // Multi-space runs (the value-column pad) never fuse: the operator
        // bind requires exactly one space between operator and operand.
        let mut out = String::new();
        push_folded(&mut out, &[seg("aaaa −   $9.99", Seg::Plain)], 7, 0);
        assert_eq!(out, "aaaa −\n$9.99\n");
    }

    /// Physical layout at the golden width: every rendered line fits in
    /// BRIEF_WRAP_WIDTH print columns, folded continuations indent to the
    /// value column, and no line carries trailing spaces.
    #[test]
    fn wrap_layout() {
        let text = fixture_text();
        for l in text.lines() {
            assert!(
                l.chars().count() <= BRIEF_WRAP_WIDTH,
                "{} cols: {l}",
                l.chars().count()
            );
            assert_eq!(l, l.trim_end(), "trailing space: {l:?}");
        }
        // The long decision sentence wraps into the value column whole-word.
        assert!(
            text.contains(
                "at the 25% checkpoint your median window sat at\n                   \
                 22% — you are at 27%"
            ),
            "{text}"
        );
        assert!(
            text.contains("(weekly carries\n                   the dollars)"),
            "{text}"
        );
    }

    /// The §2.8 status line, including `gap unmeasured` and whole dollars.
    #[test]
    fn status_line_format() {
        let status = render_status(&fixture_reports(), NOW);
        assert_eq!(
            status,
            "codex primary 27% · ≈$15–$39 left · resets 4d 11h | \
             claude seven_day 36% · gap unmeasured · resets 2d 21h | \
             claude five_hour 62% · resets 2h 10m"
        );
    }
}
