use clap::{Parser, ValueEnum};
use sid_analyzer::analysis::effects::{EffectSpan, EffectThresholds, detect_effects};
use sid_analyzer::analysis::inputs::AnalysisInputs;
use sid_analyzer::analysis::note::detect_notes;
use sid_analyzer::analysis::sid_program::AnalyzedSidProgram;
use sid_analyzer::analysis::{SystemClock, analyze};
use sid_analyzer::audio::SampleRate;
use sid_analyzer::emu;
use sid_analyzer::export::{json, midi, native, synth, text};
use sid_analyzer::header::{self, Header, SubtuneIndex};
use sid_analyzer::playerid::PlayerDb;
use sid_analyzer::songlengths::{self, SongLengths};
use sid_analyzer::stil::{StilDatabase, StilEntry, StilFieldKind};
use sid_analyzer::support::UnsupportedInputKind;
use sid_analyzer::trace::Trace;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

const EXIT_PARSE: u8 = 1;
const EXIT_READ: u8 = 2;
const EXIT_EMU: u8 = 3;
const EXIT_EXPORT: u8 = 4;
const EXIT_UNSUPPORTED: u8 = 5;

const ABOUT: &str = "Analyze Commodore 64 SID music files";

const LONG_ABOUT: &str = "\
sid-analyzer parses a PSID/RSID file, runs its 6502 init and play routines
through a built-in emulator, captures reads and writes in the SID register
window ($D400-$D41C) per play call, and reconstructs musical information:
voice frequencies as MIDI notes, ADSR envelopes, waveforms, filter
parameters, and detected effects (HardSync, RingMod, PWM, FilterSweep,
FilterResonanceSweep, FilterModeModulation, Sample, Portamento, Vibrato,
Tremolo, Arpeggio, Voice3Modulator).

Modes:
  No flags             Print SID header metadata only.
  --frames N           Emulate N play frames after init; print summary.
  --trace              Add raw per-frame register-write dump.
  --effects            Add detected effect spans.
  --format text        siddump-style tracker table.
  --format json        JSON analysis summary (use --pretty for indentation).
  --format capture-json
                       Canonical lossless ordered SID-bus capture JSON.
  --format program-json
                       Target-neutral analyzed SID program debug JSON.
  --format midi        Format-1 MIDI with pitch bend and envelope expression
                       (requires --output PATH).
  --format digi-wav    Reconstructed cycle-stamped $D418 PCM streams
                       (requires --output PATH).
  --format synth       Pertylizer .ptz project file (derived from the
                       emulated register stream; requires --output PATH).
  --format synth-native
                       Pertylizer .ptz built from the driver's own song
                       tables (identified by code signature). This strict mode
                       fails for unsupported drivers or variants; choose
                       --format synth explicitly for trace-derived output.
  --format synth-modern
                       Modern analog Pertylizer interpretation built from the
                       driver's own song tables.
  --enhance 1-10       Add role-aware production effects to synth or
                       synth-native without changing the music data.

When --format is set, only that format is written and all other stdout output
is suppressed. MIDI, digi WAV, and all synth formats require --output; text and
JSON formats use stdout when --output is omitted.

Examples:
  sid-analyzer foo.sid
  sid-analyzer foo.sid --frames 3000 --trace
  sid-analyzer foo.sid --frames 3000 --format text
  sid-analyzer foo.sid --frames 3000 --format json --pretty --output foo.json
  sid-analyzer foo.sid --frames 3000 --format midi --output foo.mid
  sid-analyzer foo.sid --frames 3000 --format synth --output foo.ptz
  sid-analyzer foo.sid --frames 3000 --format synth-native --output foo.ptz
  sid-analyzer foo.sid --frames 3000 --format synth-native --enhance 5 --output foo.ptz
  sid-analyzer foo.sid --frames 3000 --format synth-modern --output foo.ptz
  sid-analyzer foo.sid --frames 3000 --clock ntsc --format json";

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum Format {
    /// siddump-style per-frame tracker table.
    Text,
    /// Self-describing JSON summary (header, notes, effects).
    Json,
    /// Target-neutral analyzed SID program debug JSON.
    ProgramJson,
    /// Canonical lossless ordered SID-bus capture JSON.
    CaptureJson,
    /// Format-1 MIDI file: one track per voice, 1 tick = 1 play frame.
    Midi,
    /// Reconstructed cycle-stamped `$D418` PCM streams as content-addressed WAV files.
    DigiWav,
    /// Pertylizer `.ptz` project (one instrument per output track).
    Synth,
    /// Pertylizer `.ptz` built from the driver's own song tables,
    /// selected by playroutine code-signature identification. Strictly errors
    /// without creating output when timing, location, decode, or validation is
    /// unsupported; never switches to `synth` implicitly.
    SynthNative,
    /// Modern analog interpretation built from the driver's own song tables.
    /// Preserves notes, arrangement, expression, and editable automation while
    /// replacing SID voices with role-aware layered Pertylizer instruments.
    SynthModern,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, ValueEnum)]
