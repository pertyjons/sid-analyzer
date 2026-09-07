//! Reproducible corpus baseline (PLAN §1).
//!
//! Exports every tune in a corpus directory twice — once trace-derived
//! (`--format synth`) and once strictly native (`--format synth-native`) —
//! validates every produced project against the mirrored Pertylizer schema,
//! and writes a deterministic comparison table plus a JSON report.
//!
//! The point is that no outcome is silent. A tune that cannot be emulated, a
//! driver with no extractor, a variant whose tables will not decode: each
//! becomes a typed row with the reason attached, never a quiet fallback to
//! trace export. Two runs over an unchanged tree produce identical reports.

use clap::Parser;
use rayon::prelude::*;
use serde::Serialize;
use sid_analyzer::analysis::SystemClock;
use sid_analyzer::analysis::sid_program::AnalyzedSidProgram;
use sid_analyzer::emu;
use sid_analyzer::export::synth::lowering::TargetCapabilities;
use sid_analyzer::export::synth::{CensusSummary, NativeCapability};
use sid_analyzer::export::{native, synth};
use sid_analyzer::header::{self, Header, SubtuneIndex};
use sid_analyzer::playerid::PlayerDb;
use sid_analyzer::songlengths::{self, SongLengths};
use sid_analyzer::support::UnsupportedInputKind;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

const EXIT_USAGE: u8 = 1;
const EXIT_IO: u8 = 2;

/// Tunes pinned to a short diagnostic window instead of their full length.
///
/// These are the known-slow or known-failing cases (PLAN §1): pinning the
/// window keeps an unattended run bounded and, more importantly, makes their
/// numbers comparable between runs while the underlying bugs are open.
const DIAGNOSTIC_WINDOWS: &[&str] = &[
    "Auf_Wiedersehen_Monty",
    "Comic_Bakery",
    "Knucklebusters",
    "Ocean_Loader_1",
    "Sigma_Seven",
    "Warhawk",
];

const ABOUT: &str = "Export a SID corpus in trace and strict-native modes and report a baseline";

const LONG_ABOUT: &str = "\
Walks --corpus for .sid files and, for each one, runs both export paths: the
trace-derived synth export and the strict native export. Records elapsed time,
project size, musical and automation-only pattern counts, placements, note
graphs, automation points, native capability, and forward-model residuals for
each, then validates every written project against the mirrored Pertylizer
schema.

Every input produces a row. Unsupported RSID and MUS/STR files are skipped
with a typed reason; native
rejections carry the extractor's typed reason. The native path never falls back
to trace output — the two modes are reported side by side so a regression in
either is visible.

Frame counts come from the HVSC Songlengths database (subtune 1, at the
resolved clock rate), falling back to --fallback-secs when a tune is absent.
Tunes with a pinned diagnostic window run at --diagnostic-frames instead.

Output is deterministic: rows are sorted by file name and the report is stable
across runs over an unchanged tree.

Example:
  sid-corpus-baseline --corpus assets/music --out-dir exports/baseline";

#[derive(Parser)]
#[command(name = "sid-corpus-baseline", about = ABOUT, long_about = LONG_ABOUT)]
struct Cli {
    /// Directory to scan for .sid files.
    #[arg(long, default_value = "assets/music")]
    corpus: PathBuf,

    /// Optional local HVSC Songlengths.md5 database for full-length frame counts.
    /// Without it, use --fallback-secs for tunes without a diagnostic window.
    #[arg(long, env = "HVSC_SONGLENGTHS")]
    songlengths: Option<PathBuf>,

    /// Directory for the exported projects and the JSON report.
    #[arg(long, default_value = "exports/baseline")]
    out_dir: PathBuf,

    /// JSON report path. Defaults to <out-dir>/baseline.json.
    #[arg(long)]
    report: Option<PathBuf>,

    /// Mirrored Pertylizer project schema used for validation.
    #[arg(long, default_value = "docs/pertylizer/project.schema.json")]
    schema: PathBuf,

    /// Skip schema validation entirely (recorded as a reason in the report).
    #[arg(long)]
    no_schema: bool,

    /// Fallback duration in seconds for tunes absent from the Songlengths database.
    #[arg(long, default_value_t = 120)]
    fallback_secs: u32,

    /// Frame count for tunes with a pinned diagnostic window.
    #[arg(long, default_value_t = 1500)]
    diagnostic_frames: u32,

    /// Run pinned diagnostic tunes at full length too.
    #[arg(long)]
    full: bool,

