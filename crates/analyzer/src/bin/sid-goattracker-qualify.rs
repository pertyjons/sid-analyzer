use clap::{Parser, ValueEnum};
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

const DRIVER_V1: &str = "GoatTracker_V1.x";
const DRIVER_V2: &str = "GoatTracker_V2.x";

#[derive(Parser)]
#[command(about = "Qualify GoatTracker native extraction across a SID corpus")]
struct Cli {
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long, default_value_t = 1_500)]
    frames: u32,
    #[arg(long, value_enum, default_value_t = Family::All)]
    family: Family,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum Family {
    #[default]
    All,
    V1,
    V2,
}

impl Family {
    fn includes(self, driver: &str) -> bool {
        match self {
            Self::All => matches!(driver, DRIVER_V1 | DRIVER_V2),
            Self::V1 => driver == DRIVER_V1,
            Self::V2 => driver == DRIVER_V2,
        }
    }
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
    family: FamilyReport,
    summary: Vec<FamilySummary>,
    results: Vec<QualificationResult>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum FamilyReport {
    All,
    V1,
    V2,
}

impl From<Family> for FamilyReport {
    fn from(value: Family) -> Self {
        match value {
            Family::All => Self::All,
            Family::V1 => Self::V1,
            Family::V2 => Self::V2,
        }
    }
}

#[derive(Serialize)]
struct FamilySummary {
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

fn qualify_one(
    root: &Path,
    path: &Path,
    driver: &str,
    frames: u32,
) -> Result<QualificationResult, AppError> {
    let bytes = std::fs::read(path).map_err(|source| AppError::Read {
        path: path.to_owned(),
        source,
    })?;
    let path = relative_path(root, path);
    let header = match header::parse(&bytes) {
        Ok(header) => header,
        Err(error) => {
            return Ok(QualificationResult {
                path,
                driver: driver.to_owned(),
                subtune: SubtuneIndex(1),
                outcome: QualificationOutcome::Rejected {
                    class: FailureClass::Header,
                    detail: error.to_string(),
                },
            });
        }
    };
    let subtune = header.start_song;
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
            let validation = program
                .semantic
                .native
                .as_ref()
                .map(|native| &native.validation);
            let structured = program
                .semantic
                .native
                .as_ref()
                .is_some_and(|native| native.recovered_structure.is_some());
            QualificationOutcome::Accepted {
                extractor: extractor.to_owned(),
                notes,
                placements,
                structured,
                precision: validation.map_or(0.0, |report| report.precision),
                recall: validation.map_or(0.0, |report| report.recall),
            }
        }
        Err(error) => QualificationOutcome::Rejected {
            class: FailureClass::for_error(&error),
            detail: error.to_string(),
        },
    };
    Ok(QualificationResult {
        path,
        driver: driver.to_owned(),
        subtune,
        outcome,
    })
}

fn identified_paths(cli: &Cli) -> Result<Vec<(PathBuf, String)>, AppError> {
    let db = PlayerDb::embedded();
    let paths = sid_paths(&cli.corpus)?;
    let rows: Vec<Result<Option<(PathBuf, String)>, AppError>> = paths
        .par_iter()
        .map(|path| {
            let bytes = std::fs::read(path).map_err(|source| AppError::Read {
                path: path.clone(),
                source,
            })?;
            Ok(db
                .identify(&bytes)
                .filter(|driver| cli.family.includes(driver))
                .map(|driver| (path.clone(), driver.to_owned())))
        })
        .collect();
    let mut identified = Vec::new();
    for row in rows {
        if let Some(row) = row? {
            identified.push(row);
        }
    }
    if let Some(limit) = cli.limit {
        identified.truncate(limit);
    }
    Ok(identified)
}

fn summaries(results: &[QualificationResult]) -> Vec<FamilySummary> {
    let mut summaries: BTreeMap<&str, FamilySummary> = BTreeMap::new();
    for result in results {
        let summary = summaries
            .entry(&result.driver)
            .or_insert_with(|| FamilySummary {
                driver: result.driver.clone(),
                identified: 0,
                accepted: 0,
                structured: 0,
                failures: BTreeMap::new(),
            });
        summary.identified += 1;
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
    summaries.into_values().collect()
}

fn qualify(cli: &Cli) -> Result<QualificationReport, AppError> {
    let identified = identified_paths(cli)?;
    let rows: Vec<Result<QualificationResult, AppError>> = identified
        .par_iter()
        .map(|(path, driver)| qualify_one(&cli.corpus, path, driver, cli.frames))
        .collect();
    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    results.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(QualificationReport {
        schema_version: 1,
        corpus: cli.corpus.display().to_string(),
        frames: cli.frames,
        family: cli.family.into(),
        summary: summaries(&results),
        results,
    })
}

fn run() -> Result<(), AppError> {
    let cli = Cli::parse();
    let started = std::time::Instant::now();
    let report = qualify(&cli)?;
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
    let identified: usize = report
        .summary
        .iter()
        .map(|summary| summary.identified)
        .sum();
    let accepted: usize = report.summary.iter().map(|summary| summary.accepted).sum();
    let structured: usize = report
        .summary
        .iter()
        .map(|summary| summary.structured)
        .sum();
    eprintln!(
        "GoatTracker qualification: {accepted}/{identified} accepted, {structured} structured in {:.2}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("sid-goattracker-qualify: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_are_sorted_and_keep_failure_classes_separate() {
        let results = vec![
            QualificationResult {
                path: "b.sid".to_owned(),
                driver: DRIVER_V2.to_owned(),
                subtune: SubtuneIndex(1),
                outcome: QualificationOutcome::Rejected {
                    class: FailureClass::DecodeFailed,
                    detail: "bad pattern".to_owned(),
                },
            },
            QualificationResult {
                path: "a.sid".to_owned(),
                driver: DRIVER_V1.to_owned(),
                subtune: SubtuneIndex(1),
                outcome: QualificationOutcome::Accepted {
                    extractor: "goattracker-v1".to_owned(),
                    notes: 1,
                    placements: 1,
                    structured: true,
                    precision: 1.0,
                    recall: 1.0,
                },
            },
        ];
        let summaries = summaries(&results);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].driver, DRIVER_V1);
        assert_eq!(summaries[0].accepted, 1);
        assert_eq!(summaries[0].structured, 1);
        assert_eq!(summaries[1].failures[&FailureClass::DecodeFailed], 1);
    }
}
