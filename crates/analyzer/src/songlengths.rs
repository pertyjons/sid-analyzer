use md5::{Digest, Md5};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::time::Duration;

/// Compute the HVSC SID-MD5 — the hash HVSC's `Songlengths.md5` is keyed
/// on. Since HVSC #71 this is simply MD5 over the **full file contents**
/// (header + data), per the HVSC Songlengths FAQ ("THE NEW FORMAT").
///
/// The pre-#71 "old format" used a different, multi-step hash derived
/// from the parsed header — not supported here. If you have an
/// `assets/Songlengths.txt` in the old format, regenerate the modern
/// `Songlengths.md5` from a current HVSC distribution instead.
#[must_use]
pub fn compute_sid_md5(bytes: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Parsed HVSC Songlengths database: MD5 → list of per-subtune durations.
pub struct SongLengths {
    map: HashMap<[u8; 16], Vec<Duration>>,
}

impl SongLengths {
    /// Parse an HVSC `Songlengths.md5` file. Comment lines (`;`), section
    /// headers (`[...]`), and blank lines are skipped. Each data line is
    /// `<32-hex md5>=<times>` where times are space-separated `MM:SS` or
    /// `MM:SS.fff` tokens, optionally followed by a `(X)` flag that's
    /// stripped.
    pub fn load(path: &Path) -> io::Result<Self> {
        let reader = BufReader::new(File::open(path)?);
        let mut map: HashMap<[u8; 16], Vec<Duration>> = HashMap::new();
        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('[') {
                continue;
            }
            let Some((hash_str, times_str)) = line.split_once('=') else {
                continue;
            };
            let Some(hash) = parse_md5_hex(hash_str.trim()) else {
                continue;
            };
            let Some(times) = times_str
                .split_whitespace()
                .map(parse_duration_token)
                .collect::<Option<Vec<Duration>>>()
            else {
                continue;
            };
            if !times.is_empty() {
                map.insert(hash, times);
            }
        }
        Ok(Self { map })
    }

    #[must_use]
    pub fn lookup(&self, md5: &[u8; 16]) -> Option<&[Duration]> {
        self.map.get(md5).map(Vec::as_slice)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

fn parse_md5_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0_u8; 16];
    for i in 0..16 {
        let hi = hex_digit(bytes[2 * i])?;
        let lo = hex_digit(bytes[2 * i + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn parse_duration_token(tok: &str) -> Option<Duration> {
    // Strip optional "(X...)" flag suffix used by HVSC entries.
    let core = tok.split_once('(').map_or(tok, |(t, _)| t);
    let (m_str, s_str) = core.split_once(':')?;
    let mins: u64 = m_str.parse().ok()?;
    let secs: f64 = s_str.parse().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(mins as f64 * 60.0 + secs).ok()
}

/// Format a duration as `M:SS` (truncated to whole seconds).
#[must_use]
pub fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    format!("{}:{:02}", total / 60, total % 60)
}
