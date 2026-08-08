//! Shared plain-text formatters. Rendering rules live here so every surface
//! (brief, status, doctor) prints the same value the same way — the JSON
//! carries full precision, the text formats it under these rules.

use chrono::{TimeZone, Utc};

/// Dollars: two decimals, thousands comma. `$187.29`, `$1,846.20`.
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

/// Whole dollars, never negative (status only). `$15`.
pub fn usd_whole(x: f64) -> String {
    if x.is_nan() {
        return "$?".into();
    }
    format!("${:.0}", x.max(0.0))
}

/// Integer count with thousands commas: `41,218`. One numeral rule per
/// surface — token counts group like the dollars they sit beside.
pub fn count(n: i64) -> String {
    let neg = n < 0;
    let mut s = n.unsigned_abs().to_string();
    let mut grouped = String::new();
    while s.len() > 3 {
        let tail = s.split_off(s.len() - 3);
        grouped = format!(",{tail}{grouped}");
    }
    format!("{}{}{}", if neg { "-" } else { "" }, s, grouped)
}

/// UTC date `YYYY-MM-DD`.
pub fn date(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".into())
}

/// Compact duration: `4d 11h` / `2h 10m` / `5m`.
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

/// Achieved coverage of a band, as a percent with trailing zeros trimmed:
/// 0.9375 → `93.75%`, 0.875 → `87.5%`, 0.9921875 → `99.2188%`.
pub fn coverage_pct(frac: f64) -> String {
    let s = format!("{:.4}", frac * 100.0);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_formats() {
        assert_eq!(usd(187.29), "$187.29");
        assert_eq!(usd(1846.2), "$1,846.20");
        assert_eq!(usd(0.0), "$0.00");
        assert_eq!(usd(-3.5), "-$3.50");
        assert_eq!(usd(f64::NAN), "$?");
    }

    #[test]
    fn usd_whole_rounds() {
        assert_eq!(usd_whole(15.09), "$15");
        assert_eq!(usd_whole(39.02), "$39");
        assert_eq!(usd_whole(17.5), "$18");
    }

    #[test]
    fn count_groups_thousands() {
        assert_eq!(count(41_218), "41,218");
        assert_eq!(count(358_288), "358,288");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000_000), "1,000,000");
        assert_eq!(count(0), "0");
    }

    #[test]
    fn coverage_trims_trailing_zeros() {
        assert_eq!(coverage_pct(0.9375), "93.75%");
        assert_eq!(coverage_pct(0.875), "87.5%");
        assert_eq!(coverage_pct(0.9921875), "99.2188%");
        assert_eq!(coverage_pct(0.5), "50%");
    }

    #[test]
    fn dur_short_units() {
        assert_eq!(dur_short(4 * 86_400 + 11 * 3600), "4d 11h");
        assert_eq!(dur_short(2 * 3600 + 600), "2h 10m");
        assert_eq!(dur_short(300), "5m");
    }
}
