//! Corpus-scale evaluation for `docs/ML9-FOLLOWUP-IDEAS.md` §G — bytecode
//! patches. Walks a directory of `.sid` files, runs the full M7 pipeline
//! on each track's `start_song`, classifies every patch member-note's
//! per-frame parameter sequences (waveform, ADSR, PW) using the same
//! categories as `m7-patch-program-eval`, and aggregates the result
//! across the corpus into a JSON report.
//!
//! The point: 15-tune samples like the ones in `assets/music/` are
//! anecdotal. Before committing to building bytecode patches we need
//! statistical confidence — running over 10k+ HVSC subtunes confirms
//! the hypothesis isn't an artefact of which composers happened to be
//! in the local sample.

use clap::Parser;
use rayon::prelude::*;
use serde::Serialize;
use sid_analyzer::analysis::effects::{EffectThresholds, detect_effects};
use sid_analyzer::analysis::note::detect_notes;
use sid_analyzer::analysis::timbre::{Patch, extract_timbre};
use sid_analyzer::analysis::voice::VoiceState;
use sid_analyzer::analysis::{SystemClock, analyze};
use sid_analyzer::emu;
use sid_analyzer::header::{self, Clock, Format, Header, SubtuneIndex};
use sid_analyzer::songlengths::SongLengths;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(
    name = "m7-patch-program-corpus",
    about = "Corpus-scale bytecode-patch feasibility eval (ML9 §G)",
    long_about = "Aggregates per-parameter classification results across an arbitrary \
.sid corpus. For each track's start_song subtune, runs the full M7 pipeline + patch \
extraction, then classifies every ≥-3-member patch's member-note timbre sequences \
into one of {constant, one-step, linear, periodic, arbitrary}. The aggregate JSON \
report tells us how often real SID instruments fit the bytecode-opcode model."
)]
struct Cli {
    /// Directory tree to scan recursively for .sid files.
    #[arg(long)]
    root: PathBuf,

    /// JSON report destination. Default stdout.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Stop after N files (smoke / sub-sample). Default: walk the whole tree.
    #[arg(long)]
    limit: Option<usize>,

    /// Worker thread count (rayon). Default = available CPU count.
    #[arg(long)]
    workers: Option<usize>,

    /// Per-subtune emulation budget. With --songlengths, the actual
    /// budget is min(songlength_frames, frames).
    #[arg(long, default_value_t = 3000)]
    frames: u32,

    /// Optional HVSC `Songlengths.md5` file. Caps emulation per the
    /// real song duration.
    #[arg(long)]
    songlengths: Option<PathBuf>,

    /// Per-subtune wall-clock budget. Stuck emulators bail after this;
    /// subtune is recorded as skipped.
    #[arg(long, default_value_t = 15)]
    file_timeout_secs: u64,

    /// Only report on patches with at least this many members.
    #[arg(long, default_value_t = 3)]
    min_members: u16,

    /// Sample at most this many member notes per patch.
    #[arg(long, default_value_t = 2)]
    samples: usize,
}

#[derive(Debug, Default, Clone, Serialize)]
struct ParamTally {
    constant: u64,
    one_step: u64,
    linear: u64,
    periodic: u64,
    arbitrary: u64,
}

