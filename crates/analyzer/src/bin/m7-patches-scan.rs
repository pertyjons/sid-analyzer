//! M7 Slice 2 scan: run the patch extractor across a SID corpus and
//! report per-subtune coverage + patch breakdowns.
//!
//! Same corpus-walk shape as `m7-baseline` but the downstream step is
//! `extract_characteristics` → `extract_patches`, not the raw key
//! co-share counter. Designed for ad-hoc exploration ahead of Slice 5:
//! "does v1 hold up across composers, not just Hubbard?"

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use sid_analyzer::analysis::effects::{EffectThresholds, detect_effects};
use sid_analyzer::analysis::note::detect_notes;
use sid_analyzer::analysis::timbre::{HardwareTrick, Patch, extract_timbre};
use sid_analyzer::analysis::{SystemClock, analyze};
use sid_analyzer::emu;
use sid_analyzer::header::{self, Header, MAX_SUBTUNES, SubtuneIndex};
use walkdir::WalkDir;

const DEFAULT_FRAMES: u32 = 3000;
const DEFAULT_MAX_ROWS: usize = 30;
const PROGRESS_INTERVAL: usize = 100;
const HEARTBEAT_SECS: u64 = 3;

#[derive(Parser, Debug)]
#[command(
    name = "m7-patches-scan",
    about = "Run extract_patches across a SID corpus and report coverage stats per subtune.",
    long_about = "Walks --corpus for .sid files, runs the full analyzer pipeline + Slice 1 \
characteristics extraction + Slice 2 patch clustering on each subtune, and prints a per-subtune \
summary plus an aggregate. RSID skipped (Kernal/BASIC ROM stubs not implemented).\n\n\
Use --subtunes start (default) to scan only each file's start_song, or all to walk every subtune."
)]
struct Cli {
    /// Directory tree to scan recursively for .sid files.
    #[arg(long)]
    corpus: PathBuf,

    /// Play frames per subtune. 3000 ≈ 60 s at PAL.
    #[arg(long, default_value_t = DEFAULT_FRAMES)]
    frames: u32,

    /// Which subtunes to analyze.
    #[arg(long, value_enum, default_value_t = SubtuneMode::Start)]
    subtunes: SubtuneMode,

    /// Stop after scanning this many .sid files (after lexicographic
    /// sort). Useful for HVSC sampling.
    #[arg(long)]
    limit: Option<usize>,

    /// Drop subtunes with fewer than this many detected notes from the
    /// table + aggregate. Set to 5 to suppress one-shot SFX subtunes.
    #[arg(long, default_value_t = 0)]
    min_notes: u32,

    /// Worker thread pool size. Defaults to available parallelism.
    #[arg(long)]
    workers: Option<usize>,

    /// Maximum rows to print in the per-subtune table. Excess rows
    /// are collapsed to a "… N more" footer; the aggregate still
    /// covers the full set.
    #[arg(long, default_value_t = DEFAULT_MAX_ROWS)]
    max_rows: usize,

    /// Wall-clock budget per subtune (seconds). The emulator bails
    /// out between frames when the budget elapses. Lets the scan
    /// proceed past pathologically slow play-routines instead of
    /// blocking the worker thread for minutes.
    #[arg(long, default_value_t = 3.0)]
    file_timeout_secs: f32,

    /// Subtunes that complete but take longer than this (seconds)
    /// are recorded in a "slow files" list printed after the scan,
    /// for later exclusion or investigation.
    #[arg(long, default_value_t = 1.0)]
    slow_file_threshold_secs: f32,

