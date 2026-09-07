use clap::{Parser, Subcommand, ValueEnum};
use md5::{Digest, Md5};
use serde::Deserialize;
use sid_analyzer::audio::abtest::{
    AbTestError, AudioBudget, compare_wav_files_windowed, run_fixture_matrix,
};
use sid_analyzer::audio::{AudioWindow, FeatureConfig, SampleIndex, SampleRate};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const PERTYLIZER_PROTOCOL_VERSION: PertylizerProtocolVersion = PertylizerProtocolVersion(1);
const PERTYLIZER_BIT_DEPTH: &str = "32f";
const PERTYLIZER_TAIL_SECONDS: RenderTailSeconds = RenderTailSeconds(0);
const MIN_RENDER_SAMPLE_RATE: u32 = 8_000;
const MAX_RENDER_SAMPLE_RATE: u32 = 384_000;
const MAX_RENDER_SECONDS: u32 = 300;
const RENDER_GUARD_SECONDS: u32 = 1;
const MAX_RENDER_BYTES: u64 = 512 * 1024 * 1024;
const RENDER_CHANNELS: u32 = 2;
const RENDER_BYTES_PER_SAMPLE: u32 = 4;

#[derive(Debug, Parser)]
#[command(
    name = "sid-abtest",
    about = "Compare reference and candidate audio renders online or from offline fixtures"
)]
struct Cli {
    #[command(subcommand)]
    command: AbTestCommand,
}

#[derive(Debug, Subcommand)]
enum AbTestCommand {
    Wav {
        reference: PathBuf,
        candidate: PathBuf,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Run a versioned offline fixture matrix containing WAVs and/or pinned profiles.
    Matrix {
        manifest: PathBuf,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    Render {
        sid: PathBuf,
        project: PathBuf,
        #[arg(long, default_value_t = SongNumber(1))]
        song: SongNumber,
        /// Length of the source-aligned comparison window.
        #[arg(long, default_value_t = RenderSeconds(10))]
        seconds: RenderSeconds,
        /// Offset from the start of the song to the comparison window.
        #[arg(long, default_value_t = RenderStartSeconds(0))]
        start_seconds: RenderStartSeconds,
        #[arg(long, default_value_t = RenderSampleRate(44_100))]
        sample_rate: RenderSampleRate,
        #[arg(long)]
        voice: Option<VoiceNumber>,
        #[arg(long, value_enum, default_value_t = SidModelOption::Mos6581)]
        sid_model: SidModelOption,
        #[arg(long, default_value = "sidplayfp")]
        sidplayfp: PathBuf,
        #[arg(long)]
        pertylizer_renderer: PathBuf,
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy)]
struct SongNumber(u16);

impl std::fmt::Display for SongNumber {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(output)
    }
}

impl std::str::FromStr for SongNumber {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let number = value
            .parse::<u16>()
            .map_err(|_| "song must be a positive integer".to_owned())?;
        (number > 0)
            .then_some(Self(number))
            .ok_or_else(|| "song must be at least 1".to_owned())
    }
}

#[derive(Debug, Clone, Copy)]
struct RenderSeconds(u32);

#[derive(Debug, Clone, Copy)]
struct RenderStartSeconds(u32);

impl std::fmt::Display for RenderSeconds {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(output)
    }
}

impl std::fmt::Display for RenderStartSeconds {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(output)
    }
}

#[derive(Debug, Clone, Copy)]
struct RenderSampleRate(u32);

impl std::fmt::Display for RenderSampleRate {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(output)
    }
}

impl std::str::FromStr for RenderSampleRate {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let rate = value
            .parse::<u32>()
            .map_err(|_| "sample rate must be a positive integer".to_owned())?;
        (MIN_RENDER_SAMPLE_RATE..=MAX_RENDER_SAMPLE_RATE)
            .contains(&rate)
            .then_some(Self(rate))
            .ok_or_else(|| {
                format!(
                    "sample rate must be between {MIN_RENDER_SAMPLE_RATE} and {MAX_RENDER_SAMPLE_RATE} Hz"
                )
            })
    }
}