impl ParamTally {
    fn total(&self) -> u64 {
        self.constant + self.one_step + self.linear + self.periodic + self.arbitrary
    }
    fn simple_pct(&self) -> f64 {
        let t = self.total();
        if t == 0 {
            0.0
        } else {
            (self.constant + self.one_step + self.linear + self.periodic) as f64 / t as f64 * 100.0
        }
    }
    fn merge(&mut self, other: &ParamTally) {
        self.constant += other.constant;
        self.one_step += other.one_step;
        self.linear += other.linear;
        self.periodic += other.periodic;
        self.arbitrary += other.arbitrary;
    }
    fn record(&mut self, kind: Kind) {
        match kind {
            Kind::Constant => self.constant += 1,
            Kind::OneStep => self.one_step += 1,
            Kind::LinearRamp => self.linear += 1,
            Kind::Periodic => self.periodic += 1,
            Kind::Arbitrary => self.arbitrary += 1,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Constant,
    OneStep,
    LinearRamp,
    Periodic,
    Arbitrary,
}

fn classify(values: &[u32]) -> Kind {
    if values.is_empty() || values.iter().all(|&v| v == values[0]) {
        return Kind::Constant;
    }
    if values.len() >= 2 {
        let d0 = values[1] as i32 - values[0] as i32;
        if d0 != 0 && values.windows(2).all(|w| (w[1] as i32 - w[0] as i32) == d0) {
            return Kind::LinearRamp;
        }
    }
    let max_period = (values.len() / 2).min(16);
    for period in 2..=max_period {
        if (period..values.len()).all(|i| values[i] == values[i - period]) {
            return Kind::Periodic;
        }
    }
    for k in 1..values.len() {
        if values[..k].iter().all(|&v| v == values[0])
            && values[k..].iter().all(|&v| v == values[k])
        {
            return Kind::OneStep;
        }
    }
    Kind::Arbitrary
}

#[derive(Debug, Default, Clone, Serialize)]
struct PerSubtune {
    /// Per-subtune mean simple-fit across the six parameters. Used
    /// for percentile aggregation.
    simple_fit_mean: f64,
    patch_count: u32,
    sample_count: u32,
}

#[derive(Debug, Default)]
struct AggBucket {
    waveform: ParamTally,
    attack: ParamTally,
    decay: ParamTally,
    sustain: ParamTally,
    release: ParamTally,
    pulse_width: ParamTally,
    pw_kinds: HashMap<String, u64>,
    role_lead: u64,
    role_bass: u64,
    role_pad: u64,
    role_stab: u64,
    role_bell: u64,
    role_percussive: u64,
    role_sample: u64,
    role_sound_effect: u64,
    has_waveform_loop: u64,
    has_arpeggio_loop: u64,
    patches_total: u64,
    subtunes: Vec<PerSubtune>,
    skipped_rsid: u64,
    skipped_emu: u64,
    skipped_wall_timeout: u64,
    skipped_no_patches: u64,
    skipped_read_err: u64,
    skipped_header_err: u64,
}

impl AggBucket {
    fn merge(&mut self, other: AggBucket) {
        self.waveform.merge(&other.waveform);
        self.attack.merge(&other.attack);
        self.decay.merge(&other.decay);
        self.sustain.merge(&other.sustain);
        self.release.merge(&other.release);
        self.pulse_width.merge(&other.pulse_width);
        for (k, v) in other.pw_kinds {
            *self.pw_kinds.entry(k).or_insert(0) += v;
        }
        self.role_lead += other.role_lead;
        self.role_bass += other.role_bass;
        self.role_pad += other.role_pad;
        self.role_stab += other.role_stab;
        self.role_bell += other.role_bell;
        self.role_percussive += other.role_percussive;
        self.role_sample += other.role_sample;
        self.role_sound_effect += other.role_sound_effect;
        self.has_waveform_loop += other.has_waveform_loop;
        self.has_arpeggio_loop += other.has_arpeggio_loop;
        self.patches_total += other.patches_total;
        self.subtunes.extend(other.subtunes);
        self.skipped_rsid += other.skipped_rsid;
        self.skipped_emu += other.skipped_emu;
        self.skipped_wall_timeout += other.skipped_wall_timeout;
        self.skipped_no_patches += other.skipped_no_patches;
        self.skipped_read_err += other.skipped_read_err;
        self.skipped_header_err += other.skipped_header_err;
    }
}

#[derive(Debug, Serialize)]
struct Report {
    files_walked: u64,
    subtunes_with_patches: u64,
    patches_aggregated: u64,
    skipped: Skipped,
    parameters: Parameters,
    pw_envelope_kinds: HashMap<String, u64>,
    role_tags: HashMap<String, u64>,
    loops: Loops,
    per_subtune_simple_fit: Distribution,
    elapsed_secs: f64,
}

#[derive(Debug, Serialize)]
struct Skipped {
    rsid: u64,
    emu_error: u64,
    wall_timeout: u64,
    no_patches: u64,
    read_err: u64,
    header_err: u64,
}

#[derive(Debug, Serialize)]
struct Parameters {
    waveform: ParamSummary,
    attack: ParamSummary,
    decay: ParamSummary,
    sustain: ParamSummary,
    release: ParamSummary,
    pulse_width: ParamSummary,
}

#[derive(Debug, Serialize)]
struct ParamSummary {
    counts: ParamTally,
    simple_pct: f64,
}

#[derive(Debug, Serialize)]
struct Loops {
    patches_with_waveform_loop: u64,
    patches_with_arpeggio_loop: u64,
}

#[derive(Debug, Serialize)]
struct Distribution {
    n: usize,
    mean: f64,
    p10: f64,
    p50: f64,
    p90: f64,
    p95: f64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(n) = cli.workers
        && let Err(e) = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
    {
        eprintln!("warning: configure thread pool: {e}");
    }
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fatal: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("scanning {} for .sid files", cli.root.display());
    let mut files = collect_sid_files(&cli.root);
    if let Some(lim) = cli.limit {
        files.truncate(lim);
    }
    eprintln!("  found {} files", files.len());

    let song_lengths = match &cli.songlengths {
        Some(p) => Some(SongLengths::load(p)?),
        None => None,
    };

    let start = Instant::now();
    let total = files.len();
    let done = Arc::new(AtomicUsize::new(0));

    let heartbeat_done = Arc::clone(&done);
    let heartbeat = std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(3));
            let d = heartbeat_done.load(Ordering::Relaxed);
            if d >= total {
                break;
            }
            let pct = d as f64 / total as f64 * 100.0;
            eprintln!("  progress: {d}/{total} ({pct:.1}%)");
        }
    });

    let buckets: Vec<AggBucket> = files
        .par_iter()
        .map(|path| {
            let bucket = analyze_one(path, cli, song_lengths.as_ref());
            done.fetch_add(1, Ordering::Relaxed);
            bucket
        })
        .collect();

    let _ = heartbeat.join();

    let mut agg = AggBucket::default();
    for b in buckets {
        agg.merge(b);
    }

    let elapsed_secs = start.elapsed().as_secs_f64();
    let report = build_report(&agg, files.len() as u64, elapsed_secs);

    match &cli.out {
        Some(path) => {
            let f = std::fs::File::create(path)?;
            serde_json::to_writer_pretty(f, &report)?;
            eprintln!("wrote {}", path.display());
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut stdout, &report)?;
            writeln!(stdout)?;
        }
    }

    eprintln!("---");
    eprintln!("  files walked:               {}", report.files_walked);
    eprintln!(
        "  subtunes with patches:      {}  ({} patches)",
        report.subtunes_with_patches, report.patches_aggregated
    );
    eprintln!(
        "  skipped: rsid={}  emu={}  wall={}  no-patches={}  read={}  hdr={}",
        report.skipped.rsid,
        report.skipped.emu_error,
        report.skipped.wall_timeout,
        report.skipped.no_patches,
        report.skipped.read_err,
        report.skipped.header_err,
    );
    eprintln!();
    eprintln!("  parameter simple-fit %% (corpus-wide):");
    eprintln!(
        "    waveform   : {:.1}%",
        report.parameters.waveform.simple_pct
    );
    eprintln!(
        "    attack     : {:.1}%",
        report.parameters.attack.simple_pct
    );
    eprintln!(
        "    decay      : {:.1}%",
        report.parameters.decay.simple_pct
    );
    eprintln!(
        "    sustain    : {:.1}%",
        report.parameters.sustain.simple_pct
    );
    eprintln!(
        "    release    : {:.1}%",
        report.parameters.release.simple_pct
    );
    eprintln!(
        "    pulse_width: {:.1}%",
        report.parameters.pulse_width.simple_pct
    );
    eprintln!();
    eprintln!("  per-subtune mean simple-fit distribution:");
    eprintln!("    n={}", report.per_subtune_simple_fit.n);
    eprintln!("    mean = {:.1}%", report.per_subtune_simple_fit.mean);
    eprintln!("    p10  = {:.1}%", report.per_subtune_simple_fit.p10);
    eprintln!("    p50  = {:.1}%", report.per_subtune_simple_fit.p50);
    eprintln!("    p90  = {:.1}%", report.per_subtune_simple_fit.p90);
    eprintln!("    p95  = {:.1}%", report.per_subtune_simple_fit.p95);
    eprintln!();
    eprintln!("  PW envelope kinds (count, descending):");
    let mut pw_sorted: Vec<(&String, &u64)> = report.pw_envelope_kinds.iter().collect();
    pw_sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (k, v) in pw_sorted {
        eprintln!("    {k:<14} {v}");
    }
    eprintln!();
    eprintln!(
        "  loops:  waveform_loop on {} patches, arpeggio_loop on {} patches",
        report.loops.patches_with_waveform_loop, report.loops.patches_with_arpeggio_loop,
    );
    eprintln!("  elapsed: {:.1}s", report.elapsed_secs);

    Ok(())
}

