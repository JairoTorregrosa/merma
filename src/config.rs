//! Configuration and environment discovery.
//!
//! Philosophy (user's global rule): discovery paths have sensible defaults, but
//! anything that would FABRICATE DATA on absence fails loudly with a remedy.
//! A missing optional config file is fine; a missing plan price when computing
//! subscription waste is an error, never a silent $0.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct FileConfig {
    /// Statusline command merma chains to after recording a snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_statusline: Option<String>,
    /// Override auto-detected Claude plan (plan id from prices.toml, e.g. "default_claude_max_20x").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_plan_id: Option<String>,
    /// Override Claude monthly price outright (wins over claude_plan_id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_monthly_usd: Option<f64>,
    /// Override Codex monthly price outright (wins over snapshot plan_type).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_monthly_usd: Option<f64>,
    /// Claude Code version for the OAuth User-Agent header (else `claude --version`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_version: Option<String>,
    /// Enable the unofficial Claude OAuth usage endpoint poller (default: true;
    /// see README "Terms of Service" — easy to switch off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_oauth_enabled: Option<bool>,
    /// Poll cadences (seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_poll_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wham_poll_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Cfg {
    pub merma_home: PathBuf,
    pub claude_home: PathBuf,
    pub codex_home: PathBuf,
    pub file: FileConfig,
}

impl Cfg {
    pub fn load() -> Result<Self> {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        let merma_home = std::env::var_os("MERMA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".merma"));
        let claude_home = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let cfg_path = merma_home.join("config.toml");
        let file: FileConfig = if cfg_path.exists() {
            let text = std::fs::read_to_string(&cfg_path)
                .with_context(|| format!("cannot read {}", cfg_path.display()))?;
            toml::from_str(&text).with_context(|| {
                format!("invalid config {} — fix or delete it", cfg_path.display())
            })?
        } else {
            FileConfig::default()
        };
        for (name, v) in [
            ("claude_monthly_usd", file.claude_monthly_usd),
            ("codex_monthly_usd", file.codex_monthly_usd),
        ] {
            if let Some(v) = v {
                if !v.is_finite() || v < 0.0 {
                    anyhow::bail!(
                        "invalid {name} = {v} in {} — must be a finite, non-negative price",
                        cfg_path.display()
                    );
                }
            }
        }
        Ok(Cfg {
            merma_home,
            claude_home,
            codex_home,
            file,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.merma_home.join("merma.db")
    }

    pub fn spool_dir(&self) -> PathBuf {
        self.merma_home.join("spool")
    }

    pub fn statusline_spool(&self) -> PathBuf {
        self.spool_dir().join("claude-statusline.jsonl")
    }

    pub fn hook_error_log(&self) -> PathBuf {
        self.spool_dir().join("hook-errors.log")
    }

    pub fn config_path(&self) -> PathBuf {
        self.merma_home.join("config.toml")
    }

    pub fn prices_override_path(&self) -> PathBuf {
        self.merma_home.join("prices.toml")
    }

    pub fn claude_settings_json(&self) -> PathBuf {
        self.claude_home.join("settings.json")
    }

    pub fn claude_projects_dir(&self) -> PathBuf {
        self.claude_home.join("projects")
    }

    /// ~/.claude.json (top-level Claude Code state, holds oauthAccount).
    pub fn claude_state_json(&self) -> Result<PathBuf> {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(home.join(".claude.json"))
    }

    pub fn codex_session_roots(&self) -> Vec<PathBuf> {
        vec![
            self.codex_home.join("sessions"),
            self.codex_home.join("archived_sessions"),
        ]
    }

    pub fn codex_auth_json(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }

    /// Detected Claude plan: (label, monthly_usd, approx).
    pub fn claude_plan(&self, book: &crate::pricing::PriceBook) -> Result<(String, f64, bool)> {
        if let Some(usd) = self.file.claude_monthly_usd {
            return Ok(("Claude (configured price)".into(), usd, false));
        }
        let tier = if let Some(id) = &self.file.claude_plan_id {
            id.clone()
        } else {
            let path = self.claude_state_json()?;
            let text = std::fs::read_to_string(&path).with_context(|| {
                format!(
                    "cannot read {} for plan auto-detection — remedy: set claude_plan_id or \
                     claude_monthly_usd in {}",
                    path.display(),
                    self.config_path().display()
                )
            })?;
            let v: serde_json::Value =
                serde_json::from_str(&text).context("~/.claude.json is not valid JSON")?;
            v.pointer("/oauthAccount/organizationRateLimitTier")
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .with_context(|| {
                    format!(
                        "no oauthAccount.organizationRateLimitTier in {} — remedy: set \
                         claude_plan_id or claude_monthly_usd in {}",
                        path.display(),
                        self.config_path().display()
                    )
                })?
        };
        match book.plan(crate::store::CLAUDE, &tier) {
            Some(p) => Ok((p.label.clone(), p.monthly_usd, p.approx)),
            None => bail!(
                "unknown Claude plan tier {tier:?} — remedy: add it to prices.toml [[plan]] or set \
                 claude_monthly_usd in {}",
                self.config_path().display()
            ),
        }
    }

    /// Codex plan for a given plan_type string from snapshots.
    pub fn codex_plan(
        &self,
        book: &crate::pricing::PriceBook,
        plan_type: &str,
    ) -> Result<(String, f64, bool)> {
        if let Some(usd) = self.file.codex_monthly_usd {
            return Ok(("Codex (configured price)".into(), usd, false));
        }
        match book.plan(crate::store::CODEX, plan_type) {
            Some(p) => Ok((p.label.clone(), p.monthly_usd, p.approx)),
            None => bail!(
                "unknown Codex plan_type {plan_type:?} — remedy: add it to prices.toml [[plan]] or \
                 set codex_monthly_usd in {}",
                self.config_path().display()
            ),
        }
    }

    /// Claude Code version string for the OAuth User-Agent (never guessed silently).
    pub fn claude_version(&self) -> Result<String> {
        if let Some(v) = &self.file.claude_version {
            return Ok(v.clone());
        }
        let out = std::process::Command::new("claude")
            .arg("--version")
            .output()
            .context(
                "cannot run `claude --version` for the OAuth User-Agent — remedy: set \
                 claude_version in ~/.merma/config.toml",
            )?;
        let text = String::from_utf8_lossy(&out.stdout);
        let ver = text
            .split_whitespace()
            .next()
            .filter(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .context(
                "unexpected `claude --version` output — remedy: set claude_version in config",
            )?;
        Ok(ver.to_string())
    }
}

/// Ensure a directory exists (used for merma_home and spool).
pub fn ensure_dir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).with_context(|| format!("cannot create {}", p.display()))
}
