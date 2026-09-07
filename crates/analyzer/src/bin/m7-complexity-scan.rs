//! Complex-sound census: walk a SID corpus and rank the timbral cases
//! that the synth-native export currently collapses to a single static
//! oscillator, so we attack the biggest fidelity gap first.
//!
//! Same corpus-walk shape as `m7-patches-scan` (parallel WalkDir, per-file
//! deadline, panic-catch, heartbeat, RSID skip), but the downstream step
//! tallies a *complexity taxonomy* per note instead of patch coverage.
//!
//! Each note is weighed two ways: by note count and by `length_frames`
//! (audible duration ≈ perceptual weight). A 100-frame combined-waveform
//! lead should outrank a 2-frame blip. The categories that the export
//! renders losslessly today (plain single waveform) are shown for context;
//! the ones it collapses (combined waveforms, multi-waveform notes,
//! hard-sync, `$D418` sample) are ranked by frame-share — that ranking is
//! the build order for the export work.
//!
//! "Multi-waveform" counts a note that visits ≥2 *distinct non-zero*
//! waveforms; transitions to/from the silent `0x00` gate frame are excluded,
//! so hard-restart and release tails do not inflate it (the export already
//! lands on the first audible waveform).

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
use sid_analyzer::analysis::timbre::{HardwareTrick, SeqOrLoop, WaveformCombo, extract_timbre};
use sid_analyzer::analysis::{SystemClock, analyze};
use sid_analyzer::emu;
use sid_analyzer::header::{self, Header, MAX_SUBTUNES, SubtuneIndex};
use walkdir::WalkDir;

const DEFAULT_FRAMES: u32 = 3000;
const PROGRESS_INTERVAL: usize = 100;
const HEARTBEAT_SECS: u64 = 3;

#[derive(Parser, Debug)]
#[command(
    name = "m7-complexity-scan",
    about = "Census the complex-sound cases the synth-native export collapses, ranked by audible-frame share."
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

    /// Stop after this many .sid files (after lexicographic sort).
    #[arg(long)]
    limit: Option<usize>,

    /// Drop subtunes with fewer than this many detected notes.
    #[arg(long, default_value_t = 8)]
    min_notes: u32,

    /// Worker thread pool size. Defaults to available parallelism.
    #[arg(long)]
    workers: Option<usize>,

    /// How many "most complex" subtunes to list as ear-test candidates.
    #[arg(long, default_value_t = 20)]
    top_files: usize,

    /// Wall-clock budget per subtune (seconds).
    #[arg(long, default_value_t = 3.0)]
    file_timeout_secs: f32,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SubtuneMode {
    Start,
    All,
}

/// A note count paired with its audible-frame sum.
#[derive(Default, Clone, Copy)]
struct Tally {
    notes: u64,
    frames: u64,
}

impl Tally {
    fn add(&mut self, frames: u16) {
        self.notes += 1;
        self.frames += u64::from(frames);
    }

    fn merge(&mut self, o: Tally) {
        self.notes += o.notes;
        self.frames += o.frames;
    }
}

/// One bucket per complexity category. Categories are non-exclusive: a
/// note with a combined waveform *and* mid-note switching lands in both.
#[derive(Default, Clone, Copy)]
struct Counters {
    total: Tally,
    plain: Tally,

    combined_any: Tally,
    combo_trisaw: Tally,
    combo_pulsetri: Tally,
    combo_pulsesaw: Tally,
    combo_noiselock: Tally,
    combo_triple: Tally,

    multi_wf_any: Tally,
    multi_wf_2: Tally,
    multi_wf_3plus: Tally,

    // Within multi-waveform notes: how much of the audible span the single
    // most-held non-zero waveform covers. High coverage ⇒ the export's
    // dominant-waveform pick recovers the note; low coverage ⇒ genuine
    // alternation that needs sequence rendering.
    dom_ge90: Tally,
    dom_70_90: Tally,
    dom_50_70: Tally,
    dom_lt50: Tally,

    // Multi-waveform notes the dominant-waveform pick changed vs the old
    // first-audible pick — the share of frames that fix made more faithful.
    mispick: Tally,