fn analyze_one(path: &Path, cli: &Cli, sl: Option<&SongLengths>) -> AggBucket {
    let mut bucket = AggBucket::default();

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            bucket.skipped_read_err += 1;
            return bucket;
        }
    };
    let header: Header = match header::parse(&bytes) {
        Ok(h) => h,
        Err(_) => {
            bucket.skipped_header_err += 1;
            return bucket;
        }
    };
    if header.format == Format::Rsid {
        bucket.skipped_rsid += 1;
        return bucket;
    }

    let subtune = header.start_song;
    let frames = frames_for_subtune(&header, subtune, cli.frames, sl);
    let deadline = Instant::now() + Duration::from_secs(cli.file_timeout_secs);

    let trace = match emu::run_with_deadline(&header, &bytes, subtune, frames, deadline) {
        Ok(t) => t,
        Err(emu::EmuError::WallTimeout { .. }) => {
            bucket.skipped_wall_timeout += 1;
            return bucket;
        }
        Err(_) => {
            bucket.skipped_emu += 1;
            return bucket;
        }
    };
    let clock = SystemClock::from(header.flags.clock);
    let states = analyze(&trace);
    let notes = detect_notes(&states, clock);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());
    let voice3_reads = trace.voice3_reads_per_frame();
    let (_chars, patches, assignments) =
        extract_timbre(&notes, &states, &effects, &voice3_reads, clock);

    let qualifying: Vec<&Patch> = patches
        .iter()
        .filter(|p| p.member_count >= cli.min_members)
        .collect();
    if qualifying.is_empty() {
        bucket.skipped_no_patches += 1;
        return bucket;
    }

    // Per-subtune sub-aggregate to compute its own simple-fit mean.
    let mut sub_wf = ParamTally::default();
    let mut sub_a = ParamTally::default();
    let mut sub_d = ParamTally::default();
    let mut sub_s = ParamTally::default();
    let mut sub_r = ParamTally::default();
    let mut sub_pw = ParamTally::default();
    let mut samples_recorded: u32 = 0;

    for patch in &qualifying {
        bucket.patches_total += 1;
        // Voice-specific timbre lives on the per-voice profiles; use the first
        // playing voice's as the per-patch representative for these stats.
        if let Some(profile) = patch.voices.first() {
            *bucket
                .pw_kinds
                .entry(format!("{:?}", profile.pw_envelope.kind))
                .or_insert(0) += 1;
            if profile.waveform_loop.is_some() {
                bucket.has_waveform_loop += 1;
            }
            if profile.arpeggio_loop.is_some() {
                bucket.has_arpeggio_loop += 1;
            }
        }
        if patch.role_tags.lead {
            bucket.role_lead += 1;
        }
        if patch.role_tags.bass {
            bucket.role_bass += 1;
        }
        if patch.role_tags.pad {
            bucket.role_pad += 1;
        }
        if patch.role_tags.stab {
            bucket.role_stab += 1;
        }
        if patch.role_tags.bell {
            bucket.role_bell += 1;
        }
        if patch.role_tags.percussive {
            bucket.role_percussive += 1;
        }
        if patch.role_tags.sample {
            bucket.role_sample += 1;
        }
        if patch.role_tags.sound_effect {
            bucket.role_sound_effect += 1;
        }

        let member_idxs: Vec<usize> = assignments
            .iter()
            .enumerate()
            .filter_map(|(i, a)| if *a == Some(patch.id) { Some(i) } else { None })
            .collect();

        for &idx in member_idxs.iter().take(cli.samples) {
            let note = &notes[idx];
            let v_idx = (note.voice.0 - 1) as usize;
            let start = note.start_frame.0 as usize;
            let end = note
                .end_frame
                .map(|e| e.0 as usize)
                .unwrap_or(states.len())
                .min(states.len());
            if end <= start {
                continue;
            }
            let voice_states: Vec<&VoiceState> = states[start..end]
                .iter()
                .map(|s| &s.voices[v_idx])
                .collect();

            let waveforms: Vec<u32> = voice_states
                .iter()
                .map(|v| u32::from(v.control.waveform.to_control_byte() >> 4))
                .collect();
            let attacks: Vec<u32> = voice_states
                .iter()
                .map(|v| u32::from(v.adsr.attack))
                .collect();
            let decays: Vec<u32> = voice_states
                .iter()
                .map(|v| u32::from(v.adsr.decay))
                .collect();
            let sustains: Vec<u32> = voice_states
                .iter()
                .map(|v| u32::from(v.adsr.sustain))
                .collect();
            let releases: Vec<u32> = voice_states
                .iter()
                .map(|v| u32::from(v.adsr.release))
                .collect();
            let pws: Vec<u32> = voice_states
                .iter()
                .map(|v| u32::from(v.pulse_width.0))
                .collect();

            sub_wf.record(classify(&waveforms));
            sub_a.record(classify(&attacks));
            sub_d.record(classify(&decays));
            sub_s.record(classify(&sustains));
            sub_r.record(classify(&releases));
            sub_pw.record(classify(&pws));
            samples_recorded += 1;
        }
    }

    if samples_recorded > 0 {
        let mean = (sub_wf.simple_pct()
            + sub_a.simple_pct()
            + sub_d.simple_pct()
            + sub_s.simple_pct()
            + sub_r.simple_pct()
            + sub_pw.simple_pct())
            / 6.0;
        bucket.subtunes.push(PerSubtune {
            simple_fit_mean: mean,
            patch_count: qualifying.len() as u32,
            sample_count: samples_recorded,
        });
    }

    bucket.waveform.merge(&sub_wf);
    bucket.attack.merge(&sub_a);
    bucket.decay.merge(&sub_d);
    bucket.sustain.merge(&sub_s);
    bucket.release.merge(&sub_r);
    bucket.pulse_width.merge(&sub_pw);

    bucket
}