enum ClockOverride {
    /// Use the SID header's clock flag (PAL fallback when unknown).
    #[default]
    Auto,
    Pal,
    Ntsc,
}

#[derive(Clone, Copy, Debug)]
struct DigiOutputRate(SampleRate);

impl std::fmt::Display for DigiOutputRate {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.0.fmt(output)
    }
}

impl std::str::FromStr for DigiOutputRate {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let rate = value
            .parse::<u32>()
            .map_err(|_| "digi sample rate must be a positive integer".to_owned())?;
        (rate > 0)
            .then_some(Self(SampleRate(rate)))
            .ok_or_else(|| "digi sample rate must be at least 1 Hz".to_owned())
    }
}

impl ClockOverride {
    fn resolve(self, header_clock: header::Clock) -> SystemClock {
        match self {
            Self::Auto => SystemClock::from(header_clock),
            Self::Pal => SystemClock::Pal,
            Self::Ntsc => SystemClock::Ntsc,
        }
    }
}

#[derive(Parser)]
#[command(version, about = ABOUT, long_about = LONG_ABOUT)]
struct Cli {
    /// Path to a PSID or RSID file.
    file: PathBuf,

    /// Subtune to run (1-based). Defaults to the header's start_song.
    #[arg(long, value_name = "N")]
    subtune: Option<u16>,

    /// Number of play calls to emulate after init. With `--format`, zero uses
    /// the selected subtune's supplied Songlengths duration when available.
    /// Without `--format`, zero prints the header without emulation.
    #[arg(long, value_name = "N", default_value_t = 0)]
    frames: u32,

    /// Override the host clock domain (otherwise derived from the SID
    /// header, with PAL as the fallback when the header is ambiguous).
    #[arg(long, value_enum, default_value_t = ClockOverride::Auto)]
    clock: ClockOverride,

    /// Print every captured SID register write per frame to stdout.
    /// Useful for low-level debugging; ignored when `--format` is set.
    #[arg(long)]
    trace: bool,

    /// Print detected effect spans to stdout. Ignored when `--format` is set.
    #[arg(long)]
    effects: bool,

    /// Structured export format. Suppresses all other stdout output. Text and
    /// JSON formats use stdout when `--output` is omitted; MIDI, digi WAV, and
    /// all synth formats require `--output`.
    #[arg(long, value_enum, value_name = "FORMAT")]
    format: Option<Format>,

    /// Add role-aware warmth, weight, space, and dynamics while preserving the
    /// exported notes, timing, structure, expression, and SID oscillators.
    /// Supported by `synth` and `synth-native`; 5 is balanced and 6-10 are
    /// progressively heavier.
    #[arg(long, value_name = "1-10")]
    enhance: Option<synth::EnhancementAmount>,