impl std::str::FromStr for RenderSeconds {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let seconds = value
            .parse::<u32>()
            .map_err(|_| "seconds must be a positive integer".to_owned())?;
        (1..=MAX_RENDER_SECONDS)
            .contains(&seconds)
            .then_some(Self(seconds))
            .ok_or_else(|| format!("seconds must be between 1 and {MAX_RENDER_SECONDS}"))
    }
}

impl std::str::FromStr for RenderStartSeconds {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let seconds = value
            .parse::<u32>()
            .map_err(|_| "start seconds must be a non-negative integer".to_owned())?;
        (seconds < MAX_RENDER_SECONDS)
            .then_some(Self(seconds))
            .ok_or_else(|| format!("start seconds must be below {MAX_RENDER_SECONDS}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
struct PertylizerProtocolVersion(u32);

impl std::fmt::Display for PertylizerProtocolVersion {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(output)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderTailSeconds(u32);

impl std::fmt::Display for RenderTailSeconds {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(output)
    }
}

#[derive(Debug, Clone, Copy)]
struct VoiceNumber(u8);

impl std::fmt::Display for VoiceNumber {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(output)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(transparent)]
struct PertylizerTrackId(u16);

impl std::fmt::Display for PertylizerTrackId {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(output)
    }
}

#[derive(Debug, Deserialize)]
struct PertylizerProject {
    song: PertylizerSong,
}

#[derive(Debug, Deserialize)]
struct PertylizerSong {
    tracks: Vec<PertylizerTrack>,
}

#[derive(Debug, Deserialize)]
struct PertylizerTrack {
    id: PertylizerTrackId,
    name: String,
}

#[derive(Debug, Deserialize)]
struct PertylizerRenderReceipt {
    protocol_version: PertylizerProtocolVersion,
    input: ReceiptFile,
    output: ReceiptFile,
    audio: ReceiptAudio,
    mix: ReceiptMix,
    warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ReceiptFile {
    path: String,
    bytes: u64,
}

#[derive(Debug, Deserialize)]
struct ReceiptAudio {
    sample_rate: u32,
    channels: u16,
    bit_depth: u16,
    sample_format: ReceiptSampleFormat,
    frames: u64,
    requested_seconds: f32,
    tail_seconds: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ReceiptSampleFormat {
    Int,
    Float,
}

#[derive(Debug, Deserialize)]
struct ReceiptMix {
    soloed: Vec<PertylizerTrackId>,
    muted: Vec<PertylizerTrackId>,
    audible: Vec<ReceiptTrack>,
}

#[derive(Debug, Deserialize)]
struct ReceiptTrack {
    id: PertylizerTrackId,
}

impl std::str::FromStr for VoiceNumber {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let voice = value
            .parse::<u8>()
            .map_err(|_| "voice must be 1, 2, or 3".to_owned())?;
        (1..=3)
            .contains(&voice)
            .then_some(Self(voice))
            .ok_or_else(|| "voice must be 1, 2, or 3".to_owned())
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SidModelOption {
    Mos6581,
    Mos8580,
}

impl SidModelOption {
    fn sidplayfp_argument(self) -> &'static str {
        match self {
            Self::Mos6581 => "-mof",
            Self::Mos8580 => "-mnf",
        }
    }

    fn protocol_name(self) -> &'static str {
        match self {
            Self::Mos6581 => "6581",
            Self::Mos8580 => "8580",
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<bool, AppError> {
    let (reference, candidate, cache, out, window) = match cli.command {
        AbTestCommand::Wav {
            reference,
            candidate,
            cache,
            out,
        } => (reference, candidate, cache, out, None),
        AbTestCommand::Render {
            sid,
            project,
            song,
            seconds,
            start_seconds,
            sample_rate,
            voice,
            sid_model,
            sidplayfp,
            pertylizer_renderer,
            cache,
            out,
        } => {
            let render_seconds = validate_render_size(start_seconds, seconds, sample_rate)?;
            fs::create_dir_all(&cache).map_err(|source| AppError::Io {
                operation: "create render cache",
                path: cache.clone(),
                source,
            })?;
            let sidplayfp = resolve_renderer(&sidplayfp)?;
            let pertylizer_renderer = resolve_renderer(&pertylizer_renderer)?;
            let sidplayfp_identity = renderer_identity("sidplayfp", &sidplayfp)?;
            let pertylizer_identity = renderer_identity(
                &format!(
                    "pertylizer-protocol-{PERTYLIZER_PROTOCOL_VERSION}-{PERTYLIZER_BIT_DEPTH}-tail{PERTYLIZER_TAIL_SECONDS}"
                ),
                &pertylizer_renderer,
            )?;
            let solo_tracks = pertylizer_voice_tracks(&project, voice)?;
            let reference = render_cache_path(
                &cache,
                &sid,
                &sidplayfp_identity,
                song,
                render_seconds,
                SampleRate(sample_rate.0),
                voice,
                sid_model,
            )?;
            let candidate = render_cache_path(
                &cache,
                &project,
                &pertylizer_identity,
                song,
                render_seconds,
                SampleRate(sample_rate.0),
                voice,
                sid_model,
            )?;
            let candidate_receipt = render_receipt_path(&candidate);
            if !reference.is_file() {
                render_sidplayfp(
                    &sid,
                    &reference,
                    sidplayfp,
                    song,
                    render_seconds,
                    SampleRate(sample_rate.0),
                    voice,
                    sid_model,
                )?;
            }
            if !candidate.is_file() || !candidate_receipt.is_file() {
                render_pertylizer(
                    &project,
                    &candidate,
                    &candidate_receipt,
                    pertylizer_renderer,
                    render_seconds,
                    SampleRate(sample_rate.0),
                    &solo_tracks,
                )?;
            }
            validate_pertylizer_receipt(
                &candidate_receipt,
                &project,
                &candidate,
                render_seconds,
                SampleRate(sample_rate.0),
                &solo_tracks,
            )?;
            let sample_rate =
                usize::try_from(sample_rate.0).map_err(|_| AppError::SampleWindowOverflow {
                    start: start_seconds,
                    length: seconds,
                    sample_rate,
                })?;
            let start = usize::try_from(start_seconds.0)
                .ok()
                .and_then(|seconds| seconds.checked_mul(sample_rate));
            let length = usize::try_from(seconds.0)
                .ok()
                .and_then(|seconds| seconds.checked_mul(sample_rate));
            let (Some(start), Some(length)) = (start, length) else {
                return Err(AppError::SampleWindowOverflow {
                    start: start_seconds,
                    length: seconds,
                    sample_rate: RenderSampleRate(sample_rate as u32),
                });
            };
            (
                reference,
                candidate,
                Some(cache),
                out,
                Some(AudioWindow {
                    start: SampleIndex(start),
                    length: SampleIndex(length),
                }),
            )
        }
        AbTestCommand::Matrix {
            manifest,
            cache,
            out,
        } => {
            let report = run_fixture_matrix(&manifest, cache.as_deref())?;
            write_report(&report, out.as_deref())?;
            return Ok(report.expectations_met);
        }
    };
    let report = compare_wav_files_windowed(
        &reference,
        &candidate,
        FeatureConfig::default(),
        AudioBudget::default(),
        cache.as_deref(),
        window,
    )?;
    write_report(&report, out.as_deref())?;
    Ok(report.accepted)
}

fn validate_render_size(
    start: RenderStartSeconds,
    seconds: RenderSeconds,
    sample_rate: RenderSampleRate,
) -> Result<RenderSeconds, AppError> {
    let Some(window_end) = start.0.checked_add(seconds.0) else {
        return Err(AppError::RenderSpanTooLong {
            start,
            length: seconds,
        });
    };
    let Some(total_seconds) = window_end.checked_add(RENDER_GUARD_SECONDS) else {
        return Err(AppError::RenderSpanTooLong {
            start,
            length: seconds,
        });
    };
    if total_seconds > MAX_RENDER_SECONDS {
        return Err(AppError::RenderSpanTooLong {
            start,
            length: seconds,
        });
    }
    let bytes = u64::from(total_seconds)
        * u64::from(sample_rate.0)
        * u64::from(RENDER_CHANNELS)
        * u64::from(RENDER_BYTES_PER_SAMPLE);
    if bytes > MAX_RENDER_BYTES {
        return Err(AppError::RenderTooLarge {
            seconds: RenderSeconds(total_seconds),
            sample_rate,
            bytes,
        });
    }
    Ok(RenderSeconds(total_seconds))
}

fn write_report(report: &impl serde::Serialize, output: Option<&Path>) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(report)?;
    if let Some(path) = output {
        fs::write(path, bytes).map_err(|source| AppError::Io {
            operation: "write report",
            path: path.to_path_buf(),
            source,
        })?;
    } else {
        println!("{}", String::from_utf8_lossy(&bytes));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_cache_path(
    cache: &Path,
    input: &Path,
    renderer: &RendererCacheIdentity,
    song: SongNumber,
    seconds: RenderSeconds,
    sample_rate: SampleRate,
    voice: Option<VoiceNumber>,
    model: SidModelOption,
) -> Result<PathBuf, AppError> {
    let bytes = fs::read(input).map_err(|source| AppError::Io {
        operation: "read render input",
        path: input.to_path_buf(),
        source,
    })?;
    let canonical_input = fs::canonicalize(input).map_err(|source| AppError::Io {
        operation: "resolve render input",
        path: input.to_path_buf(),
        source,
    })?;
    let mut hasher = Md5::new();
    hasher.update(canonical_input.to_string_lossy().as_bytes());
    hasher.update(&bytes);
    hasher.update(renderer.0.as_bytes());
    hasher.update(song.0.to_le_bytes());
    hasher.update(seconds.0.to_le_bytes());
    hasher.update(sample_rate.0.to_le_bytes());
    hasher.update([voice.map_or(0, |voice| voice.0)]);
    hasher.update(model.protocol_name().as_bytes());
    let digest: [u8; 16] = hasher.finalize().into();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(cache.join(format!("{hex}.wav")))
}

#[derive(Debug, Clone)]
struct RendererCacheIdentity(String);

fn resolve_renderer(executable: &Path) -> Result<PathBuf, AppError> {
    if executable.components().count() > 1 {
        return executable
            .is_file()
            .then(|| executable.to_path_buf())
            .ok_or_else(|| AppError::RendererNotFound(executable.to_path_buf()));
    }
    let Some(search_path) = std::env::var_os("PATH") else {
        return Err(AppError::RendererNotFound(executable.to_path_buf()));
    };
    std::env::split_paths(&search_path)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| AppError::RendererNotFound(executable.to_path_buf()))
}

fn renderer_identity(name: &str, executable: &Path) -> Result<RendererCacheIdentity, AppError> {
    let bytes = fs::read(executable).map_err(|source| AppError::Io {
        operation: "read renderer executable",
        path: executable.to_path_buf(),
        source,
    })?;
    let digest: [u8; 16] = Md5::digest(bytes).into();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(RendererCacheIdentity(format!("{name}-{hex}")))
}

fn render_receipt_path(output: &Path) -> PathBuf {
    output.with_extension("render.json")
}

fn pertylizer_voice_tracks(
    project: &Path,
    voice: Option<VoiceNumber>,
) -> Result<Vec<PertylizerTrackId>, AppError> {
    let Some(voice) = voice else {
        return Ok(Vec::new());
    };
    let bytes = fs::read(project).map_err(|source| AppError::Io {
        operation: "read Pertylizer project",
        path: project.to_path_buf(),
        source,
    })?;
    let parsed: PertylizerProject =
        serde_json::from_slice(&bytes).map_err(|source| AppError::ProjectJson {
            path: project.to_path_buf(),
            source,
        })?;
    select_voice_tracks(parsed.song.tracks, project, voice)
}

fn select_voice_tracks(
    tracks: Vec<PertylizerTrack>,
    project: &Path,
    voice: VoiceNumber,
) -> Result<Vec<PertylizerTrackId>, AppError> {
    let prefix = format!("V{}", voice.0);
    let mut ids = BTreeSet::new();
    for track in tracks {
        let matches_voice = track
            .name
            .strip_prefix(&prefix)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace));
        if matches_voice && !ids.insert(track.id) {
            return Err(AppError::DuplicateVoiceTrack {
                project: project.to_path_buf(),
                voice,
                track: track.id,
            });
        }
    }
    if ids.is_empty() {
        return Err(AppError::MissingVoiceTrack {
            project: project.to_path_buf(),
            voice,
        });
    }
    Ok(ids.into_iter().collect())
}

fn validate_pertylizer_receipt(
    receipt_path: &Path,
    input: &Path,
    output: &Path,
    seconds: RenderSeconds,
    sample_rate: SampleRate,
    solo_tracks: &[PertylizerTrackId],
) -> Result<(), AppError> {
    let bytes = fs::read(receipt_path).map_err(|source| AppError::Io {
        operation: "read Pertylizer render receipt",
        path: receipt_path.to_path_buf(),
        source,
    })?;
    let receipt: PertylizerRenderReceipt =
        serde_json::from_slice(&bytes).map_err(|source| AppError::ReceiptJson {
            path: receipt_path.to_path_buf(),
            source,
        })?;
    let invalid = |detail: String| AppError::InvalidReceipt {
        path: receipt_path.to_path_buf(),
        detail,
    };
    if receipt.protocol_version != PERTYLIZER_PROTOCOL_VERSION {
        return Err(invalid(format!(
            "protocol version is {}, expected {}",
            receipt.protocol_version, PERTYLIZER_PROTOCOL_VERSION
        )));
    }
    validate_receipt_file(receipt_path, "input", &receipt.input, input)?;
    validate_receipt_file(receipt_path, "output", &receipt.output, output)?;
    if receipt.audio.sample_rate != sample_rate.0 {
        return Err(invalid(format!(
            "sample rate is {}, expected {}",
            receipt.audio.sample_rate, sample_rate.0
        )));
    }
    if receipt.audio.channels == 0 || receipt.audio.frames == 0 {
        return Err(invalid(format!(
            "audio has {} channels and {} frames",
            receipt.audio.channels, receipt.audio.frames
        )));
    }
    if receipt.audio.bit_depth != 32 || receipt.audio.sample_format != ReceiptSampleFormat::Float {
        return Err(invalid(format!(
            "sample format is {:?} {}, expected float 32",
            receipt.audio.sample_format, receipt.audio.bit_depth
        )));
    }
    if receipt.audio.requested_seconds != seconds.0 as f32 {
        return Err(invalid(format!(
            "requested duration is {}, expected {}",
            receipt.audio.requested_seconds, seconds.0
        )));
    }
    if receipt.audio.tail_seconds != PERTYLIZER_TAIL_SECONDS.0 as f32 {
        return Err(invalid(format!(
            "tail duration is {}, expected {}",
            receipt.audio.tail_seconds, PERTYLIZER_TAIL_SECONDS
        )));
    }
    if !receipt.mix.muted.is_empty() {
        return Err(invalid(format!(
            "renderer unexpectedly muted tracks {:?}",
            receipt.mix.muted
        )));
    }
    let expected: BTreeSet<_> = solo_tracks.iter().copied().collect();
    let actual: BTreeSet<_> = receipt.mix.soloed.iter().copied().collect();
    if actual.len() != receipt.mix.soloed.len() || actual != expected {
        return Err(invalid(format!(
            "soloed tracks are {:?}, expected {:?}",
            receipt.mix.soloed, solo_tracks
        )));
    }
    if !expected.is_empty() {
        let audible: BTreeSet<_> = receipt.mix.audible.iter().map(|track| track.id).collect();
        if audible != expected {
            return Err(invalid(format!(
                "audible tracks are {audible:?}, expected {expected:?}"
            )));
        }
    }
    for warning in receipt.warnings {
        eprintln!("Pertylizer render warning: {warning}");
    }
    Ok(())
}

fn validate_receipt_file(
    receipt_path: &Path,
    label: &'static str,
    file: &ReceiptFile,
    expected: &Path,
) -> Result<(), AppError> {
    let invalid = |detail: String| AppError::InvalidReceipt {
        path: receipt_path.to_path_buf(),
        detail,
    };
    let actual_path = fs::canonicalize(Path::new(&file.path)).map_err(|source| {
        invalid(format!(
            "{label} path {:?} cannot be resolved: {source}",
            file.path
        ))
    })?;
    let expected_path = fs::canonicalize(expected).map_err(|source| AppError::Io {
        operation: "resolve rendered artifact",
        path: expected.to_path_buf(),
        source,
    })?;
    if actual_path != expected_path {
        return Err(invalid(format!(
            "{label} path is {actual_path:?}, expected {expected_path:?}"
        )));
    }
    let actual_bytes = fs::metadata(expected)
        .map_err(|source| AppError::Io {
            operation: "inspect rendered artifact",
            path: expected.to_path_buf(),
            source,
        })?
        .len();
    if file.bytes != actual_bytes {
        return Err(invalid(format!(
            "{label} size is {}, expected {actual_bytes}",
            file.bytes
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_sidplayfp(
    sid: &Path,
    output: &Path,
    executable: PathBuf,
    song: SongNumber,
    seconds: RenderSeconds,
    sample_rate: SampleRate,
    voice: Option<VoiceNumber>,
    model: SidModelOption,
) -> Result<(), AppError> {
    let mut arguments = vec![
        OsString::from("--residfp"),
        OsString::from("-m"),
        OsString::from(format!("-f{}", sample_rate.0)),
        OsString::from(format!("-t{}", seconds.0)),
        OsString::from(format!("-o{}", song.0)),
        OsString::from(model.sidplayfp_argument()),
        OsString::from(format!("-w{}", output.display())),
    ];
    if let Some(voice) = voice {
        for other in 1..=3 {
            if other != voice.0 {
                arguments.push(OsString::from(format!("-u{other}")));
            }
        }
    }
    arguments.push(sid.as_os_str().to_owned());
    execute_renderer(&executable, &arguments, output)
}

fn render_pertylizer(
    project: &Path,
    output: &Path,
    receipt: &Path,
    executable: PathBuf,
    seconds: RenderSeconds,
    sample_rate: SampleRate,
    solo_tracks: &[PertylizerTrackId],
) -> Result<(), AppError> {
    let arguments =
        pertylizer_render_arguments(project, output, receipt, seconds, sample_rate, solo_tracks);
    execute_renderer(&executable, &arguments, output)?;
    if !receipt.is_file() {
        return Err(AppError::MissingRender {
            command: reproducible_command(&executable, &arguments),
            output: receipt.to_path_buf(),
        });
    }
    Ok(())
}

fn pertylizer_render_arguments(
    project: &Path,
    output: &Path,
    receipt: &Path,
    seconds: RenderSeconds,
    sample_rate: SampleRate,
    solo_tracks: &[PertylizerTrackId],
) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("render"),
        OsString::from("--protocol-version"),
        OsString::from(PERTYLIZER_PROTOCOL_VERSION.to_string()),
        OsString::from("--input"),
        project.as_os_str().to_owned(),
        OsString::from("--output"),
        output.as_os_str().to_owned(),
        OsString::from("--sample-rate"),
        OsString::from(sample_rate.0.to_string()),
        OsString::from("--bit-depth"),
        OsString::from(PERTYLIZER_BIT_DEPTH),
        OsString::from("--seconds"),
        OsString::from(seconds.0.to_string()),
        OsString::from("--tail-seconds"),
        OsString::from(PERTYLIZER_TAIL_SECONDS.to_string()),
        OsString::from("--result-json"),
        receipt.as_os_str().to_owned(),
    ];
    for track in solo_tracks {
        arguments.push(OsString::from("--solo-track"));
        arguments.push(OsString::from(track.to_string()));
    }
    arguments
}

fn execute_renderer(
    executable: &Path,
    arguments: &[OsString],
    expected_output: &Path,
) -> Result<(), AppError> {
    let command = reproducible_command(executable, arguments);
    let output = Command::new(executable)
        .args(arguments)
        .output()
        .map_err(|source| AppError::RenderLaunch {
            command: command.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(AppError::RenderFailed {
            command,
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    if !expected_output.is_file() {
        return Err(AppError::MissingRender {
            command,
            output: expected_output.to_path_buf(),
        });
    }
    Ok(())
}

fn reproducible_command(executable: &Path, arguments: &[OsString]) -> String {
    std::iter::once(executable.as_os_str())
        .chain(arguments.iter().map(OsString::as_os_str))
        .map(|argument| format!("{argument:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error(transparent)]
    Compare(#[from] AbTestError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("failed to parse Pertylizer project {path}: {source}")]
    ProjectJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to parse Pertylizer render receipt {path}: {source}")]
    ReceiptJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid Pertylizer render receipt {path}: {detail}")]
    InvalidReceipt { path: PathBuf, detail: String },
    #[error(
        "{seconds} seconds at {sample_rate} Hz needs a {bytes}-byte Pertylizer render buffer, over the {MAX_RENDER_BYTES}-byte maximum"
    )]
    RenderTooLarge {
        seconds: RenderSeconds,
        sample_rate: RenderSampleRate,
        bytes: u64,
    },
    #[error(
        "render window start {start} seconds plus length {length} seconds and the {RENDER_GUARD_SECONDS}-second renderer guard exceed the {MAX_RENDER_SECONDS}-second maximum"
    )]
    RenderSpanTooLong {
        start: RenderStartSeconds,
        length: RenderSeconds,
    },
    #[error(
        "render window start {start} seconds and length {length} seconds overflow at {sample_rate} Hz"
    )]
    SampleWindowOverflow {
        start: RenderStartSeconds,
        length: RenderSeconds,
        sample_rate: RenderSampleRate,
    },
    #[error("failed to {operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("failed to launch renderer command {command}: {source}")]
    RenderLaunch { command: String, source: io::Error },
    #[error("renderer command {command} failed with status {status:?}: {stderr}")]
    RenderFailed {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    #[error("renderer command {command} succeeded but did not create {output:?}")]
    MissingRender { command: String, output: PathBuf },
    #[error("renderer executable was not found: {0:?}")]
    RendererNotFound(PathBuf),
    #[error("Pertylizer project {project:?} has no track for SID voice {voice}")]
    MissingVoiceTrack {
        project: PathBuf,
        voice: VoiceNumber,
    },
    #[error("Pertylizer project {project:?} maps SID voice {voice} to duplicate track id {track}")]
    DuplicateVoiceTrack {
        project: PathBuf,
        voice: VoiceNumber,
        track: PertylizerTrackId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argument_strings(arguments: Vec<OsString>) -> Vec<String> {
        arguments
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn render_launch_error_contains_a_reproducible_command() {
        let executable = Path::new("/definitely/missing/sid-renderer");
        let error = execute_renderer(
            executable,
            &[
                OsString::from("--input"),
                OsString::from("project with space.ptz"),
            ],
            Path::new("missing.wav"),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("/definitely/missing/sid-renderer"));
        assert!(message.contains("project with space.ptz"));
    }

    #[test]
    fn pertylizer_arguments_cover_the_version_1_render_contract() {
        let arguments = argument_strings(pertylizer_render_arguments(
            Path::new("project with space.ptz"),
            Path::new("candidate.wav"),
            Path::new("candidate.render.json"),
            RenderSeconds(10),
            SampleRate(48_000),
            &[PertylizerTrackId(2), PertylizerTrackId(7)],
        ));
        assert_eq!(
            arguments,
            [
                "render",
                "--protocol-version",
                "1",
                "--input",
                "project with space.ptz",
                "--output",
                "candidate.wav",
                "--sample-rate",
                "48000",
                "--bit-depth",
                "32f",
                "--seconds",
                "10",
                "--tail-seconds",
                "0",
                "--result-json",
                "candidate.render.json",
                "--solo-track",
                "2",
                "--solo-track",
                "7",
            ]
        );
    }

    #[test]
    fn voice_selection_uses_every_matching_stable_track_id() {
        let project: PertylizerProject = serde_json::from_str(
            r#"{
                "song": {
                    "tracks": [
                        {"id": 7, "name": "V2 Lead · i8"},
                        {"id": 1, "name": "V1 Bass · i2"},
                        {"id": 2, "name": "V2 drum (drop) · i3"},
                        {"id": 9, "name": "V20 unrelated"}
                    ]
                }
            }"#,
        )
        .unwrap();
        let tracks = select_voice_tracks(
            project.song.tracks,
            Path::new("candidate.ptz"),
            VoiceNumber(2),
        )
        .unwrap();
        assert_eq!(tracks, [PertylizerTrackId(2), PertylizerTrackId(7)]);
    }

    #[test]
    fn full_mix_does_not_require_sid_analyzer_track_names() {
        let tracks = pertylizer_voice_tracks(Path::new("missing.ptz"), None).unwrap();
        assert!(tracks.is_empty());
    }

    #[test]
    fn render_cache_separates_identical_inputs_at_different_paths() {
        let directory = std::env::temp_dir().join(format!(
            "sid-abtest-cache-input-paths-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let first = directory.join("first.ptz");
        let second = directory.join("second.ptz");
        fs::write(&first, b"same project").unwrap();
        fs::write(&second, b"same project").unwrap();
        let renderer = RendererCacheIdentity("renderer".to_owned());
        let cache = directory.join("cache");

        let first_cache = render_cache_path(
            &cache,
            &first,
            &renderer,
            SongNumber(1),
            RenderSeconds(10),
            SampleRate(44_100),
            None,
            SidModelOption::Mos6581,
        )
        .unwrap();
        let second_cache = render_cache_path(
            &cache,
            &second,
            &renderer,
            SongNumber(1),
            RenderSeconds(10),
            SampleRate(44_100),
            None,
            SidModelOption::Mos6581,
        )
        .unwrap();

        assert_ne!(first_cache, second_cache);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn render_limits_match_pertylizer_before_starting_the_reference() {
        assert!("300".parse::<RenderSeconds>().is_ok());
        assert!("0.25".parse::<RenderSeconds>().is_err());
        assert!("301".parse::<RenderSeconds>().is_err());
        assert!("8000".parse::<RenderSampleRate>().is_ok());
        assert!("384000".parse::<RenderSampleRate>().is_ok());
        assert!("7999".parse::<RenderSampleRate>().is_err());
        assert!("384001".parse::<RenderSampleRate>().is_err());
        assert!(
            validate_render_size(
                RenderStartSeconds(0),
                RenderSeconds(300),
                RenderSampleRate(384_000)
            )
            .is_err()
        );
        assert!(
            validate_render_size(
                RenderStartSeconds(291),
                RenderSeconds(10),
                RenderSampleRate(44_100)
            )
            .is_err()
        );
        assert_eq!(
            validate_render_size(
                RenderStartSeconds(25),
                RenderSeconds(10),
                RenderSampleRate(44_100)
            )
            .unwrap()
            .0,
            36
        );
    }
}
