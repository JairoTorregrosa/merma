//! Live Ratatui dashboard.
//!
//! Design system (one system across all tabs):
//! - Structure is whitespace and alignment; hierarchy is brightness
//!   (bold / default / DarkGray). No borders, no background fills, no
//!   `Modifier::DIM` — body text uses the terminal's own foreground so the
//!   dashboard reads on dark and light themes alike.
//! - One teal accent is graphic ink only (meter fills, the burn-up curve,
//!   heatmap ramp); green/yellow/red are reserved for true status glyphs.
//! - The empty band between the burn-up curve and the 100% ceiling IS the
//!   waste; the gap figure in the title anchors it with dollars.
//! - Heatmap cells encode magnitude twice — glyph density (▁…█) and a
//!   pale→vivid teal ramp — so identity survives colorblindness and remaps.

use crate::collectors::ScanSummary;
use crate::config::Cfg;
use crate::engine::waste::{build_provider_report, CacheMode, ProviderReport};
use crate::engine::windows::reconstruct;
use crate::pricing::PriceBook;
use crate::report::{dur_short, usd};
use crate::store::{Store, CLAUDE, CODEX};
use crate::wrapped::SPARK;
use anyhow::Result;
use chrono::Datelike;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Chart, Dataset, GraphType, Paragraph};
use ratatui::Frame;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// The overview's "THIS WEEK · IN API DOLLARS" label is tied to this 7-day
/// period — change one, change both.
const DASH_PERIOD_SECS: i64 = 7 * 86_400;
const RESCAN_EVERY: Duration = Duration::from_secs(30);
const METER_WIDTH: usize = 24;

// ---------------------------------------------------------------------------
// Design tokens (private to the dashboard: this file owns exactly one surface).

fn truecolor() -> bool {
    static TC: OnceLock<bool> = OnceLock::new();
    *TC.get_or_init(|| {
        std::env::var("COLORTERM")
            .map(|v| v.contains("truecolor") || v.contains("24bit"))
            .unwrap_or(false)
    })
}

/// The single accent: graphic ink only, never text.
fn accent() -> Color {
    if truecolor() {
        Color::Rgb(0, 150, 160)
    } else {
        Color::Cyan
    }
}

/// Sequential magnitude ramp, pale sage-teal → vivid teal. The axis is
/// pale→vivid (not light→dark) so it does not invert on dark terminals.
fn ramp(v: f64) -> Color {
    if !truecolor() {
        return Color::Cyan;
    }
    let v = v.clamp(0.0, 1.0);
    let lerp = |a: f64, b: f64| (a + (b - a) * v).round() as u8;
    Color::Rgb(lerp(158.0, 0.0), lerp(204.0, 150.0), lerp(201.0, 160.0))
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Half-cell-resolution meter: returns (accent fill `━…╸`, dim rest `─…`).
/// The label always lives OUTSIDE the meter — text never overlaps ink.
fn meter(ratio: f64, width: usize) -> (String, String) {
    let units = (ratio.clamp(0.0, 1.0) * width as f64 * 2.0).round() as usize;
    let full = (units / 2).min(width);
    let half = if units % 2 == 1 && full < width { 1 } else { 0 };
    let mut fill = "━".repeat(full);
    if half == 1 {
        fill.push('╸');
    }
    (fill, "─".repeat(width - full - half))
}

/// Word-boundary wrap that slices the original string (verbatim honesty:
/// only the break spaces are consumed). Falls back to a hard break when a
/// single run exceeds the width.
fn wrap_text(s: &str, first_w: usize, cont_w: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    let mut w = first_w.max(8);
    while rest.chars().count() > w {
        let mut break_at = None;
        for (ci, (bi, ch)) in rest.char_indices().enumerate() {
            if ci > w {
                break;
            }
            if ch == ' ' && ci > 0 {
                break_at = Some(bi);
            }
        }
        match break_at {
            Some(bi) => {
                out.push(rest[..bi].to_string());
                rest = &rest[bi + 1..];
            }
            None => {
                let bi = rest
                    .char_indices()
                    .nth(w)
                    .map(|(b, _)| b)
                    .unwrap_or(rest.len());
                out.push(rest[..bi].to_string());
                rest = &rest[bi..];
            }
        }
        w = cont_w.max(8);
    }
    out.push(rest.to_string());
    out
}

/// One density cell of the shared 8-step ramp (`wrapped::SPARK`).
fn spark_cell(v: f64) -> char {
    SPARK[(v.clamp(0.0, 1.0) * 7.0).round() as usize]
}

/// Dim month labels aligned under the 52 heatmap cells: a 3-letter lowercase
/// label at every week whose start month differs from the previous week's,
/// skipping any label that would leave <1 space gap from the previous one.
fn month_ticks(weeks: &[i64]) -> String {
    const M: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let month = |ts: i64| {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|d| d.month0() as usize)
            .unwrap_or(0)
    };
    let mut buf: Vec<char> = vec![' '; weeks.len()];
    let mut last_end: Option<usize> = None;
    for i in 0..weeks.len() {
        let m = month(weeks[i]);
        if i > 0 && m == month(weeks[i - 1]) {
            continue;
        }
        // A label renders whole or not at all: a clipped tail letter reads
        // as a bug on the time axis. The first week always gets its label.
        if i + 3 > buf.len() || last_end.is_some_and(|e| i < e + 1) {
            continue;
        }
        for (j, ch) in M[m].chars().enumerate() {
            buf[i + j] = ch;
        }
        last_end = Some(i + 3);
    }
    buf.into_iter().collect::<String>().trim_end().to_string()
}