    /// Destination path for `--format`. Defaults to stdout for text and JSON
    /// formats; required for `midi`, `digi-wav`, `synth`, `synth-native`, and
    /// `synth-modern`.
    #[arg(
        long,
        value_name = "PATH",
        required_if_eq_any([
            ("format", "midi"),
            ("format", "digi-wav"),
            ("format", "synth"),
            ("format", "synth-native"),
            ("format", "synth-modern"),
        ])
    )]
    output: Option<PathBuf>,

    /// Output sample rate for `--format digi-wav`.
    #[arg(long, default_value_t = DigiOutputRate(SampleRate(44_100)))]
    digi_sample_rate: DigiOutputRate,

    /// Pretty-print JSON output with indentation.
    #[arg(long)]
    pretty: bool,

    /// Disable forward-model correction for controlled A/B diagnostics.
    #[arg(long, hide = true)]
    unstable_no_forward_gate: bool,

    /// Bake arpeggios into notes instead of using the native processor.
    #[arg(long, hide = true)]
    unstable_no_arp_processor: bool,

    /// Force emulation/analysis/export of an RSID file. RSID is out of
    /// scope — the emulator has no Kernal/BASIC/IRQ environment — so the
    /// output is unreliable. Without this flag, RSID input is refused for
    /// any path that emulates (header-only inspection still works).
    #[arg(long)]
    allow_rsid: bool,

    /// Optional local HVSC `Songlengths.md5` database. No database is bundled
    /// or loaded implicitly. Can also be set via `HVSC_SONGLENGTHS`.
    #[arg(long, value_name = "PATH", env = "HVSC_SONGLENGTHS")]
    songlengths: Option<PathBuf>,

    /// Path to an HVSC STIL.txt database. Matching metadata is printed in the
    /// header view and included in analysis JSON. Can also be set via
    /// `HVSC_STIL`.
    #[arg(long, value_name = "PATH", env = "HVSC_STIL")]
    stil: Option<PathBuf>,
}

impl Cli {
    fn synth_options(&self, style: synth::SynthStyle) -> synth::SynthOptions {
        synth::SynthOptions {
            forward_gate: !self.unstable_no_forward_gate,
            arpeggiator_processor: !self.unstable_no_arp_processor,
            style,
            enhancement: self.enhance,
        }
    }

    fn validate_enhancement_format(&self) -> Result<(), &'static str> {
        let Some(_) = self.enhance else {
            return Ok(());
        };
        match self.format {
            Some(Format::Synth | Format::SynthNative) => Ok(()),
            Some(Format::SynthModern) => {
                Err("--enhance cannot be combined with --format synth-modern")
            }
            Some(_) | None => Err("--enhance requires --format synth or synth-native"),
        }
    }

    fn resolved_subtune(&self, header: &Header) -> SubtuneIndex {
        SubtuneIndex(self.subtune.unwrap_or(header.start_song.0))
    }

    /// Look up subtune durations in the supplied local database.
    fn lookup_lengths(&self, _header: &Header, bytes: &[u8]) -> Option<Vec<std::time::Duration>> {
        let path = self.songlengths.as_ref()?;
        let db = match SongLengths::load(path) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("songlengths {}: {e}", path.display());
                return None;
            }
        };
        let md5 = songlengths::compute_sid_md5(bytes);
        db.lookup(&md5).map(<[_]>::to_vec)
    }

    fn resolved_export_frames(
        &self,
        header: &Header,
        bytes: &[u8],
        timing: emu::PlaybackTiming,
    ) -> Result<Option<u32>, emu::EmuError> {
        if self.frames > 0 {
            return Ok(Some(self.frames));
        }
        let subtune = self.resolved_subtune(header);
        let Some(duration) = subtune.0.checked_sub(1).and_then(|index| {
            self.lookup_lengths(header, bytes)?
                .get(index as usize)
                .copied()
        }) else {
            return Ok(None);
        };
        let resolved = if timing.cia_timed {
            emu::resolve_playback_timing(header, bytes, subtune, timing)?
        } else {
            timing
        };
        Ok(Some(resolved.calls_for_duration(duration)))
    }

    fn lookup_stil(&self) -> Option<StilEntry> {
        let path = self.stil.as_deref()?;
        let database = match StilDatabase::load(path) {
            Ok(database) => database,
            Err(error) => {
                eprintln!("STIL {}: {error}", path.display());
                return None;
            }
        };
        database.lookup(&self.file).cloned()
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(error) = cli.validate_enhancement_format() {
        eprintln!("error: {error}");
        return ExitCode::from(EXIT_EXPORT);
    }
    let bytes = match std::fs::read(&cli.file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {}: {e}", cli.file.display());
            return ExitCode::from(EXIT_READ);
        }
    };
    let header = match header::parse(&bytes) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("parse: {e}");
            return ExitCode::from(EXIT_PARSE);
        }
    };

    match cli.format {
        Some(format) => run_export(&cli, &header, &bytes, format),
        None => run_diagnostic(&cli, &header, &bytes),
    }
}

