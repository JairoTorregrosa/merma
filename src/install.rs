//! `merma install` — wire the collectors into the user's environment.
//!
//! 1. Statusline hook: merma becomes the statusLine command in Claude Code's
//!    settings.json, chaining to whatever was configured before. Claude keeps
//!    no utilization history — waste history accrues only from install forward,
//!    so this runs early and `merma doctor` nags until it does.
//! 2. Transcript retention: cleanupPeriodDays defaults to 30 — history
//!    evaporates. `--fix-retention` raises it to 365.
//! 3. Optional launchd agent (`--launchd`): runs `merma collect` every 15 min
//!    so snapshots and scans accrue even without an open dashboard.

use crate::config::{ensure_dir, Cfg};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

const LAUNCHD_LABEL: &str = "com.merma.collect";

pub struct InstallReport {
    pub actions: Vec<String>,
    pub warnings: Vec<String>,
}

fn read_settings(cfg: &Cfg) -> Result<(PathBuf, serde_json::Value)> {
    let path = cfg.claude_settings_json();
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {} — is Claude Code installed?", path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON — fix it first", path.display()))?;
    Ok((path, v))
}

fn write_settings(path: &PathBuf, v: &serde_json::Value) -> Result<()> {
    let backup = path.with_extension(format!(
        "json.merma-backup-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S")
    ));
    std::fs::copy(path, &backup).with_context(|| format!("backing up to {}", backup.display()))?;
    std::fs::write(path, serde_json::to_string_pretty(v)? + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn install(
    cfg: &Cfg,
    launchd: bool,
    fix_retention: bool,
    print_only: bool,
) -> Result<InstallReport> {
    let mut rep = InstallReport {
        actions: Vec::new(),
        warnings: Vec::new(),
    };
    let exe = std::env::current_exe().context("cannot determine merma binary path")?;
    let hook_cmd = format!("{} statusline-hook", exe.display());
    let (settings_path, mut settings) = read_settings(cfg)?;

    let current_cmd = settings
        .pointer("/statusLine/command")
        .and_then(|x| x.as_str())
        .map(str::to_string);

    match &current_cmd {
        Some(c) if c.contains("merma") => {
            rep.actions
                .push(format!("statusline hook already installed ({c})"));
        }
        _ => {
            if print_only {
                rep.actions.push(format!(
                    "WOULD set statusLine.command = {hook_cmd:?} (chaining to {current_cmd:?})"
                ));
            } else {
                // Persist the chain target first so the hook never runs unchained.
                let mut file_cfg = cfg.file.clone();
                file_cfg.chain_statusline = current_cmd.clone();
                ensure_dir(&cfg.merma_home)?;
                std::fs::write(
                    cfg.config_path(),
                    toml::to_string_pretty(&file_cfg).context("serializing config")?,
                )
                .with_context(|| format!("writing {}", cfg.config_path().display()))?;
                settings["statusLine"] = serde_json::json!({
                    "type": "command",
                    "command": hook_cmd,
                });
                write_settings(&settings_path, &settings)?;
                rep.actions.push(format!(
                    "statusline hook installed (chains to {})",
                    current_cmd
                        .as_deref()
                        .unwrap_or("nothing — merma renders its own line")
                ));
            }
        }
    }

    // Retention.
    let retention = settings.get("cleanupPeriodDays").and_then(|x| x.as_i64());
    match retention {
        Some(d) if d >= 90 => rep
            .actions
            .push(format!("transcript retention OK (cleanupPeriodDays = {d})")),
        _ => {
            if fix_retention && !print_only {
                let (p, mut s) = read_settings(cfg)?; // re-read: may have been rewritten above
                s["cleanupPeriodDays"] = serde_json::json!(365);
                write_settings(&p, &s)?;
                rep.actions.push("set cleanupPeriodDays = 365".into());
            } else {
                rep.warnings.push(format!(
                    "cleanupPeriodDays is {} — Claude transcripts are deleted after ~30 days and \
                     every lost day is unmeasurable waste; run `merma install --fix-retention`",
                    retention.map_or("unset".into(), |d| d.to_string())
                ));
            }
        }
    }

    if launchd {
        if cfg!(target_os = "macos") {
            let plist = launchd_plist(&exe)?;
            let path = launchd_path()?;
            if print_only {
                rep.actions.push(format!("WOULD write {}", path.display()));
            } else {
                ensure_dir(path.parent().unwrap())?;
                std::fs::write(&path, plist)?;
                let _ = std::process::Command::new("launchctl")
                    .args(["unload", path.to_str().unwrap()])
                    .output();
                let out = std::process::Command::new("launchctl")
                    .args(["load", path.to_str().unwrap()])
                    .output()
                    .context("running launchctl load")?;
                if !out.status.success() {
                    bail!(
                        "launchctl load failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                rep.actions
                    .push(format!("launchd collector installed ({})", path.display()));
            }
        } else {
            bail!("--launchd is macOS-only; schedule `merma collect` with cron/systemd instead");
        }
    }
    Ok(rep)
}

pub fn uninstall(cfg: &Cfg) -> Result<InstallReport> {
    let mut rep = InstallReport {
        actions: Vec::new(),
        warnings: Vec::new(),
    };
    let (settings_path, mut settings) = read_settings(cfg)?;
    let current_cmd = settings
        .pointer("/statusLine/command")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    if current_cmd.contains("merma") {
        match &cfg.file.chain_statusline {
            Some(prev) => {
                settings["statusLine"] = serde_json::json!({"type": "command", "command": prev});
            }
            None => {
                settings
                    .as_object_mut()
                    .context("settings.json is not an object")?
                    .remove("statusLine");
            }
        }
        write_settings(&settings_path, &settings)?;
        rep.actions.push("statusline hook removed".into());
    } else {
        rep.actions.push("statusline hook was not installed".into());
    }
    if let Ok(path) = launchd_path() {
        if path.exists() {
            let _ = std::process::Command::new("launchctl")
                .args(["unload", path.to_str().unwrap()])
                .output();
            std::fs::remove_file(&path)?;
            rep.actions.push("launchd collector removed".into());
        }
    }
    Ok(rep)
}

fn launchd_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home dir")?
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

fn launchd_plist(exe: &std::path::Path) -> Result<String> {
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>collect</string>
        <string>--quiet</string>
    </array>
    <key>StartInterval</key><integer>900</integer>
    <key>RunAtLoad</key><true/>
    <key>StandardErrorPath</key><string>/tmp/merma-collect.err</string>
</dict>
</plist>
"#,
        exe.display()
    ))
}