    /// Print per-patch role-tag detail for each subtune.
    #[arg(long)]
    verbose: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SubtuneMode {
    Start,
    All,
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

    let mut files = collect_sid_files(&cli.corpus);
    if files.is_empty() {
        eprintln!("no .sid files under {}", cli.corpus.display());
        std::process::exit(1);
    }
    if let Some(limit) = cli.limit {
        files.truncate(limit);
    }
    let total_files = files.len();
    eprintln!("scanning {total_files} files…");

    let corpus_root = cli.corpus.clone();
    let processed = Arc::new(AtomicUsize::new(0));
    let slow_counter = Arc::new(AtomicUsize::new(0));
    let timeout_counter = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let start = std::time::Instant::now();

    // Heartbeat thread: ticks every HEARTBEAT_SECS regardless of file
    // completion, so the user can tell the scan is alive even when
    // the per-100-files progress counter stalls (all workers stuck on
    // legitimately slow play-routines).
    let heartbeat = {
        let processed = Arc::clone(&processed);
        let slow_counter = Arc::clone(&slow_counter);
        let timeout_counter = Arc::clone(&timeout_counter);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(HEARTBEAT_SECS));
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                eprintln!(
                    "  ♥ done={done}/{total_files}  elapsed={el:.1}s  slow={slow}  timeouts={t}",
                    done = processed.load(Ordering::Relaxed),
                    el = start.elapsed().as_secs_f32(),
                    slow = slow_counter.load(Ordering::Relaxed),
                    t = timeout_counter.load(Ordering::Relaxed),
                );
            }
        })
    };

    let results: Vec<FileOutcome> = files
        .into_par_iter()
        .map(|path| {
            let rel = path
                .strip_prefix(&corpus_root)
                .unwrap_or(&path)
                .to_path_buf();
            let out = scan_file(&path, &rel, &cli);
            slow_counter.fetch_add(out.slow.len(), Ordering::Relaxed);
            timeout_counter.fetch_add(out.timed_out.len(), Ordering::Relaxed);
            let done = processed.fetch_add(1, Ordering::Relaxed) + 1;
            if done.is_multiple_of(PROGRESS_INTERVAL) {
                eprintln!(
                    "  [{done}/{total_files}] elapsed={:.1}s",
                    start.elapsed().as_secs_f32()
                );
            }
            out
        })
        .collect();

    shutdown.store(true, Ordering::Relaxed);
    let _ = heartbeat.join();

    let mut rows: Vec<Row> = Vec::new();
    let mut rsid_skipped = 0usize;
    let mut slow: Vec<SlowRecord> = Vec::new();
    let mut timed_out: Vec<TimeoutRecord> = Vec::new();
    for o in results {
        if o.was_rsid {
            rsid_skipped += 1;
        }
        rows.extend(o.rows);
        slow.extend(o.slow);
        timed_out.extend(o.timed_out);
    }
    rows.retain(|r| r.note_count >= cli.min_notes);
    rows.sort_by(|a, b| (&a.file, a.subtune).cmp(&(&b.file, b.subtune)));
    slow.sort_by(|a, b| {
        b.elapsed_secs
            .partial_cmp(&a.elapsed_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    eprintln!(
        "done: {} subtunes ≥ {} notes; {} RSID skipped; {} slow; {} timed out; elapsed={:.1}s\n",
        rows.len(),
        cli.min_notes,
        rsid_skipped,
        slow.len(),
        timed_out.len(),
        start.elapsed().as_secs_f32()
    );

    print_table(&rows, cli.verbose, cli.max_rows);
    print_aggregate(&rows, rsid_skipped);
    print_composer_summary(&rows);
    print_v3_lfo_subtunes(&rows);
    print_slow_files(&slow);
    print_timed_out_files(&timed_out);
}

#[derive(Default, Debug)]
struct FileOutcome {
    rows: Vec<Row>,
    was_rsid: bool,
    slow: Vec<SlowRecord>,
    timed_out: Vec<TimeoutRecord>,
}

#[derive(Debug)]
struct SlowRecord {
    file: String,
    subtune: u16,
    elapsed_secs: f32,
}

#[derive(Debug)]
struct TimeoutRecord {
    file: String,
    subtune: u16,
    frames_completed: u32,
    frames_requested: u32,
    elapsed_secs: f32,
}

#[derive(Debug)]
struct Row {
    file: String,
    subtune: u16,
    author: String,
    note_count: u32,
    /// Integer count of notes with `Some(patch_id)`. Carrying this
    /// directly (rather than re-deriving from a `coverage: f32`)
    /// keeps aggregate sums exact.
    covered: u32,
    patch_count: usize,
    /// Number of patches whose hardware_tricks include
    /// `Voice3LfoSource` (Slice 3 detection).
    v3_lfo_patches: usize,
    patches: Vec<Patch>,
}

impl Row {
    fn coverage(&self) -> f32 {
        if self.note_count == 0 {
            0.0
        } else {
            self.covered as f32 / self.note_count as f32
        }
    }
}

fn scan_file(abs: &Path, rel: &Path, cli: &Cli) -> FileOutcome {
    let rel_str = rel.to_string_lossy().into_owned();
    let bytes = match fs::read(abs) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("  skip {rel_str}: read: {e}");
            return FileOutcome::default();
        }
    };
    let header = match header::parse(&bytes) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("  skip {rel_str}: header: {e}");
            return FileOutcome::default();
        }
    };
    if header.format == header::Format::Rsid {
        return FileOutcome {
            was_rsid: true,
            ..FileOutcome::default()
        };
    }
    let clock = SystemClock::from(header.flags.clock);
    let subtunes: Vec<SubtuneIndex> = match cli.subtunes {
        SubtuneMode::Start => vec![SubtuneIndex(header.start_song.0.max(1))],
        SubtuneMode::All => (1..=header.songs.0.min(MAX_SUBTUNES.0))
            .map(SubtuneIndex)
            .collect(),
    };

    let mut out = FileOutcome::default();
    for s in subtunes {
        scan_subtune(&header, &bytes, s, clock, cli, &rel_str, &mut out);
    }
    out
}