/// Center lines in a region — both axes, no box: voids stay empty terminal.
fn center(f: &mut Frame, area: Rect, lines: Vec<Line>) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let n = (lines.len() as u16).min(area.height);
    let rect = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(n) / 2,
        width: area.width,
        height: n,
    };
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), rect);
}

// ---------------------------------------------------------------------------

struct App {
    store: Store,
    cfg: Cfg,
    book: PriceBook,
    mode: CacheMode,
    tab: usize,
    reports: Vec<ProviderReport>,
    util_heat: Vec<(String, Vec<(i64, f64)>)>, // provider → weekly peak pct
    status_line: String,
    last_scan: Instant,
}

impl App {
    fn rebuild(&mut self) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let t0 = now - DASH_PERIOD_SECS;
        self.reports = [CODEX, CLAUDE]
            .iter()
            .map(|p| {
                build_provider_report(&self.store, &self.book, &self.cfg, p, t0, now, self.mode)
            })
            .collect::<Result<Vec<_>>>()?;
        self.util_heat = [CODEX, CLAUDE]
            .iter()
            .map(|p| Ok((p.to_string(), weekly_peaks(&self.store, p)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(())
    }
}

/// Weekly peak utilization of the billing window across all history.
fn weekly_peaks(store: &Store, provider: &str) -> Result<Vec<(i64, f64)>> {
    let now = chrono::Utc::now().timestamp();
    let mut best: std::collections::BTreeMap<i64, f64> = Default::default();
    for wid in store.window_ids(provider)? {
        if wid == "seven_day_opus" || wid == "seven_day_sonnet" {
            continue;
        }
        let snaps = store.snapshots_between(provider, Some(&wid), 0, now)?;
        for inst in reconstruct(snaps) {
            let week = (inst.last_ts() / 86_400 + 3).div_euclid(7) * 7 * 86_400 - 3 * 86_400;
            let e = best.entry(week).or_default();
            *e = e.max(inst.peak_pct());
        }
    }
    Ok(best.into_iter().collect())
}

fn scan_status(sum: &ScanSummary) -> Option<String> {
    let mut parts: Vec<String> = sum.warnings.iter().map(|w| format!("⚠ {w}")).collect();
    if sum.parse_errors > 0 {
        parts.push(format!("⚠ {} unparseable lines skipped", sum.parse_errors));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

pub fn run_dashboard(store: Store, cfg: Cfg, book: PriceBook, scan: ScanSummary) -> Result<()> {
    // Background pollers write through ONE long-lived connection (WAL). Poll
    // failures are surfaced in the footer — a dashboard silently showing stale
    // data would violate the loud-failure policy.
    let stop = Arc::new(AtomicBool::new(false));
    let poll_status: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    {
        let stop = stop.clone();
        let poll_status = poll_status.clone();
        let db = cfg.db_path();
        let cfg2 = Cfg {
            file: cfg.file.clone(),
            ..cfg.clone()
        };
        std::thread::spawn(move || {
            let wham_every = cfg2.file.wham_poll_secs.unwrap_or(120).max(60);
            let oauth_every = cfg2.file.oauth_poll_secs.unwrap_or(180).max(120);
            let mut last_wham = Instant::now() - Duration::from_secs(wham_every);
            let mut last_oauth = Instant::now() - Duration::from_secs(oauth_every);
            let mut conn: Option<Store> = None;
            let set_status = |msg: Option<String>| {
                if let Ok(mut g) = poll_status.lock() {
                    *g = msg;
                }
            };
            while !stop.load(Ordering::Relaxed) {
                let wham_due = last_wham.elapsed().as_secs() >= wham_every;
                let oauth_due = cfg2.file.claude_oauth_enabled != Some(false)
                    && last_oauth.elapsed().as_secs() >= oauth_every;
                if wham_due || oauth_due {
                    if conn.is_none() {
                        match Store::open(&db) {
                            Ok(s) => conn = Some(s),
                            Err(e) => set_status(Some(format!("poll db error: {e:#}"))),
                        }
                    }
                    if let Some(s) = conn.as_mut() {
                        let mut errs: Vec<String> = Vec::new();
                        if wham_due {
                            if let Err(e) = crate::collectors::codex_live::poll(s, &cfg2) {
                                errs.push(format!("wham: {e:#}"));
                            }
                            last_wham = Instant::now();
                        }
                        if oauth_due {
                            if let Err(e) = crate::collectors::claude_oauth::poll(s, &cfg2) {
                                errs.push(format!("oauth: {e:#}"));
                            }
                            last_oauth = Instant::now();
                        }
                        set_status((!errs.is_empty()).then(|| errs.join(" · ")));
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }

    let mut terminal = ratatui::init();
    let mut app = App {
        store,
        cfg,
        book,
        mode: CacheMode::Full,
        tab: 0,
        reports: Vec::new(),
        util_heat: Vec::new(),
        status_line: scan_status(&scan).unwrap_or_default(),
        last_scan: Instant::now(),
    };
    let mut last_poll_err: Option<String> = None;
    let res = (|| -> Result<()> {
        app.rebuild()?;
        loop {
            if let Ok(g) = poll_status.lock() {
                if *g != last_poll_err {
                    // Poll state changed: show the new error, or clear a
                    // recovered one instead of displaying it forever.
                    app.status_line = g.as_ref().map(|e| format!("⚠ {e}")).unwrap_or_default();
                    last_poll_err = g.clone();
                }
            }
            terminal.draw(|f| draw(f, &app))?;
            if event::poll(Duration::from_millis(1000))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Tab | KeyCode::Right => app.tab = (app.tab + 1) % 4,
                        KeyCode::BackTab | KeyCode::Left => app.tab = (app.tab + 3) % 4,
                        KeyCode::Char(c @ '1'..='4') => app.tab = (c as usize) - ('1' as usize),
                        KeyCode::Char('c') => {
                            app.mode = match app.mode {
                                CacheMode::Full => CacheMode::OutputOnly,
                                CacheMode::OutputOnly => CacheMode::Full,
                            };
                            app.rebuild()?;
                        }
                        KeyCode::Char('r') => {
                            let sum = crate::scan::run_scan(&mut app.store, &app.cfg)?;
                            app.status_line = match scan_status(&sum) {
                                Some(s) => s,
                                None => format!("rescanned: +{} events", sum.events_inserted),
                            };
                            app.rebuild()?;
                        }
                        _ => {}
                    }
                }
            }
            if app.last_scan.elapsed() >= RESCAN_EVERY {
                match crate::scan::run_scan(&mut app.store, &app.cfg) {
                    Ok(sum) => {
                        if let Some(s) = scan_status(&sum) {
                            app.status_line = s;
                        }
                    }
                    Err(e) => app.status_line = format!("⚠ rescan failed: {e:#}"),
                }
                app.last_scan = Instant::now();
                app.rebuild()?;
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    ratatui::restore();
    res
}

fn draw(f: &mut Frame, app: &App) {
    // Blank rows fence the body off from the tab bar and the footer: in a
    // borderless system whose only structure is whitespace, both edges of
    // the screen need breathing room.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());
    draw_tab_bar(f, rows[0], app.tab);
    match app.tab {
        0 => draw_overview(f, rows[2], app),
        1 => draw_provider(f, rows[2], app, CODEX),
        2 => draw_provider(f, rows[2], app, CLAUDE),
        _ => draw_heatmap(f, rows[2], app),
    }
    draw_footer(f, rows[4], app);
}

/// Hand-rolled tab bar: dim wordmark, bold-vs-dim titles, no dividers.
/// Fixed positions — spatial memory is the navigation.
fn draw_tab_bar(f: &mut Frame, area: Rect, active: usize) {
    let titles = ["overview", "codex", "claude", "heatmap"];
    let mut spans = vec![Span::styled("  merma", dim())];
    for (i, t) in titles.iter().enumerate() {
        spans.push(Span::raw("   "));
        spans.push(if i == active {
            Span::styled(*t, bold())
        } else {
            Span::styled(*t, dim())
        });
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Footer status area: yellow glyphs, default-fg text, dim ` · ` joiners;
/// a quiet dim line for the clean-rescan confirmation.
fn status_spans(s: &str) -> Vec<Span<'static>> {
    if s.starts_with("rescanned:") {
        return vec![Span::styled(s.to_string(), dim())];
    }
    let mut out = Vec::new();
    for (i, part) in s.split(" · ").enumerate() {
        if i > 0 {
            out.push(Span::styled(" · ", dim()));
        }
        if let Some(rest) = part.strip_prefix("⚠ ") {
            out.push(Span::styled("⚠ ", Style::new().fg(Color::Yellow)));
            out.push(Span::raw(rest.to_string()));
        } else {
            out.push(Span::raw(part.to_string()));
        }
    }
    out
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let mode = match app.mode {
        CacheMode::Full => "full API-equiv",
        CacheMode::OutputOnly => "output-only",
    };
    let mut hints: Vec<(String, String)> = vec![
        ("q".into(), "quit".into()),
        ("tab/1-4".into(), "views".into()),
        ("c".into(), format!("cache [{mode}]")),
        ("r".into(), "rescan".into()),
    ];
    fn hints_width(h: &[(String, String)]) -> usize {
        2 + h
            .iter()
            .map(|(k, l)| k.chars().count() + 1 + l.chars().count())
            .sum::<usize>()
            + 3 * h.len().saturating_sub(1)
    }
    // Status (right cluster) outranks hints: drop hints from the right first
    // (keep at least `q quit`), then truncate the status text with `…`.
    let avail = area.width as usize;
    let mut status = app.status_line.clone();
    let mut status_w = status.chars().count();
    let extra = |sw: usize| if sw > 0 { sw + 4 } else { 0 }; // 2 gap + 2 right margin
    while hints.len() > 1 && hints_width(&hints) + extra(status_w) > avail {
        hints.pop();
    }
    if status_w > 0 && hints_width(&hints) + extra(status_w) > avail {
        status = truncate_chars(&status, avail.saturating_sub(hints_width(&hints) + 4));
        status_w = status.chars().count();
    }
    let mut spans = vec![Span::raw("  ")];
    for (i, (k, l)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim()));
        }
        spans.push(Span::raw(k.clone()));
        spans.push(Span::styled(format!(" {l}"), dim()));
    }
    if status_w > 0 {
        let pad = avail.saturating_sub(hints_width(&hints) + status_w + 2);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(status_spans(&status));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_overview(f: &mut Frame, area: Rect, app: &App) {
    let now = chrono::Utc::now().timestamp();
    let mut lines: Vec<Line> = vec![
        // "THIS WEEK" = DASH_PERIOD_SECS (7 days) — the label and the period
        // constant travel together.
        Line::from(Span::styled("  THIS WEEK · IN API DOLLARS", dim())),
        Line::default(),
    ];
    for r in &app.reports {
        let extracted = match r.cache_mode {
            CacheMode::Full => r.api.full_usd,
            CacheMode::OutputOnly => r.api.output_only_usd,
        };
        let mut spans = vec![
            Span::styled(format!("  {:<9}", r.provider), bold()),
            Span::styled("extracted ", dim()),
            Span::styled(usd(extracted), bold()),
        ];
        if let (Some(pm), Some(pw)) = (&r.period_max_usd, &r.period_waste_usd) {
            spans.push(Span::styled("   of est. ", dim()));
            spans.push(Span::raw(usd(pm.med)));
            spans.push(Span::styled(" possible  →  ", dim()));
            // Waste is the product's headline DATA, not a failure state:
            // bold default fg, never red.
            spans.push(Span::styled(
                format!("{} – {}", usd(pw.p25), usd(pw.p75)),
                bold(),
            ));
            spans.push(Span::styled(" left on table", dim()));
        }
        lines.push(Line::from(spans));
        for e in &r.errors {
            lines.push(Line::from(vec![
                Span::raw("           "),
                Span::styled("✗", Style::new().fg(Color::Red)),
                Span::raw(format!(" {e}")),
            ]));
        }
        let mut body: Vec<Line> = Vec::new();
        if let Some(u) = &r.utilization {
            let (fill, rest) = meter(u.weighted_peak_pct / 100.0, METER_WIDTH);
            body.push(Line::from(vec![
                Span::raw("           "),
                Span::styled(fill, Style::new().fg(accent())),
                Span::styled(rest, dim()),
                Span::raw(format!(
                    "  extracted {:.0}% of plan window · wasting {} of {} covered",
                    u.weighted_peak_pct,
                    usd(u.waste_usd),
                    usd(u.covered_plan_usd)
                )),
                Span::styled(format!("   {}", u.window_id), dim()),
            ]));
        }
        for w in &r.windows {
            if let Some((ts, pct, resets)) = w.current {
                if now - ts > 30 * 86_400 {
                    continue;
                }
                let eta = resets
                    .map(|ra| dur_short((ra - now).max(0)))
                    .unwrap_or_else(|| "?".into());
                body.push(Line::from(Span::styled(
                    format!(
                        "           {:<16}{pct:>5.1}% used · resets in {eta}",
                        w.window_id
                    ),
                    dim(),
                )));
            }
        }
        if !body.is_empty() {
            lines.push(Line::default());
            lines.extend(body);
        }
        lines.push(Line::default());
    }
    let used = (lines.len() as u16).min(area.height);
    let any_util = app.reports.iter().any(|r| r.utilization.is_some());
    f.render_widget(Paragraph::new(lines), area);
    if !any_util {
        // Stanzas above keep every extracted figure and ✗ error visible; the
        // hint centers in the remaining void.
        let rest = Rect {
            x: area.x,
            y: area.y + used,
            width: area.width,
            height: area.height.saturating_sub(used),
        };
        center(
            f,
            rest,
            vec![
                Line::from(Span::styled("no utilization windows measured yet", dim())),
                Line::default(),
                Line::from(Span::styled(
                    "run `merma install`, then use Claude or Codex",
                    dim(),
                )),
            ],
        );
    }
}

fn draw_provider(f: &mut Frame, area: Rect, app: &App, provider: &str) {
    let Some(r) = app.reports.iter().find(|r| r.provider == provider) else {
        // No report at all: say so instead of today's silent blank.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(area);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(provider.to_string(), bold()),
            ])),
            rows[0],
        );
        center(
            f,
            rows[1],
            vec![
                Line::from(Span::styled(format!("no {provider} data yet"), dim())),
                Line::default(),
                Line::from(Span::styled("run `merma scan`", dim())),
            ],
        );
        return;
    };
    // The billing window backing the utilization figure, and its live instance.
    let win = r.windows.iter().find(|w| {
        r.utilization
            .as_ref()
            .is_some_and(|u| u.window_id == w.window_id)
    });
    let cur = win.and_then(|w| {
        let snaps = app
            .store
            .snapshots_between(
                &r.provider,
                Some(&w.window_id),
                w.instances.last().map(|i| i.first_ts).unwrap_or(0),
                i64::MAX,
            )
            .unwrap_or_default();
        reconstruct(snaps).pop()
    });

    // Summary block (instances · models · notes) — built first so the layout
    // can pin it above the footer at its exact height.
    let width = area.width as usize;
    let mut summary: Vec<Line> = Vec::new();
    if let Some(w) = win {
        summary.push(Line::from(Span::styled(
            format!("  RECENT {} INSTANCES", w.window_id.to_uppercase()),
            dim(),
        )));
        for i in w.instances.iter().rev().take(3) {
            let mut sp = vec![
                Span::raw(format!(
                    "  {} → {}",
                    crate::report::date(i.first_ts),
                    crate::report::date(i.last_ts)
                )),
                Span::styled("    peak ", dim()),
                Span::raw(format!("{:>3.0}%", i.peak_pct)),
                Span::styled("    extracted ", dim()),
                Span::raw(usd(i.extracted_usd)),
            ];
            if let Some(rate) = i.usd_per_pct {
                sp.push(Span::styled(format!("    ${rate:.2}/pct"), dim()));
            }
            summary.push(Line::from(sp));
        }
        summary.push(Line::default());
    }
    let mut sp = vec![Span::styled("  TOP MODELS", dim())];
    if !r.api.by_model.is_empty() {
        sp.push(Span::raw("   "));
        for (i, m) in r.api.by_model.iter().take(3).enumerate() {
            if i > 0 {
                sp.push(Span::styled(" · ", dim()));
            }
            sp.push(Span::raw(format!("{} {}", m.model, usd(m.full_usd))));
        }
    }
    summary.push(Line::from(sp));
    if !r.notes.is_empty() {
        summary.push(Line::default());
        for n in &r.notes {
            // Engine notes render verbatim: yellow glyph, default-fg text,
            // word-wrapped with a hanging indent — never truncated.
            let w = width.saturating_sub(6);
            let chunks = wrap_text(n, w, w);
            for (i, chunk) in chunks.into_iter().enumerate() {
                if i == 0 {
                    summary.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("⚠", Style::new().fg(Color::Yellow)),
                        Span::raw(format!(" {chunk}")),
                    ]));
                } else {
                    summary.push(Line::from(format!("    {chunk}")));
                }
            }
        }
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(summary.len() as u16),
        ])
        .split(area);

    // Title: bold provider + dim window id; the gap figure right-aligned.
    let mut left = vec![Span::raw("  "), Span::styled(r.provider.clone(), bold())];
    if let (Some(w), Some(_)) = (win, &cur) {
        left.push(Span::styled(format!(" · {} window", w.window_id), dim()));
    }
    let mut right: Vec<Span> = Vec::new();
    if let Some(cur) = &cur {
        match &r.max_extraction {
            Some(m) => {
                let pk = cur.peak_pct();
                let lo = (m.window_max_usd.p25 * (100.0 - pk) / 100.0).max(0.0);
                let hi = (m.window_max_usd.p75 * (100.0 - pk) / 100.0).max(0.0);
                right.push(Span::styled("gap ≈ ", dim()));
                right.push(Span::styled(format!("{} – {}", usd(lo), usd(hi)), bold()));
                right.push(Span::styled(" left in this window", dim()));
            }
            None => right.push(Span::styled("gap = unmeasured waste", dim())),
        }
    }
    let lw: usize = left.iter().map(|s| s.width()).sum();
    let rw: usize = right.iter().map(|s| s.width()).sum();
    let pad = width.saturating_sub(lw + rw + 2);
    if !right.is_empty() && pad >= 1 {
        left.push(Span::raw(" ".repeat(pad)));
        left.extend(right);
    }
    f.render_widget(Paragraph::new(Line::from(left)), rows[0]);

    match &cur {
        Some(cur) => draw_burnup(f, rows[2], cur),
        None => center(
            f,
            rows[2],
            vec![
                Line::from(Span::styled(
                    format!("no complete {provider} billing window observed yet"),
                    dim(),
                )),
                Line::from(Span::styled(
                    "the burn-up fills as utilization history accrues (`merma install` wires the hook)",
                    dim(),
                )),
            ],
        ),
    }
    f.render_widget(Paragraph::new(summary), rows[4]);
}

/// Borderless burn-up: accent used-% curve pressing against a dim ceiling
/// line at 100%. The void between them IS the gap — no legend, no fill.
fn draw_burnup(f: &mut Frame, area: Rect, cur: &crate::engine::windows::WindowInstance) {
    let t0 = cur.first_ts();
    let mut hours: Vec<(f64, f64)> = cur
        .points
        .iter()
        .map(|(ts, pct)| ((*ts - t0) as f64 / 3600.0, *pct))
        .collect();
    if hours.len() == 1 {
        // A one-point Line dataset draws nothing; duplicate the point so a
        // fresh window still shows its dot.
        hours.push(hours[0]);
    }
    let window_hours = cur
        .window_minutes
        .map(|m| m as f64 / 60.0)
        .unwrap_or_else(|| hours.last().map(|p| p.0).unwrap_or(1.0).max(1.0));
    let x_max = window_hours.max(hours.last().map(|p| p.0).unwrap_or(0.0));
    let ceiling = [(0.0, 100.0), (x_max, 100.0)];
    let datasets = vec![
        Dataset::default()
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(dim())
            .data(&ceiling),
        Dataset::default()
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(accent()))
            .data(&hours),
    ];
    let chart_area = Rect {
        x: area.x + 2.min(area.width),
        width: area.width.saturating_sub(2),
        ..area
    };
    f.render_widget(
        Chart::new(datasets)
            .legend_position(None)
            .x_axis(
                Axis::default()
                    .bounds([0.0, x_max])
                    .labels(vec![
                        Span::styled("0h", dim()),
                        Span::styled(format!("{:.0}h", x_max / 2.0), dim()),
                        Span::styled(format!("{x_max:.0}h"), dim()),
                    ])
                    .style(dim()),
            )
            .y_axis(
                Axis::default()
                    .bounds([0.0, 100.0])
                    .labels(vec![
                        Span::styled("0%", dim()),
                        Span::styled("50%", dim()),
                        Span::styled("100%", dim()),
                    ])
                    .style(dim()),
            ),
        chart_area,
    );
}

fn draw_heatmap(f: &mut Frame, area: Rect, app: &App) {
    let now = chrono::Utc::now().timestamp();
    let this_week = (now / 86_400 + 3).div_euclid(7) * 7 * 86_400 - 3 * 86_400;
    let weeks: Vec<i64> = (0..52).rev().map(|i| this_week - i * 7 * 86_400).collect();
    let ticks = month_ticks(&weeks);
    let tick_line = || Line::from(Span::styled(format!("            {ticks}"), dim()));
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "  UTILIZATION · 52 WEEKS · peak % of billing window per week",
            dim(),
        )),
        Line::default(),
    ];
    for (prov, series) in &app.util_heat {
        let map: std::collections::BTreeMap<i64, f64> = series.iter().cloned().collect();
        let mut spans = vec![Span::styled(format!("  {prov:<10}"), dim())];
        for wk in &weeks {
            match map.get(wk) {
                Some(pct) => {
                    let v = (pct / 100.0).clamp(0.0, 1.0);
                    spans.push(Span::styled(
                        spark_cell(v).to_string(),
                        Style::new().fg(ramp(v)),
                    ));
                }
                None => spans.push(Span::styled("·", dim())),
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(tick_line());
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  EXTRACTED · 52 WEEKS · API-equivalent $ per week, scaled to each provider's best week",
        dim(),
    )));
    lines.push(Line::default());
    // $ heatmap from weekly series (honors the current cache mode — the
    // series is built per-mode in rebuild()).
    for r in &app.reports {
        let map: std::collections::BTreeMap<i64, f64> = r.weekly_series.iter().cloned().collect();
        let best = map.values().cloned().fold(0.0f64, f64::max);
        let mut spans = vec![Span::styled(format!("  {:<10}", r.provider), dim())];
        for wk in &weeks {
            match map.get(wk) {
                Some(v) if best > 0.0 => {
                    let n = (v / best).clamp(0.0, 1.0);
                    spans.push(Span::styled(
                        spark_cell(n).to_string(),
                        Style::new().fg(ramp(n)),
                    ));
                }
                _ => spans.push(Span::styled("·", dim())),
            }
        }
        spans.push(Span::raw("    "));
        spans.push(Span::styled("best", dim()));
        spans.push(Span::raw("   "));
        spans.push(Span::raw(format!("{:>8}", usd(best))));
        lines.push(Line::from(spans));
    }
    lines.push(tick_line());
    lines.push(Line::default());
    let mut legend = vec![Span::raw("  ")];
    for (i, g) in SPARK.iter().enumerate() {
        let v = i as f64 / 7.0;
        legend.push(Span::styled(g.to_string(), Style::new().fg(ramp(v))));
    }
    legend.push(Span::raw("   "));
    legend.push(Span::styled(
        "0% → 100%    · no data    rightmost column = this week",
        dim(),
    ));
    lines.push(Line::from(legend));
    f.render_widget(Paragraph::new(lines), area);
}
