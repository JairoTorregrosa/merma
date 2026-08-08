//! Live Ratatui dashboard.
//!
//! Visualization rules (dataviz method, adapted to terminal cells):
//! - The heatmap encodes MAGNITUDE → one hue, light→dark, plus glyph density
//!   (▁▂▃…█) so identity survives colorblindness and theme remaps.
//! - Gauges encode STATE (how much of the plan is burning away) → ANSI status
//!   colors that respect the user's terminal theme, always next to explicit
//!   numbers — color is never the only encoding.
//! - The burn-up chart fills UNDER the utilization curve; the empty band up to
//!   the 100% ceiling IS the waste, and it carries a $ label.
//! - The headline is a stat tile, not a chart.

use crate::collectors::ScanSummary;
use crate::config::Cfg;
use crate::engine::waste::{build_provider_report, CacheMode, ProviderReport};
use crate::engine::windows::reconstruct;
use crate::pricing::PriceBook;
use crate::report::{dur_short, usd};
use crate::store::{Store, CLAUDE, CODEX};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, Gauge, GraphType, Paragraph, Tabs};
use ratatui::Frame;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DASH_PERIOD_SECS: i64 = 7 * 86_400;
const RESCAN_EVERY: Duration = Duration::from_secs(30);

/// Sequential single-hue ramp (teal), light→dark, for magnitude cells.
fn ramp_color(v: f64) -> Color {
    let v = v.clamp(0.0, 1.0);
    let lerp = |a: f64, b: f64| (a + (b - a) * v) as u8;
    Color::Rgb(lerp(210.0, 8.0), lerp(235.0, 105.0), lerp(233.0, 114.0))
}

fn glyph(v: f64) -> char {
    const G: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    G[(v.clamp(0.0, 1.0) * 8.0).round() as usize]
}

/// Status color for the WASTE framing: low extraction = money burning (red),
/// high extraction = healthy (green). Numbers always accompany it.
fn waste_status(extracted_pct: f64) -> Color {
    if extracted_pct >= 70.0 {
        Color::Green
    } else if extracted_pct >= 35.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

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
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(f.area());
    let titles = ["overview", "codex", "claude", "heatmap"];
    f.render_widget(
        Tabs::new(titles.iter().map(|t| Line::from(*t)))
            .select(app.tab)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)),
        rows[0],
    );
    match app.tab {
        0 => draw_overview(f, rows[1], app),
        1 => draw_provider(f, rows[1], app, CODEX),
        2 => draw_provider(f, rows[1], app, CLAUDE),
        _ => draw_heatmap(f, rows[1], app),
    }
    let mode = match app.mode {
        CacheMode::Full => "full API-equiv",
        CacheMode::OutputOnly => "output-only",
    };
    f.render_widget(
        Paragraph::new(format!(
            " q quit · tab/1-4 views · c cache mode [{mode}] · r rescan   {}",
            app.status_line
        ))
        .style(Style::default().add_modifier(Modifier::DIM)),
        rows[2],
    );
}

