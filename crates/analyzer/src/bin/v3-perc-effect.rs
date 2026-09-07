//! Net-effect measurement for two changes:
//!   Task 1 — voice-3 read-trap ($D41B) now returns a live oscillator signal:
//!            scope = how many subtunes actually read voice 3.
//!   Task 2 — percussion classification (cap 16→24 + melodic-pitch guard):
//!            delta = notes that flip percussive ↔ non-percussive.
//!
//! Both predicates are recomputed inline from `NoteCharacteristics` public
//! fields so OLD and NEW classification are compared in one corpus pass (the
//! engine already runs the NEW `is_percussive`; this binary doesn't call it).
//! Same corpus-walk shape as `m7-complexity-scan`.

use std::ffi::OsStr;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use clap::Parser;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use sid_analyzer::analysis::effects::{EffectThresholds, detect_effects};
use sid_analyzer::analysis::note::detect_notes;
use sid_analyzer::analysis::timbre::{
    AttackClass, HardwareTrick, NoteCharacteristics, PitchBehavior, extract_timbre,
};
use sid_analyzer::analysis::{SystemClock, analyze};
use sid_analyzer::emu;
use sid_analyzer::header::{self, MAX_SUBTUNES, SubtuneIndex};
use walkdir::WalkDir;

const DEFAULT_FRAMES: u32 = 3000;
const HEARTBEAT_SECS: u64 = 3;

#[derive(Parser, Debug)]
#[command(
    name = "v3-perc-effect",
    about = "Net effect of the V3 read-trap and percussion-cap changes."
)]
struct Cli {
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long, default_value_t = DEFAULT_FRAMES)]
    frames: u32,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, default_value_t = 3.0)]
    file_timeout_secs: f32,
    #[arg(long)]
    workers: Option<usize>,
}

#[derive(Default, Clone, Copy)]
struct Stats {
    subtunes: u64,
    subtunes_reading_v3: u64,
    notes: u64,
    old_perc: u64,
    new_perc: u64,
    gained_perc: u64, // non-perc(old) → perc(new): the cap raise wins
    lost_perc: u64,   // perc(old) → non-perc(new): the melodic guard removes
}

impl Stats {
    fn merge(&mut self, o: &Stats) {
        self.subtunes += o.subtunes;
        self.subtunes_reading_v3 += o.subtunes_reading_v3;
        self.notes += o.notes;
        self.old_perc += o.old_perc;
        self.new_perc += o.new_perc;
        self.gained_perc += o.gained_perc;
        self.lost_perc += o.lost_perc;
    }
}

fn has_trick(c: &NoteCharacteristics, want: fn(&HardwareTrick) -> bool) -> bool {
    c.hardware_tricks.iter().any(want)
}

/// Shared body of both predicates (attack + non-melodic-behaviour + positive
/// percussive signal), parameterised by the length cap and whether the melodic
/// pitch-range guard applies.
fn is_perc(c: &NoteCharacteristics, cap: u16, melodic_guard: bool) -> bool {
    if c.length_frames > cap {
        return false;
    }
    if !matches!(c.attack, AttackClass::Instant | AttackClass::Fast) {
        return false;
    }
    if matches!(
        c.pitch_behavior,
        PitchBehavior::Vibrato | PitchBehavior::Portamento | PitchBehavior::Arpeggio
    ) {
        return false;
    }
    if melodic_guard
        && c.pitch_range_semitones >= 2
        && !matches!(c.pitch_behavior, PitchBehavior::OneShotSweep)
    {
        return false;
    }
    let hard_sync = has_trick(c, |t| matches!(t, HardwareTrick::HardSync));
    let ring_mod = has_trick(c, |t| matches!(t, HardwareTrick::RingMod));
    c.noise_share > 0.0
        || (matches!(c.pitch_behavior, PitchBehavior::OneShotSweep) && c.pitch_range_semitones >= 2)
        || hard_sync
        || ring_mod
}

