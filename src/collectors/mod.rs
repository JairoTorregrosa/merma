pub mod claude_jsonl;
pub mod claude_oauth;
pub mod codex_live;
pub mod codex_rollout;
pub mod statusline;

/// Result of one incremental scan pass over a source.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ScanSummary {
    pub files_seen: usize,
    pub files_parsed: usize,
    pub events_inserted: usize,
    pub snapshots_inserted: usize,
    pub parse_errors: usize,
    /// Non-fatal anomalies worth surfacing (loudness without aborting a whole scan).
    pub warnings: Vec<String>,
}

impl ScanSummary {
    pub fn absorb(&mut self, other: ScanSummary) {
        self.files_seen += other.files_seen;
        self.files_parsed += other.files_parsed;
        self.events_inserted += other.events_inserted;
        self.snapshots_inserted += other.snapshots_inserted;
        self.parse_errors += other.parse_errors;
        self.warnings.extend(other.warnings);
    }
}

pub(crate) fn iso_to_epoch(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
}

/// FNV-1a of a file's first `PREFIX_HASH_LEN` bytes. Incremental resume must
/// prove the on-disk file is still the same log it left off in — size+mtime
/// alone accept same-size rewrites and mtime has 1-second resolution.
pub(crate) const PREFIX_HASH_LEN: usize = 256;

/// FNV-1a over the first `min(cap, PREFIX_HASH_LEN)` bytes. `cap` is the file
/// size at hash time, so a short file that later grows past the prefix length
/// can still be re-verified over the same byte range.
pub(crate) fn prefix_hash(path: &std::path::Path, cap: i64) -> std::io::Result<u64> {
    use std::io::Read;
    let want = (cap.max(0) as usize).min(PREFIX_HASH_LEN);
    let mut buf = vec![0u8; want];
    let mut f = std::fs::File::open(path)?;
    let mut filled = 0usize;
    while filled < want {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in &buf[..filled] {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(h)
}

/// True when it is safe to resume `path` at `byte_offset`: the stored prefix
/// hash (computed when the file was `prior_size` bytes) still matches and the
/// byte before the offset is a newline — i.e. the offset still sits on a line
/// boundary of the same log.
pub(crate) fn resume_is_safe(
    path: &std::path::Path,
    stored_hash: Option<u64>,
    prior_size: i64,
    byte_offset: i64,
) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    match stored_hash {
        // Pre-upgrade scan state has no hash: accept once; the rescan stores it.
        None => Ok(true),
        Some(h) => {
            if prefix_hash(path, prior_size)? != h {
                return Ok(false);
            }
            if byte_offset > 0 {
                let mut f = std::fs::File::open(path)?;
                f.seek(SeekFrom::Start(byte_offset as u64 - 1))?;
                let mut b = [0u8; 1];
                if f.read(&mut b)? != 1 || b[0] != b'\n' {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
}