    hard_sync: Tally,
    ring_mod: Tally,
    d418: Tally,
}

impl Counters {
    fn merge(&mut self, o: &Counters) {
        self.total.merge(o.total);
        self.plain.merge(o.plain);
        self.combined_any.merge(o.combined_any);
        self.combo_trisaw.merge(o.combo_trisaw);
        self.combo_pulsetri.merge(o.combo_pulsetri);
        self.combo_pulsesaw.merge(o.combo_pulsesaw);
        self.combo_noiselock.merge(o.combo_noiselock);
        self.combo_triple.merge(o.combo_triple);
        self.multi_wf_any.merge(o.multi_wf_any);
        self.multi_wf_2.merge(o.multi_wf_2);
        self.multi_wf_3plus.merge(o.multi_wf_3plus);
        self.dom_ge90.merge(o.dom_ge90);
        self.dom_70_90.merge(o.dom_70_90);
        self.dom_50_70.merge(o.dom_50_70);
        self.dom_lt50.merge(o.dom_lt50);
        self.mispick.merge(o.mispick);
        self.hard_sync.merge(o.hard_sync);
        self.ring_mod.merge(o.ring_mod);
        self.d418.merge(o.d418);
    }
}

/// A subtune's complexity, for the ear-test candidate ranking.
struct Row {
    file: String,
    subtune: u16,
    author: String,
    notes: u64,
    frames: u64,
    /// Frames of notes the export collapses (combined / switch / sync / D418),
    /// counted once per note (union, no double-count).
    lossy_frames: u64,
}

impl Row {
    fn lossy_fraction(&self) -> f32 {
        if self.frames == 0 {
            0.0
        } else {
            self.lossy_frames as f32 / self.frames as f32
        }
    }
}

#[derive(Default)]
struct FileOutcome {
    counters: Counters,
    rows: Vec<Row>,
    was_rsid: bool,
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
    files.sort();
    if let Some(limit) = cli.limit {
        files.truncate(limit);
    }
    let total_files = files.len();
    eprintln!("scanning {total_files} files…");

    let corpus_root = cli.corpus.clone();
    let processed = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let start = std::time::Instant::now();

