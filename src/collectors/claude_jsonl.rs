//! Incremental scanner for Claude Code transcript JSONL files.
//!
//! Correctness invariants (verified against this machine's data, 2026-08):
//! - Duplicate assistant entries are ~57% of the stream → dedup by
//!   message.id + requestId is load-bearing, not defensive.
//! - `costUSD` is absent under subscription auth; when present it is a client
//!   estimate — merma always computes cost itself from tokens × dated prices.
//! - `cache_creation.ephemeral_{1h,5m}` split matters for pricing (1h = 2×
//!   base input, 5m = 1.25×); pre-split transcripts land in cache_w_unsplit.
//! - Sidechain/subagent usage burns the same quota → included, flagged.

use super::{iso_to_epoch, prefix_hash, resume_is_safe, ScanSummary};
use crate::config::Cfg;
use crate::store::{ScanState, Store, UsageEvent, CLAUDE};
use anyhow::Result;
use memchr::memmem;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileState {
    #[serde(default)]
    prefix_hash: Option<u64>,
}

pub fn scan(store: &mut Store, cfg: &Cfg) -> Result<ScanSummary> {
    let mut sum = ScanSummary::default();
    let root = cfg.claude_projects_dir();
    if !root.is_dir() {
        sum.warnings.push(format!(
            "claude projects dir missing: {} — no Claude history will be ingested",
            root.display()
        ));
        return Ok(sum);
    }
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(&root) {
        match entry {
            Ok(e)
                if e.path().is_file()
                    && e.path().extension().and_then(|x| x.to_str()) == Some("jsonl") =>
            {
                files.push(e.into_path());
            }
            Ok(_) => {}
            // An unreadable directory hides files: that is missing data, not noise.
            Err(e) => sum.warnings.push(format!("transcript walk error: {e}")),
        }
    }
    sum.files_seen = files.len();
    let mut any_rewritten = false;
    for p in &files {
        match scan_file(store, p, &root) {
            Ok((s, rewritten)) => {
                any_rewritten |= rewritten;
                sum.absorb(s);
            }
            Err(e) => sum.warnings.push(format!("{}: {e:#}", p.display())),
        }
    }
    // ~57% of assistant entries are duplicates across files, and only the
    // first-seen copy is stored. If a stored winner's file was rewritten, its
    // events were deleted — copies in OTHER (unchanged, hence skipped) files
    // must be re-offered, so rescan the whole provider once. INSERT OR IGNORE
    // makes this idempotent.
    if any_rewritten {
        sum.warnings.push(
            "a transcript was rewritten — full Claude rescan performed to restore \
             deduplicated events from other files"
                .into(),
        );
        store.reset_scan_states(CLAUDE)?;
        for p in &files {
            match scan_file(store, p, &root) {
                Ok((s, _)) => sum.absorb(s),
                Err(e) => sum.warnings.push(format!("{}: {e:#}", p.display())),
            }
        }
    }
    Ok(sum)
}

