//! Dated price tables, loaded from embedded prices.toml (or ~/.merma/prices.toml override).
//!
//! Every lookup is date-aware: the price depends on WHEN the tokens were consumed.
//! Unknown model → the event lands in an "unpriced" bucket that is loudly reported;
//! it is never silently priced at zero inside a total that claims completeness.

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use serde::Deserialize;
use std::path::Path;

pub const EMBEDDED_PRICES: &str = include_str!("../prices.toml");

fn date_to_epoch(d: &str) -> Result<i64> {
    let nd = NaiveDate::parse_from_str(d, "%Y-%m-%d")
        .with_context(|| format!("bad date in price table: {d:?} (want YYYY-MM-DD)"))?;
    Ok(nd
        .and_hms_opt(0, 0, 0)
        .expect("midnight always valid")
        .and_utc()
        .timestamp())
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudePriceRaw {
    pub prefix: String,
    pub from: String,
    pub until: Option<String>,
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
    #[serde(default)]
    pub approx: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiPriceRaw {
    pub prefix: String,
    pub from: String,
    pub until: Option<String>,
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub approx: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreditRateRaw {
    pub prefix: String,
    pub from: String,
    pub until: Option<String>,
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Plan {
    pub provider: String,
    pub id: String,
    pub monthly_usd: f64,
    pub label: String,
    #[serde(default)]
    pub approx: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Constants {
    pub codex_credit_usd: f64,
    #[serde(default)]
    pub codex_credit_usd_approx: bool,
}

#[derive(Debug, Deserialize)]
struct PriceFile {
    claude: Vec<ClaudePriceRaw>,
    openai: Vec<OpenAiPriceRaw>,
    codex_credits: Vec<CreditRateRaw>,
    plan: Vec<Plan>,
    constants: Constants,
}

#[derive(Debug, Clone)]
pub struct DatedEntry<T> {
    pub prefix: String,
    pub from: i64,
    pub until: i64, // exclusive; i64::MAX for open-ended
    pub price: T,
    pub approx: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ClaudeRates {
    pub input: f64,
    pub cw_5m: f64,
    pub cw_1h: f64,
    pub read: f64,
    pub output: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct OpenAiRates {
    pub input: f64,
    pub cached: f64,
    pub output: f64,
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub struct CreditRates {
    pub input: f64,
    pub cached: f64,
    pub output: f64,
}

pub struct PriceBook {
    claude: Vec<DatedEntry<ClaudeRates>>,
    openai: Vec<DatedEntry<OpenAiRates>>,
    credits: Vec<DatedEntry<CreditRates>>,
    pub plans: Vec<Plan>,
    pub constants: Constants,
    pub source: String, // "embedded" or the override path
}

fn pick<'a, T>(entries: &'a [DatedEntry<T>], model: &str, ts: i64) -> Option<&'a DatedEntry<T>> {
    // Longest matching prefix wins; within a prefix, the entry whose date window contains ts.
    entries
        .iter()
        .filter(|e| model.starts_with(&e.prefix) && ts >= e.from && ts < e.until)
        .max_by_key(|e| e.prefix.len())
}

impl PriceBook {
    /// (claude, openai, credit-rate) entry counts, for diagnostics.
    pub fn entry_counts(&self) -> (usize, usize, usize) {
        (self.claude.len(), self.openai.len(), self.credits.len())
    }

    pub fn load(override_path: Option<&Path>) -> Result<Self> {
        let (text, source) = match override_path {
            Some(p) if p.exists() => (
                std::fs::read_to_string(p)
                    .with_context(|| format!("cannot read price override {}", p.display()))?,
                p.display().to_string(),
            ),
            _ => (EMBEDDED_PRICES.to_string(), "embedded".to_string()),
        };
        let raw: PriceFile = toml::from_str(&text).with_context(|| {
            format!("invalid price table ({source}) — refusing to guess prices")
        })?;
        if raw.claude.is_empty() || raw.openai.is_empty() || raw.codex_credits.is_empty() {
            bail!(
                "price table ({source}) has empty sections — refusing to run with no prices \
                 (an override must include [[claude]], [[openai]] and [[codex_credits]]; \
                 copy missing sections from the embedded prices.toml)"
            );
        }
        let mut book = PriceBook {
            claude: Vec::new(),
            openai: Vec::new(),
            credits: Vec::new(),
            plans: raw.plan,
            constants: raw.constants,
            source,
        };
        for c in raw.claude {
            book.claude.push(DatedEntry {
                from: date_to_epoch(&c.from)?,
                until: c
                    .until
                    .as_deref()
                    .map(date_to_epoch)
                    .transpose()?
                    .unwrap_or(i64::MAX),
                price: ClaudeRates {
                    input: c.input,
                    cw_5m: c.cache_write_5m,
                    cw_1h: c.cache_write_1h,
                    read: c.cache_read,
                    output: c.output,
                },
                approx: c.approx,
                prefix: c.prefix,
            });
        }
        for o in raw.openai {
            book.openai.push(DatedEntry {
                from: date_to_epoch(&o.from)?,
                until: o
                    .until
                    .as_deref()
                    .map(date_to_epoch)
                    .transpose()?
                    .unwrap_or(i64::MAX),
                price: OpenAiRates {
                    input: o.input,
                    cached: o.cached_input,
                    output: o.output,
                    cache_write: o.cache_write,
                },
                approx: o.approx,
                prefix: o.prefix,
            });
        }
        for c in raw.codex_credits {
            book.credits.push(DatedEntry {
                from: date_to_epoch(&c.from)?,
                until: c
                    .until
                    .as_deref()
                    .map(date_to_epoch)
                    .transpose()?
                    .unwrap_or(i64::MAX),
                price: CreditRates {
                    input: c.input,
                    cached: c.cached_input,
                    output: c.output,
                },
                approx: false,
                prefix: c.prefix,
            });
        }
        book.validate()?;
        Ok(book)
    }

    /// Refuse a table that could produce plausible-looking but wrong numbers:
    /// bad date ranges, ambiguous overlapping eras, non-finite/negative rates,
    /// or duplicate plans. Loud at load, not wrong at report time.
    fn validate(&self) -> Result<()> {
        let src = &self.source;
        fn check_entries<T>(
            entries: &[DatedEntry<T>],
            section: &str,
            src: &str,
            rates_of: impl Fn(&T) -> Vec<f64>,
        ) -> Result<()> {
            for e in entries {
                if e.prefix.is_empty() {
                    bail!("price table ({src}): empty model prefix in [[{section}]]");
                }
                if e.from >= e.until {
                    bail!(
                        "price table ({src}): [[{section}]] {} has from ≥ until",
                        e.prefix
                    );
                }
                for r in rates_of(&e.price) {
                    if !r.is_finite() || r < 0.0 {
                        bail!(
                            "price table ({src}): [[{section}]] {} has a negative or \
                             non-finite rate",
                            e.prefix
                        );
                    }
                }
            }
            for (i, a) in entries.iter().enumerate() {
                for b in &entries[i + 1..] {
                    if a.prefix == b.prefix && a.from < b.until && b.from < a.until {
                        bail!(
                            "price table ({src}): [[{section}]] {} has overlapping date \
                             windows — which era applies is ambiguous",
                            a.prefix
                        );
                    }
                }
            }
            Ok(())
        }
        check_entries(&self.claude, "claude", src, |r: &ClaudeRates| {
            vec![r.input, r.cw_5m, r.cw_1h, r.read, r.output]
        })?;
        check_entries(&self.openai, "openai", src, |r: &OpenAiRates| {
            let mut v = vec![r.input, r.cached, r.output];
            v.extend(r.cache_write);
            v
        })?;
        check_entries(&self.credits, "codex_credits", src, |r: &CreditRates| {
            vec![r.input, r.cached, r.output]
        })?;
        for (i, a) in self.plans.iter().enumerate() {
            if !a.monthly_usd.is_finite() || a.monthly_usd < 0.0 {
                bail!(
                    "price table ({src}): plan {}/{} has a negative or non-finite price",
                    a.provider,
                    a.id
                );
            }
            if self.plans[i + 1..]
                .iter()
                .any(|b| a.provider == b.provider && a.id == b.id)
            {
                bail!(
                    "price table ({src}): duplicate plan {}/{} — which price applies is \
                     ambiguous",
                    a.provider,
                    a.id
                );
            }
        }
        if !self.constants.codex_credit_usd.is_finite() || self.constants.codex_credit_usd < 0.0 {
            bail!("price table ({src}): constants.codex_credit_usd is negative or non-finite");
        }
        Ok(())
    }

    pub fn claude_rates(
        &self,
        model: &str,
        ts: i64,
    ) -> Option<(&DatedEntry<ClaudeRates>, ClaudeRates)> {
        pick(&self.claude, model, ts).map(|e| (e, e.price))
    }

    pub fn openai_rates(
        &self,
        model: &str,
        ts: i64,
    ) -> Option<(&DatedEntry<OpenAiRates>, OpenAiRates)> {
        pick(&self.openai, model, ts).map(|e| (e, e.price))
    }

    pub fn credit_rates(&self, model: &str, ts: i64) -> Option<CreditRates> {
        pick(&self.credits, model, ts).map(|e| e.price)
    }

    pub fn plan(&self, provider: &str, id: &str) -> Option<&Plan> {
        self.plans
            .iter()
            .find(|p| p.provider == provider && p.id == id)
    }
}

/// Cost of one normalized usage event, in USD at API list prices.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct EventCost {
    pub full_usd: f64, // everything the API would bill (incl. cache reads + writes)
    pub output_only_usd: f64, // just output tokens — the cache-skeptic view
    pub approx: bool,  // priced from an era-approximate entry
}

pub fn claude_event_cost(book: &PriceBook, e: &crate::store::UsageEvent) -> Option<EventCost> {
    let (entry, r) = book.claude_rates(&e.model, e.ts)?;
    let m = 1e-6;
    // Unsplit cache writes are billed at the 5m rate (the cheaper write): a
    // deliberate under-estimate for pre-split transcripts, flagged approx.
    let full = e.input as f64 * r.input * m
        + e.cache_w_5m as f64 * r.cw_5m * m
        + e.cache_w_1h as f64 * r.cw_1h * m
        + e.cache_w_unsplit as f64 * r.cw_5m * m
        + e.cached_input as f64 * r.read * m
        + e.output as f64 * r.output * m;
    Some(EventCost {
        full_usd: full,
        output_only_usd: e.output as f64 * r.output * m,
        approx: entry.approx || e.cache_w_unsplit > 0,
    })
}

pub fn codex_event_cost(book: &PriceBook, e: &crate::store::UsageEvent) -> Option<EventCost> {
    let (entry, r) = book.openai_rates(&e.model, e.ts)?;
    let m = 1e-6;
    let cw = r.cache_write.unwrap_or(r.input); // API bills cache writes; rate unknown → input rate
    let full = e.input as f64 * r.input * m
        + e.cached_input as f64 * r.cached * m
        + e.cache_w_unsplit as f64 * cw * m
        + e.output as f64 * r.output * m;
    Some(EventCost {
        full_usd: full,
        output_only_usd: e.output as f64 * r.output * m,
        approx: entry.approx || (e.cache_w_unsplit > 0 && r.cache_write.is_none()),
    })
}

/// Codex credits consumed by one event (None before the credit era or unknown model).
pub fn codex_event_credits(book: &PriceBook, e: &crate::store::UsageEvent) -> Option<f64> {
    let r = book.credit_rates(&e.model, e.ts)?;
    let m = 1e-6;
    Some(
        e.input as f64 * r.input * m
            + e.cached_input as f64 * r.cached * m
            + e.output as f64 * r.output * m,
    )
}

/// Models that run through the Codex CLI but not on the OpenAI subscription
/// (e.g. moonshotai/kimi-k3 via other providers). They consume no ChatGPT
/// quota. An UNKNOWN model is NOT external — it is subscription usage we
/// cannot attribute, and it must surface as unpriced, never quietly excluded.
pub fn is_external_model(provider: &str, model: &str) -> bool {
    provider == crate::store::CODEX && model.contains('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_table_parses() {
        let book = PriceBook::load(None).expect("embedded prices must parse");
        assert!(book.plans.len() >= 5);
        assert!((book.constants.codex_credit_usd - 0.04).abs() < 1e-9);
    }

    #[test]
    fn longest_prefix_wins() {
        let book = PriceBook::load(None).unwrap();
        let ts = date_to_epoch("2026-08-01").unwrap();
        let (_, mini) = book.openai_rates("gpt-5.4-mini", ts).unwrap();
        let (_, base) = book.openai_rates("gpt-5.4", ts).unwrap();
        assert!(mini.input < base.input);
        let (_, max) = book.openai_rates("gpt-5.1-codex-max", ts).unwrap();
        assert!((max.input - 1.25).abs() < 1e-9);
    }

    #[test]
    fn date_aware_sonnet_intro_flip() {
        let book = PriceBook::load(None).unwrap();
        let before = date_to_epoch("2026-08-15").unwrap();
        let after = date_to_epoch("2026-09-15").unwrap();
        let (_, intro) = book.claude_rates("claude-sonnet-5", before).unwrap();
        let (_, std) = book.claude_rates("claude-sonnet-5", after).unwrap();
        assert!((intro.input - 2.0).abs() < 1e-9);
        assert!((std.input - 3.0).abs() < 1e-9);
    }

    #[test]
    fn terra_price_cut_boundary() {
        let book = PriceBook::load(None).unwrap();
        let pre = date_to_epoch("2026-07-15").unwrap();
        let post = date_to_epoch("2026-08-01").unwrap();
        let (_, a) = book.openai_rates("gpt-5.6-terra", pre).unwrap();
        let (_, b) = book.openai_rates("gpt-5.6-terra", post).unwrap();
        assert!((a.input - 2.5).abs() < 1e-9);
        assert!((b.input - 2.0).abs() < 1e-9);
    }

    #[test]
    fn external_models_flagged() {
        assert!(is_external_model(crate::store::CODEX, "moonshotai/kimi-k3"));
        assert!(!is_external_model(crate::store::CODEX, "gpt-5.6-sol"));
        assert!(!is_external_model(crate::store::CLAUDE, "claude-fable-5"));
    }
}
