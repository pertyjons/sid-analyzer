//! Corpus-scale SID scanner. Walks a directory tree of `.sid` files, runs the
//! standard analysis pipeline on each, and writes per-file and per-subtune
//! aggregates to SQLite for offline querying.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{Write, stderr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use rusqlite::{Connection, params};
use walkdir::WalkDir;

use sid_analyzer::analysis::effects::{Effect, EffectSpan, EffectThresholds, detect_effects};
use sid_analyzer::analysis::note::{MidiNote, NoteEvent, detect_notes};
use sid_analyzer::analysis::{SystemClock, VoiceId, analyze};
use sid_analyzer::emu;
use sid_analyzer::header::{self, Header, MAX_SUBTUNES, SubtuneIndex};
use sid_analyzer::songlengths::compute_sid_md5;
use sid_analyzer::support::UnsupportedInputKind;

const DEFAULT_FRAMES: u32 = 1500;
const DEFAULT_DB_PATH: &str = "corpus.sqlite";
const PROGRESS_INTERVAL: usize = 200;
const DB_BATCH_SIZE: usize = 64;

#[derive(Parser, Debug)]
#[command(
    name = "sid-corpus-scan",
    about = "Bulk-analyze a tree of SID files and write per-subtune statistics to SQLite.",
    long_about = "Walks --root recursively for .sid files, runs the standard sid-analyzer \
pipeline on each (header parse → emulator → effect detection), and stores per-file and \
per-subtune aggregates in a SQLite database. Unsupported RSID and MUS/STR inputs \
are recorded with typed skip reasons instead of being emulated.\n\n\
By default scans only the file's start_song to keep runtime bounded; use --subtunes all \
to scan every subtune. Resumable: re-running with the same --db skips files whose MD5 \
is already present (disable with --no-resume).\n\n\
Example:\n  \
sid-corpus-scan --root /path/to/HVSC --db hvsc.sqlite --workers 8"
)]
struct Cli {
    /// Directory tree to scan recursively for .sid files.
    #[arg(long)]
    root: PathBuf,

    /// SQLite database to write to. Created if it does not exist.
    #[arg(long, default_value = DEFAULT_DB_PATH)]
    db: PathBuf,

    /// Worker threads for analysis. Defaults to available parallelism.
    #[arg(long)]
    workers: Option<usize>,

    /// Number of play frames per subtune. 1500 ≈ 30 s at PAL.
    #[arg(long, default_value_t = DEFAULT_FRAMES)]
    frames: u32,

    /// Which subtunes of each file to analyze.
    #[arg(long, value_enum, default_value_t = SubtuneMode::Start)]
    subtunes: SubtuneMode,

    /// Stop after analyzing this many files (testing).
    #[arg(long)]
    limit: Option<usize>,

    /// Re-analyze files even if their MD5 is already in the database.
    #[arg(long)]
    no_resume: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SubtuneMode {
    /// Only the file's `start_song`.
    Start,
    /// Every subtune from 1..=songs.
    All,
}

#[derive(Copy, Clone, Debug)]
struct AnalysisOpts {
    frames: u32,
    mode: SubtuneMode,
}

fn main() {
    let cli = Cli::parse();

    if let Some(n) = cli.workers
        && let Err(e) = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
    {
        eprintln!("warning: could not configure thread pool: {e}");
    }

    if let Err(e) = run(&cli) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("walking {root}: {source}")]
    Walk {
        root: PathBuf,
        #[source]
        source: walkdir::Error,
    },
    #[error("opening database {path}: {source}")]
    DbOpen {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("DB writer thread panicked")]
    WriterPanicked,
}

