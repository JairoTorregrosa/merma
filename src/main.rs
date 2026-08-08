//! merma — measures the dollars you leave on the table in the AI
//! subscriptions you pay for.
//!
//! `merma` (no args) prints the brief: left on the table over the period,
//! the live gap per open window, the confidence basis, and the decision.

mod brief;
mod collectors;
mod config;
mod doctor;
mod engine;
mod fmt;
mod install;
mod pricing;
mod scan;
mod store;
mod theme;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Cfg;
use pricing::PriceBook;
use store::Store;

#[derive(Parser)]
#[command(
    name = "merma",
    version,
    about = "What your AI subscriptions leave on the table — estimated from your own history",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Brief period: 7d, 30d, 90d, 365d, all, or Nd
    #[arg(long, default_value = "30d")]
    period: String,
    /// claude, codex, or both
    #[arg(long)]
    provider: Option<String>,
    /// Machine-readable JSON output (brief/status/doctor/scan)
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
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
    /// Incremental scan of local history (transcripts + rollouts + spool)
    Scan,
    /// Scan + live polls (used by the launchd agent)
    Collect {
        #[arg(long)]
        quiet: bool,
    },
    /// One-line current status (for scripts and statuslines)
    Status,
    /// Diagnose every data source, with live cross-checks
    Doctor,
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

/// Ingestion problems must reach the user on EVERY read path — a brief over
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
        // The brief — TTY-styled, piped-plain, `--json` for the 0.2.0 contract.
        None => {
            surface_scan_problems(&scan::run_scan(&mut store, &cfg)?);
            let (t0, t1, label) = brief::parse_period(&cli.period, now)?;
            let provs = brief::providers_from_flag(cli.provider.as_deref())?;
            let reports = brief::build_reports(&store, &book, &cfg, &provs, t0, t1)?;
            if cli.json {
                let json = brief::brief_json(reports, &cli.period, t0, t1);
                println!("{}", serde_json::to_string_pretty(&json)?);
            } else {
                let requested = cli.period.trim().to_lowercase();
                print!(
                    "{}",
                    brief::render_brief(&reports, &label, &requested, t1, brief::BRIEF_WRAP_WIDTH)
                );
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
            let (t0, t1, _) = brief::parse_period("30d", now)?;
            let provs = brief::providers_from_flag(None)?;
            let reports = brief::build_reports(&store, &book, &cfg, &provs, t0, t1)?;
            if cli.json {
                let json = brief::brief_json(reports, "30d", t0, t1);
                println!("{}", serde_json::to_string_pretty(&json)?);
            } else {
                println!("{}", brief::render_status(&reports, now));
            }
            Ok(0)
        }
        Some(Cmd::StatuslineHook) => unreachable!("handled above"),
    }
}