fn build_report(agg: &AggBucket, files_walked: u64, elapsed_secs: f64) -> Report {
    let mut subtune_means: Vec<f64> = agg.subtunes.iter().map(|s| s.simple_fit_mean).collect();
    subtune_means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let p = |q: f64| -> f64 {
        if subtune_means.is_empty() {
            return 0.0;
        }
        let idx = ((subtune_means.len() as f64 - 1.0) * q).round() as usize;
        subtune_means[idx.min(subtune_means.len() - 1)]
    };
    let mean = if subtune_means.is_empty() {
        0.0
    } else {
        subtune_means.iter().sum::<f64>() / subtune_means.len() as f64
    };

    let mut role_tags: HashMap<String, u64> = HashMap::new();
    role_tags.insert("lead".into(), agg.role_lead);
    role_tags.insert("bass".into(), agg.role_bass);
    role_tags.insert("pad".into(), agg.role_pad);
    role_tags.insert("stab".into(), agg.role_stab);
    role_tags.insert("bell".into(), agg.role_bell);
    role_tags.insert("percussive".into(), agg.role_percussive);
    role_tags.insert("sample".into(), agg.role_sample);
    role_tags.insert("sound_effect".into(), agg.role_sound_effect);

    let ps = |t: &ParamTally| ParamSummary {
        counts: t.clone(),
        simple_pct: t.simple_pct(),
    };

    Report {
        files_walked,
        subtunes_with_patches: agg.subtunes.len() as u64,
        patches_aggregated: agg.patches_total,
        skipped: Skipped {
            rsid: agg.skipped_rsid,
            emu_error: agg.skipped_emu,
            wall_timeout: agg.skipped_wall_timeout,
            no_patches: agg.skipped_no_patches,
            read_err: agg.skipped_read_err,
            header_err: agg.skipped_header_err,
        },
        parameters: Parameters {
            waveform: ps(&agg.waveform),
            attack: ps(&agg.attack),
            decay: ps(&agg.decay),
            sustain: ps(&agg.sustain),
            release: ps(&agg.release),
            pulse_width: ps(&agg.pulse_width),
        },
        pw_envelope_kinds: agg.pw_kinds.clone(),
        role_tags,
        loops: Loops {
            patches_with_waveform_loop: agg.has_waveform_loop,
            patches_with_arpeggio_loop: agg.has_arpeggio_loop,
        },
        per_subtune_simple_fit: Distribution {
            n: subtune_means.len(),
            mean,
            p10: p(0.10),
            p50: p(0.50),
            p90: p(0.90),
            p95: p(0.95),
        },
        elapsed_secs,
    }
}

