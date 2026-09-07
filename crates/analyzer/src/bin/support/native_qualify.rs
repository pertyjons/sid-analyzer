use clap::Parser;
use rayon::prelude::*;
use serde::Serialize;
use sid_analyzer::analysis::SystemClock;
use sid_analyzer::emu::PlaybackTiming;
use sid_analyzer::export::native::{EmulationStage, NativeError, extract_native};
use sid_analyzer::header::{self, SubtuneIndex};
use sid_analyzer::playerid::PlayerDb;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy)]
pub(crate) struct FamilySpec {
    pub driver: &'static str,
    pub label: &'static str,
    pub default_frames: u32,
}

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long)]
    frames: Option<u32>,
    #[arg(long, default_value_t = 30)]
    timeout_seconds: u64,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("could not read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("could not serialize qualification report: {0}")]
    Serialize(serde_json::Error),
    #[error("could not write qualification report: {0}")]
    Write(io::Error),
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
    fn for_error(error: &NativeError) -> Self {
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

#[derive(Serialize)]
struct QualificationReport {
    schema_version: u32,
    corpus: String,
    frames: u32,
    timeout_seconds: u64,
    summary: QualificationSummary,
    results: Vec<QualificationResult>,
}

#[derive(Serialize)]
struct QualificationSummary {
    driver: String,
    identified: usize,
    accepted: usize,
    structured: usize,
    failures: BTreeMap<FailureClass, usize>,
}

#[derive(Serialize)]
struct QualificationResult {
    path: String,
    driver: String,
    subtune: SubtuneIndex,
    outcome: QualificationOutcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum QualificationOutcome {
    Accepted {
        extractor: String,
        notes: usize,
        placements: usize,
        structured: bool,
        precision: f64,
        recall: f64,
    },
    Rejected {
        class: FailureClass,
        detail: String,
    },
}

fn sid_paths(root: &Path) -> Result<Vec<PathBuf>, AppError> {
    let mut directories = vec![root.to_owned()];
    let mut paths = Vec::new();
    while let Some(directory) = directories.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|source| AppError::Read {
            path: directory.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| AppError::Read {
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            if path.is_dir() {
                directories.push(path);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("sid") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn rejected(
    spec: FamilySpec,
    root: &Path,
    path: &Path,
    subtune: SubtuneIndex,
    class: FailureClass,
    detail: String,
) -> QualificationResult {
    QualificationResult {
        path: relative_path(root, path),
        driver: spec.driver.to_owned(),
        subtune,
        outcome: QualificationOutcome::Rejected { class, detail },
    }
}

fn qualify_one(
    spec: FamilySpec,
    root: &Path,
    path: &Path,
    frames: u32,
    timeout: Duration,
) -> Result<QualificationResult, AppError> {
    let bytes = std::fs::read(path).map_err(|source| AppError::Read {
        path: path.to_owned(),
        source,
    })?;
    let header = match header::parse(&bytes) {
        Ok(header) => header,
        Err(error) => {
            return Ok(rejected(
                spec,
                root,
                path,
                SubtuneIndex(1),
                FailureClass::Header,
                error.to_string(),
            ));
        }
    };
    let subtune = header.start_song;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let clock = SystemClock::from(header.flags.clock);
        let timing = PlaybackTiming::for_subtune_with_clock(&header, subtune, clock);
        let db = PlayerDb::embedded();
        let outcome = match extract_native(&db, &header, &bytes, subtune, timing, frames) {
            Ok((_driver, extractor, program)) => {
                let notes = program.semantic.notes.len();
                let placements = program
                    .semantic
                    .structure
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|voice| voice.placements.len())
                    .sum();
                let native = program.semantic.native.as_ref();
                let structured = native
                    .is_some_and(|overlay| overlay.recovered_structure.is_some())
                    && program.semantic.structure.is_some();
                QualificationOutcome::Accepted {
                    extractor: extractor.to_owned(),
                    notes,
                    placements,
                    structured,
                    precision: native.map_or(0.0, |overlay| overlay.validation.precision),
                    recall: native.map_or(0.0, |overlay| overlay.validation.recall),
                }
            }
            Err(error) => QualificationOutcome::Rejected {
                class: FailureClass::for_error(&error),
                detail: error.to_string(),
            },
        };
        let _ = sender.send(outcome);
    });
    let outcome = match receiver.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => QualificationOutcome::Rejected {
            class: FailureClass::Timeout,
            detail: format!("qualification exceeded {} seconds", timeout.as_secs()),
        },
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => QualificationOutcome::Rejected {
            class: FailureClass::Invariant,
            detail: "qualification worker disconnected".to_owned(),
        },
    };
    Ok(QualificationResult {
        path: relative_path(root, path),
        driver: spec.driver.to_owned(),
        subtune,
        outcome,
    })
}

fn identified_paths(spec: FamilySpec, cli: &Cli) -> Result<Vec<PathBuf>, AppError> {
    let db = PlayerDb::embedded();
    let paths = sid_paths(&cli.corpus)?;
    let rows: Vec<Result<Option<PathBuf>, AppError>> = paths
        .par_iter()
        .map(|path| {
            let bytes = std::fs::read(path).map_err(|source| AppError::Read {
                path: path.clone(),
                source,
            })?;
            Ok((db.identify(&bytes) == Some(spec.driver)).then(|| path.clone()))
        })
        .collect();
    let mut identified = Vec::new();
    for row in rows {
        if let Some(path) = row? {
            identified.push(path);
        }
    }
    identified.sort();
    if let Some(limit) = cli.limit {
        identified.truncate(limit);
    }
    Ok(identified)
}

fn summarize(spec: FamilySpec, results: &[QualificationResult]) -> QualificationSummary {
    let mut summary = QualificationSummary {
        driver: spec.driver.to_owned(),
        identified: results.len(),
        accepted: 0,
        structured: 0,
        failures: BTreeMap::new(),
    };
    for result in results {
        match result.outcome {
            QualificationOutcome::Accepted { structured, .. } => {
                summary.accepted += 1;
                summary.structured += usize::from(structured);
            }
            QualificationOutcome::Rejected { class, .. } => {
                *summary.failures.entry(class).or_default() += 1;
            }
        }
    }
    summary
}

fn qualify(spec: FamilySpec, cli: &Cli, frames: u32) -> Result<QualificationReport, AppError> {
    let identified = identified_paths(spec, cli)?;
    let timeout = Duration::from_secs(cli.timeout_seconds);
    let rows: Vec<Result<QualificationResult, AppError>> = identified
        .par_iter()
        .map(|path| qualify_one(spec, &cli.corpus, path, frames, timeout))
        .collect();
    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    results.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(QualificationReport {
        schema_version: 1,
        corpus: cli.corpus.display().to_string(),
        frames,
        timeout_seconds: cli.timeout_seconds,
        summary: summarize(spec, &results),
        results,
    })
}

fn run(spec: FamilySpec) -> Result<(), AppError> {
    let cli = Cli::parse();
    let frames = cli.frames.unwrap_or(spec.default_frames);
    let started = std::time::Instant::now();
    let report = qualify(spec, &cli, frames)?;
    let bytes = serde_json::to_vec_pretty(&report).map_err(AppError::Serialize)?;
    match cli.output {
        Some(path) => std::fs::write(path, bytes).map_err(AppError::Write)?,
        None => {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            output.write_all(&bytes).map_err(AppError::Write)?;
            output.write_all(b"\n").map_err(AppError::Write)?;
        }
    }
    eprintln!(
        "{} qualification: {}/{} accepted, {} structured in {:.2}s",
        spec.label,
        report.summary.accepted,
        report.summary.identified,
        report.summary.structured,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

pub(crate) fn main(spec: FamilySpec) {
    if let Err(error) = run(spec) {
        eprintln!("sid-{}-qualify: {error}", spec.label.to_ascii_lowercase());
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: FamilySpec = FamilySpec {
        driver: "Test_Driver",
        label: "Test",
        default_frames: 400,
    };

    #[test]
    fn summary_keeps_failure_classes_separate() {
        let results = vec![
            QualificationResult {
                path: "b.sid".to_owned(),
                driver: SPEC.driver.to_owned(),
                subtune: SubtuneIndex(1),
                outcome: QualificationOutcome::Rejected {
                    class: FailureClass::DecodeUnreliable,
                    detail: "pitch mismatch".to_owned(),
                },
            },
            QualificationResult {
                path: "a.sid".to_owned(),
                driver: SPEC.driver.to_owned(),
                subtune: SubtuneIndex(1),
                outcome: QualificationOutcome::Accepted {
                    extractor: "test".to_owned(),
                    notes: 1,
                    placements: 1,
                    structured: true,
                    precision: 1.0,
                    recall: 1.0,
                },
            },
            QualificationResult {
                path: "c.sid".to_owned(),
                driver: SPEC.driver.to_owned(),
                subtune: SubtuneIndex(1),
                outcome: QualificationOutcome::Rejected {
                    class: FailureClass::Timeout,
                    detail: "slow".to_owned(),
                },
            },
        ];
        let summary = summarize(SPEC, &results);
        assert_eq!(summary.identified, 3);
        assert_eq!(summary.accepted, 1);
        assert_eq!(summary.structured, 1);
        assert_eq!(summary.failures[&FailureClass::DecodeUnreliable], 1);
        assert_eq!(summary.failures[&FailureClass::Timeout], 1);
    }
}