fn main() {
    let cli = Cli::parse();
    if let Some(n) = cli.workers
        && let Err(e) = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
    {
        eprintln!("warning: thread pool: {e}");
    }

    let mut files = collect_sid_files(&cli.corpus);
    files.sort();
    if let Some(limit) = cli.limit {
        files.truncate(limit);
    }
    if files.is_empty() {
        eprintln!("no .sid files under {}", cli.corpus.display());
        std::process::exit(1);
    }
    let total = files.len();
    eprintln!("scanning {total} files…");

    let root = cli.corpus.clone();
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
                    "  ♥ {}/{total}  {:.0}s",
                    processed.load(Ordering::Relaxed),
                    start.elapsed().as_secs_f32()
                );
            }
        })
    };

    let agg = files
        .into_par_iter()
        .map(|path| {
            let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
            let s = scan_file(&path, &cli);
            processed.fetch_add(1, Ordering::Relaxed);
            let _ = rel;
            s
        })
        .reduce(Stats::default, |mut a, b| {
            a.merge(&b);
            a
        });

    shutdown.store(true, Ordering::Relaxed);
    let _ = heartbeat.join();

    let pct = |n: u64, d: u64| {
        if d == 0 {
            0.0
        } else {
            100.0 * n as f64 / d as f64
        }
    };
    println!(
        "\n=== net effect ({} subtunes, {} notes) ===",
        agg.subtunes, agg.notes
    );
    println!(
        "Task 1 (V3 read-trap): {} / {} subtunes read voice 3 ({:.2}%) — the scope where OSC3 now matters",
        agg.subtunes_reading_v3,
        agg.subtunes,
        pct(agg.subtunes_reading_v3, agg.subtunes)
    );
    println!("Task 2 (percussion cap 16→24 + melodic guard):");
    println!(
        "  percussive notes: {} (old) → {} (new)  [{:+}]",
        agg.old_perc,
        agg.new_perc,
        agg.new_perc as i64 - agg.old_perc as i64
    );
    println!(
        "  + gained (cap raise, non-perc→perc): {} ({:.3}% of notes)",
        agg.gained_perc,
        pct(agg.gained_perc, agg.notes)
    );
    println!(
        "  − lost (melodic guard, perc→non-perc): {} ({:.3}% of notes)",
        agg.lost_perc,
        pct(agg.lost_perc, agg.notes)
    );
    eprintln!("done in {:.1}s", start.elapsed().as_secs_f32());
}

fn scan_file(abs: &Path, cli: &Cli) -> Stats {
    let Ok(bytes) = fs::read(abs) else {
        return Stats::default();
    };
    let Ok(header) = header::parse(&bytes) else {
        return Stats::default();
    };
    if header.format == header::Format::Rsid {
        return Stats::default();
    }
    let clock = SystemClock::from(header.flags.clock);
    let subtunes: Vec<SubtuneIndex> = (1..=header.songs.0.min(MAX_SUBTUNES.0))
        .map(SubtuneIndex)
        .collect();
    let mut s = Stats::default();
    let deadline_dur = Duration::from_secs_f32(cli.file_timeout_secs);
    for sub in subtunes {
        let deadline = std::time::Instant::now() + deadline_dur;
        let trace = match catch_unwind(AssertUnwindSafe(|| {
            emu::run_with_deadline(&header, &bytes, sub, cli.frames, deadline)
        })) {
            Ok(Ok(t)) => t,
            _ => continue,
        };
        s.subtunes += 1;
        let v3 = trace.voice3_reads_per_frame();
        if v3.iter().any(|&n| n > 0) {
            s.subtunes_reading_v3 += 1;
        }
        let states = analyze(&trace);
        let notes = detect_notes(&states, clock);
        if notes.is_empty() {
            continue;
        }
        let spans = detect_effects(&trace, &states, EffectThresholds::default());
        let (chars, _patches, _assign) = extract_timbre(&notes, &states, &spans, &v3, clock);
        for c in &chars {
            s.notes += 1;
            let old = is_perc(c, 16, false);
            let new = is_perc(c, 24, true);
            if old {
                s.old_perc += 1;
            }
            if new {
                s.new_perc += 1;
            }
            match (old, new) {
                (false, true) => s.gained_perc += 1,
                (true, false) => s.lost_perc += 1,
                _ => {}
            }
        }
    }
    s
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
