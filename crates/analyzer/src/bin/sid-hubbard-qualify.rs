use clap::Parser;
use md5::{Digest, Md5};
use serde::Serialize;
use sid_analyzer::analysis::SystemClock;
use sid_analyzer::emu::PlaybackTiming;
use sid_analyzer::export::native::{
    NativeValidationPolicy, NativeValidationReport, extract_native,
};
use sid_analyzer::export::synth::write_synth_quiet;
use sid_analyzer::header::{self, SubtuneIndex};
use sid_analyzer::playerid::PlayerDb;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(about = "Qualify Rob Hubbard native extraction across SID fixtures")]
struct Cli {
    #[arg(long, default_value = "assets/music")]
    assets: PathBuf,
    #[arg(long, default_value_t = 1500)]
    frames: u32,
    #[arg(long)]
    all_subtunes: bool,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("could not read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: header::Error,
    },
    #[error("could not serialize qualification report: {0}")]
    Serialize(serde_json::Error),
    #[error("could not write qualification report: {0}")]
    Write(io::Error),
}

#[derive(Serialize)]
struct QualificationReport {
    frames: u32,
    all_subtunes: bool,
    policy: NativeValidationPolicy,
    summary: QualificationSummary,
    results: Vec<QualificationResult>,
}

#[derive(Serialize)]
struct QualificationSummary {
    accepted: usize,
    rejected: usize,
}

#[derive(Serialize)]
struct QualificationResult {
    asset: String,
    subtune: SubtuneIndex,
    outcome: QualificationOutcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum QualificationOutcome {
    Accepted {
        extractor: String,
        notes: usize,
        patches: usize,
        placements: usize,
        provenance_fields: usize,
        project_md5: String,
        census_md5: String,
        validation: Box<NativeValidationReport>,
    },
    Rejected {
        reason: String,
    },
}

fn md5_hex(bytes: impl AsRef<[u8]>) -> String {
    Md5::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

fn qualify(cli: &Cli) -> Result<QualificationReport, AppError> {
    let db = PlayerDb::embedded();
    let mut results = Vec::new();
    for path in sid_paths(&cli.assets)? {
        let bytes = std::fs::read(&path).map_err(|source| AppError::Read {
            path: path.clone(),
            source,
        })?;
        if db.identify(&bytes) != Some("Rob_Hubbard") {
            continue;
        }
        let header = header::parse(&bytes).map_err(|source| AppError::Parse {
            path: path.clone(),
            source,
        })?;
        let subtunes: Vec<_> = if cli.all_subtunes {
            (1..=header.songs.0).map(SubtuneIndex).collect()
        } else {
            vec![header.start_song]
        };
        let asset = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<non-utf8>")
            .to_owned();
        for subtune in subtunes {
            let clock = SystemClock::from(header.flags.clock);
            let timing = PlaybackTiming::for_subtune_with_clock(&header, subtune, clock);
            let outcome = match extract_native(&db, &header, &bytes, subtune, timing, cli.frames) {
                Ok((_driver, extractor, program)) => {
                    let notes = program.semantic.notes.len();
                    let patches = program.semantic.patches.len();
                    let placements = program
                        .semantic
                        .structure
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .map(|voice| voice.placements.len())
                        .sum();
                    let Some(native) = program.semantic.native.as_ref() else {
                        return Err(AppError::Write(io::Error::other(
                            "native extraction produced no semantic overlay",
                        )));
                    };
                    let provenance_fields = native.fields.len();
                    let validation = native.validation.clone();
                    let mut project = Vec::new();
                    let census =
                        write_synth_quiet(&program, &mut project).map_err(AppError::Write)?;
                    let census_bytes = serde_json::to_vec(&census).map_err(AppError::Serialize)?;
                    QualificationOutcome::Accepted {
                        extractor: extractor.to_owned(),
                        notes,
                        patches,
                        placements,
                        provenance_fields,
                        project_md5: md5_hex(project),
                        census_md5: md5_hex(census_bytes),
                        validation: Box::new(validation),
                    }
                }
                Err(error) => QualificationOutcome::Rejected {
                    reason: error.to_string(),
                },
            };
            results.push(QualificationResult {
                asset: asset.clone(),
                subtune,
                outcome,
            });
        }
    }
    let accepted = results
        .iter()
        .filter(|result| matches!(&result.outcome, QualificationOutcome::Accepted { .. }))
        .count();
    Ok(QualificationReport {
        frames: cli.frames,
        all_subtunes: cli.all_subtunes,
        policy: NativeValidationPolicy::default(),
        summary: QualificationSummary {
            accepted,
            rejected: results.len().saturating_sub(accepted),
        },
        results,
    })
}

fn run() -> Result<(), AppError> {
    let cli = Cli::parse();
    let started = std::time::Instant::now();
    let report = qualify(&cli)?;
    let bytes = serde_json::to_vec_pretty(&report).map_err(AppError::Serialize)?;
    let result = match cli.output {
        Some(path) => std::fs::write(path, bytes).map_err(AppError::Write),
        None => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(&bytes).map_err(AppError::Write)?;
            out.write_all(b"\n").map_err(AppError::Write)
        }
    };
    eprintln!(
        "Hubbard qualification: {} accepted, {} rejected, {} outcomes in {:.2}s",
        report.summary.accepted,
        report.summary.rejected,
        report.results.len(),
        started.elapsed().as_secs_f64()
    );
    result
}

fn main() {
    if let Err(error) = run() {
        eprintln!("sid-hubbard-qualify: {error}");
        std::process::exit(1);
    }
}
