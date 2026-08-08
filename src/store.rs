//! SQLite store. Single file at ~/.merma/merma.db (WAL).
//!
//! Token normalization convention (applies to BOTH providers):
//!   input_tokens        = NON-cached input (Codex raw input minus cached; Claude input_tokens as-is)
//!   cached_input_tokens = cache READS (Codex cached_input_tokens; Claude cache_read_input_tokens)
//!   cache_w_5m/1h       = Claude ephemeral cache writes by TTL
//!   cache_w_unsplit     = Codex cache_write_input_tokens, or Claude cache writes from
//!                         transcripts that predate the TTL split

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub const CLAUDE: &str = "claude";
pub const CODEX: &str = "codex";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS scan_files(
  path        TEXT PRIMARY KEY,
  provider    TEXT NOT NULL,
  size        INTEGER NOT NULL,
  mtime       INTEGER NOT NULL,
  byte_offset INTEGER NOT NULL,
  line_no     INTEGER NOT NULL,
  state_json  TEXT
);
CREATE TABLE IF NOT EXISTS usage_events(
  id                  INTEGER PRIMARY KEY,
  provider            TEXT NOT NULL,
  ts                  INTEGER NOT NULL,
  model               TEXT NOT NULL,
  input_tokens        INTEGER NOT NULL DEFAULT 0,
  cached_input_tokens INTEGER NOT NULL DEFAULT 0,
  cache_w_5m          INTEGER NOT NULL DEFAULT 0,
  cache_w_1h          INTEGER NOT NULL DEFAULT 0,
  cache_w_unsplit     INTEGER NOT NULL DEFAULT 0,
  output_tokens       INTEGER NOT NULL DEFAULT 0,
  session_id          TEXT,
  is_sidechain        INTEGER NOT NULL DEFAULT 0,
  dedup_key           TEXT NOT NULL UNIQUE,
  source_file         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage_events(provider, ts);
CREATE TABLE IF NOT EXISTS snapshots(
  id             INTEGER PRIMARY KEY,
  provider       TEXT NOT NULL,
  window_id      TEXT NOT NULL,
  window_minutes INTEGER,
  used_percent   REAL NOT NULL,
  resets_at      INTEGER,
  ts             INTEGER NOT NULL,
  source         TEXT NOT NULL,
  plan_type      TEXT,
  UNIQUE(provider, window_id, ts, source)
);
CREATE INDEX IF NOT EXISTS idx_snap ON snapshots(provider, window_id, ts);
"#;

#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub provider: &'static str,
    pub ts: i64,
    pub model: String,
    pub input: i64,
    pub cached_input: i64,
    pub cache_w_5m: i64,
    pub cache_w_1h: i64,
    pub cache_w_unsplit: i64,
    pub output: i64,
    pub session_id: Option<String>,
    pub is_sidechain: bool,
    pub dedup_key: String,
    pub source_file: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Snapshot {
    pub provider: String,
    pub window_id: String,
    pub window_minutes: Option<i64>,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
    pub ts: i64,
    pub source: String,
    pub plan_type: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanState {
    pub size: i64,
    pub mtime: i64,
    pub byte_offset: i64,
    pub line_no: i64,
    pub state_json: Option<String>,
}

pub struct Store {
    pub conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create data dir {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.execute_batch(SCHEMA).context("applying schema")?;
        Ok(Store { conn })
    }

    pub fn scan_state(&self, path: &str) -> Result<Option<ScanState>> {
        self.conn
            .query_row(
                "SELECT size, mtime, byte_offset, line_no, state_json FROM scan_files WHERE path=?1",
                params![path],
                |r| {
                    Ok(ScanState {
                        size: r.get(0)?,
                        mtime: r.get(1)?,
                        byte_offset: r.get(2)?,
                        line_no: r.get(3)?,
                        state_json: r.get(4)?,
                    })
                },
            )
            .optional()
            .context("reading scan state")
    }

    pub fn save_scan_state(&self, path: &str, provider: &str, st: &ScanState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO scan_files(path, provider, size, mtime, byte_offset, line_no, state_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(path) DO UPDATE SET size=?3, mtime=?4, byte_offset=?5, line_no=?6, state_json=?7",
            params![path, provider, st.size, st.mtime, st.byte_offset, st.line_no, st.state_json],
        )?;
        Ok(())
    }

    pub fn delete_events_for_file(&self, source_file: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM usage_events WHERE source_file=?1",
            params![source_file],
        )?;
        Ok(())
    }

    pub fn delete_snapshots_for_source(&self, source: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM snapshots WHERE source=?1", params![source])?;
        Ok(())
    }

    /// Forget all scan positions for a provider so the next scan re-reads every
    /// file from byte zero (events dedup by key, so this is idempotent).
    pub fn reset_scan_states(&self, provider: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM scan_files WHERE provider=?1",
            params![provider],
        )?;
        Ok(())
    }

    /// INSERT OR IGNORE (dedup by dedup_key). Returns number actually inserted.
    pub fn insert_usage_events(&mut self, events: &[UsageEvent]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0usize;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO usage_events
                 (provider, ts, model, input_tokens, cached_input_tokens, cache_w_5m, cache_w_1h,
                  cache_w_unsplit, output_tokens, session_id, is_sidechain, dedup_key, source_file)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            )?;
            for e in events {
                n += stmt.execute(params![
                    e.provider,
                    e.ts,
                    e.model,
                    e.input,
                    e.cached_input,
                    e.cache_w_5m,
                    e.cache_w_1h,
                    e.cache_w_unsplit,
                    e.output,
                    e.session_id,
                    e.is_sidechain as i64,
                    e.dedup_key,
                    e.source_file,
                ])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn insert_snapshots(&mut self, snaps: &[Snapshot]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0usize;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO snapshots
                 (provider, window_id, window_minutes, used_percent, resets_at, ts, source, plan_type)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            for s in snaps {
                n += stmt.execute(params![
                    s.provider,
                    s.window_id,
                    s.window_minutes,
                    s.used_percent,
                    s.resets_at,
                    s.ts,
                    s.source,
                    s.plan_type,
                ])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn usage_between(&self, provider: &str, t0: i64, t1: i64) -> Result<Vec<UsageEvent>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts, model, input_tokens, cached_input_tokens, cache_w_5m, cache_w_1h,
                    cache_w_unsplit, output_tokens, session_id, is_sidechain, dedup_key, source_file
             FROM usage_events WHERE provider=?1 AND ts>=?2 AND ts<?3 ORDER BY ts",
        )?;
        let prov: &'static str = if provider == CLAUDE { CLAUDE } else { CODEX };
        let rows = stmt
            .query_map(params![provider, t0, t1], |r| {
                Ok(UsageEvent {
                    provider: prov,
                    ts: r.get(0)?,
                    model: r.get(1)?,
                    input: r.get(2)?,
                    cached_input: r.get(3)?,
                    cache_w_5m: r.get(4)?,
                    cache_w_1h: r.get(5)?,
                    cache_w_unsplit: r.get(6)?,
                    output: r.get(7)?,
                    session_id: r.get(8)?,
                    is_sidechain: r.get::<_, i64>(9)? != 0,
                    dedup_key: r.get(10)?,
                    source_file: r.get(11)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn snapshots_between(
        &self,
        provider: &str,
        window_id: Option<&str>,
        t0: i64,
        t1: i64,
    ) -> Result<Vec<Snapshot>> {
        let sql = match window_id {
            Some(_) => {
                "SELECT provider, window_id, window_minutes, used_percent, resets_at, ts, source, plan_type
                 FROM snapshots WHERE provider=?1 AND window_id=?2 AND ts>=?3 AND ts<?4 ORDER BY ts"
            }
            None => {
                "SELECT provider, window_id, window_minutes, used_percent, resets_at, ts, source, plan_type
                 FROM snapshots WHERE provider=?1 AND ts>=?2 AND ts<?3 ORDER BY ts"
            }
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let map = |r: &rusqlite::Row<'_>| {
            Ok(Snapshot {
                provider: r.get(0)?,
                window_id: r.get(1)?,
                window_minutes: r.get(2)?,
                used_percent: r.get(3)?,
                resets_at: r.get(4)?,
                ts: r.get(5)?,
                source: r.get(6)?,
                plan_type: r.get(7)?,
            })
        };
        let rows = match window_id {
            Some(w) => stmt
                .query_map(params![provider, w, t0, t1], map)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            None => stmt
                .query_map(params![provider, t0, t1], map)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    pub fn window_ids(&self, provider: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT DISTINCT window_id FROM snapshots WHERE provider=?1")?;
        let rows = stmt
            .query_map(params![provider], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn latest_plan_type(&self, provider: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT plan_type FROM snapshots
                 WHERE provider=?1 AND plan_type IS NOT NULL ORDER BY ts DESC LIMIT 1",
                params![provider],
                |r| r.get(0),
            )
            .optional()
            .context("reading latest plan type")
    }

    pub fn usage_bounds(&self, provider: &str) -> Result<Option<(i64, i64)>> {
        self.conn
            .query_row(
                "SELECT MIN(ts), MAX(ts) FROM usage_events WHERE provider=?1",
                params![provider],
                |r| {
                    let lo: Option<i64> = r.get(0)?;
                    let hi: Option<i64> = r.get(1)?;
                    Ok(lo.zip(hi))
                },
            )
            .context("reading usage bounds")
    }

    /// Plan-type transition points (ts, plan) across all snapshots, compressed
    /// to changes only — the plan history for cost integration.
    pub fn plan_type_points(&self, provider: &str) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts, plan_type FROM snapshots
             WHERE provider=?1 AND plan_type IS NOT NULL ORDER BY ts",
        )?;
        let rows = stmt
            .query_map(params![provider], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out: Vec<(i64, String)> = Vec::new();
        for (ts, pt) in rows {
            if out.last().map(|(_, p)| p != &pt).unwrap_or(true) {
                out.push((ts, pt));
            }
        }
        Ok(out)
    }

    pub fn snapshot_bounds(&self, provider: &str) -> Result<Option<(i64, i64)>> {
        self.conn
            .query_row(
                "SELECT MIN(ts), MAX(ts) FROM snapshots WHERE provider=?1",
                params![provider],
                |r| {
                    let lo: Option<i64> = r.get(0)?;
                    let hi: Option<i64> = r.get(1)?;
                    Ok(lo.zip(hi))
                },
            )
            .context("reading snapshot bounds")
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .context("meta get")
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn counts(&self) -> Result<(i64, i64)> {
        let events: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))?;
        let snaps: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))?;
        Ok((events, snaps))
    }
}