fn run(cli: &Cli) -> Result<(), AppError> {
    let start = Instant::now();
    eprintln!(
        "scanning {root} → {db} (frames={frames}, subtunes={mode:?})",
        root = cli.root.display(),
        db = cli.db.display(),
        frames = cli.frames,
        mode = cli.subtunes
    );

    let mut conn = open_db(&cli.db)?;
    init_schema(&conn)?;
    store_meta(&conn, cli)?;

    let already_scanned: HashSet<[u8; 16]> = if cli.no_resume {
        HashSet::new()
    } else {
        load_known_md5s(&conn)?
    };
    if !already_scanned.is_empty() {
        eprintln!(
            "resume: {n} files already in DB will be skipped",
            n = already_scanned.len()
        );
    }

    let files = collect_sid_files(&cli.root, cli.limit)?;
    eprintln!("found {n} .sid files", n = files.len());

    let opts = AnalysisOpts {
        frames: cli.frames,
        mode: cli.subtunes,
    };
    let root = cli.root.clone();
    let known = Arc::new(already_scanned);

    let (tx, rx) = channel::<FileOutcome>();

    let writer_handle = thread::spawn(move || -> Result<usize, rusqlite::Error> {
        let mut buf: Vec<FileOutcome> = Vec::with_capacity(DB_BATCH_SIZE);
        let mut written = 0usize;
        for outcome in rx.iter() {
            buf.push(outcome);
            if buf.len() >= DB_BATCH_SIZE {
                write_batch(&mut conn, &buf)?;
                written += buf.len();
                buf.clear();
            }
        }
        if !buf.is_empty() {
            write_batch(&mut conn, &buf)?;
            written += buf.len();
        }
        Ok(written)
    });

    let processed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total = files.len();

    files.into_par_iter().for_each_with(tx, |tx, path| {
        let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
        if let Some(outcome) = analyze_file(&path, &rel, &known, opts) {
            let _ = tx.send(outcome);
        }
        let done = processed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if done.is_multiple_of(PROGRESS_INTERVAL) {
            let _ = writeln!(
                stderr(),
                "  [{done}/{total}] elapsed={elapsed:.1}s",
                elapsed = start.elapsed().as_secs_f32()
            );
        }
    });

    let written = writer_handle
        .join()
        .map_err(|_| AppError::WriterPanicked)??;

    eprintln!(
        "done: wrote {written} files in {elapsed:.1}s",
        elapsed = start.elapsed().as_secs_f32()
    );
    Ok(())
}

#[derive(Debug)]
struct FileOutcome {
    rel_path: String,
    md5: [u8; 16],
    size_bytes: u64,
    header: Option<Header>,
    skipped: Option<UnsupportedInputKind>,
    error: Option<String>,
    subtunes: Vec<SubtuneOutcome>,
}

impl FileOutcome {
    fn read_error(rel_path: String, message: String) -> Self {
        Self {
            rel_path,
            md5: [0; 16],
            size_bytes: 0,
            header: None,
            skipped: None,
            error: Some(message),
            subtunes: Vec::new(),
        }
    }

