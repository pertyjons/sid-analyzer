//! Identify the C64 playroutine of SID files by code signature, a parallel
//! front-end over the [`sid_analyzer::playerid`] matcher. Walks a directory
//! tree (or scans single files), matches each against a `sidid.cfg` signature
//! database, and reports per-player tallies.
//!
//! Attribution: the matching algorithm and the bundled `assets/sidid.cfg`
//! database derive from Cadaver's **SIDId** (Covert Bitops, BSD-licensed); the
//! signatures were contributed by the HVSC/C64 scene. Full credits and licence
//! are in the `playerid` module docs and `assets/sidid.cfg.NOTICE`.
//!
//! Example:
//!   sid-playerid --root /path/to/HVSC --config assets/sidid.cfg --workers 8

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use walkdir::WalkDir;

use sid_analyzer::playerid::PlayerDb;

/// Default location of the vendored SIDId signature database. See
/// `assets/sidid.cfg.NOTICE` for its upstream copyright and licence.
const DEFAULT_CONFIG: &str = "assets/sidid.cfg";

#[derive(Parser, Debug)]
#[command(
    name = "sid-playerid",
    about = "Identify C64 playroutines in SID files by code signature (SIDId-compatible).",
    long_about = "Scans .sid files and reports which playroutine (music driver) each \
uses, matched against a sidid.cfg signature database. Walks --root recursively, or pass \
one or more files/directories as positional arguments. Prints a per-file listing \
(unless --quiet) and a summary tally of detected players plus identified/unidentified \
counts.\n\nExample:\n  sid-playerid --root /path/to/HVSC --workers 8"
)]
struct Cli {
    /// Directory tree to scan recursively for .sid files.
    #[arg(long)]
    root: Option<PathBuf>,

    /// Files or directories to scan (alternative/addition to --root).
    paths: Vec<PathBuf>,

    /// Signature database (sidid.cfg) to match against.
    #[arg(long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,

    /// Worker threads. Defaults to available parallelism.
    #[arg(long)]
    workers: Option<usize>,

    /// Report every matching player per file, not just the first (SIDId -m).
    #[arg(long)]
    multi: bool,

    /// Suppress the per-file listing; print only the summary.
    #[arg(long)]
    quiet: bool,

    /// Also list unidentified files in the per-file listing.
    #[arg(long)]
    show_unidentified: bool,

    /// Write per-file results as CSV to this path (path,player).
    #[arg(long)]
    csv: Option<PathBuf>,

    /// Stop after this many files (testing).
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("loading config: {0}")]
    Config(#[from] sid_analyzer::playerid::LoadError),
    #[error("no input: pass --root or positional paths")]
    NoInput,
    #[error("writing CSV {path}: {source}")]
    Csv {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Per-file outcome carried back from the parallel scan.
struct FileResult {
    path: PathBuf,
    players: Vec<String>,
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

    if let Err(e) = run(&cli) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

fn run(cli: &Cli) -> Result<(), AppError> {
    let db = Arc::new(PlayerDb::load(&cli.config)?);
    eprintln!(
        "loaded {} player signatures from {}",
        db.players.len(),
        cli.config.display()
    );

    let files = collect_files(cli);
    if files.is_empty() {
        return Err(AppError::NoInput);
    }
    eprintln!("scanning {} .sid files…", files.len());

    let multi = cli.multi;
    let db_ref = Arc::clone(&db);
    let results: Vec<FileResult> = files
        .par_iter()
        .map(|path| {
            let players = match std::fs::read(path) {
                Ok(data) => {
                    if multi {
                        db_ref
                            .identify_all(&data)
                            .into_iter()
                            .map(str::to_string)
                            .collect()
                    } else {
                        db_ref
                            .identify(&data)
                            .map(str::to_string)
                            .into_iter()
                            .collect()
                    }
                }
                Err(e) => {
                    eprintln!("warning: reading {}: {e}", path.display());
                    Vec::new()
                }
            };
            FileResult {
                path: path.clone(),
                players,
            }
        })
        .collect();

    report(cli, &results)?;
    Ok(())
}

fn collect_files(cli: &Cli) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(root) = &cli.root {
        roots.push(root.clone());
    }
    roots.extend(cli.paths.iter().cloned());

    let mut files = Vec::new();
    for root in &roots {
        if root.is_file() {
            if is_sid(root) {
                files.push(root.clone());
            }
            continue;
        }
        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            if entry.file_type().is_file() && is_sid(entry.path()) {
                files.push(entry.into_path());
            }
        }
    }
    files.sort_unstable();
    if let Some(limit) = cli.limit {
        files.truncate(limit);
    }
    files
}

fn is_sid(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|e| e.eq_ignore_ascii_case("sid"))
}

fn report(cli: &Cli, results: &[FileResult]) -> Result<(), AppError> {
    let mut tally: HashMap<&str, usize> = HashMap::new();
    let mut identified = 0usize;

    for r in results {
        if r.players.is_empty() {
            if !cli.quiet && cli.show_unidentified {
                println!("{:<56} *Unidentified*", r.path.display());
            }
        } else {
            identified += 1;
            for p in &r.players {
                *tally.entry(p.as_str()).or_default() += 1;
            }
            if !cli.quiet {
                println!("{:<56} {}", r.path.display(), r.players.join(", "));
            }
        }
    }

    if let Some(csv_path) = &cli.csv {
        write_csv(csv_path, results)?;
    }

    let mut ranked: Vec<(&str, usize)> = tally.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    eprintln!("\nDetected players:");
    for (name, count) in &ranked {
        eprintln!("{name:<28} {count}");
    }
    let total = results.len();
    eprintln!("\nStatistics:");
    eprintln!("Identified               {identified}");
    eprintln!("Unidentified             {}", total - identified);
    eprintln!("Total files examined     {total}");
    Ok(())
}

fn write_csv(path: &Path, results: &[FileResult]) -> Result<(), AppError> {
    use std::io::Write;
    let mut out = String::from("path,player\n");
    for r in results {
        let player = if r.players.is_empty() {
            "".to_string()
        } else {
            r.players.join("|")
        };
        let p = r.path.display().to_string().replace('"', "\"\"");
        out.push_str(&format!("\"{p}\",\"{player}\"\n"));
    }
    let mut f = std::fs::File::create(path).map_err(|source| AppError::Csv {
        path: path.display().to_string(),
        source,
    })?;
    f.write_all(out.as_bytes())
        .map_err(|source| AppError::Csv {
            path: path.display().to_string(),
            source,
        })?;
    Ok(())
}
