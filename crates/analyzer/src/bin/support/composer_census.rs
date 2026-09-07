use clap::{Parser, ValueEnum};
use rayon::prelude::*;
use serde::Serialize;
use sid_analyzer::analysis::effects::{Effect, EffectThresholds, detect_effects};
use sid_analyzer::analysis::note::{NoteEvent, detect_notes};
use sid_analyzer::analysis::timbre::{
    AttackClass, DrumSubclass, HardwareTrick, NoteCharacteristics, PitchBehavior, ReleaseClass,
    RoleTags, WaveformCombo, extract_timbre,
};
use sid_analyzer::analysis::{FrameState, VoiceId, analyze};
use sid_analyzer::emu::{self, PlaybackTiming};
use sid_analyzer::export::native::{EmulationStage, NativeError, extract_native};
use sid_analyzer::header::{self, Format, Header, SubtuneIndex};
use sid_analyzer::playerid::PlayerDb;
use sid_analyzer::songlengths::{self, SongLengths};
use sid_analyzer::trace::FrameIndex;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const DEFAULT_CALLS: u32 = 1_500;
const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_TOP_EXAMPLES: usize = 5;

#[derive(Debug, Parser)]
#[command(
    name = "sid-composer-census",
    about = "Measure common structure, musical traits, and improvement targets across a composer's SID files"
)]
struct Cli {
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long)]
    subject: String,
    #[arg(long)]
    driver_filter: Option<String>,
    #[arg(long, default_value_t = DEFAULT_CALLS)]
    frames: u32,
    #[arg(long, default_value_t = DEFAULT_CALLS)]
    native_frames: u32,
    #[arg(long)]
    full_length: bool,
    /// Local duration database for --full-length (unused in fixed-frame mode).
    #[arg(long, env = "HVSC_SONGLENGTHS")]
    songlengths: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = SubtuneMode::Start)]
    subtunes: SubtuneMode,
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECONDS)]
    timeout_seconds: u64,
    #[arg(long)]
    workers: Option<usize>,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, default_value_t = DEFAULT_TOP_EXAMPLES)]
    top_examples: usize,
    #[arg(long)]
    corpus_label: Option<String>,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    summary_output: Option<PathBuf>,
    #[arg(long)]
    markdown_output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum SubtuneMode {
    Start,
    All,
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("could not read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("could not walk {path}: {source}")]
    Walk {
        path: PathBuf,
        source: walkdir::Error,
    },
    #[error("could not serialize composer census: {0}")]
    Serialize(serde_json::Error),
    #[error("could not write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("could not load song lengths from {path}: {source}")]
    SongLengths { path: PathBuf, source: io::Error },
    #[error("full-length analysis needs --songlengths or HVSC_SONGLENGTHS")]
    SongLengthsUnavailable,
    #[error(
        "frames, native-frames, timeout-seconds, and top-examples must all be greater than zero"
    )]
    ZeroLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum FailureClass {
    Header,
    TimingInexact,
    LocateFailed,
    LocateAmbiguous,
    DecodeEmpty,
    DecodeFailed,
    DecodeUnreliable,
    EmulationTimingLoad,
    EmulationTimingInit,
    EmulationExtractorSetup,
    EmulationExtractorLoad,
    EmulationExtractorInit,
    EmulationNativeSampling,
    EmulationTrace,
    Invariant,
    NoExtractor,
    Timeout,
    UnsupportedConfiguration,
}

impl FailureClass {
    fn for_native_error(error: &NativeError) -> Self {
        match error {
            NativeError::TimingInexact { .. } => Self::TimingInexact,
            NativeError::LocateFailed { .. } => Self::LocateFailed,
            NativeError::LocateAmbiguous { .. } => Self::LocateAmbiguous,
            NativeError::DecodeEmpty { .. } => Self::DecodeEmpty,
            NativeError::DecodeFailed { .. } => Self::DecodeFailed,
            NativeError::DecodeUnreliable { .. } => Self::DecodeUnreliable,
            NativeError::Emulation { stage, .. } => match stage {
                EmulationStage::TimingLoad => Self::EmulationTimingLoad,
                EmulationStage::TimingInit => Self::EmulationTimingInit,
                EmulationStage::ExtractorSetup => Self::EmulationExtractorSetup,
                EmulationStage::ExtractorLoad => Self::EmulationExtractorLoad,
                EmulationStage::ExtractorInit => Self::EmulationExtractorInit,
                EmulationStage::NativeSampling => Self::EmulationNativeSampling,
                EmulationStage::Trace => Self::EmulationTrace,
            },
            NativeError::Invariant { .. } | NativeError::StructureInvariant { .. } => {
                Self::Invariant
            }
            NativeError::Unidentified
            | NativeError::NoExtractor { .. }
            | NativeError::NotImplemented { .. } => Self::NoExtractor,
            NativeError::UnsupportedConfiguration { .. } => Self::UnsupportedConfiguration,
        }
    }
}

#[derive(Debug, Serialize)]
struct CensusReport {
    schema_version: u32,
    subject: String,
    corpus_label: String,
    driver_filter: Option<String>,
    configuration: CensusConfiguration,
    summary: CensusSummary,
    results: Vec<CensusResult>,
}

#[derive(Debug, Serialize)]
struct CensusSnapshot<'a> {
    schema_version: u32,
    subject: &'a str,
    corpus_label: &'a str,
    driver_filter: &'a Option<String>,
    configuration: &'a CensusConfiguration,
    summary: &'a CensusSummary,
}

#[derive(Debug, Serialize)]
struct CensusConfiguration {
    fallback_calls: u32,
    native_call_limit: u32,
    full_length: bool,
    songlengths: Option<String>,
    subtunes: SubtuneMode,
    timeout_seconds: u64,
    top_examples: usize,
}

#[derive(Debug, Serialize)]
struct CensusSummary {
    selected_files: usize,
    player_ids: BTreeMap<String, usize>,
    psid_files: usize,
    rsid_files: usize,
    analyzed_subtunes: usize,
    analysis_windows: BTreeMap<AnalysisWindowSource, usize>,
    inexact_timing_subtunes: usize,
    trace_failures: BTreeMap<String, FailureAggregate>,
    totals: TraceTotals,
    native: NativeSummary,
    traits: TraitSummary,
    extraction_priorities: Vec<ExtractionPriority>,
    representation_priorities: Vec<RepresentationPriority>,
}

#[derive(Debug, Default, Serialize)]
struct TraceTotals {
    frames: u64,
    notes: u64,
    patches: u64,
}

#[derive(Debug, Serialize)]
struct NativeSummary {
    attempted_files: usize,
    accepted_files: usize,
    fully_accepted_files: usize,
    structured_files: usize,
    attempted_subtunes: usize,
    accepted_subtunes: usize,
    structured_subtunes: usize,
    failures: BTreeMap<FailureClass, FailureAggregate>,
    validation: ValidationDistribution,
    authored_features: BTreeMap<String, CountAggregate>,
}

