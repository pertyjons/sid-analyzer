//! M7 Slice 0 — baseline measurement of patch-extraction premise.
//!
//! For each subtune in a corpus, count how many distinct
//! `(adsr, waveform)` pairs the detected notes form, and how
//! many notes share their pair with at least one other note in the
//! same subtune. The aggregate `pair_coshare_fraction` is the
//! Definition of Done for Slice 0: ≥ 50–70 % across the corpus says
//! the patch-clustering premise holds; lower says reconsider it.
//!
//! Same panic-safe analyzer wrapper as `sid-corpus-scan`, but writes
//! one JSON file instead of SQLite — the output is fixture-shaped
//! and meant to be checked into `tests/fixtures/`.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufWriter, Write, stderr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use serde::Serialize;
use walkdir::WalkDir;

use sid_analyzer::analysis::effects::{EffectSpan, EffectThresholds, detect_effects};
use sid_analyzer::analysis::note::{NoteEvent, detect_notes};
use sid_analyzer::analysis::voice::Adsr;
use sid_analyzer::analysis::{FrameState, SystemClock, analyze};
use sid_analyzer::emu;
use sid_analyzer::header::{self, Header, MAX_SUBTUNES, SubtuneIndex};

const DEFAULT_FRAMES: u32 = 1500;
const PROGRESS_INTERVAL: usize = 50;
/// First 4 frames of each note's waveform — the "shape" of the attack.
const WAVE_SEQ_LEN: usize = 4;

#[derive(Parser, Debug)]
#[command(
    name = "m7-baseline",
    about = "Measure how much per-note structure SID corpora share — Slice 0 of the M7 plan.",
    long_about = "Walks --corpus recursively for .sid files, runs the standard analyzer pipeline \
on each subtune, and reports per-subtune statistics on (adsr, waveform) pair counts plus an \
aggregate co-share fraction. RSID files are skipped (Kernal/BASIC ROM stubs not implemented).\n\n\
The aggregate co-share fraction is the headline number: it is the DoD signal for Slice 0. \
≥ 50–70 % over a mixed Hubbard/Galway/Tel subset means patches will compress; \
lower means the premise needs reconsideration.\n\n\
Example:\n  m7-baseline --corpus assets/music --out tests/fixtures/m7_baseline.json"
)]
struct Cli {
    /// Directory tree to scan recursively for .sid files.
    #[arg(long)]
    corpus: PathBuf,

    /// Where to write the JSON report.
    #[arg(long)]
    out: PathBuf,

    /// Number of play frames per subtune. 1500 ≈ 30 s at PAL.
    #[arg(long, default_value_t = DEFAULT_FRAMES)]
    frames: u32,

    /// Which subtunes of each file to analyze.
    #[arg(long, value_enum, default_value_t = SubtuneMode::All)]
    subtunes: SubtuneMode,

    /// Worker threads for analysis. Defaults to available parallelism.
    #[arg(long)]
    workers: Option<usize>,

    /// Stop after scanning this many files (smoke testing).
    #[arg(long)]
    limit: Option<usize>,