    /// Worker threads. Defaults to the rayon default (one per core).
    #[arg(long)]
    workers: Option<usize>,
}

/// How a tune's frame count was chosen. Recorded so a report reader can tell a
/// full-length measurement from a bounded diagnostic one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Window {
    /// Songlengths duration for subtune 1 at the resolved clock rate.
    FullLength,
    /// Pinned short window (`DIAGNOSTIC_WINDOWS`).
    Diagnostic,
    /// Tune absent from the Songlengths database.
    Fallback,
}

impl Window {
    fn label(self) -> &'static str {
        match self {
            Self::FullLength => "full",
            Self::Diagnostic => "diag",
            Self::Fallback => "fallb",
        }
    }
}

/// Schema validation state for one written project.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum SchemaResult {
    Valid,
    Invalid {
        reason: String,
    },
    /// Not attempted, with the reason (no python3, no jsonschema module,
    /// or `--no-schema`).
    Skipped {
        reason: String,
    },
}

impl SchemaResult {
    fn label(&self) -> &'static str {
        match self {
            Self::Valid => "ok",
            Self::Invalid { .. } => "INVALID",
            Self::Skipped { .. } => "-",
        }
    }
}

/// Outcome of one export mode for one tune.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum ModeOutcome {
    Exported {
        elapsed_ms: u128,
        bytes: u64,
        path: String,
        schema: SchemaResult,
        summary: Box<CensusSummary>,
    },
    /// The mode did not apply and produced no file, with the typed reason.
    Rejected { reason: String },
}

impl ModeOutcome {
    fn summary(&self) -> Option<&CensusSummary> {
        match self {
            Self::Exported { summary, .. } => Some(summary),
            Self::Rejected { .. } => None,
        }
    }

    fn schema_mut(&mut self) -> Option<&mut SchemaResult> {
        match self {
            Self::Exported { schema, .. } => Some(schema),
            Self::Rejected { .. } => None,
        }
    }

    fn path(&self) -> Option<&str> {
        match self {
            Self::Exported { path, .. } => Some(path),
            Self::Rejected { .. } => None,
        }
    }
}

/// One corpus entry.
#[derive(Debug, Clone, Serialize)]
struct Row {
    name: String,
    /// `Some` for an analysable PSID; `None` when the file was skipped whole
    /// (RSID, unreadable, unparsable) — then `skipped` carries the reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    frames: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window: Option<Window>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<CorpusSkipReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<ModeOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native: Option<ModeOutcome>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum CorpusSkipKind {
    Unreadable,
    InvalidHeader,
    RsidSystemEnvironment,
    MusStrPayload,
}

#[derive(Debug, Clone, Serialize)]
struct CorpusSkipReason {
    kind: CorpusSkipKind,
    detail: String,
}

impl CorpusSkipReason {
    fn unreadable(error: &io::Error) -> Self {
        Self {
            kind: CorpusSkipKind::Unreadable,
            detail: error.to_string(),
        }
    }

    fn invalid_header(error: &header::Error) -> Self {
        Self {
            kind: CorpusSkipKind::InvalidHeader,
            detail: error.to_string(),
        }
    }

    fn unsupported(kind: UnsupportedInputKind) -> Self {
        let corpus_kind = match kind {
            UnsupportedInputKind::RsidSystemEnvironment => CorpusSkipKind::RsidSystemEnvironment,
            UnsupportedInputKind::MusStrPayload => CorpusSkipKind::MusStrPayload,
        };
        Self {
            kind: corpus_kind,
            detail: kind.detail().to_owned(),
        }
    }
}

impl std::fmt::Display for CorpusSkipReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.detail)
    }
}

#[derive(Debug, Serialize)]
struct Totals {
    tunes: usize,
    skipped: usize,
    trace_exported: usize,
    trace_rejected: usize,
    native_decoded: usize,
    native_structured: usize,
    native_rejected: usize,
    schema_valid: usize,
    schema_invalid: usize,
    schema_skipped: usize,
    elapsed_secs: f64,
}

#[derive(Debug, Serialize)]
struct Report {
    revisions: BaselineRevisions,
    performance: BaselinePerformance,
    corpus: String,
    schema: String,
    fallback_secs: u32,
    diagnostic_frames: u32,
    full: bool,
    rows: Vec<Row>,
    totals: Totals,
}

#[derive(Debug, Serialize)]
struct BaselineRevisions {
    analyzer: String,
    pertylizer: String,
}