#[derive(Debug, Default, Serialize)]
struct FailureAggregate {
    files: usize,
    subtunes: usize,
    examples: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
struct ValidationDistribution {
    precision_mean: Option<f64>,
    precision_min: Option<f64>,
    precision_median: Option<f64>,
    recall_mean: Option<f64>,
    recall_min: Option<f64>,
    recall_median: Option<f64>,
}

#[derive(Debug, Default, Serialize)]
struct CountAggregate {
    files: usize,
    subtunes: usize,
    occurrences: u64,
}

#[derive(Debug, Default, Serialize)]
struct TraitSummary {
    effects: BTreeMap<String, TraitAggregate>,
    waveforms: BTreeMap<String, TraitAggregate>,
    roles: BTreeMap<String, TraitAggregate>,
    attacks: BTreeMap<String, TraitAggregate>,
    releases: BTreeMap<String, TraitAggregate>,
    pitch_behaviors: BTreeMap<String, TraitAggregate>,
    hardware: BTreeMap<String, TraitAggregate>,
    chip: BTreeMap<String, TraitAggregate>,
}

#[derive(Debug, Default, Serialize)]
struct TraitAggregate {
    files: usize,
    subtunes: usize,
    occurrences: u64,
    frames: u64,
    examples: Vec<EvidenceWindow>,
}

#[derive(Debug, Serialize)]
struct ExtractionPriority {
    rank: usize,
    gap: String,
    affected_files: usize,
    affected_subtunes: usize,
    examples: Vec<String>,
    recommended_action: String,
}

#[derive(Debug, Serialize)]
struct RepresentationPriority {
    rank: usize,
    target: String,
    evidence: String,
    affected_files: usize,
    affected_subtunes: usize,
    occurrences: u64,
    frames: u64,
    examples: Vec<EvidenceWindow>,
    recommended_action: String,
}

#[derive(Debug, Serialize)]
struct CensusResult {
    path: String,
    player_id: Option<String>,
    title: Option<String>,
    released: Option<String>,
    format: Option<String>,
    subtune: Option<SubtuneIndex>,
    subtunes: Option<u16>,
    clock: Option<String>,
    sid_model: Option<String>,
    analysis_window: Option<AnalysisWindow>,
    trace: TraceOutcome,
    native: NativeOutcome,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum TraceOutcome {
    Analyzed { metrics: Box<TraceMetrics> },
    Skipped { reason: String },
    Rejected { class: String, detail: String },
}

#[derive(Debug, Serialize)]
struct TraceMetrics {
    frames: u32,
    notes: usize,
    patches: usize,
    traits: TraceTraits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum AnalysisWindowSource {
    FixedCalls,
    SongLength,
    MissingSongLengthFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct AnalysisWindow {
    source: AnalysisWindowSource,
    requested_calls: u32,
    native_calls: u32,
    timing_exact: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    song_duration_milliseconds: Option<SongDurationMilliseconds>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
struct SongDurationMilliseconds(u64);

#[derive(Debug, Default, Serialize)]
struct TraceTraits {
    effects: BTreeMap<String, TraitUsage>,
    waveforms: BTreeMap<String, TraitUsage>,
    roles: BTreeMap<String, TraitUsage>,
    attacks: BTreeMap<String, TraitUsage>,
    releases: BTreeMap<String, TraitUsage>,
    pitch_behaviors: BTreeMap<String, TraitUsage>,
    hardware: BTreeMap<String, TraitUsage>,
    chip: BTreeMap<String, TraitUsage>,
}

#[derive(Debug, Default, Serialize)]
struct TraitUsage {
    occurrences: u64,
    frames: u64,
    examples: Vec<EvidenceWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EvidenceWindow {
    path: String,
    title: String,
    subtune: SubtuneIndex,
    voice: Option<VoiceId>,
    start_frame: FrameIndex,
    end_frame: FrameIndex,
    start_second: SourceSecond,
    duration_seconds: WindowDurationSeconds,
}

impl EvidenceWindow {
    fn frame_count(&self) -> u64 {
        u64::from(
            self.end_frame
                .0
                .saturating_sub(self.start_frame.0)
                .saturating_add(1),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
struct SourceSecond(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
struct WindowDurationSeconds(u32);

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum NativeOutcome {
    Accepted {
        extractor: String,
        structured: bool,
        notes: usize,
        patches: usize,
        placements: usize,
        precision: f64,
        recall: f64,
        authored_features: BTreeMap<String, u64>,
    },
    Rejected {
        class: FailureClass,
        detail: String,
    },
    Skipped {
        reason: String,
    },
}

#[derive(Debug)]
struct TraitAccumulator {
    files: BTreeSet<String>,
    subtunes: usize,
    occurrences: u64,
    frames: u64,
    examples: Vec<EvidenceWindow>,
}

impl TraitAccumulator {
    fn new() -> Self {
        Self {
            files: BTreeSet::new(),
            subtunes: 0,
            occurrences: 0,
            frames: 0,
            examples: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct FailureAccumulator {
    files: BTreeSet<String>,
    subtunes: usize,
    examples: BTreeMap<String, String>,
}

impl FailureAccumulator {
    fn new() -> Self {
        Self {
            files: BTreeSet::new(),
            subtunes: 0,
            examples: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct CountAccumulator {
    files: BTreeSet<String>,
    subtunes: usize,
    occurrences: u64,
}

impl CountAccumulator {
    fn new() -> Self {
        Self {
            files: BTreeSet::new(),
            subtunes: 0,
            occurrences: 0,
        }
    }
}

fn collect_sid_paths(root: &Path) -> Result<Vec<PathBuf>, AppError> {
    let mut paths = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|source| AppError::Walk {
            path: root.to_owned(),
            source,
        })?;
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("sid"))
        {
            paths.push(entry.into_path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn read(path: &Path) -> Result<Vec<u8>, AppError> {
    std::fs::read(path).map_err(|source| AppError::Read {
        path: path.to_owned(),
        source,
    })
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn selected_paths(
    root: &Path,
    db: &PlayerDb,
    driver_filter: Option<&str>,
) -> Result<Vec<PathBuf>, AppError> {
    let paths = collect_sid_paths(root)?;
    let Some(driver_filter) = driver_filter else {
        eprintln!("selecting all {} SID files...", paths.len());
        return Ok(paths);
    };
    eprintln!(
        "identifying {driver_filter} among {} SID files...",
        paths.len()
    );
    let rows: Vec<Result<Option<PathBuf>, AppError>> = paths
        .par_iter()
        .map(|path| {
            let bytes = read(path)?;
            Ok((db.identify(&bytes) == Some(driver_filter)).then(|| path.clone()))
        })
        .collect();
    let mut selected = Vec::new();
    for row in rows {
        if let Some(path) = row? {
            selected.push(path);
        }
    }
    selected.sort();
    Ok(selected)
}

fn census_path(
    root: &Path,
    path: &Path,
    cli: &Cli,
    db: Arc<PlayerDb>,
    song_lengths: Option<&SongLengths>,
) -> Result<Vec<CensusResult>, AppError> {
    let bytes = read(path)?;
    let relative = relative_path(root, path);
    let player_id = db.identify(&bytes).map(str::to_owned);
    let header = match header::parse(&bytes) {
        Ok(header) => header,
        Err(error) => {
            return Ok(vec![CensusResult {
                path: relative,
                player_id,
                title: None,
                released: None,
                format: None,
                subtune: None,
                subtunes: None,
                clock: None,
                sid_model: None,
                analysis_window: None,
                trace: TraceOutcome::Rejected {
                    class: "header".to_owned(),
                    detail: error.to_string(),
                },
                native: NativeOutcome::Rejected {
                    class: FailureClass::Header,
                    detail: error.to_string(),
                },
            }]);
        }
    };
    let subtunes: Vec<_> = match cli.subtunes {
        SubtuneMode::Start => vec![header.start_song],
        SubtuneMode::All => (1..=header.songs.0).map(SubtuneIndex).collect(),
    };
    let durations =
        song_lengths.and_then(|lengths| lengths.lookup(&songlengths::compute_sid_md5(&bytes)));
    if header.format == Format::Rsid {
        return Ok(subtunes
            .into_iter()
            .map(|subtune| skipped_rsid_result(&relative, player_id.as_deref(), &header, subtune))
            .collect());
    }
    Ok(subtunes
        .into_iter()
        .map(|subtune| {
            analyze_subtune(
                SubtuneAnalysis {
                    path: &relative,
                    player_id: player_id.as_deref(),
                    header: &header,
                    bytes: &bytes,
                    cli,
                    song_duration: durations.and_then(|durations| {
                        subtune
                            .0
                            .checked_sub(1)
                            .and_then(|index| durations.get(index as usize))
                            .copied()
                    }),
                },
                subtune,
                Arc::clone(&db),
            )
        })
        .collect())
}

fn result_shell(
    path: &str,
    player_id: Option<&str>,
    header: &Header,
    subtune: SubtuneIndex,
    analysis_window: Option<AnalysisWindow>,
    trace: TraceOutcome,
    native: NativeOutcome,
) -> CensusResult {
    CensusResult {
        path: path.to_owned(),
        player_id: player_id.map(str::to_owned),
        title: Some(header.name.clone()),
        released: Some(header.released.clone()),
        format: Some(header.format.to_string()),
        subtune: Some(subtune),
        subtunes: Some(header.songs.0),
        clock: Some(header.flags.clock.to_string()),
        sid_model: Some(header.flags.sid_model.to_string()),
        analysis_window,
        trace,
        native,
    }
}

fn skipped_rsid_result(
    path: &str,
    player_id: Option<&str>,
    header: &Header,
    subtune: SubtuneIndex,
) -> CensusResult {
    result_shell(
        path,
        player_id,
        header,
        subtune,
        None,
        TraceOutcome::Skipped {
            reason: "rsid_host_unavailable".to_owned(),
        },
        NativeOutcome::Skipped {
            reason: "rsid_host_unavailable".to_owned(),
        },
    )
}

struct SubtuneAnalysis<'a> {
    path: &'a str,
    player_id: Option<&'a str>,
    header: &'a Header,
    bytes: &'a [u8],
    cli: &'a Cli,
    song_duration: Option<Duration>,
}

fn analyze_subtune(
    input: SubtuneAnalysis<'_>,
    subtune: SubtuneIndex,
    db: Arc<PlayerDb>,
) -> CensusResult {
    let SubtuneAnalysis {
        path,
        player_id,
        header,
        bytes,
        cli,
        song_duration,
    } = input;
    let mut analysis_window = match analysis_window(header, bytes, subtune, cli, song_duration) {
        Ok(window) => window,
        Err(error) => {
            return result_shell(
                path,
                player_id,
                header,
                subtune,
                None,
                TraceOutcome::Rejected {
                    class: "timing".to_owned(),
                    detail: error.to_string(),
                },
                NativeOutcome::Skipped {
                    reason: "timing_unavailable".to_owned(),
                },
            );
        }
    };
    let deadline = Instant::now() + Duration::from_secs(cli.timeout_seconds);
    let trace = match catch_unwind(AssertUnwindSafe(|| {
        emu::run_with_deadline(
            header,
            bytes,
            subtune,
            analysis_window.requested_calls,
            deadline,
        )
    })) {
        Ok(Ok(trace)) => trace,
        Ok(Err(error)) => {
            let class = if matches!(error, emu::EmuError::WallTimeout { .. }) {
                "timeout"
            } else {
                "emulation"
            };
            return result_shell(
                path,
                player_id,
                header,
                subtune,
                Some(analysis_window),
                TraceOutcome::Rejected {
                    class: class.to_owned(),
                    detail: error.to_string(),
                },
                NativeOutcome::Skipped {
                    reason: "trace_unavailable".to_owned(),
                },
            );
        }
        Err(_) => {
            return result_shell(
                path,
                player_id,
                header,
                subtune,
                Some(analysis_window),
                TraceOutcome::Rejected {
                    class: "panic".to_owned(),
                    detail: "trace analysis panicked".to_owned(),
                },
                NativeOutcome::Skipped {
                    reason: "trace_unavailable".to_owned(),
                },
            );
        }
    };
    let timing = PlaybackTiming::for_subtune(header, subtune).resolved_from_trace(&trace);
    analysis_window.timing_exact = Some(trace.timing_exact && timing.exact());
    let metrics = match catch_unwind(AssertUnwindSafe(|| {
        trace_metrics(path, header, subtune, &trace, timing, cli.top_examples)
    })) {
        Ok(metrics) => metrics,
        Err(_) => {
            return result_shell(
                path,
                player_id,
                header,
                subtune,
                Some(analysis_window),
                TraceOutcome::Rejected {
                    class: "panic".to_owned(),
                    detail: "musical feature analysis panicked".to_owned(),
                },
                NativeOutcome::Skipped {
                    reason: "trace_analysis_unavailable".to_owned(),
                },
            );
        }
    };
    let native = native_outcome(
        header.clone(),
        bytes.to_vec(),
        subtune,
        timing,
        analysis_window.native_calls,
        Duration::from_secs(cli.timeout_seconds),
        db,
    );
    result_shell(
        path,
        player_id,
        header,
        subtune,
        Some(analysis_window),
        TraceOutcome::Analyzed {
            metrics: Box::new(metrics),
        },
        native,
    )
}

fn analysis_window(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    cli: &Cli,
    song_duration: Option<Duration>,
) -> Result<AnalysisWindow, emu::EmuError> {
    if !cli.full_length {
        return Ok(AnalysisWindow {
            source: AnalysisWindowSource::FixedCalls,
            requested_calls: cli.frames,
            native_calls: cli.native_frames.min(cli.frames),
            timing_exact: None,
            song_duration_milliseconds: None,
        });
    }
    let Some(duration) = song_duration else {
        return Ok(AnalysisWindow {
            source: AnalysisWindowSource::MissingSongLengthFallback,
            requested_calls: cli.frames,
            native_calls: cli.native_frames.min(cli.frames),
            timing_exact: None,
            song_duration_milliseconds: None,
        });
    };
    let timing = PlaybackTiming::for_subtune(header, subtune);
    let timing = if timing.cia_timed {
        emu::resolve_playback_timing(header, bytes, subtune, timing)?
    } else {
        timing
    };
    let requested_calls = timing.calls_for_duration(duration);
    Ok(AnalysisWindow {
        source: AnalysisWindowSource::SongLength,
        requested_calls,
        native_calls: cli.native_frames.min(requested_calls),
        timing_exact: None,
        song_duration_milliseconds: Some(SongDurationMilliseconds(
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        )),
    })
}

fn trace_metrics(
    path: &str,
    header: &Header,
    subtune: SubtuneIndex,
    trace: &sid_analyzer::trace::Trace,
    timing: PlaybackTiming,
    top_examples: usize,
) -> TraceMetrics {
    let states = analyze(trace);
    let notes = detect_notes(&states, timing.clock);
    let effects = detect_effects(trace, &states, EffectThresholds::default());
    let voice3_reads = trace.voice3_reads_per_frame();
    let (characteristics, patches, _) =
        extract_timbre(&notes, &states, &effects, &voice3_reads, timing.clock);
    let context = WindowContext {
        path,
        title: &header.name,
        subtune,
        timing,
        top_examples,
    };
    let mut traits = TraceTraits::default();
    record_effects(&mut traits.effects, &effects, &context);
    record_note_traits(&mut traits, &notes, &characteristics, &context);
    record_chip_traits(&mut traits.chip, &states, &context);
    TraceMetrics {
        frames: states.len() as u32,
        notes: notes.len(),
        patches: patches.len(),
        traits,
    }
}

struct WindowContext<'a> {
    path: &'a str,
    title: &'a str,
    subtune: SubtuneIndex,
    timing: PlaybackTiming,
    top_examples: usize,
}

impl WindowContext<'_> {
    fn window(
        &self,
        voice: Option<VoiceId>,
        start_frame: FrameIndex,
        end_frame: FrameIndex,
    ) -> EvidenceWindow {
        let calls_per_second = self.timing.calls_per_second();
        let start_second = (f64::from(start_frame.0) / calls_per_second).floor() as u32;
        let end_second =
            (f64::from(end_frame.0.saturating_add(1)) / calls_per_second).ceil() as u32;
        EvidenceWindow {
            path: self.path.to_owned(),
            title: self.title.to_owned(),
            subtune: self.subtune,
            voice,
            start_frame,
            end_frame,
            start_second: SourceSecond(start_second),
            duration_seconds: WindowDurationSeconds(end_second.saturating_sub(start_second).max(1)),
        }
    }
}

fn record_effects(
    target: &mut BTreeMap<String, TraitUsage>,
    effects: &[sid_analyzer::analysis::effects::EffectSpan],
    context: &WindowContext<'_>,
) {
    for span in effects {
        let key = effect_key(span.effect).to_owned();
        let window = context.window(span.voice, span.start_frame, span.end_frame);
        record_trait(target, key, window, context.top_examples);
    }
}

fn record_note_traits(
    target: &mut TraceTraits,
    notes: &[NoteEvent],
    characteristics: &[NoteCharacteristics],
    context: &WindowContext<'_>,
) {
    for (note, characteristic) in notes.iter().zip(characteristics) {
        let end_frame = note.end_frame.unwrap_or_else(|| {
            FrameIndex(
                note.start_frame
                    .0
                    .saturating_add(u32::from(characteristic.length_frames).saturating_sub(1)),
            )
        });
        let window = context.window(Some(note.voice), note.start_frame, end_frame);
        record_trait(
            &mut target.waveforms,
            waveform_key(characteristic.dominant_waveform).to_owned(),
            window.clone(),
            context.top_examples,
        );
        record_trait(
            &mut target.attacks,
            attack_key(characteristic.attack).to_owned(),
            window.clone(),
            context.top_examples,
        );
        record_trait(
            &mut target.releases,
            release_key(characteristic.release_behavior).to_owned(),
            window.clone(),
            context.top_examples,
        );
        record_trait(
            &mut target.pitch_behaviors,
            pitch_key(characteristic.pitch_behavior).to_owned(),
            window.clone(),
            context.top_examples,
        );
        record_roles(
            &mut target.roles,
            characteristic.role_tags,
            &window,
            context.top_examples,
        );
        record_hardware(
            &mut target.hardware,
            characteristic,
            &window,
            context.top_examples,
        );
    }
}

fn record_roles(
    target: &mut BTreeMap<String, TraitUsage>,
    roles: RoleTags,
    window: &EvidenceWindow,
    top_examples: usize,
) {
    for (active, key) in [
        (roles.percussive, "percussive"),
        (roles.bass, "bass"),
        (roles.lead, "lead"),
        (roles.pad, "pad"),
        (roles.stab, "stab"),
        (roles.bell, "bell"),
        (roles.sample, "sample"),
        (roles.sound_effect, "sound_effect"),
    ] {
        if active {
            record_trait(target, key.to_owned(), window.clone(), top_examples);
        }
    }
    if let Some(subclass) = roles.drum_subclass {
        record_trait(
            target,
            format!("drum_{}", drum_key(subclass)),
            window.clone(),
            top_examples,
        );
    }
}

fn record_hardware(
    target: &mut BTreeMap<String, TraitUsage>,
    characteristic: &NoteCharacteristics,
    window: &EvidenceWindow,
    top_examples: usize,
) {
    for trick in &characteristic.hardware_tricks {
        let key = match trick {
            HardwareTrick::CombinedWaveform(combo) => {
                record_trait(
                    target,
                    "combined_waveform".to_owned(),
                    window.clone(),
                    top_examples,
                );
                format!("combined_waveform_{}", waveform_combo_key(*combo))
            }
            HardwareTrick::TestBitUsage => "test_bit".to_owned(),
            HardwareTrick::MidNoteWaveformSwitch { .. } => "mid_note_waveform_switch".to_owned(),
            HardwareTrick::D418Sample { .. } => "d418_sample".to_owned(),
            HardwareTrick::HardSync => "hard_sync".to_owned(),
            HardwareTrick::RingMod => "ring_mod".to_owned(),
            HardwareTrick::Voice3LfoSource => "voice3_lfo_source".to_owned(),
        };
        record_trait(target, key, window.clone(), top_examples);
    }
    if characteristic.waveform_program().is_some() {
        record_trait(
            target,
            "waveform_program".to_owned(),
            window.clone(),
            top_examples,
        );
    }
    if characteristic
        .waveform_sequence
        .loop_body_cloned()
        .is_some()
    {
        record_trait(
            target,
            "waveform_loop".to_owned(),
            window.clone(),
            top_examples,
        );
    }
    if characteristic.pitch_relative_loop.is_some() {
        record_trait(
            target,
            "arpeggio_loop".to_owned(),
            window.clone(),
            top_examples,
        );
    }
}

fn record_chip_traits(
    target: &mut BTreeMap<String, TraitUsage>,
    states: &[FrameState],
    context: &WindowContext<'_>,
) {
    record_runs(target, "filter_routed", states, context, |state| {
        state.filter.routing.any()
    });
    record_runs(target, "filter_audible", states, context, |state| {
        state.filter.routing.any()
            && (state.filter.mode.low_pass
                || state.filter.mode.band_pass
                || state.filter.mode.high_pass)
    });
    record_changes_after_initialization(
        target,
        "dynamic_filter_routing",
        states,
        context,
        |left, right| left.filter.routing != right.filter.routing,
    );
    record_changes_after_initialization(
        target,
        "dynamic_filter_mode",
        states,
        context,
        |left, right| left.filter.mode != right.filter.mode,
    );
    record_changes_after_initialization(
        target,
        "dynamic_filter_topology",
        states,
        context,
        |left, right| {
            left.filter.routing != right.filter.routing || left.filter.mode != right.filter.mode
        },
    );
}

fn record_runs(
    target: &mut BTreeMap<String, TraitUsage>,
    key: &str,
    states: &[FrameState],
    context: &WindowContext<'_>,
    predicate: impl Fn(&FrameState) -> bool,
) {
    let mut start = None;
    for (index, state) in states.iter().enumerate() {
        if predicate(state) {
            start.get_or_insert(index);
        } else if let Some(open) = start.take() {
            let window = context.window(None, states[open].frame, states[index - 1].frame);
            record_trait(target, key.to_owned(), window, context.top_examples);
        }
    }
    if let Some(open) = start
        && let Some(last) = states.last()
    {
        let window = context.window(None, states[open].frame, last.frame);
        record_trait(target, key.to_owned(), window, context.top_examples);
    }
}

fn record_changes_after_initialization(
    target: &mut BTreeMap<String, TraitUsage>,
    key: &str,
    states: &[FrameState],
    context: &WindowContext<'_>,
    predicate: impl Fn(&FrameState, &FrameState) -> bool,
) {
    for pair in states.windows(2).skip(8) {
        if predicate(&pair[0], &pair[1]) {
            let window = context.window(None, pair[1].frame, pair[1].frame);
            record_trait(target, key.to_owned(), window, context.top_examples);
        }
    }
}

fn record_trait(
    target: &mut BTreeMap<String, TraitUsage>,
    key: String,
    window: EvidenceWindow,
    top_examples: usize,
) {
    let entry = target.entry(key).or_default();
    entry.occurrences += 1;
    entry.frames += window.frame_count();
    entry.examples.push(window);
    sort_and_truncate_windows(&mut entry.examples, top_examples);
}

fn native_outcome(
    header: Header,
    bytes: Vec<u8>,
    subtune: SubtuneIndex,
    timing: PlaybackTiming,
    frames: u32,
    timeout: Duration,
    db: Arc<PlayerDb>,
) -> NativeOutcome {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = match catch_unwind(AssertUnwindSafe(|| {
            extract_native(&db, &header, &bytes, subtune, timing, frames)
        })) {
            Ok(Ok((_driver, extractor, program))) => {
                let placements = program
                    .semantic
                    .structure
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|voice| voice.placements.len())
                    .sum();
                let structured = program.semantic.structure.is_some()
                    && program
                        .semantic
                        .native
                        .as_ref()
                        .is_some_and(|native| native.recovered_structure.is_some());
                let authored_features = authored_features(&program.semantic.patches);
                match program.semantic.native.as_ref() {
                    Some(native) => NativeOutcome::Accepted {
                        extractor: extractor.to_owned(),
                        structured,
                        notes: program.semantic.notes.len(),
                        patches: program.semantic.patches.len(),
                        placements,
                        precision: native.validation.precision,
                        recall: native.validation.recall,
                        authored_features,
                    },
                    None => NativeOutcome::Rejected {
                        class: FailureClass::Invariant,
                        detail: "native extraction produced no semantic overlay".to_owned(),
                    },
                }
            }
            Ok(Err(error)) => NativeOutcome::Rejected {
                class: FailureClass::for_native_error(&error),
                detail: error.to_string(),
            },
            Err(_) => NativeOutcome::Rejected {
                class: FailureClass::Invariant,
                detail: "native extraction panicked".to_owned(),
            },
        };
        let _ = sender.send(outcome);
    });
    match receiver.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => NativeOutcome::Rejected {
            class: FailureClass::Timeout,
            detail: format!("native extraction exceeded {} seconds", timeout.as_secs()),
        },
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => NativeOutcome::Rejected {
            class: FailureClass::Invariant,
            detail: "native extraction worker disconnected".to_owned(),
        },
    }
}

fn authored_features(patches: &[sid_analyzer::analysis::timbre::Patch]) -> BTreeMap<String, u64> {
    let mut features = BTreeMap::new();
    for patch in patches {
        if patch.drum_drop {
            *features.entry("drum_drop".to_owned()).or_default() += 1;
        }
        if patch.authored_definition.is_some() {
            *features
                .entry("complete_instrument_definition".to_owned())
                .or_default() += 1;
        }
        if let Some(effects) = patch.authored_effects {
            *features.entry("authored_effects".to_owned()).or_default() += 1;
            for (present, key) in [
                (effects.vibrato.is_some(), "vibrato"),
                (effects.pwm.is_some(), "pwm"),
                (effects.pw_offset.is_some(), "pulse_width_offset"),
                (effects.chirp_up, "chirp_up"),
                (effects.arp, "arpeggio"),
            ] {
                if present {
                    *features.entry(key.to_owned()).or_default() += 1;
                }
            }
        }
    }
    features
}

fn summarize(results: &[CensusResult], top_examples: usize) -> CensusSummary {
    let selected_files: BTreeSet<_> = results.iter().map(|result| result.path.clone()).collect();
    let mut player_id_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for result in results {
        player_id_files
            .entry(
                result
                    .player_id
                    .clone()
                    .unwrap_or_else(|| "unidentified".to_owned()),
            )
            .or_default()
            .insert(result.path.clone());
    }
    let player_ids = player_id_files
        .into_iter()
        .map(|(player, files)| (player, files.len()))
        .collect();
    let psid_files: BTreeSet<_> = results
        .iter()
        .filter(|result| result.format.as_deref() == Some("PSID"))
        .map(|result| result.path.clone())
        .collect();
    let rsid_files: BTreeSet<_> = results
        .iter()
        .filter(|result| result.format.as_deref() == Some("RSID"))
        .map(|result| result.path.clone())
        .collect();
    let mut totals = TraceTotals::default();
    let mut analyzed_subtunes = 0usize;
    let mut analysis_windows = BTreeMap::new();
    let mut inexact_timing_subtunes = 0usize;
    let mut trace_failure_accumulators: BTreeMap<String, FailureAccumulator> = BTreeMap::new();
    let mut trait_accumulators = TraitAccumulators::new();
    let mut native_attempted = 0usize;
    let mut native_accepted = 0usize;
    let mut native_structured = 0usize;
    let mut native_file_outcomes: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let mut failure_accumulators: BTreeMap<FailureClass, FailureAccumulator> = BTreeMap::new();
    let mut authored_accumulators: BTreeMap<String, CountAccumulator> = BTreeMap::new();
    let mut precisions = Vec::new();
    let mut recalls = Vec::new();

    for result in results {
        match &result.trace {
            TraceOutcome::Analyzed { metrics } => {
                analyzed_subtunes += 1;
                if let Some(window) = result.analysis_window {
                    *analysis_windows.entry(window.source).or_default() += 1;
                    inexact_timing_subtunes += usize::from(window.timing_exact == Some(false));
                }
                totals.frames += u64::from(metrics.frames);
                totals.notes += metrics.notes as u64;
                totals.patches += metrics.patches as u64;
                trait_accumulators.add(&result.path, &metrics.traits);
            }
            TraceOutcome::Rejected { class, detail } => {
                let accumulator = trace_failure_accumulators
                    .entry(class.clone())
                    .or_insert_with(FailureAccumulator::new);
                accumulator.files.insert(result.path.clone());
                accumulator.subtunes += 1;
                accumulator
                    .examples
                    .entry(result.path.clone())
                    .or_insert_with(|| detail.clone());
            }
            TraceOutcome::Skipped { .. } => {}
        }
        match &result.native {
            NativeOutcome::Accepted {
                structured,
                precision,
                recall,
                authored_features,
                ..
            } => {
                native_attempted += 1;
                native_accepted += 1;
                native_structured += usize::from(*structured);
                let file = native_file_outcomes.entry(result.path.clone()).or_default();
                file.0 += 1;
                file.1 += 1;
                file.2 += usize::from(*structured);
                precisions.push(*precision);
                recalls.push(*recall);
                for (feature, occurrences) in authored_features {
                    let accumulator = authored_accumulators
                        .entry(feature.clone())
                        .or_insert_with(CountAccumulator::new);
                    accumulator.files.insert(result.path.clone());
                    accumulator.subtunes += 1;
                    accumulator.occurrences += occurrences;
                }
            }
            NativeOutcome::Rejected { class, detail } => {
                native_attempted += 1;
                native_file_outcomes
                    .entry(result.path.clone())
                    .or_default()
                    .0 += 1;
                let accumulator = failure_accumulators
                    .entry(*class)
                    .or_insert_with(FailureAccumulator::new);
                accumulator.files.insert(result.path.clone());
                accumulator.subtunes += 1;
                accumulator
                    .examples
                    .entry(result.path.clone())
                    .or_insert_with(|| detail.clone());
            }
            NativeOutcome::Skipped { .. } => {}
        }
    }

    let traits = trait_accumulators.finish(top_examples);
    let trace_failures = finish_failure_map(trace_failure_accumulators, top_examples);
    let failures: BTreeMap<_, _> = failure_accumulators
        .into_iter()
        .map(|(class, accumulator)| (class, finish_failure_accumulator(accumulator, top_examples)))
        .collect();
    let authored_features = authored_accumulators
        .into_iter()
        .map(|(feature, accumulator)| {
            (
                feature,
                CountAggregate {
                    files: accumulator.files.len(),
                    subtunes: accumulator.subtunes,
                    occurrences: accumulator.occurrences,
                },
            )
        })
        .collect();
    let native = NativeSummary {
        attempted_files: native_file_outcomes.len(),
        accepted_files: native_file_outcomes
            .values()
            .filter(|(_, accepted, _)| *accepted > 0)
            .count(),
        fully_accepted_files: native_file_outcomes
            .values()
            .filter(|(attempted, accepted, _)| *attempted == *accepted)
            .count(),
        structured_files: native_file_outcomes
            .values()
            .filter(|(_, _, structured)| *structured > 0)
            .count(),
        attempted_subtunes: native_attempted,
        accepted_subtunes: native_accepted,
        structured_subtunes: native_structured,
        failures,
        validation: validation_distribution(&mut precisions, &mut recalls),
        authored_features,
    };
    let rsid_subtunes = results
        .iter()
        .filter(|result| result.format.as_deref() == Some("RSID"))
        .count();
    let extraction_priorities =
        extraction_priorities(&native, &trace_failures, rsid_files.len(), rsid_subtunes);
    let representation_priorities = representation_priorities(&traits);
    CensusSummary {
        selected_files: selected_files.len(),
        player_ids,
        psid_files: psid_files.len(),
        rsid_files: rsid_files.len(),
        analyzed_subtunes,
        analysis_windows,
        inexact_timing_subtunes,
        trace_failures,
        totals,
        native,
        traits,
        extraction_priorities,
        representation_priorities,
    }
}

fn finish_failure_map(
    source: BTreeMap<String, FailureAccumulator>,
    top_examples: usize,
) -> BTreeMap<String, FailureAggregate> {
    source
        .into_iter()
        .map(|(class, accumulator)| (class, finish_failure_accumulator(accumulator, top_examples)))
        .collect()
}

fn finish_failure_accumulator(
    accumulator: FailureAccumulator,
    top_examples: usize,
) -> FailureAggregate {
    let mut examples: Vec<_> = accumulator
        .examples
        .into_iter()
        .map(|(path, detail)| format!("{path}: {detail}"))
        .collect();
    examples.truncate(top_examples);
    FailureAggregate {
        files: accumulator.files.len(),
        subtunes: accumulator.subtunes,
        examples,
    }
}

struct TraitAccumulators {
    effects: BTreeMap<String, TraitAccumulator>,
    waveforms: BTreeMap<String, TraitAccumulator>,
    roles: BTreeMap<String, TraitAccumulator>,
    attacks: BTreeMap<String, TraitAccumulator>,
    releases: BTreeMap<String, TraitAccumulator>,
    pitch_behaviors: BTreeMap<String, TraitAccumulator>,
    hardware: BTreeMap<String, TraitAccumulator>,
    chip: BTreeMap<String, TraitAccumulator>,
}

impl TraitAccumulators {
    fn new() -> Self {
        Self {
            effects: BTreeMap::new(),
            waveforms: BTreeMap::new(),
            roles: BTreeMap::new(),
            attacks: BTreeMap::new(),
            releases: BTreeMap::new(),
            pitch_behaviors: BTreeMap::new(),
            hardware: BTreeMap::new(),
            chip: BTreeMap::new(),
        }
    }

    fn add(&mut self, path: &str, traits: &TraceTraits) {
        add_trait_map(&mut self.effects, path, &traits.effects);
        add_trait_map(&mut self.waveforms, path, &traits.waveforms);
        add_trait_map(&mut self.roles, path, &traits.roles);
        add_trait_map(&mut self.attacks, path, &traits.attacks);
        add_trait_map(&mut self.releases, path, &traits.releases);
        add_trait_map(&mut self.pitch_behaviors, path, &traits.pitch_behaviors);
        add_trait_map(&mut self.hardware, path, &traits.hardware);
        add_trait_map(&mut self.chip, path, &traits.chip);
    }

    fn finish(self, top_examples: usize) -> TraitSummary {
        TraitSummary {
            effects: finish_trait_map(self.effects, top_examples),
            waveforms: finish_trait_map(self.waveforms, top_examples),
            roles: finish_trait_map(self.roles, top_examples),
            attacks: finish_trait_map(self.attacks, top_examples),
            releases: finish_trait_map(self.releases, top_examples),
            pitch_behaviors: finish_trait_map(self.pitch_behaviors, top_examples),
            hardware: finish_trait_map(self.hardware, top_examples),
            chip: finish_trait_map(self.chip, top_examples),
        }
    }
}

fn add_trait_map(
    target: &mut BTreeMap<String, TraitAccumulator>,
    path: &str,
    source: &BTreeMap<String, TraitUsage>,
) {
    for (key, usage) in source {
        let accumulator = target
            .entry(key.clone())
            .or_insert_with(TraitAccumulator::new);
        accumulator.files.insert(path.to_owned());
        accumulator.subtunes += 1;
        accumulator.occurrences += usage.occurrences;
        accumulator.frames += usage.frames;
        accumulator.examples.extend(usage.examples.iter().cloned());
    }
}

fn finish_trait_map(
    source: BTreeMap<String, TraitAccumulator>,
    top_examples: usize,
) -> BTreeMap<String, TraitAggregate> {
    source
        .into_iter()
        .map(|(key, mut accumulator)| {
            sort_and_truncate_windows(&mut accumulator.examples, top_examples);
            (
                key,
                TraitAggregate {
                    files: accumulator.files.len(),
                    subtunes: accumulator.subtunes,
                    occurrences: accumulator.occurrences,
                    frames: accumulator.frames,
                    examples: accumulator.examples,
                },
            )
        })
        .collect()
}

fn sort_and_truncate_windows(windows: &mut Vec<EvidenceWindow>, limit: usize) {
    windows.sort_by(|left, right| {
        right
            .frame_count()
            .cmp(&left.frame_count())
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.subtune.0.cmp(&right.subtune.0))
            .then_with(|| left.start_frame.0.cmp(&right.start_frame.0))
            .then_with(|| left.voice.cmp(&right.voice))
    });
    windows.truncate(limit);
}

fn validation_distribution(precisions: &mut [f64], recalls: &mut [f64]) -> ValidationDistribution {
    precisions.sort_by(f64::total_cmp);
    recalls.sort_by(f64::total_cmp);
    ValidationDistribution {
        precision_mean: mean(precisions),
        precision_min: precisions.first().copied(),
        precision_median: median(precisions),
        recall_mean: mean(recalls),
        recall_min: recalls.first().copied(),
        recall_median: median(recalls),
    }
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else if values.len().is_multiple_of(2) {
        Some((values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0)
    } else {
        values.get(values.len() / 2).copied()
    }
}

fn extraction_priorities(
    native: &NativeSummary,
    trace_failures: &BTreeMap<String, FailureAggregate>,
    rsid_files: usize,
    rsid_subtunes: usize,
) -> Vec<ExtractionPriority> {
    let mut rows: Vec<_> = native
        .failures
        .iter()
        .map(|(class, aggregate)| ExtractionPriority {
            rank: 0,
            gap: failure_key(*class).to_owned(),
            affected_files: aggregate.files,
            affected_subtunes: aggregate.subtunes,
            examples: aggregate.examples.clone(),
            recommended_action: extraction_action(*class).to_owned(),
        })
        .collect();
    for (class, aggregate) in trace_failures {
        rows.push(ExtractionPriority {
            rank: 0,
            gap: format!("trace_{class}"),
            affected_files: aggregate.files,
            affected_subtunes: aggregate.subtunes,
            examples: aggregate.examples.clone(),
            recommended_action: "Reproduce the trace failure and add the smallest reusable emulator or host correction required by its failure class.".to_owned(),
        });
    }
    if rsid_files > 0 {
        rows.push(ExtractionPriority {
            rank: 0,
            gap: "rsid_host".to_owned(),
            affected_files: rsid_files,
            affected_subtunes: rsid_subtunes,
            examples: Vec::new(),
            recommended_action: "Scope a separate RSID host with explicit Kernal, BASIC, CIA, and interrupt requirements.".to_owned(),
        });
    }
    rows.sort_by(|left, right| {
        right
            .affected_files
            .cmp(&left.affected_files)
            .then_with(|| right.affected_subtunes.cmp(&left.affected_subtunes))
            .then_with(|| left.gap.cmp(&right.gap))
    });
    for (index, row) in rows.iter_mut().enumerate() {
        row.rank = index + 1;
    }
    rows
}

fn representation_priorities(traits: &TraitSummary) -> Vec<RepresentationPriority> {
    let specifications = [
        (
            "shared_filter_routing",
            "chip",
            "dynamic_filter_topology",
            "Qualify one chip-global filter with dynamic voice routing and mode transitions.",
        ),
        (
            "combined_waveform_fidelity",
            "hardware",
            "combined_waveform",
            "Remeasure 6581 combined-waveform behavior and pin cross-tune render controls.",
        ),
        (
            "waveform_program",
            "hardware",
            "waveform_program",
            "Qualify longer SID waveform sequences with loop and continuation semantics.",
        ),
        (
            "shared_hard_sync",
            "effects",
            "hard_sync",
            "Qualify a shared live oscillator relationship for hard-sync source and destination.",
        ),
        (
            "shared_ring_modulation",
            "effects",
            "ring_mod",
            "Qualify a shared moving modulator and recalibrate the remaining bright 6581 response.",
        ),
        (
            "vibrato_shape_and_delay",
            "pitch_behaviors",
            "vibrato",
            "Recover vibrato onset, rate, depth, and shape before replacing measured pitch automation.",
        ),
        (
            "sampler_bundle",
            "effects",
            "sample",
            "Qualify sampler bundle writing and loading for reconstructed D418 PCM.",
        ),
        (
            "voice3_modulation",
            "effects",
            "voice3_modulator",
            "Qualify exact OSC3-driven modulation where causal evidence identifies a supported consumer.",
        ),
    ];
    let mut rows = Vec::new();
    for (target, category, key, action) in specifications {
        let map = match category {
            "effects" => &traits.effects,
            "hardware" => &traits.hardware,
            "chip" => &traits.chip,
            "pitch_behaviors" => &traits.pitch_behaviors,
            _ => continue,
        };
        if let Some(aggregate) = map.get(key) {
            rows.push(RepresentationPriority {
                rank: 0,
                target: target.to_owned(),
                evidence: format!("{category}/{key}"),
                affected_files: aggregate.files,
                affected_subtunes: aggregate.subtunes,
                occurrences: aggregate.occurrences,
                frames: aggregate.frames,
                examples: aggregate.examples.clone(),
                recommended_action: action.to_owned(),
            });
        }
    }
    rows.sort_by(|left, right| {
        right
            .affected_files
            .cmp(&left.affected_files)
            .then_with(|| right.frames.cmp(&left.frames))
            .then_with(|| left.target.cmp(&right.target))
    });
    for (index, row) in rows.iter_mut().enumerate() {
        row.rank = index + 1;
    }
    rows
}

fn effect_key(effect: Effect) -> &'static str {
    match effect {
        Effect::HardSync => "hard_sync",
        Effect::RingMod => "ring_mod",
        Effect::Sample => "sample",
        Effect::Pwm => "pwm",
        Effect::FilterSweep => "filter_sweep",
        Effect::FilterResonanceSweep => "filter_resonance_sweep",
        Effect::FilterModeModulation => "filter_mode_modulation",
        Effect::Portamento => "portamento",
        Effect::Vibrato => "vibrato",
        Effect::Tremolo => "tremolo",
        Effect::Arpeggio => "arpeggio",
        Effect::Voice3Modulator => "voice3_modulator",
    }
}

fn waveform_key(byte: u8) -> &'static str {
    match byte & 0xf0 {
        0x00 => "silent",
        0x10 => "triangle",
        0x20 => "sawtooth",
        0x30 => "triangle_sawtooth",
        0x40 => "pulse",
        0x50 => "triangle_pulse",
        0x60 => "sawtooth_pulse",
        0x70 => "triangle_sawtooth_pulse",
        0x80 => "noise",
        0x90 => "triangle_noise",
        0xa0 => "sawtooth_noise",
        0xb0 => "triangle_sawtooth_noise",
        0xc0 => "pulse_noise",
        0xd0 => "triangle_pulse_noise",
        0xe0 => "sawtooth_pulse_noise",
        _ => "triangle_sawtooth_pulse_noise",
    }
}

fn attack_key(attack: AttackClass) -> &'static str {
    match attack {
        AttackClass::Instant => "instant",
        AttackClass::Fast => "fast",
        AttackClass::Medium => "medium",
        AttackClass::Slow => "slow",
    }
}

fn release_key(release: ReleaseClass) -> &'static str {
    match release {
        ReleaseClass::GateCut => "gate_cut",
        ReleaseClass::NaturalDecay => "natural_decay",
        ReleaseClass::Sustained => "sustained",
    }
}

fn pitch_key(pitch: PitchBehavior) -> &'static str {
    match pitch {
        PitchBehavior::Stable => "stable",
        PitchBehavior::Vibrato => "vibrato",
        PitchBehavior::Portamento => "portamento",
        PitchBehavior::Arpeggio => "arpeggio",
        PitchBehavior::OneShotSweep => "one_shot_sweep",
    }
}

fn drum_key(drum: DrumSubclass) -> &'static str {
    match drum {
        DrumSubclass::Kick => "kick",
        DrumSubclass::Snare => "snare",
        DrumSubclass::HihatClosed => "hihat_closed",
        DrumSubclass::HihatOpen => "hihat_open",
        DrumSubclass::Tom => "tom",
        DrumSubclass::PercMetallic => "metallic",
    }
}

fn waveform_combo_key(combo: WaveformCombo) -> &'static str {
    match combo {
        WaveformCombo::TriSaw => "triangle_sawtooth",
        WaveformCombo::PulseTri => "triangle_pulse",
        WaveformCombo::PulseSaw => "sawtooth_pulse",
        WaveformCombo::NoiseLock => "noise_lock",
        WaveformCombo::Triple => "triple",
    }
}

fn failure_key(class: FailureClass) -> &'static str {
    match class {
        FailureClass::Header => "header",
        FailureClass::TimingInexact => "timing_inexact",
        FailureClass::LocateFailed => "locate_failed",
        FailureClass::LocateAmbiguous => "locate_ambiguous",
        FailureClass::DecodeEmpty => "decode_empty",
        FailureClass::DecodeFailed => "decode_failed",
        FailureClass::DecodeUnreliable => "decode_unreliable",
        FailureClass::EmulationTimingLoad => "emulation_timing_load",
        FailureClass::EmulationTimingInit => "emulation_timing_init",
        FailureClass::EmulationExtractorSetup => "emulation_extractor_setup",
        FailureClass::EmulationExtractorLoad => "emulation_extractor_load",
        FailureClass::EmulationExtractorInit => "emulation_extractor_init",
        FailureClass::EmulationNativeSampling => "emulation_native_sampling",
        FailureClass::EmulationTrace => "emulation_trace",
        FailureClass::Invariant => "invariant",
        FailureClass::NoExtractor => "no_extractor",
        FailureClass::Timeout => "timeout",
        FailureClass::UnsupportedConfiguration => "unsupported_configuration",
    }
}

fn extraction_action(class: FailureClass) -> &'static str {
    match class {
        FailureClass::TimingInexact => {
            "Cluster timing variants and recover their file-derived CIA or stall schedule."
        }
        FailureClass::LocateFailed => {
            "Cluster post-init code signatures and implement the highest-impact engine or layout family behind typed rejection."
        }
        FailureClass::LocateAmbiguous => {
            "Strengthen locator evidence for the ambiguous family without weakening ambiguity rejection."
        }
        FailureClass::DecodeEmpty | FailureClass::DecodeFailed | FailureClass::DecodeUnreliable => {
            "Group decoder failures by recovered layout and implement the highest-impact grammar variant behind native validation."
        }
        FailureClass::EmulationTimingLoad
        | FailureClass::EmulationTimingInit
        | FailureClass::EmulationExtractorSetup
        | FailureClass::EmulationExtractorLoad
        | FailureClass::EmulationExtractorInit
        | FailureClass::EmulationNativeSampling
        | FailureClass::EmulationTrace => {
            "Group emulation failures by stage and add the smallest reusable host or CPU behavior required by the largest cluster."
        }
        FailureClass::NoExtractor => {
            "Group uncovered player IDs and add an extractor only when a coherent, high-impact family justifies it."
        }
        FailureClass::Timeout => {
            "Profile the slowest files and bound or optimize the shared hot path before increasing corpus budgets."
        }
        FailureClass::UnsupportedConfiguration => {
            "Separate genuine unsupported SID configurations from driver variants and retain typed rejection."
        }
        FailureClass::Header | FailureClass::Invariant => {
            "Audit the representative failures before changing production behavior."
        }
    }
}

fn render_markdown(report: &CensusReport) -> String {
    let summary = &report.summary;
    let mut output = String::new();
    output.push_str(&format!("# {} corpus census\n\n", report.subject));
    output.push_str("Generated by `sid-composer-census`; do not edit measured values by hand.\n\n");
    output.push_str(&format!(
        "Corpus `{}` contains **{} selected files**: **{} PSID** and **{} RSID**. The run analyzed **{} subtunes**, **{} calls**, **{} notes**, and **{} trace-derived patches**.\n\n",
        report.corpus_label,
        summary.selected_files,
        summary.psid_files,
        summary.rsid_files,
        summary.analyzed_subtunes,
        summary.totals.frames,
        summary.totals.notes,
        summary.totals.patches,
    ));
    output.push_str("SIDId distribution: ");
    let player_ids: Vec<_> = summary
        .player_ids
        .iter()
        .map(|(player, files)| format!("`{player}` {files}"))
        .collect();
    output.push_str(&player_ids.join(", "));
    output.push_str(".\n\n");
    output.push_str(&format!(
        "Native extraction accepted at least one subtune in **{}/{} PSID files**, accepted every attempted subtune in **{} files**, and recovered structure for **{} files**. At subtune level it accepted **{}/{}** and structured **{}**.\n\n",
        summary.native.accepted_files,
        summary.native.attempted_files,
        summary.native.fully_accepted_files,
        summary.native.structured_files,
        summary.native.accepted_subtunes,
        summary.native.attempted_subtunes,
        summary.native.structured_subtunes,
    ));
    output.push_str("## Extraction priorities\n\n");
    output.push_str("| Rank | Gap | Files | Subtunes | Recommended action |\n");
    output.push_str("|---:|---|---:|---:|---|\n");
    for priority in &summary.extraction_priorities {
        output.push_str(&format!(
            "| {} | `{}` | {} | {} | {} |\n",
            priority.rank,
            priority.gap,
            priority.affected_files,
            priority.affected_subtunes,
            priority.recommended_action,
        ));
    }
    output.push_str("\n## Representation priorities\n\n");
    output.push_str("| Rank | Target | Files | Occurrences | Frames | Representative window |\n");
    output.push_str("|---:|---|---:|---:|---:|---|\n");
    for priority in &summary.representation_priorities {
        let example = priority
            .examples
            .first()
            .map(markdown_window)
            .unwrap_or_else(|| "-".to_owned());
        output.push_str(&format!(
            "| {} | `{}` | {} | {} | {} | {} |\n",
            priority.rank,
            priority.target,
            priority.affected_files,
            priority.occurrences,
            priority.frames,
            example,
        ));
    }
    output.push_str("\n## Common musical traits\n\n");
    markdown_trait_table(&mut output, "Effects", &summary.traits.effects);
    markdown_trait_table(&mut output, "Waveforms", &summary.traits.waveforms);
    markdown_trait_table(&mut output, "Roles", &summary.traits.roles);
    markdown_trait_table(
        &mut output,
        "Hardware and programs",
        &summary.traits.hardware,
    );
    output.push_str("## Method and limits\n\n");
    let subtune_scope = match report.configuration.subtunes {
        SubtuneMode::Start => "the declared start subtune",
        SubtuneMode::All => "all declared subtunes",
    };
    let window_method = if report.configuration.full_length {
        let song_lengths = summary
            .analysis_windows
            .get(&AnalysisWindowSource::SongLength)
            .copied()
            .unwrap_or_default();
        let fallbacks = summary
            .analysis_windows
            .get(&AnalysisWindowSource::MissingSongLengthFallback)
            .copied()
            .unwrap_or_default();
        format!(
            "uses HVSC song lengths for {song_lengths} analyzed subtunes and the {}-call fallback for {fallbacks}",
            report.configuration.fallback_calls,
        )
    } else {
        format!(
            "uses a fixed {} play calls per subtune",
            report.configuration.fallback_calls,
        )
    };
    output.push_str(&format!(
        "The run uses {subtune_scope}, {window_method}, caps native validation at {} play calls, and applies a {} second trace and native-extraction timeout. Playback timing remained inexact for {} analyzed subtunes. Trait counts are trace-derived and effect spans are non-exclusive; the vibrato priority uses the resolved per-note pitch behavior. Native acceptance uses the production validation gate. Longest detected spans select fixture candidates. RSID files receive metadata and coverage accounting only because the project has no RSID host. Render priorities measure prevalence, not audible residual; each behavior change still requires a pinned reSID-versus-Pertylizer A/B fixture.\n",
        report.configuration.native_call_limit,
        report.configuration.timeout_seconds,
        summary.inexact_timing_subtunes,
    ));
    output
}

fn markdown_trait_table(
    output: &mut String,
    title: &str,
    traits: &BTreeMap<String, TraitAggregate>,
) {
    let mut rows: Vec<_> = traits.iter().collect();
    rows.sort_by(|(left_key, left), (right_key, right)| {
        right
            .files
            .cmp(&left.files)
            .then_with(|| right.frames.cmp(&left.frames))
            .then_with(|| left_key.cmp(right_key))
    });
    output.push_str(&format!("### {title}\n\n"));
    output.push_str("| Trait | Files | Occurrences | Frames |\n");
    output.push_str("|---|---:|---:|---:|\n");
    for (key, aggregate) in rows.into_iter().take(12) {
        output.push_str(&format!(
            "| `{key}` | {} | {} | {} |\n",
            aggregate.files, aggregate.occurrences, aggregate.frames,
        ));
    }
    output.push('\n');
}

fn markdown_window(window: &EvidenceWindow) -> String {
    let voice = window
        .voice
        .map_or_else(|| "mix".to_owned(), |voice| format!("V{voice}"));
    format!(
        "`{}` subtune {}, {} at {}s for {}s",
        window.path, window.subtune, voice, window.start_second.0, window.duration_seconds.0,
    )
}

fn corpus_label(cli: &Cli) -> String {
    cli.corpus_label.clone().unwrap_or_else(|| {
        cli.corpus
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("corpus")
            .to_owned()
    })
}

fn load_song_lengths(cli: &Cli) -> Result<(Option<SongLengths>, Option<String>), AppError> {
    if !cli.full_length {
        return Ok((None, None));
    }
    let path = cli
        .songlengths
        .as_ref()
        .ok_or(AppError::SongLengthsUnavailable)?;
    let lengths = SongLengths::load(path).map_err(|source| AppError::SongLengths {
        path: path.clone(),
        source,
    })?;
    Ok((Some(lengths), Some(path.to_string_lossy().into_owned())))
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    std::fs::write(path, bytes).map_err(|source| AppError::Write {
        path: path.to_owned(),
        source,
    })
}

fn write_report(report: &CensusReport, cli: &Cli) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(report).map_err(AppError::Serialize)?;
    match &cli.output {
        Some(path) => write_bytes(path, &bytes)?,
        None => {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            output.write_all(&bytes).map_err(|source| AppError::Write {
                path: PathBuf::from("<stdout>"),
                source,
            })?;
            output.write_all(b"\n").map_err(|source| AppError::Write {
                path: PathBuf::from("<stdout>"),
                source,
            })?;
        }
    }
    if let Some(path) = &cli.summary_output {
        let snapshot = CensusSnapshot {
            schema_version: report.schema_version,
            subject: &report.subject,
            corpus_label: &report.corpus_label,
            driver_filter: &report.driver_filter,
            configuration: &report.configuration,
            summary: &report.summary,
        };
        let bytes = serde_json::to_vec_pretty(&snapshot).map_err(AppError::Serialize)?;
        write_bytes(path, &bytes)?;
    }
    if let Some(path) = &cli.markdown_output {
        write_bytes(path, render_markdown(report).as_bytes())?;
    }
    Ok(())
}

fn run() -> Result<(), AppError> {
    let cli = Cli::parse();
    if cli.frames == 0
        || cli.native_frames == 0
        || cli.timeout_seconds == 0
        || cli.top_examples == 0
    {
        return Err(AppError::ZeroLimit);
    }
    if let Some(workers) = cli.workers
        && let Err(error) = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build_global()
    {
        eprintln!("warning: could not configure worker pool: {error}");
    }
    let started = Instant::now();
    let db = Arc::new(PlayerDb::embedded());
    let (song_lengths, songlengths_source) = load_song_lengths(&cli)?;
    let mut paths = selected_paths(&cli.corpus, &db, cli.driver_filter.as_deref())?;
    if let Some(limit) = cli.limit {
        paths.truncate(limit);
    }
    eprintln!(
        "analyzing {} selected files using {}...",
        paths.len(),
        if cli.full_length {
            "HVSC song lengths"
        } else {
            "fixed call windows"
        },
    );
    let completed = AtomicUsize::new(0);
    let rows: Vec<Result<Vec<CensusResult>, AppError>> = paths
        .par_iter()
        .map(|path| {
            let result = census_path(
                &cli.corpus,
                path,
                &cli,
                Arc::clone(&db),
                song_lengths.as_ref(),
            );
            let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
            if count.is_multiple_of(25) || count == paths.len() {
                eprintln!(
                    "  analyzed {count}/{} files in {:.1}s",
                    paths.len(),
                    started.elapsed().as_secs_f64(),
                );
            }
            result
        })
        .collect();
    let mut results = Vec::new();
    for row in rows {
        results.extend(row?);
    }
    results.sort_by(|left, right| {
        left.path.cmp(&right.path).then_with(|| {
            left.subtune
                .map(|subtune| subtune.0)
                .cmp(&right.subtune.map(|subtune| subtune.0))
        })
    });
    let report = CensusReport {
        schema_version: 3,
        subject: cli.subject.clone(),
        corpus_label: corpus_label(&cli),
        driver_filter: cli.driver_filter.clone(),
        configuration: CensusConfiguration {
            fallback_calls: cli.frames,
            native_call_limit: cli.native_frames,
            full_length: cli.full_length,
            songlengths: songlengths_source,
            subtunes: cli.subtunes,
            timeout_seconds: cli.timeout_seconds,
            top_examples: cli.top_examples,
        },
        summary: summarize(&results, cli.top_examples),
        results,
    };
    write_report(&report, &cli)?;
    eprintln!(
        "{} census: {} PSID files, {} analyzed subtunes, {}/{} native subtunes accepted in {:.1}s",
        report.subject,
        report.summary.psid_files,
        report.summary.analyzed_subtunes,
        report.summary.native.accepted_subtunes,
        report.summary.native.attempted_subtunes,
        started.elapsed().as_secs_f64(),
    );
    Ok(())
}

pub(crate) fn main() {
    if let Err(error) = run() {
        eprintln!("sid-composer-census: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sid_analyzer::analysis::voice::Waveform;

    fn window(path: &str, start: u32, end: u32) -> EvidenceWindow {
        EvidenceWindow {
            path: path.to_owned(),
            title: path.to_owned(),
            subtune: SubtuneIndex(1),
            voice: Some(VoiceId::V1),
            start_frame: FrameIndex(start),
            end_frame: FrameIndex(end),
            start_second: SourceSecond(start / 50),
            duration_seconds: WindowDurationSeconds(1),
        }
    }

    #[test]
    fn trait_aggregation_counts_unique_files_and_keeps_longest_examples() {
        let mut first = TraceTraits::default();
        record_trait(
            &mut first.effects,
            "ring_mod".to_owned(),
            window("a.sid", 0, 9),
            5,
        );
        record_trait(
            &mut first.effects,
            "ring_mod".to_owned(),
            window("a.sid", 20, 24),
            5,
        );
        let mut second = TraceTraits::default();
        record_trait(
            &mut second.effects,
            "ring_mod".to_owned(),
            window("b.sid", 0, 19),
            5,
        );
        let mut accumulators = TraitAccumulators::new();
        accumulators.add("a.sid", &first);
        accumulators.add("b.sid", &second);
        let summary = accumulators.finish(2);
        let ring = &summary.effects["ring_mod"];
        assert_eq!(ring.files, 2);
        assert_eq!(ring.subtunes, 2);
        assert_eq!(ring.occurrences, 3);
        assert_eq!(ring.frames, 35);
        assert_eq!(ring.examples.len(), 2);
        assert_eq!(ring.examples[0].path, "b.sid");
        assert_eq!(ring.examples[1].start_frame, FrameIndex(0));
    }

    #[test]
    fn representation_priorities_are_ranked_by_affected_files_then_frames() {
        let mut traits = TraitSummary::default();
        traits.effects.insert(
            "hard_sync".to_owned(),
            TraitAggregate {
                files: 3,
                subtunes: 3,
                occurrences: 4,
                frames: 100,
                examples: vec![window("sync.sid", 0, 99)],
            },
        );
        traits.hardware.insert(
            "combined_waveform".to_owned(),
            TraitAggregate {
                files: 5,
                subtunes: 5,
                occurrences: 6,
                frames: 50,
                examples: vec![window("combo.sid", 0, 49)],
            },
        );
        let priorities = representation_priorities(&traits);
        assert_eq!(priorities.len(), 2);
        assert_eq!(priorities[0].target, "combined_waveform_fidelity");
        assert_eq!(priorities[0].rank, 1);
        assert_eq!(priorities[1].target, "shared_hard_sync");
    }

    #[test]
    fn waveform_keys_cover_every_sid_waveform_mask() {
        let mut keys = BTreeSet::new();
        for mask in 0..=15u8 {
            keys.insert(waveform_key(mask << 4));
        }
        assert_eq!(keys.len(), 16);
        assert_eq!(
            waveform_key(Waveform::default().to_control_byte()),
            "silent"
        );
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn full_length_window_uses_resolved_song_duration() {
        let bytes = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../assets/music/Auf_Wiedersehen_Monty.sid"),
        )
        .unwrap();
        let header = header::parse(&bytes).unwrap();
        let cli = Cli::try_parse_from([
            "sid-composer-census",
            "--corpus",
            ".",
            "--subject",
            "Test Composer",
            "--full-length",
        ])
        .unwrap();
        let duration = Duration::from_secs(2);
        let window =
            analysis_window(&header, &bytes, header.start_song, &cli, Some(duration)).unwrap();
        assert_eq!(window.source, AnalysisWindowSource::SongLength);
        assert_eq!(
            window.requested_calls,
            PlaybackTiming::for_subtune(&header, header.start_song).calls_for_duration(duration)
        );
        assert_eq!(window.native_calls, window.requested_calls);
        assert_eq!(
            window.song_duration_milliseconds,
            Some(SongDurationMilliseconds(2_000))
        );
    }
}