fn scan_file(store: &mut Store, path: &Path, root: &Path) -> Result<(ScanSummary, bool)> {
    let mut sum = ScanSummary::default();
    let meta = std::fs::metadata(path)?;
    let size = meta.len() as i64;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let key = path.display().to_string();
    // source_file: path relative to projects dir (stable, unique across projects)
    let rel = path
        .strip_prefix(root)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| key.clone());
    let mut rewritten = false;
    let mut st = match store.scan_state(&key)? {
        Some(p) if p.size == size && p.mtime == mtime => return Ok((sum, false)),
        Some(p) if p.size <= size => p,
        Some(_) => {
            rewritten = true;
            store.delete_events_for_file(&rel)?;
            ScanState::default()
        }
        None => ScanState::default(),
    };
    let fstate: FileState = match st.state_json.as_deref() {
        Some(js) => match serde_json::from_str(js) {
            Ok(f) => f,
            // Corrupt state must not fail OPEN (a None hash would disable
            // rewrite detection at a nonzero offset) — re-ingest loudly.
            Err(e) => {
                sum.warnings.push(format!(
                    "{rel}: unreadable scan state ({e}) — re-ingesting from scratch"
                ));
                rewritten = true;
                store.delete_events_for_file(&rel)?;
                st = ScanState::default();
                FileState::default()
            }
        },
        None => FileState::default(),
    };
    if st.byte_offset > 0 && !resume_is_safe(path, fstate.prefix_hash, st.size, st.byte_offset)? {
        // Same-or-larger size but different content: a rewrite, not an append.
        sum.warnings.push(format!(
            "{rel}: content changed under the resume offset — re-ingesting from scratch"
        ));
        rewritten = true;
        store.delete_events_for_file(&rel)?;
        st = ScanState::default();
    }

    let mut f = BufReader::new(std::fs::File::open(path)?);
    f.seek(SeekFrom::Start(st.byte_offset as u64))?;
    let need_a = memmem::Finder::new(b"\"assistant\"");
    let need_u = memmem::Finder::new(b"\"usage\"");

    let mut events: Vec<UsageEvent> = Vec::new();
    let mut line = Vec::with_capacity(256 * 1024);
    loop {
        line.clear();
        let n = f.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            break; // partial tail of a live session
        }
        st.byte_offset += n as i64;
        st.line_no += 1;
        if need_a.find(&line).is_none() || need_u.find(&line).is_none() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(_) => {
                sum.parse_errors += 1;
                continue;
            }
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("assistant") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        let Some(usage) = msg.get("usage") else {
            continue;
        };
        let model = msg
            .get("model")
            .and_then(|x| x.as_str())
            .unwrap_or("UNKNOWN");
        if model == "<synthetic>" {
            continue;
        }
        let g = |k: &str| usage.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
        let input = g("input_tokens");
        let read = g("cache_read_input_tokens");
        let cw_total = g("cache_creation_input_tokens");
        let output = g("output_tokens");
        let (w5, w1, unsplit) = match usage.get("cache_creation") {
            Some(cc) if cc.is_object() => (
                cc.get("ephemeral_5m_input_tokens")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0),
                cc.get("ephemeral_1h_input_tokens")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0),
                0,
            ),
            _ => (0, 0, cw_total),
        };
        if input == 0 && read == 0 && cw_total == 0 && output == 0 {
            continue;
        }
        if [input, read, cw_total, output, w5, w1]
            .iter()
            .any(|&t| t < 0)
        {
            sum.parse_errors += 1;
            continue;
        }
        let ts = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(iso_to_epoch);
        let Some(ts) = ts else {
            sum.parse_errors += 1;
            continue;
        };
        let msg_id = msg.get("id").and_then(|x| x.as_str());
        let req_id = v.get("requestId").and_then(|x| x.as_str());
        let dedup_key = match (msg_id, req_id) {
            (Some(m), Some(r)) => format!("{m}:{r}"),
            // Rare entries missing ids can't dedup across files; fall back to position.
            _ => format!("{rel}:{}", st.line_no),
        };
        events.push(UsageEvent {
            provider: CLAUDE,
            ts,
            model: model.to_string(),
            input,
            cached_input: read,
            cache_w_5m: w5,
            cache_w_1h: w1,
            cache_w_unsplit: unsplit,
            output,
            session_id: v
                .get("sessionId")
                .and_then(|x| x.as_str())
                .map(str::to_string),
            is_sidechain: v
                .get("isSidechain")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            dedup_key,
            source_file: rel.clone(),
        });
    }
    sum.files_parsed = 1;
    sum.events_inserted = store.insert_usage_events(&events)?;
    st.size = size;
    st.mtime = mtime;
    st.state_json = Some(serde_json::to_string(&FileState {
        prefix_hash: Some(prefix_hash(path, size)?),
    })?);
    store.save_scan_state(&key, CLAUDE, &st)?;
    Ok((sum, rewritten))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Cfg;

    fn cfg_with_claude_home(dir: &std::path::Path) -> Cfg {
        Cfg {
            merma_home: dir.join("merma"),
            claude_home: dir.to_path_buf(),
            codex_home: dir.join("codex"),
            file: Default::default(),
        }
    }

    fn line(ts: &str, msg_id: &str, req: &str, usage: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","requestId":"{req}","sessionId":"s1","message":{{"id":"{msg_id}","model":"claude-fable-5","usage":{usage}}}}}"#
        ) + "\n"
    }

    const SPLIT_USAGE: &str = r#"{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":50,"output_tokens":20,"cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":20}}"#;
    const UNSPLIT_USAGE: &str = r#"{"input_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":40,"output_tokens":7}"#;

    fn write_transcript(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join("projects").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// The 57%-duplicate reality: same message.id+requestId within and across
    /// files must land exactly once; synthetic and zero-usage lines skipped.
    #[test]
    fn dedups_within_and_across_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_claude_home(tmp.path());
        let dup = line("2026-08-01T10:00:00.000Z", "msg_1", "req_1", SPLIT_USAGE);
        let synthetic = r#"{"type":"assistant","timestamp":"2026-08-01T10:00:01.000Z","requestId":"req_x","message":{"id":"msg_x","model":"<synthetic>","usage":{"input_tokens":9,"output_tokens":9}}}"#.to_string() + "\n";
        let zero = line(
            "2026-08-01T10:00:02.000Z",
            "msg_z",
            "req_z",
            r#"{"input_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":0}"#,
        );
        write_transcript(
            tmp.path(),
            "proj-a/sess1.jsonl",
            &format!("{dup}{dup}{synthetic}{zero}"),
        );
        // Cross-file duplicate (continued session) + one genuinely new event.
        let fresh = line("2026-08-01T11:00:00.000Z", "msg_2", "req_2", UNSPLIT_USAGE);
        write_transcript(tmp.path(), "proj-b/sess2.jsonl", &format!("{dup}{fresh}"));

        let mut store = crate::store::Store::open(&tmp.path().join("m.db")).unwrap();
        let sum = scan(&mut store, &cfg).unwrap();
        assert_eq!(sum.parse_errors, 0);
        let evs = store.usage_between(CLAUDE, 0, i64::MAX).unwrap();
        assert_eq!(evs.len(), 2);
        let e1 = &evs[0];
        assert_eq!((e1.input, e1.cached_input, e1.output), (10, 100, 20));
        assert_eq!(
            (e1.cache_w_5m, e1.cache_w_1h, e1.cache_w_unsplit),
            (30, 20, 0)
        );
        let e2 = &evs[1];
        assert_eq!(
            (e2.cache_w_5m, e2.cache_w_1h, e2.cache_w_unsplit),
            (0, 0, 40)
        );
    }

    /// A same-size rewrite with different content is a rewrite, not an append:
    /// resuming at the old offset would skip the changed content entirely.
    #[test]
    fn same_size_rewrite_detected_and_reingested() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_claude_home(tmp.path());
        let v1 = line("2026-08-01T10:00:00.000Z", "msg_1", "req_1", SPLIT_USAGE);
        write_transcript(tmp.path(), "proj-a/s.jsonl", &v1);
        let mut store = crate::store::Store::open(&tmp.path().join("m.db")).unwrap();
        scan(&mut store, &cfg).unwrap();
        assert_eq!(store.usage_between(CLAUDE, 0, i64::MAX).unwrap().len(), 1);

        // Same byte length, different event id (msg_1 → msg_9, req_1 → req_9).
        let v2 = v1.replace("msg_1", "msg_9").replace("req_1", "req_9");
        assert_eq!(v1.len(), v2.len());
        let p = tmp.path().join("projects/proj-a/s.jsonl");
        std::fs::write(&p, &v2).unwrap();
        // force a different mtime so the size+mtime fast path doesn't skip
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .append(true)
            .open(&p)
            .unwrap()
            .set_modified(later)
            .unwrap();

        let sum = scan(&mut store, &cfg).unwrap();
        assert!(sum.warnings.iter().any(|w| w.contains("re-ingesting")));
        let evs = store.usage_between(CLAUDE, 0, i64::MAX).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(evs[0].dedup_key.starts_with("msg_9"));
    }

    /// If the file that "won" a duplicated event is rewritten without it, the
    /// copy in an unchanged file must be restored, not lost forever.
    #[test]
    fn rewrite_of_dup_winner_restores_event_from_other_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_claude_home(tmp.path());
        let dup = line("2026-08-01T10:00:00.000Z", "msg_1", "req_1", SPLIT_USAGE);
        let other = line("2026-08-01T11:00:00.000Z", "msg_2", "req_2", UNSPLIT_USAGE);
        // walk order: proj-a first → its copy of msg_1 wins
        write_transcript(tmp.path(), "proj-a/a.jsonl", &format!("{dup}{other}"));
        write_transcript(tmp.path(), "proj-b/b.jsonl", &dup);
        let mut store = crate::store::Store::open(&tmp.path().join("m.db")).unwrap();
        scan(&mut store, &cfg).unwrap();
        assert_eq!(store.usage_between(CLAUDE, 0, i64::MAX).unwrap().len(), 2);

        // proj-a shrinks to only msg_2: the stored msg_1 (sourced to proj-a)
        // is deleted, but proj-b still holds a copy.
        std::fs::write(tmp.path().join("projects/proj-a/a.jsonl"), &other).unwrap();
        let sum = scan(&mut store, &cfg).unwrap();
        assert!(sum
            .warnings
            .iter()
            .any(|w| w.contains("full Claude rescan")));
        let evs = store.usage_between(CLAUDE, 0, i64::MAX).unwrap();
        assert_eq!(evs.len(), 2, "msg_1 must be restored from proj-b");
        assert!(evs
            .iter()
            .any(|e| e.dedup_key.starts_with("msg_1") && e.source_file.contains("proj-b")));
    }

    /// A live session's partial tail line must not be consumed; the rescan
    /// after it completes picks it up exactly once.
    #[test]
    fn incremental_resume_after_partial_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_claude_home(tmp.path());
        let full = line("2026-08-01T10:00:00.000Z", "msg_1", "req_1", SPLIT_USAGE);
        let tail = line("2026-08-01T10:01:00.000Z", "msg_2", "req_2", SPLIT_USAGE);
        let (head, rest) = tail.split_at(40); // mid-JSON, no newline yet
        write_transcript(tmp.path(), "proj-a/live.jsonl", &format!("{full}{head}"));

        let mut store = crate::store::Store::open(&tmp.path().join("m.db")).unwrap();
        scan(&mut store, &cfg).unwrap();
        assert_eq!(store.usage_between(CLAUDE, 0, i64::MAX).unwrap().len(), 1);

        // Session flushes the rest of the line plus one more turn.
        let more = line("2026-08-01T10:02:00.000Z", "msg_3", "req_3", UNSPLIT_USAGE);
        let p = tmp.path().join("projects/proj-a/live.jsonl");
        let cur = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, format!("{cur}{rest}{more}")).unwrap();

        let sum = scan(&mut store, &cfg).unwrap();
        assert_eq!(sum.parse_errors, 0);
        let evs = store.usage_between(CLAUDE, 0, i64::MAX).unwrap();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[1].input, 10);
        assert_eq!(evs[2].cache_w_unsplit, 40);
    }
}