#[derive(Debug, Serialize)]
struct BaselinePerformance {
    workers: Option<WorkerCount>,
    elapsed_secs: f64,
    peak_memory_bytes: Option<PeakMemoryBytes>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(transparent)]
struct WorkerCount(usize);

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(transparent)]
struct PeakMemoryBytes(u64);

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(n) = cli.workers
        && let Err(e) = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
    {
        eprintln!("workers: {e}");
        return ExitCode::from(EXIT_USAGE);
    }

    let mut files = match collect_sid_files(&cli.corpus) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("corpus {}: {e}", cli.corpus.display());
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if files.is_empty() {
        eprintln!("corpus {}: no .sid files", cli.corpus.display());
        return ExitCode::from(EXIT_USAGE);
    }
    files.sort();

    if let Err(e) = fs::create_dir_all(&cli.out_dir) {
        eprintln!("out-dir {}: {e}", cli.out_dir.display());
        return ExitCode::from(EXIT_IO);
    }

    let lengths = cli
        .songlengths
        .as_deref()
        .and_then(|path| match SongLengths::load(path) {
            Ok(db) => Some(db),
            Err(e) => {
                eprintln!(
                    "songlengths {}: {e} — every tune falls back to {}s",
                    path.display(),
                    cli.fallback_secs
                );
                None
            }
        });

    eprintln!(
        "baseline: {} tunes from {}",
        files.len(),
        cli.corpus.display()
    );
    let started = Instant::now();
    let mut rows: Vec<Row> = files
        .par_iter()
        .map(|path| process_tune(&cli, path, lengths.as_ref()))
        .collect();

    let schema_mode = validate_schemas(&cli, &mut rows);
    let elapsed = started.elapsed();

    let totals = totals(&rows, elapsed);
    print_table(&rows, &totals, schema_mode.as_deref());

    let report = Report {
        revisions: BaselineRevisions {
            analyzer: git_revision(Path::new(".")).unwrap_or_else(|| "unknown".to_owned()),
            pertylizer: TargetCapabilities::from_pinned_mirrors()
                .map_or_else(|_| "unknown".to_owned(), |target| target.revision.0),
        },
        performance: BaselinePerformance {
            workers: cli.workers.map(WorkerCount),
            elapsed_secs: elapsed.as_secs_f64(),
            peak_memory_bytes: peak_memory_bytes(),
        },
        corpus: cli.corpus.display().to_string(),
        schema: schema_mode.unwrap_or_else(|| cli.schema.display().to_string()),
        fallback_secs: cli.fallback_secs,
        diagnostic_frames: cli.diagnostic_frames,
        full: cli.full,
        rows,
        totals,
    };
    let report_path = cli
        .report
        .clone()
        .unwrap_or_else(|| cli.out_dir.join("baseline.json"));
    match write_report(&report_path, &report) {
        Ok(()) => eprintln!("baseline: wrote {}", report_path.display()),
        Err(e) => {
            eprintln!("report {}: {e}", report_path.display());
            return ExitCode::from(EXIT_IO);
        }
    }
    ExitCode::SUCCESS
}

fn git_revision(directory: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(directory)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn peak_memory_bytes() -> Option<PeakMemoryBytes> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let kibibytes = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(PeakMemoryBytes(kibibytes.saturating_mul(1024)))
}

/// Sorted `.sid` paths directly under `dir` and its subdirectories.
fn collect_sid_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("sid"))
        {
            out.push(path.to_path_buf());
        }
    }
    Ok(out)
}

fn tune_name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Run both export modes for one tune. Never panics and never returns early
/// without a row: every failure class becomes a recorded reason.
fn process_tune(cli: &Cli, path: &Path, lengths: Option<&SongLengths>) -> Row {
    let name = tune_name(path);
    let skip = |reason: CorpusSkipReason| Row {
        name: name.clone(),
        frames: None,
        window: None,
        skipped: Some(reason),
        trace: None,
        native: None,
    };

    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => return skip(CorpusSkipReason::unreadable(&e)),
    };
    let header = match header::parse(&bytes) {
        Ok(h) => h,
        Err(e) => return skip(CorpusSkipReason::invalid_header(&e)),
    };
    if let Some(kind) = UnsupportedInputKind::classify(&header) {
        return skip(CorpusSkipReason::unsupported(kind));
    }

    let subtune = header.start_song;
    let clock = SystemClock::from(header.flags.clock);
    let timing = emu::PlaybackTiming::for_subtune_with_clock(&header, subtune, clock);
    let (frames, window) = frame_window(cli, &name, &bytes, lengths, clock);

    let trace = export_trace(cli, &name, &header, &bytes, subtune, timing, frames, clock);
    let native = export_native(cli, &name, &header, &bytes, subtune, timing, frames);

    Row {
        name,
        frames: Some(frames),
        window: Some(window),
        skipped: None,
        trace: Some(trace),
        native: Some(native),
    }
}