/// Checks shared by the diagnostic and export paths, run before any
/// emulation. Refuses out-of-scope RSID files (the emulator has no
/// Kernal/BASIC/IRQ environment, so its init/play run is unreliable) unless
/// `--allow-rsid` is set.
fn preflight(header: &Header, allow_rsid: bool) -> Result<(), ExitCode> {
    if UnsupportedInputKind::classify(header) == Some(UnsupportedInputKind::MusStrPayload) {
        eprintln!(
            "error: MUS/STR payloads are classified but not emulated; convert the tune to a \
             standard PSID driver before analysis"
        );
        return Err(ExitCode::from(EXIT_UNSUPPORTED));
    }
    if UnsupportedInputKind::classify(header) == Some(UnsupportedInputKind::RsidSystemEnvironment) {
        if allow_rsid {
            eprintln!(
                "warning: RSID is out of scope — the emulator has no Kernal/BASIC/IRQ \
                 environment, so analysis and export are unreliable."
            );
        } else {
            eprintln!(
                "error: RSID files are out of scope (they need a Kernal/BASIC/IRQ \
                 environment this emulator lacks). Re-run with --allow-rsid to force \
                 unreliable emulation; header-only inspection works without --format."
            );
            return Err(ExitCode::from(EXIT_UNSUPPORTED));
        }
    }
    Ok(())
}

fn format_name(format: Format) -> &'static str {
    match format {
        Format::Text => "text",
        Format::Json => "json",
        Format::ProgramJson => "program-json",
        Format::CaptureJson => "capture-json",
        Format::Midi => "midi",
        Format::DigiWav => "digi-wav",
        Format::Synth => "synth",
        Format::SynthNative => "synth-native",
        Format::SynthModern => "synth-modern",
    }
}

fn warn_multi_sid_boundary(header: &Header, format: Option<Format>) {
    if header.second_sid_address.is_none() {
        return;
    }
    match format {
        Some(Format::Json | Format::ProgramJson | Format::CaptureJson) => {}
        Some(format) => eprintln!(
            "warning: multi-SID capture is active, but {} output represents only the primary \
             SID; use json or program-json for secondary-chip events and analysis",
            format_name(format)
        ),
        None => eprintln!(
            "warning: multi-SID capture is active, but diagnostic voice/effect views represent \
             only the primary SID; use --format json or program-json for every chip"
        ),
    }
}

fn analyze_additional_sids(
    capture: &sid_analyzer::emu::capture::CapturedSidExecution,
    clock: SystemClock,
) -> Vec<json::SidChipAnalysis> {
    capture
        .chips
        .iter()
        .filter(|chip| chip.id != sid_analyzer::emu::capture::SidChipId::PRIMARY)
        .map(|chip| {
            let trace = capture.project_trace_for_chip(chip.id);
            let inputs = AnalysisInputs::build(&trace, clock);
            json::SidChipAnalysis {
                chip: *chip,
                frame_count: inputs.states.len(),
                patches: inputs.patches,
                notes: inputs.notes,
                effects: inputs.effects,
                voice_relations: inputs.voice_relations,
            }
        })
        .collect()
}

fn run_diagnostic(cli: &Cli, header: &Header, bytes: &[u8]) -> ExitCode {
    print_header(header, bytes);
    if let Some(lengths) = cli.lookup_lengths(header, bytes) {
        print_subtune_lengths(&lengths);
    }
    if let Some(stil) = cli.lookup_stil() {
        print_stil(&stil, cli.resolved_subtune(header));
    }
    if cli.frames == 0 {
        return ExitCode::SUCCESS;
    }
    if let Err(code) = preflight(header, cli.allow_rsid) {
        return code;
    }
    warn_multi_sid_boundary(header, None);
    let subtune = cli.resolved_subtune(header);
    let trace = match emu::run(header, bytes, subtune, cli.frames) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("emu: {e}");
            return ExitCode::from(EXIT_EMU);
        }
    };
    print_trace_summary(&trace, subtune);
    if cli.trace {
        print_trace_detail(&trace);
    }
    if cli.effects {
        let states = analyze(&trace);
        let spans = detect_effects(&trace, &states, EffectThresholds::default());
        print_effects(&spans);
    }
    ExitCode::SUCCESS
}