    let heartbeat = {
        let processed = Arc::clone(&processed);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(HEARTBEAT_SECS));
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                eprintln!(
                    "  ♥ done={done}/{total_files}  elapsed={el:.1}s",
                    done = processed.load(Ordering::Relaxed),
                    el = start.elapsed().as_secs_f32(),
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

    let mut agg = Counters::default();
    let mut rows: Vec<Row> = Vec::new();
    let mut rsid_skipped = 0usize;
    let mut subtunes = 0usize;
    for o in results {
        if o.was_rsid {
            rsid_skipped += 1;
        }
        agg.merge(&o.counters);
        subtunes += o.rows.len();
        rows.extend(o.rows);
    }

    eprintln!(
        "done: {subtunes} subtunes ≥ {} notes; {rsid_skipped} RSID skipped; elapsed={:.1}s\n",
        cli.min_notes,
        start.elapsed().as_secs_f32(),
    );

    print_report(&agg);
    print_top_files(&mut rows, cli.top_files, cli.min_notes);
}

fn scan_file(abs: &Path, rel: &Path, cli: &Cli) -> FileOutcome {
    let rel_str = rel.to_string_lossy().into_owned();
    let Ok(bytes) = fs::read(abs) else {
        return FileOutcome::default();
    };
    let header = match header::parse(&bytes) {
        Ok(h) => h,
        Err(_) => return FileOutcome::default(),
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
    let deadline = std::time::Instant::now() + Duration::from_secs_f32(cli.file_timeout_secs);
    let trace = match catch_unwind(AssertUnwindSafe(|| {
        emu::run_with_deadline(header, bytes, subtune, cli.frames, deadline)
    })) {
        Ok(Ok(t)) => t,
        _ => return,
    };

    let states = analyze(&trace);
    let notes = detect_notes(&states, clock);
    if (notes.len() as u32) < cli.min_notes {
        return;
    }

    let spans = detect_effects(&trace, &states, EffectThresholds::default());
    let voice3_reads = trace.voice3_reads_per_frame();
    let (characteristics, _patches, _assignments) =
        extract_timbre(&notes, &states, &spans, &voice3_reads, clock);

    let mut c = Counters::default();
    let mut notes_total = 0u64;
    let mut frames_total = 0u64;
    let mut lossy_frames = 0u64;

    for nc in &characteristics {
        let f = nc.length_frames;
        c.total.add(f);
        notes_total += 1;
        frames_total += u64::from(f);

        let mut lossy = false;

        if nc.has_combined_waveform() {
            c.combined_any.add(f);
            lossy = true;
            for t in &nc.hardware_tricks {
                if let HardwareTrick::CombinedWaveform(combo) = t {
                    match combo {
                        WaveformCombo::TriSaw => c.combo_trisaw.add(f),
                        WaveformCombo::PulseTri => c.combo_pulsetri.add(f),
                        WaveformCombo::PulseSaw => c.combo_pulsesaw.add(f),
                        WaveformCombo::NoiseLock => c.combo_noiselock.add(f),
                        WaveformCombo::Triple => c.combo_triple.add(f),
                    }
                }
            }
        }

        // Distinct *non-zero* waveforms only: transitions to/from the silent
        // `0x00` gate frame (hard-restart, release tail) are not a timbral
        // switch — the export already lands on the first audible waveform.
        let distinct_nonzero = {
            let mut bytes: Vec<u8> = nc
                .waveform_primary
                .iter()
                .map(|w| w.to_control_byte())
                .filter(|&b| b != 0)
                .collect();
            bytes.sort_unstable();
            bytes.dedup();
            bytes.len()
        };
        if distinct_nonzero >= 2 {
            c.multi_wf_any.add(f);
            lossy = true;
            if distinct_nonzero == 2 {
                c.multi_wf_2.add(f);
            } else {
                c.multi_wf_3plus.add(f);
            }
            if let Some((_, cov)) = dominant_waveform(&nc.waveform_sequence) {
                if cov >= 0.90 {
                    c.dom_ge90.add(f);
                } else if cov >= 0.70 {
                    c.dom_70_90.add(f);
                } else if cov >= 0.50 {
                    c.dom_50_70.add(f);
                } else {
                    c.dom_lt50.add(f);
                }
            }
            // The real change the export makes: the gate-aware dominant
            // ([`NoteCharacteristics::dominant_waveform_byte`]) vs the first
            // audible waveform. (The coverage buckets above still use the raw
            // sequence, including gate-off frames, so they read higher.)
            if nc.dominant_waveform_byte() != nc.first_waveform_byte() {
                c.mispick.add(f);
            }
        }

        for t in &nc.hardware_tricks {
            match t {
                HardwareTrick::HardSync => {
                    c.hard_sync.add(f);
                    lossy = true;
                }
                HardwareTrick::RingMod => c.ring_mod.add(f),
                HardwareTrick::D418Sample { .. } => {
                    c.d418.add(f);
                    lossy = true;
                }
                _ => {}
            }
        }

        if !lossy && nc.waveform_switches == 0 {
            c.plain.add(f);
        }
        if lossy {
            lossy_frames += u64::from(f);
        }
    }

    out.counters.merge(&c);
    out.rows.push(Row {
        file: rel.to_string(),
        subtune: subtune.0,
        author: header.author.clone(),
        notes: notes_total,
        frames: frames_total,
        lossy_frames,
    });
}

fn print_report(c: &Counters) {
    let tot_notes = c.total.notes.max(1);
    let tot_frames = c.total.frames.max(1);

    let line = |label: &str, t: Tally, verdict: &str| {
        let np = 100.0 * t.notes as f64 / tot_notes as f64;
        let fp = 100.0 * t.frames as f64 / tot_frames as f64;
        println!(
            "  {label:<26} {n:>9}  {np:>6.2}%   {fr:>11}  {fp:>6.2}%   {verdict}",
            n = t.notes,
            fr = t.frames,
        );
    };

    println!(
        "Census over {} notes / {} audible frames.\n",
        c.total.notes, c.total.frames
    );
    println!(
        "  {:<26} {:>9}  {:>7}   {:>11}  {:>7}   verdict",
        "category", "notes", "note%", "frames", "frame%"
    );
    println!("  {}", "─".repeat(86));
    line("plain single-waveform", c.plain, "faithful");
    println!("  {}", "─".repeat(86));
    println!("  GAPS (export collapses these to one static oscillator):");
    line("combined waveform (any)", c.combined_any, "LOSSY");
    line("  · tri+saw (bell)", c.combo_trisaw, "→ one wf");
    line("  · pulse+tri (hollow)", c.combo_pulsetri, "→ one wf");
    line("  · pulse+saw (nasal)", c.combo_pulsesaw, "→ one wf");
    line("  · noise-lock (perc gate)", c.combo_noiselock, "→ one wf");
    line("  · triple/quad", c.combo_triple, "→ one wf");
    line("multi-waveform (≥2 distinct)", c.multi_wf_any, "LOSSY");
    line("  · 2 distinct", c.multi_wf_2, "→ dominant wf");
    line("  · 3+ distinct", c.multi_wf_3plus, "→ dominant wf");
    println!("    fix shape — dominant waveform's coverage of the audible span:");
    line("    · ≥90% (pick-dominant)", c.dom_ge90, "cheap fix");
    line("    · 70–90%", c.dom_70_90, "cheap fix");
    line("    · 50–70%", c.dom_50_70, "borderline");
    line(
        "    · <50% (true alternation)",
        c.dom_lt50,
        "needs sequence",
    );
    line(
        "    · first-audible ≠ dominant",
        c.mispick,
        "dominant-pick win",
    );
    line("hard sync", c.hard_sync, "LOSSY (skipped)");
    line("$D418 sample", c.d418, "LOSSY (skipped)");
    line("ring mod", c.ring_mod, "partial (has_ring)");
    println!("  {}", "─".repeat(86));
    println!(
        "\n  Note: categories are non-exclusive (a note can be combined AND multi-waveform);\n  \
         sub-rows can sum above their parent. Multi-waveform counts distinct *non-zero*\n  \
         waveforms only — silent gate frames (0x00) are excluded, so it is not inflated\n  \
         by hard-restart or release transitions."
    );
}

fn print_top_files(rows: &mut [Row], top: usize, min_notes: u32) {
    rows.sort_by(|a, b| {
        b.lossy_fraction()
            .partial_cmp(&a.lossy_fraction())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    println!(
        "\nTop {top} subtunes by collapsed-frame share (≥ {min_notes} notes) — ear-test candidates:\n"
    );
    println!(
        "  {:<7} {:>6}  {:<44} author",
        "lossy%", "notes", "file#subtune"
    );
    println!("  {}", "─".repeat(86));
    for r in rows.iter().take(top) {
        let f = format!("{}#{}", r.file, r.subtune);
        let f = if f.len() > 44 {
            f[f.len() - 44..].to_string()
        } else {
            f
        };
        println!(
            "  {:>6.1} {:>6}  {:<44} {}",
            100.0 * r.lossy_fraction(),
            r.notes,
            f,
            r.author,
        );
    }
}

/// The note's single most-held non-zero waveform byte and the fraction of
/// audible (non-zero) frames it covers. `None` if the note has no audible
/// frame. For a looped sequence the loop body's distribution stands in for
/// the whole note (the body is what repeats).
fn dominant_waveform(seq: &SeqOrLoop<u8>) -> Option<(u8, f32)> {
    let bytes: &[u8] = match seq {
        SeqOrLoop::Raw(v) => v,
        SeqOrLoop::Loop { body, .. } => body,
    };
    let mut counts = [0u32; 256];
    let mut audible = 0u32;
    for &b in bytes {
        if b != 0 {
            counts[b as usize] += 1;
            audible += 1;
        }
    }
    if audible == 0 {
        return None;
    }
    let (byte, top) = counts
        .iter()
        .copied()
        .enumerate()
        .max_by_key(|&(_, n)| n)
        .map(|(i, n)| (i as u8, n))
        .unwrap_or((0, 0));
    Some((byte, top as f32 / audible as f32))
}

fn collect_sid_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().and_then(OsStr::to_str) == Some("sid"))
        .collect()
}