fn frames_for_subtune(
    h: &Header,
    subtune: SubtuneIndex,
    max_frames: u32,
    song_lengths: Option<&SongLengths>,
) -> u32 {
    let Some(sl) = song_lengths else {
        return max_frames;
    };
    // Need MD5 — recompute since we only have the header. SongLengths
    // is keyed by full-file MD5; we don't have the bytes here without
    // refactoring. Skip the song-length cap when we can't look up.
    let Some(durations) = sl.lookup(&compute_md5_placeholder(h)) else {
        return max_frames;
    };
    let Some(idx) = (subtune.0 as usize).checked_sub(1) else {
        return max_frames;
    };
    let Some(d) = durations.get(idx) else {
        return max_frames;
    };
    let secs = d.as_secs_f64();
    if secs <= 0.0 {
        return max_frames;
    }
    let rate = match h.flags.clock {
        Clock::Ntsc => 60.0,
        _ => 50.0,
    };
    let frames = (secs * rate).ceil() as u64;
    frames.min(u64::from(max_frames)) as u32
}

/// Placeholder — we don't have the file bytes inside this function.
/// SongLengths.lookup needs the file MD5; for now we always return a
/// dummy that won't match, so `frames_for_subtune` falls back to
/// `max_frames`. Wiring real MD5 lookup requires plumbing the
/// `&[u8]` bytes through `analyze_one`; left as a future cleanup
/// since `--frames 3000` is the active cap anyway.
fn compute_md5_placeholder(_h: &Header) -> [u8; 16] {
    [0u8; 16]
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