fn run_export(cli: &Cli, header: &Header, bytes: &[u8], format: Format) -> ExitCode {
    if let Err(code) = preflight(header, cli.allow_rsid) {
        return code;
    }
    warn_multi_sid_boundary(header, Some(format));
    let subtune = cli.resolved_subtune(header);
    let clock = cli.clock.resolve(header.flags.clock);
    let timing = emu::PlaybackTiming::for_subtune_with_clock(header, subtune, clock);
    let frames = match cli.resolved_export_frames(header, bytes, timing) {
        Ok(Some(frames)) => frames,
        Ok(None) => {
            eprintln!("export: no duration available; pass --frames or --songlengths");
            return ExitCode::from(EXIT_EXPORT);
        }
        Err(error) => {
            eprintln!("emu: {error}");
            return ExitCode::from(EXIT_EMU);
        }
    };
    if matches!(format, Format::SynthNative | Format::SynthModern) {
        return run_export_synth_native(cli, header, bytes, subtune, timing, frames, format);
    }

    let trace = match emu::run_with_timing(header, bytes, subtune, frames, timing) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("emu: {e}");
            return ExitCode::from(EXIT_EMU);
        }
    };
    let states = analyze(&trace);
    let timing = timing.resolved_from_trace(&trace);

    let result = match format {
        Format::Text => write_export(cli.output.as_deref(), |out| {
            text::write_text(&states, clock, out)
        }),
        Format::Json => {
            let inputs = AnalysisInputs::from_states(&trace, states, clock);
            let additional_sid_chips = analyze_additional_sids(&trace.capture, clock);
            let subtune_lengths_secs = cli
                .lookup_lengths(header, bytes)
                .map(|v| v.iter().map(|d| d.as_secs_f64()).collect());
            let stil = cli.lookup_stil();
            let program = AnalyzedSidProgram::from_analysis_inputs(
                header,
                subtune,
                timing,
                trace.capture.clone(),
                inputs,
            );
            write_export(cli.output.as_deref(), move |out| {
                let enriched_notes = json::EnrichedNote::enrich(
                    &program.semantic.notes,
                    Some(&program.semantic.patch_assignments),
                    Some(&program.semantic.characteristics),
                );
                let export = json::Export {
                    header,
                    subtune,
                    timing,
                    frame_count: program.frame_count(),
                    subtune_lengths_secs,
                    stil: stil.as_ref(),
                    patches: Some(&program.semantic.patches),
                    notes: enriched_notes,
                    effects: &program.semantic.effects,
                    voice_relations: &program.semantic.voice_relations,
                    digi_streams:
                        sid_analyzer::analysis::sid_program::observable::summarize_d418_streams(
                            &program,
                        )
                        .map_err(io::Error::other)?,
                    additional_sid_chips,
                    structure: None,
                    native: None,
                };
                json::write_json(&export, out, cli.pretty).and_then(|()| writeln!(out))
            })
        }
        Format::ProgramJson => {
            let inputs = AnalysisInputs::from_states(&trace, states, clock);
            let program = AnalyzedSidProgram::from_analysis_inputs(
                header,
                subtune,
                timing,
                trace.capture.clone(),
                inputs,
            );
            write_export(cli.output.as_deref(), move |out| {
                program
                    .write_debug_json(out, cli.pretty)
                    .and_then(|()| writeln!(out))
            })
        }
        Format::CaptureJson => write_export(cli.output.as_deref(), |out| {
            trace
                .capture
                .to_json(out, cli.pretty)
                .map_err(io::Error::other)
                .and_then(|()| writeln!(out))
        }),
        Format::Midi => {
            let notes = detect_notes(&states, clock);
            write_export(cli.output.as_deref(), |out| {
                midi::write_midi_with_expression(&notes, &states, timing, states.len(), out)
            })
        }
        Format::DigiWav => (|| {
            let inputs = AnalysisInputs::from_states(&trace, states, clock);
            let mut program = AnalyzedSidProgram::from_analysis_inputs(
                header,
                subtune,
                timing,
                trace.capture.clone(),
                inputs,
            );
            let directory = cli
                .output
                .as_deref()
                .ok_or_else(|| io::Error::other("digi-wav requires --output"))?;
            let artifacts = sid_analyzer::analysis::sid_program::observable::persist_d418_wavs(
                &mut program,
                cli.digi_sample_rate.0,
                directory,
            )
            .map_err(io::Error::other)?;
            if artifacts.is_empty() {
                Err(io::Error::other(
                    "no cycle-stamped D418 PCM stream detected",
                ))
            } else {
                for artifact in artifacts {
                    eprintln!("digi-wav: wrote {}", artifact.location.0);
                }
                Ok(())
            }
        })(),
        Format::Synth => {
            let inputs = AnalysisInputs::from_states(&trace, states, clock);
            let program = AnalyzedSidProgram::from_analysis_inputs(
                header,
                subtune,
                timing,
                trace.capture.clone(),
                inputs,
            );
            let census_path = census_sidecar_path(cli.output.as_deref());
            let result = write_export(cli.output.as_deref(), move |out| {
                synth::write_synth_with_options(
                    &program,
                    out,
                    cli.synth_options(synth::SynthStyle::SidFaithful),
                )
            });
            result.map(|census| write_census_sidecar(census_path.as_deref(), &census))
        }
        Format::SynthNative | Format::SynthModern => Err(io::Error::other(
            "native synth dispatch reached trace exporter",
        )),
    };
    if let Err(e) = result {
        eprintln!("export: {e}");
        return ExitCode::from(EXIT_EXPORT);
    }
    ExitCode::SUCCESS
}