/// Frame count and how it was chosen.
fn frame_window(
    cli: &Cli,
    name: &str,
    bytes: &[u8],
    lengths: Option<&SongLengths>,
    clock: SystemClock,
) -> (u32, Window) {
    if !cli.full && DIAGNOSTIC_WINDOWS.contains(&name) {
        return (cli.diagnostic_frames, Window::Diagnostic);
    }
    let secs = lengths
        .and_then(|db| db.lookup(&songlengths::compute_sid_md5(bytes)))
        .and_then(|d| d.first().copied())
        .map(|d| d.as_secs_f64());
    match secs {
        Some(s) => (frames_for(s, clock), Window::FullLength),
        None => (
            frames_for(f64::from(cli.fallback_secs), clock),
            Window::Fallback,
        ),
    }
}

fn frames_for(secs: f64, clock: SystemClock) -> u32 {
    let frames = secs * clock.frame_rate();
    // A zero-frame export is not a measurement; clamp so the row still runs.
    (frames.round() as u32).max(1)
}

#[allow(clippy::too_many_arguments)]
fn export_trace(
    cli: &Cli,
    name: &str,
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    timing: emu::PlaybackTiming,
    frames: u32,
    _clock: SystemClock,
) -> ModeOutcome {
    let started = Instant::now();
    let trace = match emu::run_with_timing(header, bytes, subtune, frames, timing) {
        Ok(t) => t,
        Err(e) => {
            return ModeOutcome::Rejected {
                reason: format!("emulation: {e}"),
            };
        }
    };
    let timing = timing.resolved_from_trace(&trace);
    let program = AnalyzedSidProgram::from_trace(header, subtune, timing, &trace);

    let path = cli.out_dir.join(format!("{name}.trace.ptz"));
    let written = write_project(&path, |out| synth::write_synth_quiet(&program, out));
    finish(written, &path, started)
}

fn export_native(
    cli: &Cli,
    name: &str,
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    timing: emu::PlaybackTiming,
    frames: u32,
) -> ModeOutcome {
    let started = Instant::now();
    let db = PlayerDb::embedded();
    let (_driver, _extractor, program) =
        match native::extract_native(&db, header, bytes, subtune, timing, frames) {
            Ok(parts) => parts,
            Err(e) => {
                return ModeOutcome::Rejected {
                    reason: e.to_string(),
                };
            }
        };
    let path = cli.out_dir.join(format!("{name}.native.ptz"));
    let written = write_project(&path, |out| synth::write_synth_quiet(&program, out));
    finish(written, &path, started)
}

/// Write one project atomically enough for a report: a failed write leaves no
/// half-file behind that a later schema pass would try to validate.
fn write_project<F>(path: &Path, f: F) -> io::Result<synth::Census>
where
    F: FnOnce(&mut dyn Write) -> io::Result<synth::Census>,
{
    let file = fs::File::create(path)?;
    let mut out = BufWriter::new(file);
    let census = f(&mut out)?;
    out.flush()?;
    Ok(census)
}

fn finish(written: io::Result<synth::Census>, path: &Path, started: Instant) -> ModeOutcome {
    match written {
        Ok(census) => {
            let bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            ModeOutcome::Exported {
                elapsed_ms: started.elapsed().as_millis(),
                bytes,
                path: path.display().to_string(),
                schema: SchemaResult::Skipped {
                    reason: "not yet validated".to_string(),
                },
                summary: Box::new(census.summary()),
            }
        }
        Err(e) => {
            let _ = fs::remove_file(path);
            ModeOutcome::Rejected {
                reason: format!("export: {e}"),
            }
        }
    }
}