fn headline(app: &App) -> Vec<Line<'static>> {
    let now = chrono::Utc::now().timestamp();
    let mut lines = Vec::new();
    for r in &app.reports {
        let extracted = match r.cache_mode {
            CacheMode::Full => r.api.full_usd,
            CacheMode::OutputOnly => r.api.output_only_usd,
        };
        let mut spans = vec![
            Span::styled(
                format!("{:<7}", r.provider),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("7d extracted {}", usd(extracted))),
        ];
        if let (Some(pm), Some(pw)) = (&r.period_max_usd, &r.period_waste_usd) {
            spans.push(Span::raw(format!("  of est. {} possible → ", usd(pm.med))));
            spans.push(Span::styled(
                format!("{} – {} left on table", usd(pw.p25), usd(pw.p75)),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(spans));
        for w in &r.windows {
            if let Some((ts, pct, resets)) = w.current {
                if now - ts > 30 * 86_400 {
                    continue;
                }
                let eta = resets
                    .map(|ra| dur_short((ra - now).max(0)))
                    .unwrap_or_else(|| "?".into());
                lines.push(Line::from(format!(
                    "        {} {:>5.1}% used · resets in {eta}",
                    w.window_id, pct
                )));
            }
        }
        for e in &r.errors {
            lines.push(Line::from(Span::styled(
                format!("        ✗ {e}"),
                Style::default().fg(Color::Red),
            )));
        }
    }
    lines
}

fn draw_overview(f: &mut Frame, area: Rect, app: &App) {
    let head_h = headline(app).len() as u16 + 2;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(head_h), Constraint::Min(3)])
        .split(area);
    f.render_widget(
        Paragraph::new(headline(app)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" this week, in API dollars "),
        ),
        rows[0],
    );
    // One waste gauge per provider billing window.
    let gauges: Vec<(&ProviderReport, f64, String)> = app
        .reports
        .iter()
        .filter_map(|r| {
            r.utilization.as_ref().map(|u| {
                let label = format!(
                    "{} {} · extracted {:.0}% of plan window · wasting {} of {} covered",
                    r.provider,
                    u.window_id,
                    u.weighted_peak_pct,
                    usd(u.waste_usd),
                    usd(u.covered_plan_usd),
                );
                (r, u.weighted_peak_pct, label)
            })
        })
        .collect();
    let constraints: Vec<Constraint> = gauges.iter().map(|_| Constraint::Length(3)).collect();
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if constraints.is_empty() {
            vec![Constraint::Min(1)]
        } else {
            constraints
        })
        .split(rows[1]);
    for (i, (_, pct, label)) in gauges.iter().enumerate() {
        f.render_widget(
            Gauge::default()
                .block(Block::default().borders(Borders::ALL))
                .gauge_style(Style::default().fg(waste_status(*pct)))
                .ratio((pct / 100.0).clamp(0.0, 1.0))
                .label(label.clone()),
            areas[i],
        );
    }
    if gauges.is_empty() {
        f.render_widget(
            Paragraph::new(
                "no utilization windows measured yet — run `merma install` and use Claude/Codex",
            )
            .block(Block::default().borders(Borders::ALL)),
            areas[0],
        );
    }
}

fn draw_provider(f: &mut Frame, area: Rect, app: &App, provider: &str) {
    let Some(r) = app.reports.iter().find(|r| r.provider == provider) else {
        return;
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(8)])
        .split(area);
    draw_burnup(f, rows[0], app, r);

    // Recent instances + models summary.
    let mut lines: Vec<Line> = Vec::new();
    if let Some(w) = r.windows.iter().find(|w| {
        r.utilization
            .as_ref()
            .is_some_and(|u| u.window_id == w.window_id)
    }) {
        lines.push(Line::from(Span::styled(
            format!("recent {} instances:", w.window_id),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for i in w.instances.iter().rev().take(3) {
            lines.push(Line::from(format!(
                "  {} → {}  peak {:>3.0}%  extracted {}{}",
                crate::report::date(i.first_ts),
                crate::report::date(i.last_ts),
                i.peak_pct,
                usd(i.extracted_usd),
                i.usd_per_pct
                    .map(|r| format!("  (${:.2}/pct)", r))
                    .unwrap_or_default(),
            )));
        }
    }
    let mut top = String::from("top models: ");
    for m in r.api.by_model.iter().take(3) {
        top.push_str(&format!("{} {} · ", m.model, usd(m.full_usd)));
    }
    lines.push(Line::from(top.trim_end_matches(" · ").to_string()));
    for n in &r.notes {
        lines.push(Line::from(Span::styled(
            format!("⚠ {n}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
        rows[1],
    );
}

fn draw_burnup(f: &mut Frame, area: Rect, app: &App, r: &ProviderReport) {
    let Some(w) = r.windows.iter().find(|w| {
        r.utilization
            .as_ref()
            .is_some_and(|u| u.window_id == w.window_id)
    }) else {
        f.render_widget(
            Paragraph::new(format!(
                "no {} utilization snapshots yet — history accrues after `merma install`",
                r.provider
            ))
            .block(Block::default().borders(Borders::ALL).title(" burn-up ")),
            area,
        );
        return;
    };
    // Current instance points → burn-up curve; the empty band to 100% is waste.
    let inst = w.instances.last();
    let snaps = app
        .store
        .snapshots_between(
            &r.provider,
            Some(&w.window_id),
            inst.map(|i| i.first_ts).unwrap_or(0),
            i64::MAX,
        )
        .unwrap_or_default();
    let instances = reconstruct(snaps);
    let Some(cur) = instances.last() else { return };
    let t0 = cur.first_ts();
    let hours: Vec<(f64, f64)> = cur
        .points
        .iter()
        .map(|(ts, pct)| ((*ts - t0) as f64 / 3600.0, *pct))
        .collect();
    let window_hours = cur
        .window_minutes
        .map(|m| m as f64 / 60.0)
        .unwrap_or_else(|| hours.last().map(|p| p.0).unwrap_or(1.0).max(1.0));
    let x_max = window_hours.max(hours.last().map(|p| p.0).unwrap_or(0.0));
    let ceiling = [(0.0, 100.0), (x_max, 100.0)];
    let waste_label = match (&r.period_waste_usd, &r.max_extraction) {
        (_, Some(m)) => {
            let cur_pct = cur.peak_pct();
            let lo = (m.window_max_usd.p25 * (100.0 - cur_pct) / 100.0).max(0.0);
            let hi = (m.window_max_usd.p75 * (100.0 - cur_pct) / 100.0).max(0.0);
            format!(" gap ≈ {} – {} left in this window ", usd(lo), usd(hi))
        }
        _ => " gap = unmeasured waste ".into(),
    };
    let datasets = vec![
        Dataset::default()
            .name("used %")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Bar)
            .style(Style::default().fg(Color::Green))
            .data(&hours),
        Dataset::default()
            .name("plan ceiling")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::DarkGray))
            .data(&ceiling),
    ];
    let x_labels = vec![
        Span::raw("0h"),
        Span::raw(format!("{:.0}h", x_max / 2.0)),
        Span::raw(format!("{:.0}h", x_max)),
    ];
    f.render_widget(
        Chart::new(datasets)
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} {} burn-up ·{}",
                r.provider, w.window_id, waste_label
            )))
            .x_axis(
                Axis::default()
                    .bounds([0.0, x_max])
                    .labels(x_labels)
                    .style(Style::default().add_modifier(Modifier::DIM)),
            )
            .y_axis(
                Axis::default()
                    .bounds([0.0, 100.0])
                    .labels(vec![Span::raw("0%"), Span::raw("50%"), Span::raw("100%")])
                    .style(Style::default().add_modifier(Modifier::DIM)),
            ),
        area,
    );
}