/// The `--format synth-native` path: identify the playroutine by code signature
/// and build the project from the driver's own song tables, reusing
/// [`synth::write_synth`] for output. Identification and extraction happen
/// before any file is opened, so an unsupported driver never leaves a partial
/// file behind. Native extraction is strict; callers choose `--format synth`
/// explicitly when trace-derived output is acceptable.
fn run_export_synth_native(
    cli: &Cli,
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    timing: emu::PlaybackTiming,
    frames: u32,
    format: Format,
) -> ExitCode {
    let label = format_name(format);
    let db = PlayerDb::embedded();
    let (driver, extractor, program) =
        match native::extract_native(&db, header, bytes, subtune, timing, frames) {
            Ok(parts) => parts,
            Err(e) => {
                eprintln!("{label}: {e}");
                return ExitCode::from(EXIT_EXPORT);
            }
        };
    eprintln!("{label}: driver {driver:?} via the {extractor:?} extractor");

    let census_path = census_sidecar_path(cli.output.as_deref());
    let result = write_export(cli.output.as_deref(), |out| {
        let style = match format {
            Format::SynthModern => synth::SynthStyle::ModernAnalog,
            _ => synth::SynthStyle::SidFaithful,
        };
        synth::write_synth_with_options(&program, out, cli.synth_options(style))
    });
    match result {
        Ok(census) => write_census_sidecar(census_path.as_deref(), &census),
        Err(e) => {
            eprintln!("export: {e}");
            return ExitCode::from(EXIT_EXPORT);
        }
    }
    ExitCode::SUCCESS
}

/// Write through `f` to the path (if `Some`) or to stdout. Output is
/// buffered and flushed before returning.
fn write_export<F, T>(path: Option<&Path>, f: F) -> io::Result<T>
where
    F: FnOnce(&mut dyn Write) -> io::Result<T>,
{
    let Some(path) = path else {
        let stdout = io::stdout();
        let mut out = BufWriter::new(stdout.lock());
        let value = f(&mut out)?;
        out.flush()?;
        return Ok(value);
    };

    let temporary = temporary_output_path(path);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let mut out = BufWriter::new(file);
    let result = (|| {
        let value = f(&mut out)?;
        out.flush()?;
        out.get_ref().sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(value)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn temporary_output_path(path: &Path) -> PathBuf {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output");
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), sequence))
}

/// Path for the fidelity-census sidecar next to `output` (`foo.ptz` →
/// `foo.census.json`); `None` when writing to stdout — there is no file to anchor
/// the sidecar to.
fn census_sidecar_path(output: Option<&Path>) -> Option<PathBuf> {
    output.map(|p| p.with_extension("census.json"))
}