fn scan_subtune(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    clock: SystemClock,
    cli: &Cli,
    rel: &str,
    out: &mut FileOutcome,
) {
    let timeout = std::time::Duration::from_secs_f32(cli.file_timeout_secs);
    let wall_start = std::time::Instant::now();
    let deadline = wall_start + timeout;

    let trace_result = catch_unwind(AssertUnwindSafe(|| {
        emu::run_with_deadline(header, bytes, subtune, cli.frames, deadline)
    }));
    let trace = match trace_result {
        Ok(Ok(t)) => t,
        Ok(Err(emu::EmuError::WallTimeout {
            frames_completed,
            frames_requested,
        })) => {
            let elapsed = wall_start.elapsed().as_secs_f32();
            eprintln!(
                "  TIMEOUT {rel}#{} after {elapsed:.1}s ({frames_completed}/{frames_requested} frames)",
                subtune.0
            );
            out.timed_out.push(TimeoutRecord {
                file: rel.to_string(),
                subtune: subtune.0,
                frames_completed,
                frames_requested,
                elapsed_secs: elapsed,
            });
            return;
        }
        Ok(Err(e)) => {
            eprintln!("  skip {rel}#{}: emu: {e}", subtune.0);
            return;
        }
        Err(_) => {
            eprintln!("  skip {rel}#{}: emu panic", subtune.0);
            return;
        }
    };
    let states = analyze(&trace);
    let notes = detect_notes(&states, clock);

    // Early-filter SFX subtunes: skip the per-note characteristics +
    // patch clustering entirely if we'll drop the row anyway.
    if (notes.len() as u32) < cli.min_notes {
        return;
    }

    let spans = detect_effects(&trace, &states, EffectThresholds::default());
    let voice3_reads = trace.voice3_reads_per_frame();
    let (_characteristics, patches, assignments) =
        extract_timbre(&notes, &states, &spans, &voice3_reads, clock);

    let total = assignments.len();
    let covered = assignments.iter().filter(|a| a.is_some()).count();

    let elapsed = wall_start.elapsed().as_secs_f32();
    if elapsed >= cli.slow_file_threshold_secs {
        eprintln!("  SLOW {rel}#{} {elapsed:.2}s", subtune.0);
        out.slow.push(SlowRecord {
            file: rel.to_string(),
            subtune: subtune.0,
            elapsed_secs: elapsed,
        });
    }

    let v3_lfo_patches = patches
        .iter()
        .filter(|p| {
            p.voices.iter().any(|v| {
                v.hardware_tricks
                    .iter()
                    .any(|t| matches!(t, HardwareTrick::Voice3LfoSource))
            })
        })
        .count();

    out.rows.push(Row {
        file: rel.to_string(),
        subtune: subtune.0,
        author: header.author.clone(),
        note_count: total as u32,
        covered: covered as u32,
        patch_count: patches.len(),
        v3_lfo_patches,
        patches,
    });
}