    /// Write `--out` with two-space indentation instead of compact JSON.
    #[arg(long)]
    pretty: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SubtuneMode {
    Start,
    All,
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("walking {root}: {source}")]
    Walk {
        root: PathBuf,
        #[source]
        source: walkdir::Error,
    },
    #[error("opening {path} for writing: {source}")]
    OpenOut {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("creating parent directory {path}: {source}")]
    MkdirOut {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("serializing report: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("writing report: {0}")]
    Write(#[from] std::io::Error),
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

fn run(cli: &Cli) -> Result<(), AppError> {
    let start = Instant::now();
    eprintln!(
        "scanning {corpus} (frames={frames}, subtunes={mode:?})",
        corpus = cli.corpus.display(),
        frames = cli.frames,
        mode = cli.subtunes,
    );

    let files = collect_sid_files(&cli.corpus, cli.limit)?;
    eprintln!("found {n} .sid files", n = files.len());

    let processed = Arc::new(AtomicUsize::new(0));
    let total = files.len();

    let opts = AnalysisOpts {
        frames: cli.frames,
        mode: cli.subtunes,
    };
    let corpus_root = cli.corpus.clone();

    let file_results: Vec<FileResult> = files
        .into_par_iter()
        .map(|abs_path| {
            let rel = abs_path
                .strip_prefix(&corpus_root)
                .unwrap_or(&abs_path)
                .to_path_buf();
            let result = analyze_file(&abs_path, &rel, opts);

            let done = processed.fetch_add(1, Ordering::Relaxed) + 1;
            if done.is_multiple_of(PROGRESS_INTERVAL) {
                let _ = writeln!(
                    stderr(),
                    "  [{done}/{total}] elapsed={elapsed:.1}s",
                    elapsed = start.elapsed().as_secs_f32(),
                );
            }
            result
        })
        .collect();

    let mut subtune_stats: Vec<SubtuneStats> =
        file_results.into_iter().flat_map(|f| f.subtunes).collect();
    subtune_stats.sort_by(|a, b| (&a.file, a.subtune).cmp(&(&b.file, b.subtune)));

    let aggregate = AggregateStats::from_subtunes(&subtune_stats);

    let report = BaselineReport {
        scanner_version: env!("CARGO_PKG_VERSION").to_string(),
        scanned_at: now_unix_secs(),
        corpus_root: cli.corpus.to_string_lossy().into_owned(),
        frames_per_subtune: cli.frames,
        subtune_count: subtune_stats.len() as u32,
        aggregate,
        subtunes: subtune_stats,
    };

    write_report(&cli.out, &report, cli.pretty)?;
    eprintln!(
        "wrote {path} ({n} subtunes) in {elapsed:.1}s — pair_coshare={share:.1}%",
        path = cli.out.display(),
        n = report.subtune_count,
        elapsed = start.elapsed().as_secs_f32(),
        share = report.aggregate.mean_pair_coshare_fraction * 100.0,
    );
    Ok(())
}

#[derive(Copy, Clone, Debug)]
struct AnalysisOpts {
    frames: u32,
    mode: SubtuneMode,
}

#[derive(Debug)]
struct FileResult {
    subtunes: Vec<SubtuneStats>,
}

fn analyze_file(abs_path: &Path, rel_path: &Path, opts: AnalysisOpts) -> FileResult {
    let rel_str = rel_path.to_string_lossy().into_owned();

    let bytes = match fs::read(abs_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("  skip {rel_str}: read error: {e}");
            return FileResult { subtunes: vec![] };
        }
    };

    let header = match header::parse(&bytes) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("  skip {rel_str}: header error: {e}");
            return FileResult { subtunes: vec![] };
        }
    };

    if header.format == header::Format::Rsid {
        return FileResult { subtunes: vec![] };
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
        .filter_map(|s| analyze_subtune(&header, &bytes, s, clock, opts.frames, &rel_str))
        .collect();

    FileResult { subtunes }
}

fn analyze_subtune(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    clock: SystemClock,
    frames: u32,
    rel_path: &str,
) -> Option<SubtuneStats> {
    let trace_result = catch_unwind(AssertUnwindSafe(|| {
        emu::run(header, bytes, subtune, frames)
    }));
    let trace = match trace_result {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            eprintln!("  skip {rel_path}#{}: emu: {e}", subtune.0);
            return None;
        }
        Err(_) => {
            eprintln!("  skip {rel_path}#{}: emu panic", subtune.0);
            return None;
        }
    };

    let states = analyze(&trace);
    let notes = detect_notes(&states, clock);
    let spans = detect_effects(&trace, &states, EffectThresholds::default());

    Some(SubtuneStats::compute(
        rel_path.to_string(),
        subtune,
        header.author.clone(),
        &notes,
        &states,
        &spans,
    ))
}

/// Per-subtune aggregate.
#[derive(Debug, Serialize)]
struct SubtuneStats {
    file: String,
    subtune: u16,
    author: String,

    note_count: u32,

    /// (Adsr-byte-pair, first-waveform-byte) — the simplest patch key.
    distinct_pair_count: u32,
    /// Of all notes, the fraction whose `(adsr, first_wave)` is shared
    /// with at least one other note in the same subtune.
    pair_coshare_fraction: f32,
    /// Bucketed distribution: `pair_group_sizes[i]` = count of notes
    /// whose `(adsr, first_wave)` group has size `i+1`. Index 0 is
    /// the "singleton" bucket — notes with no match.
    pair_group_sizes: Vec<u32>,

    /// Stricter key: `(adsr, first 4 waveform-bytes)`.
    distinct_waveseq4_count: u32,
    waveseq4_coshare_fraction: f32,