/// Write the fidelity-census sidecar JSON. A sidecar failure never fails the
/// export (the project file is already written) — it only warns.
fn write_census_sidecar(path: Option<&Path>, census: &synth::Census) {
    let Some(path) = path else { return };
    let result = write_export(Some(path), |out| {
        serde_json::to_writer_pretty(out, census).map_err(io::Error::other)
    });
    match result {
        Ok(()) => eprintln!("fidelity census: wrote {}", path.display()),
        Err(e) => eprintln!("fidelity census: could not write {}: {e}", path.display()),
    }
}

fn print_subtune_lengths(lengths: &[std::time::Duration]) {
    print!("lengths       :");
    for (i, d) in lengths.iter().enumerate() {
        let sep = if i == 0 { ' ' } else { ',' };
        print!("{sep} {}={}", i + 1, songlengths::format_duration(*d));
    }
    println!();
}

fn print_stil(entry: &StilEntry, subtune: SubtuneIndex) {
    println!("STIL path     : {}", entry.path.0);
    for field in &entry.fields {
        if field
            .subtune
            .is_some_and(|field_subtune| field_subtune != subtune)
        {
            continue;
        }
        let label = match field.kind {
            StilFieldKind::Name => "name",
            StilFieldKind::Title => "title",
            StilFieldKind::Artist => "artist",
            StilFieldKind::Author => "author",
            StilFieldKind::Comment => "comment",
            StilFieldKind::Bug => "bug",
        };
        println!("STIL {label:<7}: {}", field.value.replace('\n', " / "));
    }
}

fn print_effects(spans: &[EffectSpan]) {
    println!();
    if spans.is_empty() {
        println!("no effects detected");
        return;
    }
    println!("detected effects ({} spans):", spans.len());
    // Wide enough for the longest Effect Display ("FilterSweep" = 11 chars).
    const EFFECT_COL: usize = 12;
    for s in spans {
        match s.voice {
            Some(v) => print!("  V{v} "),
            None => print!("  -- "),
        }
        println!(
            " {:<w$}  frames {}..{}",
            s.effect,
            s.start_frame,
            s.end_frame,
            w = EFFECT_COL,
        );
    }
}

fn print_header(h: &Header, bytes: &[u8]) {
    println!("format        : {}", h.format);
    println!("version       : {}", h.version);

    let load_line = if h.load_address.0 == 0 {
        match h.effective_load_address(bytes) {
            Ok(eff) => format!("$0000  (embedded: {eff})"),
            Err(_) => "$0000  (embedded: <missing>)".to_string(),
        }
    } else {
        format!("{}", h.load_address)
    };
    println!("load address  : {load_line}");
    println!("init address  : {}", h.init_address);
    println!("play address  : {}", h.play_address);
    println!("songs         : {}", h.songs);
    println!("start song    : {}", h.start_song);

    let summary = h.speed_summary();
    let listed = summary.vblank.len() + summary.cia.len();
    let speed_text = if summary.cia.is_empty() {
        format!("subtunes 1-{listed} all vblank")
    } else if summary.vblank.is_empty() {
        format!("subtunes 1-{listed} all CIA")
    } else {
        let vbl: Vec<u16> = summary.vblank.iter().map(|s| s.0).collect();
        let cia: Vec<u16> = summary.cia.iter().map(|s| s.0).collect();
        format!("vblank: {vbl:?}, CIA: {cia:?}")
    };
    println!("speed         : 0x{:08X}  ({speed_text})", h.speed.0);

    println!("name          : {}", h.name);
    println!("author        : {}", h.author);
    println!("released      : {}", h.released);

    if h.version >= 2 {
        println!("clock         : {}", h.flags.clock);
        println!("sid model     : {}", h.flags.sid_model);
        if let Some(m) = h.flags.sid_model_2 {
            println!("sid model 2   : {m}");
        }
        if let Some(m) = h.flags.sid_model_3 {
            println!("sid model 3   : {m}");
        }
        if let Some(a) = h.second_sid_address {
            println!("second sid    : {a}");
        }
        if let Some(a) = h.third_sid_address {
            println!("third sid     : {a}");
        }
    }
}

fn print_trace_summary(trace: &Trace, subtune: SubtuneIndex) {
    println!();
    println!(
        "ran subtune {} : {} frames, {} SID writes, {} OSC3 reads, {} ENV3 reads",
        subtune,
        trace.frames.len(),
        trace.total_writes(),
        trace.osc3_read_count(),
        trace.env3_read_count(),
    );
}