/// Validate every written project against the mirrored schema in one python
/// process, filling each row's `SchemaResult`. Returns the schema description
/// for the report, or `None` when validation did not run.
fn validate_schemas(cli: &Cli, rows: &mut [Row]) -> Option<String> {
    let mut targets: Vec<(usize, bool, String)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if let Some(p) = row.trace.as_ref().and_then(ModeOutcome::path) {
            targets.push((i, false, p.to_string()));
        }
        if let Some(p) = row.native.as_ref().and_then(ModeOutcome::path) {
            targets.push((i, true, p.to_string()));
        }
    }

    let unavailable = |rows: &mut [Row], reason: &str| {
        for (i, native, _) in &targets {
            let slot = if *native {
                rows[*i].native.as_mut()
            } else {
                rows[*i].trace.as_mut()
            };
            if let Some(s) = slot.and_then(ModeOutcome::schema_mut) {
                *s = SchemaResult::Skipped {
                    reason: reason.to_string(),
                };
            }
        }
    };

    if cli.no_schema {
        unavailable(rows, "--no-schema");
        return None;
    }
    if targets.is_empty() {
        return None;
    }
    if !python_jsonschema_available() {
        unavailable(rows, "python3 with the jsonschema module is not available");
        return None;
    }
    if !cli.schema.is_file() {
        unavailable(rows, "schema file not found");
        return None;
    }

    let mut cmd = Command::new("python3");
    cmd.arg("-c").arg(VALIDATE_PY).arg(&cli.schema);
    for (_, _, path) in &targets {
        cmd.arg(path);
    }
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            unavailable(rows, &format!("python3 failed to start: {e}"));
            return None;
        }
    };
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        unavailable(rows, &format!("validator failed: {}", err.trim()));
        return None;
    }

    // One line per target, same order as the arguments: "ok" or "fail\t<msg>".
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    for (i, native, _) in &targets {
        let result = match lines.next() {
            Some("ok") => SchemaResult::Valid,
            Some(line) => SchemaResult::Invalid {
                reason: line.trim_start_matches("fail\t").to_string(),
            },
            None => SchemaResult::Skipped {
                reason: "validator produced no verdict".to_string(),
            },
        };
        let slot = if *native {
            rows[*i].native.as_mut()
        } else {
            rows[*i].trace.as_mut()
        };
        if let Some(s) = slot.and_then(ModeOutcome::schema_mut) {
            *s = result;
        }
    }
    Some(cli.schema.display().to_string())
}

/// Validator: one verdict line per project path, in argument order. Kept inline
/// so the binary has no external script dependency.
const VALIDATE_PY: &str = "\
import json, sys, jsonschema
schema = json.load(open(sys.argv[1]))
validator = jsonschema.Draft7Validator(schema)
for path in sys.argv[2:]:
    try:
        doc = json.load(open(path))
        errors = sorted(validator.iter_errors(doc), key=lambda e: list(e.path))
        if errors:
            e = errors[0]
            loc = '/'.join(str(p) for p in e.path)
            print('fail\\t%s: %s' % (loc or '<root>', e.message.replace('\\n', ' ')[:200]))
        else:
            print('ok')
    except Exception as exc:
        print('fail\\t%s' % str(exc).replace('\\n', ' ')[:200])
sys.stdout.flush()
";