fn draw_heatmap(f: &mut Frame, area: Rect, app: &App) {
    let now = chrono::Utc::now().timestamp();
    let this_week = (now / 86_400 + 3).div_euclid(7) * 7 * 86_400 - 3 * 86_400;
    let weeks: Vec<i64> = (0..52).rev().map(|i| this_week - i * 7 * 86_400).collect();
    let mut lines: Vec<Line> = vec![Line::from(
        "52-week utilization (peak % of billing window) — magnitude: darker = more extracted",
    )];
    for (prov, series) in &app.util_heat {
        let map: std::collections::BTreeMap<i64, f64> = series.iter().cloned().collect();
        let mut spans = vec![Span::raw(format!("{prov:<7} "))];
        for wk in &weeks {
            match map.get(wk) {
                Some(pct) => {
                    let v = pct / 100.0;
                    spans.push(Span::styled(
                        glyph(v.max(0.02)).to_string(),
                        Style::default().fg(ramp_color(v)),
                    ));
                }
                None => spans.push(Span::styled(
                    "·".to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                )),
            }
        }
        lines.push(Line::from(spans));
    }
    // $ heatmap from weekly series (both cache modes honest: uses current mode).
    lines.push(Line::from(""));
    lines.push(Line::from(
        "52-week API-equivalent $ extracted — darker = bigger week",
    ));
    for r in &app.reports {
        let map: std::collections::BTreeMap<i64, f64> = r.weekly_series.iter().cloned().collect();
        let max = map.values().cloned().fold(0.0f64, f64::max);
        let mut spans = vec![Span::raw(format!("{:<7} ", r.provider))];
        for wk in &weeks {
            match map.get(wk) {
                Some(v) if max > 0.0 => {
                    let n = v / max;
                    spans.push(Span::styled(
                        glyph(n.max(0.02)).to_string(),
                        Style::default().fg(ramp_color(n)),
                    ));
                }
                _ => spans.push(Span::styled(
                    "·".to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                )),
            }
        }
        let best = map.values().cloned().fold(0.0f64, f64::max);
        spans.push(Span::raw(format!("  best {}", usd(best))));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    let mut legend = vec![Span::raw("scale  ")];
    for i in 0..=8 {
        let v = i as f64 / 8.0;
        legend.push(Span::styled(
            glyph(v.max(0.02)).to_string(),
            Style::default().fg(ramp_color(v)),
        ));
    }
    legend.push(Span::raw("  0% → 100% (· = no data)"));
    lines.push(Line::from(legend));
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" heatmap ")),
        area,
    );
}