fn print_trace_detail(trace: &Trace) {
    const PREFIX_WIDTH: usize = "frame ".len() + 5;
    for frame in &trace.frames {
        if frame.writes.is_empty() {
            println!("frame {:5}  (no writes)", frame.frame);
            continue;
        }
        for (i, w) in frame.writes.iter().enumerate() {
            if i == 0 {
                println!("frame {:5}  {w}", frame.frame);
            } else {
                println!("{:1$}  {w}", "", PREFIX_WIDTH);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psid() -> Vec<u8> {
        const HEADER_LEN: usize = 0x7C;
        let mut bytes = vec![0; HEADER_LEN];
        bytes[0..4].copy_from_slice(b"PSID");
        bytes[4..6].copy_from_slice(&2u16.to_be_bytes());
        bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
        bytes[8..10].copy_from_slice(&0x1000u16.to_be_bytes());
        bytes[10..12].copy_from_slice(&0x1000u16.to_be_bytes());
        bytes[12..14].copy_from_slice(&0x1000u16.to_be_bytes());
        bytes[14..16].copy_from_slice(&1u16.to_be_bytes());
        bytes[16..18].copy_from_slice(&1u16.to_be_bytes());
        bytes
    }

    #[test]
    fn preflight_allows_psid() {
        let header = header::parse(&psid()).unwrap();
        assert_eq!(header.format, header::Format::Psid);
        assert!(preflight(&header, false).is_ok());
    }

    #[test]
    fn preflight_refuses_rsid_by_default() {
        let mut header = header::parse(&psid()).unwrap();
        header.format = header::Format::Rsid;
        assert!(preflight(&header, false).is_err());
    }

    #[test]
    fn preflight_allows_rsid_with_opt_in() {
        let mut header = header::parse(&psid()).unwrap();
        header.format = header::Format::Rsid;
        assert!(preflight(&header, true).is_ok());
    }

    #[test]
    fn enhance_accepts_only_the_documented_range() {
        let valid = Cli::try_parse_from([
            "sid-analyzer",
            "song.sid",
            "--format",
            "synth",
            "--enhance",
            "5",
            "--output",
            "song.ptz",
        ])
        .unwrap();
        assert_eq!(valid.enhance.map(synth::EnhancementAmount::get), Some(5));
        assert!(valid.validate_enhancement_format().is_ok());

        for invalid in ["0", "11", "heavy"] {
            assert!(
                Cli::try_parse_from([
                    "sid-analyzer",
                    "song.sid",
                    "--format",
                    "synth",
                    "--enhance",
                    invalid,
                    "--output",
                    "song.ptz",
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn enhance_is_limited_to_faithful_synth_formats() {
        let native = Cli::try_parse_from([
            "sid-analyzer",
            "song.sid",
            "--format",
            "synth-native",
            "--enhance",
            "5",
            "--output",
            "song.ptz",
        ])
        .unwrap();
        assert!(native.validate_enhancement_format().is_ok());

        for format in ["json", "synth-modern"] {
            let invalid = Cli::try_parse_from([
                "sid-analyzer",
                "song.sid",
                "--format",
                format,
                "--enhance",
                "5",
                "--output",
                "song.ptz",
            ])
            .unwrap();
            assert!(invalid.validate_enhancement_format().is_err());
        }
    }

    #[test]
    fn failed_atomic_export_preserves_existing_output() {
        let path = std::env::temp_dir().join(format!(
            "sid-analyzer-atomic-failure-{}-{}.json",
            std::process::id(),
            temporary_output_path(Path::new("probe")).display(),
        ));
        std::fs::write(&path, b"previous").unwrap();
        let result = write_export(Some(&path), |out| {
            out.write_all(b"partial")?;
            Err::<(), _>(io::Error::other("injected failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"previous");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn successful_atomic_export_replaces_existing_output() {
        let path = std::env::temp_dir().join(format!(
            "sid-analyzer-atomic-success-{}-{}.json",
            std::process::id(),
            temporary_output_path(Path::new("probe")).display(),
        ));
        std::fs::write(&path, b"previous").unwrap();
        write_export(Some(&path), |out| out.write_all(b"complete")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete");
        std::fs::remove_file(path).unwrap();
    }
}