fn python_jsonschema_available() -> bool {
    Command::new("python3")
        .args(["-c", "import jsonschema"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn totals(rows: &[Row], elapsed: Duration) -> Totals {
    let mut t = Totals {
        tunes: rows.len(),
        skipped: 0,
        trace_exported: 0,
        trace_rejected: 0,
        native_decoded: 0,
        native_structured: 0,
        native_rejected: 0,
        schema_valid: 0,
        schema_invalid: 0,
        schema_skipped: 0,
        elapsed_secs: elapsed.as_secs_f64(),
    };
    for row in rows {
        if row.skipped.is_some() {
            t.skipped += 1;
        }
        for mode in [row.trace.as_ref(), row.native.as_ref()]
            .into_iter()
            .flatten()
        {
            if let ModeOutcome::Exported { schema, .. } = mode {
                match schema {
                    SchemaResult::Valid => t.schema_valid += 1,
                    SchemaResult::Invalid { .. } => t.schema_invalid += 1,
                    SchemaResult::Skipped { .. } => t.schema_skipped += 1,
                }
            }
        }
        match row.trace.as_ref() {
            Some(ModeOutcome::Exported { .. }) => t.trace_exported += 1,
            Some(ModeOutcome::Rejected { .. }) => t.trace_rejected += 1,
            None => {}
        }
        match row.native.as_ref() {
            Some(ModeOutcome::Exported { summary, .. }) => match summary.native {
                NativeCapability::Structured => t.native_structured += 1,
                _ => t.native_decoded += 1,
            },
            Some(ModeOutcome::Rejected { .. }) => t.native_rejected += 1,
            None => {}
        }
    }
    t
}

fn print_table(rows: &[Row], totals: &Totals, schema: Option<&str>) {
    println!(
        "{:<34} {:>6} {:>5} | {:>6} {:>7} {:>4} {:>5} {:>7} {:>7} | {:>6} {:>7} {:>4} {:>5} {:>7} {:>7} {:>10}",
        "tune",
        "frames",
        "win",
        "trace",
        "size",
        "mPat",
        "place",
        "auto",
        "schema",
        "native",
        "size",
        "mPat",
        "place",
        "auto",
        "schema",
        "capability",
    );
    for row in rows {
        if let Some(reason) = &row.skipped {
            println!("{:<34} {:>6} {:>5} | skipped: {reason}", row.name, "-", "-");
            continue;
        }
        let frames = row.frames.unwrap_or(0);
        let window = row.window.map(Window::label).unwrap_or("-");
        print!("{:<34} {frames:>6} {window:>5} |", row.name);
        print_mode(row.trace.as_ref(), false);
        print_mode(row.native.as_ref(), true);
        println!();
    }

    println!();
    println!(
        "tunes {} · skipped {} · trace {} exported / {} rejected · native {} structured / {} decoded / {} rejected",
        totals.tunes,
        totals.skipped,
        totals.trace_exported,
        totals.trace_rejected,
        totals.native_structured,
        totals.native_decoded,
        totals.native_rejected,
    );
    println!(
        "schema {} valid / {} invalid / {} unchecked ({}) · elapsed {:.1}s",
        totals.schema_valid,
        totals.schema_invalid,
        totals.schema_skipped,
        schema.unwrap_or("not validated"),
        totals.elapsed_secs,
    );

    let rejections: Vec<(&str, &str)> = rows
        .iter()
        .filter_map(|r| match r.native.as_ref() {
            Some(ModeOutcome::Rejected { reason }) => Some((r.name.as_str(), reason.as_str())),
            _ => None,
        })
        .collect();
    if !rejections.is_empty() {
        println!();
        println!("native rejections:");
        for (name, reason) in rejections {
            println!("  {name:<32} {reason}");
        }
    }

    let residuals: Vec<(&str, &CensusSummary)> = rows
        .iter()
        .filter_map(|r| {
            let s = r.native.as_ref().or(r.trace.as_ref())?.summary()?;
            (s.notes_fail > 0 || s.uncovered_gated_frames > 0).then_some((r.name.as_str(), s))
        })
        .collect();
    if !residuals.is_empty() {
        println!();
        println!("worst fidelity residuals (best available mode):");
        for (name, s) in residuals {
            println!(
                "  {name:<32} {} fail · {} degraded · mean {:.1} ct · worst {:.1} ct · {} uncovered frames",
                s.notes_fail,
                s.notes_degraded,
                s.mean_cents,
                s.worst_mean_cents,
                s.uncovered_gated_frames,
            );
        }
    }
}

fn print_mode(mode: Option<&ModeOutcome>, with_capability: bool) {
    match mode {
        Some(ModeOutcome::Exported {
            elapsed_ms,
            bytes,
            schema,
            summary,
            ..
        }) => {
            print!(
                " {:>5}s {:>6}k {:>4} {:>5} {:>7} {:>7}",
                format!("{:.1}", *elapsed_ms as f64 / 1000.0),
                bytes / 1024,
                summary.patterns,
                summary.placements,
                summary.automation_points,
                schema.label(),
            );
            if with_capability {
                let cap = match summary.native {
                    NativeCapability::Structured => "structured",
                    NativeCapability::Decoded => "decoded",
                    NativeCapability::None => "trace",
                };
                print!(" {cap:>10}");
            }
        }
        Some(ModeOutcome::Rejected { .. }) => {
            print!(
                " {:>5}  {:>6} {:>4} {:>5} {:>7} {:>7}",
                "-", "-", "-", "-", "-", "-"
            );
            if with_capability {
                print!(" {:>10}", "rejected");
            }
        }
        None => {
            print!(
                " {:>5}  {:>6} {:>4} {:>5} {:>7} {:>7}",
                "-", "-", "-", "-", "-", "-"
            );
            if with_capability {
                print!(" {:>10}", "-");
            }
        }
    }
}

fn write_report(path: &Path, report: &Report) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(path)?;
    let mut out = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut out, report).map_err(io::Error::other)?;
    writeln!(out)?;
    out.flush()
}