fn role_summary(patches: &[Patch]) -> String {
    patches
        .iter()
        .map(|p| p.role_tags.to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn print_table(rows: &[Row], verbose: bool, max_rows: usize) {
    if rows.is_empty() {
        return;
    }
    println!(
        "{:<48}  {:<4}  {:<22}  {:>5}  {:>3}  {:>5}  Roles",
        "File", "Sub", "Author", "Notes", "P", "Cov%"
    );
    println!("{}", "-".repeat(120));
    let shown = rows.len().min(max_rows);
    for r in rows.iter().take(shown) {
        println!(
            "{:<48}  {:<4}  {:<22}  {:>5}  {:>3}  {:>5.1}  {}",
            truncate(&r.file, 48),
            r.subtune,
            truncate(&r.author, 22),
            r.note_count,
            r.patch_count,
            r.coverage() * 100.0,
            role_summary(&r.patches)
        );
        if verbose {
            for p in &r.patches {
                println!(
                    "      patch {}: members={} first_wave=0x{:02X} adsr={} {}",
                    p.id.0, p.member_count, p.waveform, p.adsr, p.role_tags
                );
            }
        }
    }
    if rows.len() > shown {
        println!(
            "  … {} more rows omitted (use --max-rows to expand)",
            rows.len() - shown
        );
    }
    println!();
}

fn print_composer_summary(rows: &[Row]) {
    if rows.is_empty() {
        return;
    }
    let mut by_author: HashMap<String, ComposerStats> = HashMap::new();
    for r in rows {
        let s = by_author.entry(r.author.clone()).or_default();
        s.subtunes += 1;
        s.total_notes += u64::from(r.note_count);
        s.total_covered += u64::from(r.covered);
        s.patch_counts.push(r.patch_count);
    }
    let mut ranked: Vec<(String, ComposerStats)> = by_author.into_iter().collect();
    ranked.sort_by_key(|(_, s)| std::cmp::Reverse(s.subtunes));

    println!("Per-composer summary (top 20 by subtune count):");
    println!(
        "  {:<26}  {:>5}  {:>6}  {:>5}  {:>5}  {:>5}",
        "Author", "Subs", "Notes", "P-med", "P-max", "Cov%"
    );
    println!("  {}", "-".repeat(70));
    for (author, s) in ranked.iter().take(20) {
        let mut counts = s.patch_counts.clone();
        let median = median_of(&mut counts);
        let max = counts.last().copied().unwrap_or(0);
        let coverage = if s.total_notes == 0 {
            0.0
        } else {
            s.total_covered as f32 / s.total_notes as f32 * 100.0
        };
        println!(
            "  {:<26}  {:>5}  {:>6}  {:>5}  {:>5}  {:>4.1}",
            truncate(author, 26),
            s.subtunes,
            s.total_notes,
            median,
            max,
            coverage
        );
    }
    println!();
}

/// In-place sort + middle-index pick. Returns 0 for an empty slice.
fn median_of(xs: &mut [usize]) -> usize {
    xs.sort_unstable();
    xs.get(xs.len() / 2).copied().unwrap_or(0)
}

#[derive(Default)]
struct ComposerStats {
    subtunes: u32,
    total_notes: u64,
    total_covered: u64,
    patch_counts: Vec<usize>,
}

fn print_aggregate(rows: &[Row], rsid_skipped: usize) {
    if rows.is_empty() {
        if rsid_skipped > 0 {
            println!("Aggregate: 0 subtunes analyzed (skipped {rsid_skipped} RSID files).");
        }
        return;
    }
    let n = rows.len();
    let total_notes: u64 = rows.iter().map(|r| u64::from(r.note_count)).sum();
    let total_patches: usize = rows.iter().map(|r| r.patch_count).sum();
    let total_covered: u64 = rows.iter().map(|r| u64::from(r.covered)).sum();
    let weighted_coverage = if total_notes == 0 {
        0.0
    } else {
        total_covered as f32 / total_notes as f32
    };

    let mut patch_counts: Vec<usize> = rows.iter().map(|r| r.patch_count).collect();
    let median = median_of(&mut patch_counts);
    let min = patch_counts.first().copied().unwrap_or(0);
    let max = patch_counts.last().copied().unwrap_or(0);

    let subtunes_with_zero_patches = rows.iter().filter(|r| r.patch_count == 0).count();
    let subtunes_under_70_coverage = rows
        .iter()
        .filter(|r| r.note_count > 0 && r.coverage() < 0.70)
        .count();

    println!("Aggregate ({n} subtunes analyzed, {rsid_skipped} RSID files skipped):");
    println!("  total notes:               {total_notes}");
    println!(
        "  weighted note coverage:    {:.1}%  ({total_covered}/{total_notes})",
        weighted_coverage * 100.0
    );
    println!(
        "  patches per subtune:       min={min}  median={median}  max={max}  total={total_patches}"
    );
    println!("  subtunes with 0 patches:   {subtunes_with_zero_patches}");
    println!("  subtunes < 70 % coverage:  {subtunes_under_70_coverage}");
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{truncated}…")
    }
}

fn print_v3_lfo_subtunes(rows: &[Row]) {
    let hits: Vec<&Row> = rows.iter().filter(|r| r.v3_lfo_patches > 0).collect();
    if hits.is_empty() {
        return;
    }
    println!(
        "Voice-3-as-LFO hits ({} subtunes — V3 used as modulation source for V1/V2):",
        hits.len()
    );
    for r in &hits {
        println!(
            "  {}#{}  ({}, {} patches, {} V3-LFO)",
            r.file, r.subtune, r.author, r.patch_count, r.v3_lfo_patches
        );
    }
    println!();
}

fn print_slow_files(slow: &[SlowRecord]) {
    if slow.is_empty() {
        return;
    }
    println!("Slow subtunes (over --slow-file-threshold-secs, sorted slowest first):");
    for s in slow {
        println!("  {:>6.2}s  {}#{}", s.elapsed_secs, s.file, s.subtune);
    }
    println!();
}

fn print_timed_out_files(timed: &[TimeoutRecord]) {
    if timed.is_empty() {
        return;
    }
    println!("Timed-out subtunes (bailed out at --file-timeout-secs):");
    for t in timed {
        println!(
            "  {:>6.2}s  {}#{}  ({}/{} frames)",
            t.elapsed_secs, t.file, t.subtune, t.frames_completed, t.frames_requested
        );
    }
    println!();
}

fn collect_sid_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .path()
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(OsStr::new("sid")))
        {
            out.push(entry.into_path());
        }
    }
    out.sort();
    out
}
