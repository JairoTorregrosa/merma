//! merma — the AI-subscription waste meter.
//!
//! Shows how much of your Claude Code and Codex subscriptions you actually
//! extracted vs. left on the table, priced in API-equivalent dollars.

mod collectors;
mod config;
mod doctor;
mod engine;
mod install;
mod pricing;
mod report;
mod scan;
mod store;
mod theme;
mod ui;
mod wrapped;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Cfg;
use pricing::PriceBook;
use store::Store;

#[derive(Parser)]
#[command(
    name = "merma",
    version,
    about = "AI-subscription waste meter: what you extracted vs. what you left on the table",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Machine-readable JSON output (report/doctor/status/scan)
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Retrospective waste report
    Report {
        /// 7d, 30d, 90d, 365d, all, or Nd
        #[arg(long, default_value = "30d")]
        period: String,
        /// claude, codex, or both
        #[arg(long)]
        provider: Option<String>,
        /// full (cache-included) or output-only
        #[arg(long, default_value = "full")]
        cache: String,
    },
    /// Shareable recap card
    Wrapped {
        /// 30d, 365d, all, or Nd
        #[arg(long, default_value = "30d")]
        period: String,
        #[arg(long, default_value = "full")]
        cache: String,
    },
    /// Incremental scan of local history (transcripts + rollouts + spool)
    Scan,
    /// Scan + live polls (used by the launchd agent)
    Collect {
        #[arg(long)]
        quiet: bool,
    },
    /// Install the statusline hook (and optionally a launchd collector)
    Install {
        #[arg(long)]
        launchd: bool,
        /// Raise Claude transcript retention to 365 days
        #[arg(long)]
        fix_retention: bool,
        /// Show what would change without changing it
        #[arg(long)]
        print: bool,
    },
    /// Undo `merma install`
    Uninstall,
    /// Diagnose every data source, with live cross-checks
    Doctor,
    /// One-line current status (for scripts and statuslines)
    Status,
    /// Internal: Claude Code statusline hook (reads feed on stdin)
    #[command(hide = true)]
    StatuslineHook,
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("merma error: {e:#}");
            std::process::exit(1);
        }
    }
}

/// Ingestion problems must reach the user on EVERY read path — a report over
/// silently incomplete data would present lower extracted dollars as fact.
fn surface_scan_problems(sum: &collectors::ScanSummary) {
    for w in &sum.warnings {
        eprintln!("⚠ scan: {w}");
    }
    if sum.parse_errors > 0 {
        eprintln!(
            "⚠ scan: {} unparseable line(s) skipped — totals may undercount; \
             run `merma doctor` for details",
            sum.parse_errors
        );
    }
}

fn run(cli: Cli) -> Result<i32> {
    let cfg = Cfg::load()?;

    // The hook path must not touch the DB and must never fail the statusline.
    if let Some(Cmd::StatuslineHook) = &cli.cmd {
        return Ok(collectors::statusline::run_hook(&cfg));
    }

    let book = PriceBook::load(Some(&cfg.prices_override_path()))?;
    let mut store = Store::open(&cfg.db_path())?;
    let now = chrono::Utc::now().timestamp();

    match cli.cmd {
        None => {
            use std::io::IsTerminal;
            if !std::io::stdout().is_terminal() {
                anyhow::bail!(
                    "the dashboard needs a TTY — use `merma report`, `merma status` or --json \
                     for non-interactive output"
                );
            }
            // Live dashboard: fresh scan first, polls continue inside the UI.
            let sum = scan::run_scan(&mut store, &cfg)?;
            ui::run_dashboard(store, cfg, book, sum)?;
            Ok(0)
        }
        Some(Cmd::Report {
            period,
            provider,
            cache,
        }) => {
            surface_scan_problems(&scan::run_scan(&mut store, &cfg)?);
            let (t0, t1, label) = report::parse_period(&period, now)?;
            let mode = report::parse_cache_mode(&cache)?;
            let provs = report::providers_from_flag(provider.as_deref())?;
            let reports = report::build_reports(&store, &book, &cfg, &provs, t0, t1, mode)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&reports)?);
            } else {
                print!("{}", report::render_text(&reports, &label));
            }
            Ok(0)
        }
        Some(Cmd::Wrapped { period, cache }) => {
            surface_scan_problems(&scan::run_scan(&mut store, &cfg)?);
            let (t0, t1, label) = report::parse_period(&period, now)?;
            let mode = report::parse_cache_mode(&cache)?;
            let provs = report::providers_from_flag(None)?;
            let reports = report::build_reports(&store, &book, &cfg, &provs, t0, t1, mode)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&reports)?);
            } else {
                print!("{}", wrapped::render(&reports, &label));
            }
            Ok(0)
        }
        Some(Cmd::Scan) => {
            let sum = scan::run_scan(&mut store, &cfg)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&sum)?);
            } else {
                println!(
                    "scanned {} files ({} parsed) → +{} events, +{} snapshots, {} parse errors",
                    sum.files_seen,
                    sum.files_parsed,
                    sum.events_inserted,
                    sum.snapshots_inserted,
                    sum.parse_errors
                );
                for w in &sum.warnings {
                    println!("⚠ {w}");
                }
            }
            Ok(0)
        }
        Some(Cmd::Collect { quiet }) => {
            let (sum, poll_errors) = scan::run_collect(&mut store, &cfg)?;
            if !quiet {
                println!(
                    "collected: +{} events, +{} snapshots",
                    sum.events_inserted, sum.snapshots_inserted
                );
                for e in &poll_errors {
                    println!("⚠ {e}");
                }
            }
            Ok(0)
        }
        Some(Cmd::Install {
            launchd,
            fix_retention,
            print,
        }) => {
            let rep = install::install(&cfg, launchd, fix_retention, print)?;
            for a in &rep.actions {
                println!("✓ {a}");
            }
            for w in &rep.warnings {
                println!("⚠ {w}");
            }
            Ok(0)
        }
        Some(Cmd::Uninstall) => {
            let rep = install::uninstall(&cfg)?;
            for a in &rep.actions {
                println!("✓ {a}");
            }
            Ok(0)
        }
        Some(Cmd::Doctor) => {
            surface_scan_problems(&scan::run_scan(&mut store, &cfg)?);
            let checks = doctor::run(&mut store, &cfg, &book)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                print!("{}", doctor::render_text(&checks));
            }
            let bad = checks.iter().any(|c| c.status == "fail");
            Ok(if bad { 1 } else { 0 })
        }
        Some(Cmd::Status) => {
            surface_scan_problems(&scan::run_scan(&mut store, &cfg)?);
            let (t0, t1, _) = report::parse_period("7d", now)?;
            let provs = report::providers_from_flag(None)?;
            let reports = report::build_reports(
                &store,
                &book,
                &cfg,
                &provs,
                t0,
                t1,
                engine::waste::CacheMode::Full,
            )?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&reports)?);
            } else {
                let mut parts = Vec::new();
                for r in &reports {
                    for w in &r.windows {
                        if let Some((_, pct, resets)) = w.current {
                            let eta = resets
                                .map(|ra| report::dur_short((ra - now).max(0)))
                                .unwrap_or_else(|| "?".into());
                            parts.push(format!(
                                "{} {} {:.0}% (resets {eta})",
                                r.provider, w.window_id, pct
                            ));
                        }
                    }
                }
                println!("{}", parts.join(" · "));
            }
            Ok(0)
        }
        Some(Cmd::StatuslineHook) => unreachable!("handled above"),
    }
}