    /// Notes overlapping ≥ 1 EffectSpan (any kind, any voice).
    notes_with_any_effect: u32,
    /// Notes where ≥ 1 EffectSpan covers the full note range
    /// (`[start_frame, end_frame]`).
    notes_fully_covered_by_effect: u32,
    /// Notes overlapped by an EffectSpan that doesn't cover the
    /// full range.
    notes_partially_covered: u32,
}

impl SubtuneStats {
    fn compute(
        file: String,
        subtune: SubtuneIndex,
        author: String,
        notes: &[NoteEvent],
        states: &[FrameState],
        spans: &[EffectSpan],
    ) -> Self {
        let note_count = notes.len() as u32;
        let mut pair_groups: HashMap<(u16, u8), u32> = HashMap::new();
        let mut waveseq4_groups: HashMap<(u16, [u8; WAVE_SEQ_LEN]), u32> = HashMap::new();

        for note in notes {
            let (adsr_key, first_wave, wave_seq) = extract_keys(note, states);
            *pair_groups.entry((adsr_key, first_wave)).or_default() += 1;
            *waveseq4_groups.entry((adsr_key, wave_seq)).or_default() += 1;
        }

        let (pair_coshare_fraction, pair_group_sizes) =
            coshare_and_histogram(&pair_groups, note_count);
        let (waveseq4_coshare_fraction, _) = coshare_and_histogram(&waveseq4_groups, note_count);

        let (any, full, partial) = effect_coverage(notes, spans);

        Self {
            file,
            subtune: subtune.0,
            author,
            note_count,
            distinct_pair_count: pair_groups.len() as u32,
            pair_coshare_fraction,
            pair_group_sizes,
            distinct_waveseq4_count: waveseq4_groups.len() as u32,
            waveseq4_coshare_fraction,
            notes_with_any_effect: any,
            notes_fully_covered_by_effect: full,
            notes_partially_covered: partial,
        }
    }
}

/// Pack `Adsr` into a single `u16` for use as a hash key: high byte =
/// AD register, low byte = SR register.
fn adsr_key(adsr: Adsr) -> u16 {
    let (ad, sr) = adsr.to_bytes();
    (u16::from(ad) << 8) | u16::from(sr)
}

/// For a note, pull its `(adsr_key, first_wave_byte, first_4_wave_bytes)`
/// from the per-frame state vector.
fn extract_keys(note: &NoteEvent, states: &[FrameState]) -> (u16, u8, [u8; WAVE_SEQ_LEN]) {
    let voice_idx = note.voice.to_index();
    let start = note.start_frame.0 as usize;
    let end = note
        .end_frame
        .map_or(states.len(), |f| (f.0 as usize).min(states.len()));

    let start_state = states.get(start);
    let adsr = start_state.map_or(0, |s| adsr_key(s.voices[voice_idx].adsr));
    let first_wave = start_state.map_or(0, |s| {
        s.voices[voice_idx].control.waveform.to_control_byte()
    });

    let mut wave_seq = [0u8; WAVE_SEQ_LEN];
    for (i, slot) in wave_seq.iter_mut().enumerate() {
        let idx = start + i;
        if idx < end
            && let Some(state) = states.get(idx)
        {
            *slot = state.voices[voice_idx].control.waveform.to_control_byte();
        }
    }

    (adsr, first_wave, wave_seq)
}

/// Given a `key → count` map, compute the co-share fraction and the
/// group-size histogram. `group_sizes[i]` = number of notes in a
/// group of size `i+1`.
fn coshare_and_histogram<K>(groups: &HashMap<K, u32>, note_count: u32) -> (f32, Vec<u32>) {
    if note_count == 0 {
        return (0.0, Vec::new());
    }
    let mut max_group = 0u32;
    for &g in groups.values() {
        max_group = max_group.max(g);
    }
    let mut sizes = vec![0u32; max_group as usize];
    let mut shared_notes = 0u32;
    for &g in groups.values() {
        if g >= 1 {
            sizes[g as usize - 1] += g;
        }
        if g >= 2 {
            shared_notes += g;
        }
    }
    let fraction = shared_notes as f32 / note_count as f32;
    (fraction, sizes)
}

fn effect_coverage(notes: &[NoteEvent], spans: &[EffectSpan]) -> (u32, u32, u32) {
    let mut any = 0u32;
    let mut full = 0u32;
    let mut partial = 0u32;

    for note in notes {
        let start = note.start_frame.0;
        let Some(end_frame) = note.end_frame else {
            continue;
        };
        // The note's `end_frame` is exclusive; span ends are inclusive, so
        // overlap/coverage compare against the note's last active frame.
        if end_frame.0 <= start {
            continue;
        }
        let last = end_frame.0 - 1;

        let mut has_overlap = false;
        let mut has_full = false;
        for span in spans {
            let s = span.start_frame.0;
            let e = span.end_frame.0;
            if s > last || e < start {
                continue;
            }
            has_overlap = true;
            if s <= start && e >= last {
                has_full = true;
                break;
            }
        }
        if has_overlap {
            any += 1;
            if has_full {
                full += 1;
            } else {
                partial += 1;
            }
        }
    }
    (any, full, partial)
}

#[derive(Debug, Serialize)]
struct BaselineReport {
    scanner_version: String,
    scanned_at: i64,
    corpus_root: String,
    frames_per_subtune: u32,
    subtune_count: u32,
    aggregate: AggregateStats,
    subtunes: Vec<SubtuneStats>,
}

#[derive(Debug, Serialize)]
struct AggregateStats {
    total_notes: u64,
    mean_note_count: f32,
    mean_distinct_pair_count: f32,
    /// Weighted mean: total shared notes / total notes. The DoD metric.
    mean_pair_coshare_fraction: f32,
    mean_waveseq4_coshare_fraction: f32,
    mean_effect_coverage_any: f32,
    mean_effect_coverage_full: f32,
}

impl AggregateStats {
    fn from_subtunes(subtunes: &[SubtuneStats]) -> Self {
        if subtunes.is_empty() {
            return Self {
                total_notes: 0,
                mean_note_count: 0.0,
                mean_distinct_pair_count: 0.0,
                mean_pair_coshare_fraction: 0.0,
                mean_waveseq4_coshare_fraction: 0.0,
                mean_effect_coverage_any: 0.0,
                mean_effect_coverage_full: 0.0,
            };
        }
        let n = subtunes.len() as f64;
        let total_notes: u64 = subtunes.iter().map(|s| s.note_count as u64).sum();
        let total_shared_pair: u64 = subtunes
            .iter()
            .map(|s| (s.pair_coshare_fraction as f64 * s.note_count as f64) as u64)
            .sum();
        let total_shared_seq4: u64 = subtunes
            .iter()
            .map(|s| (s.waveseq4_coshare_fraction as f64 * s.note_count as f64) as u64)
            .sum();
        let total_any: u64 = subtunes
            .iter()
            .map(|s| s.notes_with_any_effect as u64)
            .sum();
        let total_full: u64 = subtunes
            .iter()
            .map(|s| s.notes_fully_covered_by_effect as u64)
            .sum();

        let div_by_total = |x: u64| {
            if total_notes == 0 {
                0.0
            } else {
                x as f32 / total_notes as f32
            }
        };

        Self {
            total_notes,
            mean_note_count: (total_notes as f64 / n) as f32,
            mean_distinct_pair_count: subtunes
                .iter()
                .map(|s| s.distinct_pair_count as f64)
                .sum::<f64>() as f32
                / n as f32,
            mean_pair_coshare_fraction: div_by_total(total_shared_pair),
            mean_waveseq4_coshare_fraction: div_by_total(total_shared_seq4),
            mean_effect_coverage_any: div_by_total(total_any),
            mean_effect_coverage_full: div_by_total(total_full),
        }
    }
}

fn write_report(path: &Path, report: &BaselineReport, pretty: bool) -> Result<(), AppError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| AppError::MkdirOut {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let file = fs::File::create(path).map_err(|source| AppError::OpenOut {
        path: path.to_path_buf(),
        source,
    })?;
    let mut w = BufWriter::new(file);
    if pretty {
        serde_json::to_writer_pretty(&mut w, report)?;
    } else {
        serde_json::to_writer(&mut w, report)?;
    }
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
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

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adsr_key_packs_ad_register_then_sr_register() {
        // Galway-bass: attack=5, decay=9, sustain=E, release=6.
        let a = Adsr {
            attack: 0x5,
            decay: 0x9,
            sustain: 0xE,
            release: 0x6,
        };
        assert_eq!(adsr_key(a), 0x59E6);
    }

    #[test]
    fn coshare_and_histogram_aggregates_groups() {
        // 3 notes share key A, 2 share key B, 1 lonely note in C → total 6.
        let mut groups: HashMap<u8, u32> = HashMap::new();
        groups.insert(b'A', 3);
        groups.insert(b'B', 2);
        groups.insert(b'C', 1);
        let (fraction, sizes) = coshare_and_histogram(&groups, 6);
        // 5 of 6 notes (the 3 in A + the 2 in B) are in shared groups.
        assert!((fraction - 5.0 / 6.0).abs() < 1e-6, "got {fraction}");
        // sizes[0] = 1 (the lonely C), sizes[1] = 2 (the pair B),
        // sizes[2] = 3 (the triple A).
        assert_eq!(sizes, vec![1, 2, 3]);
    }

    #[test]
    fn coshare_handles_empty_input() {
        let groups: HashMap<u8, u32> = HashMap::new();
        let (fraction, sizes) = coshare_and_histogram(&groups, 0);
        assert_eq!(fraction, 0.0);
        assert!(sizes.is_empty());
    }
}