    fn header_error(rel_path: String, md5: [u8; 16], size: u64, message: String) -> Self {
        Self {
            rel_path,
            md5,
            size_bytes: size,
            header: None,
            skipped: None,
            error: Some(message),
            subtunes: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct SubtuneOutcome {
    subtune: SubtuneIndex,
    cia_timed: bool,
    frames_analyzed: u32,
    emu_error: Option<String>,
    note_count: u32,
    distinct_midi_notes: u32,
    min_midi_note: Option<MidiNote>,
    max_midi_note: Option<MidiNote>,
    gate_frames: [u32; 3],
    effects: Vec<EffectAggregate>,
}

#[derive(Debug)]
struct EffectAggregate {
    effect: Effect,
    voice: Option<VoiceId>,
    span_count: u32,
    total_frames: u32,
}

fn analyze_file(
    abs_path: &Path,
    rel_path: &Path,
    known_md5s: &HashSet<[u8; 16]>,
    opts: AnalysisOpts,
) -> Option<FileOutcome> {
    let rel_str = rel_path.to_string_lossy().into_owned();

    let bytes = match fs::read(abs_path) {
        Ok(b) => b,
        Err(e) => return Some(FileOutcome::read_error(rel_str, format!("read: {e}"))),
    };

    let md5 = compute_sid_md5(&bytes);
    if !known_md5s.is_empty() && known_md5s.contains(&md5) {
        return None;
    }

    let size = bytes.len() as u64;
    let header = match header::parse(&bytes) {
        Ok(h) => h,
        Err(e) => {
            return Some(FileOutcome::header_error(
                rel_str,
                md5,
                size,
                format!("header: {e}"),
            ));
        }
    };

    if let Some(reason) = UnsupportedInputKind::classify(&header) {
        return Some(FileOutcome {
            rel_path: rel_str,
            md5,
            size_bytes: size,
            header: Some(header),
            skipped: Some(reason),
            error: None,
            subtunes: Vec::new(),
        });
    }

    let clock = SystemClock::from(header.flags.clock);
    let subtune_indices: Vec<SubtuneIndex> = match opts.mode {
        SubtuneMode::Start => vec![SubtuneIndex(header.start_song.0.max(1))],
        SubtuneMode::All => (1..=header.songs.0.min(MAX_SUBTUNES.0))
            .map(SubtuneIndex)
            .collect(),
    };

    let subtunes = subtune_indices
        .into_iter()
        .map(|s| analyze_subtune(&header, &bytes, s, clock, opts.frames))
        .collect();

    Some(FileOutcome {
        rel_path: rel_str,
        md5,
        size_bytes: size,
        header: Some(header),
        skipped: None,
        error: None,
        subtunes,
    })
}

fn analyze_subtune(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    clock: SystemClock,
    frames: u32,
) -> SubtuneOutcome {
    let cia_timed = header.is_cia_timed(subtune);

    let trace_result = catch_unwind(AssertUnwindSafe(|| {
        emu::run(header, bytes, subtune, frames)
    }));

    let (trace, emu_error) = match trace_result {
        Ok(Ok(t)) => (Some(t), None),
        Ok(Err(e)) => (None, Some(format!("{e}"))),
        Err(_) => (None, Some("emulator panic".to_string())),
    };

    let Some(trace) = trace else {
        return SubtuneOutcome {
            subtune,
            cia_timed,
            frames_analyzed: 0,
            emu_error,
            note_count: 0,
            distinct_midi_notes: 0,
            min_midi_note: None,
            max_midi_note: None,
            gate_frames: [0; 3],
            effects: Vec::new(),
        };
    };

    let states = analyze(&trace);
    let notes = detect_notes(&states, clock);
    let spans = detect_effects(&trace, &states, EffectThresholds::default());

    let mut gate_frames = [0u32; 3];
    for st in &states {
        for (i, v) in st.voices.iter().enumerate() {
            if v.control.gate {
                gate_frames[i] += 1;
            }
        }
    }

    let (distinct_midi_notes, min_midi_note, max_midi_note) = note_stats(&notes);

    SubtuneOutcome {
        subtune,
        cia_timed,
        frames_analyzed: trace.frames.len() as u32,
        emu_error: None,
        note_count: notes.len() as u32,
        distinct_midi_notes,
        min_midi_note,
        max_midi_note,
        gate_frames,
        effects: aggregate_spans(&spans),
    }
}

fn note_stats(notes: &[NoteEvent]) -> (u32, Option<MidiNote>, Option<MidiNote>) {
    let mut mask: u128 = 0;
    let mut min_n: Option<u8> = None;
    let mut max_n: Option<u8> = None;
    for n in notes {
        let v = n.midi.0;
        mask |= 1u128 << (v & 0x7F);
        min_n = Some(min_n.map_or(v, |m| m.min(v)));
        max_n = Some(max_n.map_or(v, |m| m.max(v)));
    }
    (mask.count_ones(), min_n.map(MidiNote), max_n.map(MidiNote))
}

const VOICE_SLOTS: usize = 4;
const AGG_SLOTS: usize = Effect::ALL.len() * VOICE_SLOTS;

fn aggregate_spans(spans: &[EffectSpan]) -> Vec<EffectAggregate> {
    let mut acc = [(0u32, 0u32); AGG_SLOTS];
    for span in spans {
        let slot = agg_slot(span.effect, span.voice);
        let len = span.end_frame.0.saturating_sub(span.start_frame.0) + 1;
        acc[slot].0 += 1;
        acc[slot].1 += len;
    }
    let mut out = Vec::new();
    for (effect_idx, effect) in Effect::ALL.iter().copied().enumerate() {
        for voice_idx in 0..VOICE_SLOTS {
            let (span_count, total_frames) = acc[effect_idx * VOICE_SLOTS + voice_idx];
            if span_count == 0 {
                continue;
            }
            out.push(EffectAggregate {
                effect,
                voice: voice_id_for_slot(voice_idx),
                span_count,
                total_frames,
            });
        }
    }
    out
}

fn agg_slot(effect: Effect, voice: Option<VoiceId>) -> usize {
    (effect as usize) * VOICE_SLOTS + voice.map_or(0, |v| v.0 as usize)
}

fn voice_id_for_slot(slot: usize) -> Option<VoiceId> {
    if slot == 0 {
        None
    } else {
        Some(VoiceId(slot as u8))
    }
}

fn collect_sid_files(root: &Path, limit: Option<usize>) -> Result<Vec<PathBuf>, AppError> {
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|source| AppError::Walk {
            root: root.to_path_buf(),
            source,
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .path()
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(OsStr::new("sid")))
        {
            out.push(entry.into_path());
            if let Some(n) = limit
                && out.len() >= n
            {
                break;
            }
        }
    }
    out.sort();
    Ok(out)
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL,
    md5_hex TEXT NOT NULL UNIQUE,
    size_bytes INTEGER NOT NULL,
    format TEXT,
    version INTEGER,
    songs INTEGER,
    start_song INTEGER,
    clock TEXT,
    sid_model TEXT,
    second_sid_addr INTEGER,
    third_sid_addr INTEGER,
    title TEXT,
    author TEXT,
    released TEXT,
    speed_bitmask INTEGER,
    cia_subtunes INTEGER,
    skip_kind TEXT,
    skip_detail TEXT,
    error TEXT,
    scanned_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_files_author ON files(author);
CREATE INDEX IF NOT EXISTS idx_files_skip_kind ON files(skip_kind);

CREATE TABLE IF NOT EXISTS subtunes (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    subtune INTEGER NOT NULL,
    cia_timed INTEGER NOT NULL,
    frames_analyzed INTEGER NOT NULL,
    emu_error TEXT,
    note_count INTEGER NOT NULL,
    distinct_midi_notes INTEGER NOT NULL,
    min_midi_note INTEGER,
    max_midi_note INTEGER,
    v1_gate_frames INTEGER NOT NULL,
    v2_gate_frames INTEGER NOT NULL,
    v3_gate_frames INTEGER NOT NULL,
    UNIQUE(file_id, subtune)
);

CREATE INDEX IF NOT EXISTS idx_subtunes_file ON subtunes(file_id);

CREATE TABLE IF NOT EXISTS effect_counts (
    id INTEGER PRIMARY KEY,
    subtune_id INTEGER NOT NULL REFERENCES subtunes(id) ON DELETE CASCADE,
    effect TEXT NOT NULL,
    voice INTEGER,
    span_count INTEGER NOT NULL,
    total_frames INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_effect_counts_subtune ON effect_counts(subtune_id);
CREATE INDEX IF NOT EXISTS idx_effect_counts_effect ON effect_counts(effect);
";

fn open_db(path: &Path) -> Result<Connection, AppError> {
    let conn = Connection::open(path).map_err(|source| AppError::DbOpen {
        path: path.to_path_buf(),
        source,
    })?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(())
}

fn store_meta(conn: &Connection, cli: &Cli) -> Result<(), AppError> {
    let mut stmt = conn.prepare(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
    )?;
    stmt.execute(params!["schema_version", "2"])?;
    stmt.execute(params!["scanner_version", env!("CARGO_PKG_VERSION")])?;
    stmt.execute(params!["frames", cli.frames.to_string()])?;
    stmt.execute(params!["subtune_mode", format!("{:?}", cli.subtunes)])?;
    stmt.execute(params!["last_scan_root", cli.root.to_string_lossy()])?;
    stmt.execute(params!["last_scan_at", now_unix_secs().to_string()])?;
    Ok(())
}

fn load_known_md5s(conn: &Connection) -> Result<HashSet<[u8; 16]>, AppError> {
    let mut stmt = conn.prepare("SELECT md5_hex FROM files")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = HashSet::new();
    for r in rows {
        if let Some(bytes) = parse_md5_hex(&r?) {
            out.insert(bytes);
        }
    }
    Ok(out)
}

fn parse_md5_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = s.get(i * 2..i * 2 + 2)?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn write_batch(conn: &mut Connection, batch: &[FileOutcome]) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    let now = now_unix_secs();

    {
        let mut insert_file = tx.prepare(
            "INSERT INTO files(
                path, md5_hex, size_bytes, format, version, songs, start_song,
                clock, sid_model, second_sid_addr, third_sid_addr,
                title, author, released, speed_bitmask, cia_subtunes,
                skip_kind, skip_detail, error, scanned_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
             ON CONFLICT(md5_hex) DO UPDATE SET
                path=excluded.path,
                size_bytes=excluded.size_bytes,
                format=excluded.format,
                version=excluded.version,
                songs=excluded.songs,
                start_song=excluded.start_song,
                clock=excluded.clock,
                sid_model=excluded.sid_model,
                second_sid_addr=excluded.second_sid_addr,
                third_sid_addr=excluded.third_sid_addr,
                title=excluded.title,
                author=excluded.author,
                released=excluded.released,
                speed_bitmask=excluded.speed_bitmask,
                cia_subtunes=excluded.cia_subtunes,
                skip_kind=excluded.skip_kind,
                skip_detail=excluded.skip_detail,
                error=excluded.error,
                scanned_at=excluded.scanned_at
             RETURNING id",
        )?;
        let mut delete_subtunes = tx.prepare("DELETE FROM subtunes WHERE file_id = ?1")?;
        let mut insert_subtune = tx.prepare(
            "INSERT INTO subtunes(
                file_id, subtune, cia_timed, frames_analyzed, emu_error,
                note_count, distinct_midi_notes, min_midi_note, max_midi_note,
                v1_gate_frames, v2_gate_frames, v3_gate_frames)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        let mut insert_effect = tx.prepare(
            "INSERT INTO effect_counts(
                subtune_id, effect, voice, span_count, total_frames)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;

        for outcome in batch {
            let h = outcome.header.as_ref();
            let file_id: i64 = insert_file.query_one(
                params![
                    outcome.rel_path,
                    format_md5_hex(&outcome.md5),
                    outcome.size_bytes as i64,
                    h.map(|h| h.format.to_string()),
                    h.map(|h| h.version as i64),
                    h.map(|h| h.songs.0 as i64),
                    h.map(|h| h.start_song.0 as i64),
                    h.map(|h| h.flags.clock.to_string()),
                    h.map(|h| h.flags.sid_model.to_string()),
                    h.and_then(|h| h.second_sid_address.map(|a| a.0 as i64)),
                    h.and_then(|h| h.third_sid_address.map(|a| a.0 as i64)),
                    h.map(|h| h.name.as_str()),
                    h.map(|h| h.author.as_str()),
                    h.map(|h| h.released.as_str()),
                    h.map(|h| h.speed.0 as i64),
                    h.map(|h| h.speed.0.count_ones() as i64),
                    outcome.skipped.map(UnsupportedInputKind::code),
                    outcome.skipped.map(UnsupportedInputKind::detail),
                    outcome.error,
                    now,
                ],
                |r| r.get(0),
            )?;

            delete_subtunes.execute(params![file_id])?;

            for sub in &outcome.subtunes {
                insert_subtune.execute(params![
                    file_id,
                    sub.subtune.0 as i64,
                    if sub.cia_timed { 1 } else { 0 },
                    sub.frames_analyzed as i64,
                    sub.emu_error,
                    sub.note_count as i64,
                    sub.distinct_midi_notes as i64,
                    sub.min_midi_note.map(|n| n.0 as i64),
                    sub.max_midi_note.map(|n| n.0 as i64),
                    sub.gate_frames[0] as i64,
                    sub.gate_frames[1] as i64,
                    sub.gate_frames[2] as i64,
                ])?;
                let subtune_id = tx.last_insert_rowid();

                for eff in &sub.effects {
                    insert_effect.execute(params![
                        subtune_id,
                        eff.effect.to_string(),
                        eff.voice.map(|v| v.0 as i64),
                        eff.span_count as i64,
                        eff.total_frames as i64,
                    ])?;
                }
            }
        }
    }

    tx.commit()?;
    Ok(())
}

fn format_md5_hex(md5: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in md5 {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}
