//! sid-re — the driver reverse-engineering toolkit.
//!
//! Replaces the throwaway helpers the RE work kept rebuilding (the
//! `/tmp/dis6502.py` disassembler, one-off RAM-dump `#[ignore]` tests, ad-hoc
//! frame-by-frame cell watchers, hardcoded HVSC signature sweeps, and the
//! `bandbal.py` WAV band analyzer) with one permanent binary:
//!
//! - `dis` — disassemble a range of the post-`init` RAM image (or a raw
//!   64 KiB dump), annotating SID register operands and any driver data
//!   cells the native locators recognize.
//! - `dump` — write the post-`init` RAM image to a file, or hexdump a range.
//! - `watch` — run `play` frame by frame and print watched RAM cells (and
//!   optionally the frame's SID writes) whenever they change.
//! - `scan` — count a byte signature (with `??` wildcards) across a tree of
//!   .sid files; the family-size measure to run *before* an RE investment.
//! - `cluster` — group a native-extraction failure class by relocation-neutral
//!   post-init player-code fingerprints.
//! - `irq-scan` — classify post-init IRQ/NMI vectors in PSID files whose play
//!   address is zero.
//! - `wavebands` — band-energy profile of a WAV (e.g. a sidplayfp render),
//!   with the same default band edges as Pertylizer's `analyze_mix_bus`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use walkdir::WalkDir;

use sid_analyzer::audio::{FrequencyHz, band_powers, parse_wav};
use sid_analyzer::emu::{self, Emulator, dis, probe};
use sid_analyzer::export::native::layout_labels;
use sid_analyzer::header::{self, Format, Header, PlayAddress, SubtuneIndex};
use sid_analyzer::trace::{FrameIndex, SidRegister};

#[derive(Parser, Debug)]
#[command(
    name = "sid-re",
    about = "Driver reverse-engineering toolkit: disassemble, dump, watch, scan, IRQ audit, \
             clustering, taint, probe, and wavebands."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Disassemble a range of the post-init RAM image (.sid) or a raw 64 KiB dump (.bin).
    Dis {
        /// A .sid file (emulated through init) or a raw 65536-byte RAM image.
        file: PathBuf,
        /// Address range to disassemble, hex, half-open: LO:HI (e.g. 95E8:9700).
        #[arg(long)]
        range: String,
        /// Subtune to init (1-based). Defaults to the header's start song.
        #[arg(long)]
        song: Option<u16>,
        /// Skip the driver-layout label pass (faster, plain disassembly).
        #[arg(long)]
        no_labels: bool,
    },
    /// Write the post-init RAM image to a file, or hexdump a range of it.
    Dump {
        /// A .sid file (emulated through init) or a raw 65536-byte RAM image.
        file: PathBuf,
        /// Subtune to init (1-based). Defaults to the header's start song.
        #[arg(long)]
        song: Option<u16>,
        /// Write the full 64 KiB image here instead of hexdumping.
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Address range to hexdump, hex, half-open: LO:HI.
        #[arg(long)]
        range: Option<String>,
    },
    /// Run play frame-by-frame and print watched RAM cells when they change.
    Watch {
        /// A .sid file.
        file: PathBuf,
        /// Cells to watch: comma-separated hex addresses or LO-HI ranges
        /// (inclusive), e.g. "E50D,E571-E573".
        #[arg(long)]
        cells: String,
        /// Number of play frames to run.
        #[arg(long, default_value_t = 400)]
        frames: u32,
        /// Subtune to init (1-based). Defaults to the header's start song.
        #[arg(long)]
        song: Option<u16>,
        /// Also print each shown frame's SID register writes.
        #[arg(long)]
        sid_writes: bool,
        /// Print every frame, not only frames where a watched cell changed.
        #[arg(long)]
        all: bool,
    },
    /// Count a byte signature across .sid files (family-size measurement).
    Scan {
        /// Signature bytes in hex, space-separated, `??` = wildcard,
        /// e.g. "29 7F AA" or "C9 FE ?? 29 7F".
        #[arg(long)]
        pattern: String,
        /// Directory tree to scan recursively for .sid files.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Files or directories to scan (alternative/addition to --root).
        paths: Vec<PathBuf>,
        /// Only list files with at least this many occurrences.
        #[arg(long, default_value_t = 1)]
        min_hits: usize,
        /// Worker threads. Defaults to available parallelism.
        #[arg(long)]
        workers: Option<usize>,
    },
    /// Cluster post-init player code by relocation-neutral signatures.
    Cluster {
        /// Composer-census JSON whose matching native failure rows select files.
        #[arg(long)]
        census: Option<PathBuf>,
        /// Corpus root used to resolve census paths, or scanned without --census.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Native census class to select.
        #[arg(long, default_value = "locate_failed")]
        native_class: String,
        /// Files or directories to cluster without --census.
        paths: Vec<PathBuf>,
        /// Maximum reachable instructions retained per post-init image.
        #[arg(long, default_value_t = 2048)]
        instruction_budget: usize,
        /// Minimum pairwise Jaccard similarity joined into one cluster.
        #[arg(long, default_value_t = 0.70)]
        threshold: f64,
        /// Worker threads. Defaults to available parallelism.
        #[arg(long)]
        workers: Option<usize>,
    },
    /// Audit post-init interrupt vectors in PSID files whose play address is zero.
    IrqScan {
        /// Directory tree to scan recursively for .sid files.
        #[arg(long)]
        root: PathBuf,
        /// Initialize every subtune instead of only the header's start song.
        #[arg(long)]
        all_subtunes: bool,
        /// Also execute this many frames through the resolved IRQ handler.
        #[arg(long, default_value_t = 0)]
        frames: u32,
        /// Worker threads. Defaults to available parallelism.
        #[arg(long)]
        workers: Option<usize>,
    },
    /// Band-energy profile of a 16-bit PCM WAV (e.g. `sidplayfp -w` output).
    Wavebands {
        file: PathBuf,
        /// Band edges in Hz, comma-separated, ascending.
        #[arg(long, default_value = "100,500,2000")]
        edges: String,
    },
    /// Dynamic data-flow tracker: run play and report the origin tables
    /// feeding the SID registers, plus the sequence stream pointers.
    Taint {
        /// A .sid file.
        file: PathBuf,
        /// Number of play frames to run. Freq-table recovery needs enough
        /// frames to reach the first note-on past the tune's intro (a table
        /// is only re-sourced into the per-voice playing-frequency cell at a
        /// note-on); the default clears almost every intro.
        #[arg(long, default_value_t = 1500)]
        frames: u32,
        /// Subtune to init (1-based). Defaults to the header's start song.
        #[arg(long)]
        song: Option<u16>,
    },
    /// Differential semantics probe: mutate one stream byte at a time,
    /// replay, and diff the SID trace — classifies each byte's role (note /
    /// duration / ctrl / effect / structural) with no driver knowledge.
    Probe {
        /// A .sid file.
        file: PathBuf,
        /// Number of play frames per replay (and for the target-discovery
        /// taint run).
        #[arg(long, default_value_t = 1500)]
        frames: u32,
        /// How many stream-byte addresses to probe (taken from the head of
        /// the taint fetch log) when no --addr is given.
        #[arg(long, default_value_t = 32)]
        targets: usize,
        /// Explicit cells to probe instead of auto-discovery: comma-separated
        /// hex addresses or LO-HI ranges (inclusive), e.g. "1A07,1A10-1A14".
        #[arg(long)]
        addr: Option<String>,
        /// Subtune to init (1-based). Defaults to the header's start song.
        #[arg(long)]
        song: Option<u16>,
    },
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing SID header: {0}")]
    Header(#[from] header::Error),
    #[error("parsing census {path}: {source}")]
    Census {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("emulating: {0}")]
    Emu(#[from] emu::EmuError),
    #[error("bad {what} {input:?}: expected {expected}")]
    Parse {
        what: &'static str,
        input: String,
        expected: &'static str,
    },
    #[error("{0}")]
    Invalid(String),
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(&cli.cmd) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

fn run(cmd: &Cmd) -> Result<(), AppError> {
    match cmd {
        Cmd::Dis {
            file,
            range,
            song,
            no_labels,
        } => cmd_dis(file, range, *song, *no_labels),
        Cmd::Dump {
            file,
            song,
            out,
            range,
        } => cmd_dump(file, *song, out.as_deref(), range.as_deref()),
        Cmd::Watch {
            file,
            cells,
            frames,
            song,
            sid_writes,
            all,
        } => cmd_watch(file, cells, *frames, *song, *sid_writes, *all),
        Cmd::Scan {
            pattern,
            root,
            paths,
            min_hits,
            workers,
        } => cmd_scan(pattern, root.as_deref(), paths, *min_hits, *workers),
        Cmd::Cluster {
            census,
            root,
            native_class,
            paths,
            instruction_budget,
            threshold,
            workers,
        } => cmd_cluster(ClusterOptions {
            census: census.as_deref(),
            root: root.as_deref(),
            native_class,
            paths,
            instruction_budget: InstructionBudget(*instruction_budget),
            threshold: CodeSimilarity(*threshold),
            workers: *workers,
        }),
        Cmd::IrqScan {
            root,
            all_subtunes,
            frames,
            workers,
        } => cmd_irq_scan(root, *all_subtunes, *frames, *workers),
        Cmd::Wavebands { file, edges } => cmd_wavebands(file, edges),
        Cmd::Taint { file, frames, song } => cmd_taint(file, *frames, *song),
        Cmd::Probe {
            file,
            frames,
            targets,
            addr,
            song,
        } => cmd_probe(file, *frames, *targets, addr.as_deref(), *song),
    }
}

// ---------------------------------------------------------------- RAM images

fn read_file(path: &Path) -> Result<Vec<u8>, AppError> {
    std::fs::read(path).map_err(|source| AppError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Load `file` as a 64 KiB RAM image: a raw 65536-byte dump is used as-is, a
/// .sid file is loaded and run through `init` for the chosen subtune.
fn ram_image(file: &Path, song: Option<u16>) -> Result<Vec<u8>, AppError> {
    let bytes = read_file(file)?;
    if bytes.len() == 0x10000 && !bytes.starts_with(b"PSID") && !bytes.starts_with(b"RSID") {
        return Ok(bytes);
    }
    let (emu, _) = init_emulator(&bytes, song)?;
    Ok(emu.ram_image())
}

/// Parse the SID header, load the data, and run `init`.
fn init_emulator(bytes: &[u8], song: Option<u16>) -> Result<(Emulator, Header), AppError> {
    let header = header::parse(bytes)?;
    let subtune = song.map_or(header.start_song, SubtuneIndex);
    let mut emu = Emulator::new();
    emu.load(&header, bytes)?;
    emu.call_init(header.init_address, subtune, header.songs)?;
    Ok((emu, header))
}

/// Parse a half-open hex range "LO:HI".
fn parse_range(input: &str) -> Result<(u16, u16), AppError> {
    let err = || AppError::Parse {
        what: "range",
        input: input.to_string(),
        expected: "hex LO:HI, e.g. 95E8:9700",
    };
    let (lo, hi) = input.split_once(':').ok_or_else(err)?;
    let lo = u16::from_str_radix(lo.trim_start_matches('$'), 16).map_err(|_| err())?;
    let hi = u16::from_str_radix(hi.trim_start_matches('$'), 16).map_err(|_| err())?;
    if lo >= hi {
        return Err(err());
    }
    Ok((lo, hi))
}

// ------------------------------------------------------------------ sid-re dis

/// Human label for a SID register address, e.g. `v1.freq_hi`, `filt.mode_vol`.
fn sid_reg_label(addr: u16) -> Option<&'static str> {
    let off = addr.checked_sub(0xD400)?;
    u8::try_from(off).ok().and_then(|o| SidRegister(o).label())
}

fn cmd_dis(file: &Path, range: &str, song: Option<u16>, no_labels: bool) -> Result<(), AppError> {
    let (lo, hi) = parse_range(range)?;
    let ram = ram_image(file, song)?;

    let mut labels: HashMap<u16, Vec<String>> = HashMap::new();
    if !no_labels {
        for (addr, name) in layout_labels(&ram) {
            labels.entry(addr).or_default().push(name);
        }
    }

    for insn in dis::disassemble(&ram, lo, hi) {
        let mut notes: Vec<String> = Vec::new();
        if let Some(names) = labels.get(&insn.addr) {
            for n in names {
                notes.push(format!("<- {n}"));
            }
        }
        if let Some(target) = insn.target {
            if let Some(reg) = sid_reg_label(target) {
                notes.push(reg.to_string());
            }
            if let Some(names) = labels.get(&target) {
                notes.extend(names.iter().cloned());
            }
        }
        if notes.is_empty() {
            println!("{insn}");
        } else {
            println!("{:<32} ; {}", insn.to_string(), notes.join(", "));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- sid-re dump

fn cmd_dump(
    file: &Path,
    song: Option<u16>,
    out: Option<&Path>,
    range: Option<&str>,
) -> Result<(), AppError> {
    let ram = ram_image(file, song)?;
    if let Some(out) = out {
        let slice = match range {
            Some(r) => {
                let (lo, hi) = parse_range(r)?;
                &ram[lo as usize..hi as usize]
            }
            None => &ram[..],
        };
        std::fs::write(out, slice).map_err(|source| AppError::Io {
            path: out.display().to_string(),
            source,
        })?;
        eprintln!("wrote {} bytes to {}", slice.len(), out.display());
        return Ok(());
    }
    let (lo, hi) = parse_range(range.ok_or_else(|| {
        AppError::Invalid("hexdump needs --range LO:HI (or use --out for the full image)".into())
    })?)?;
    for row in (lo..hi).step_by(16) {
        let row_end = hi.min(row.saturating_add(16));
        let bytes = &ram[row as usize..row_end as usize];
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
        let ascii: String = bytes
            .iter()
            .map(|&b| {
                if (0x20..0x7F).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("${row:04X}  {:<47}  |{ascii}|", hex.join(" "));
    }
    Ok(())
}

// ---------------------------------------------------------------- sid-re watch

/// Parse "E50D,E571-E573" into a cell list (each range inclusive).
fn parse_cells(input: &str) -> Result<Vec<u16>, AppError> {
    let err = |part: &str| AppError::Parse {
        what: "cells",
        input: part.to_string(),
        expected: "comma-separated hex addresses or LO-HI ranges, e.g. E50D,E571-E573",
    };
    let mut cells = Vec::new();
    for part in input.split(',') {
        let part = part.trim();
        let addr =
            |s: &str| u16::from_str_radix(s.trim_start_matches('$'), 16).map_err(|_| err(part));
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (addr(a)?, addr(b)?),
            None => {
                let a = addr(part)?;
                (a, a)
            }
        };
        if lo > hi {
            return Err(err(part));
        }
        cells.extend(lo..=hi);
    }
    if cells.is_empty() || cells.len() > 64 {
        return Err(AppError::Invalid(format!(
            "{} cells given; expected 1..=64",
            cells.len()
        )));
    }
    Ok(cells)
}

fn cmd_watch(
    file: &Path,
    cells: &str,
    frames: u32,
    song: Option<u16>,
    sid_writes: bool,
    all: bool,
) -> Result<(), AppError> {
    let cells = parse_cells(cells)?;
    let bytes = read_file(file)?;
    let (mut emu, header) = init_emulator(&bytes, song)?;

    let header_row: Vec<String> = cells.iter().map(|c| format!("${c:04X}")).collect();
    println!("frame  {}", header_row.join("  "));

    let mut prev: Option<Vec<u8>> = None;
    for n in 0..frames {
        let frame = emu.run_play_frame(header.play_address, FrameIndex(n))?;
        let now: Vec<u8> = cells.iter().map(|&c| emu.read_ram(c)).collect();
        let changed = prev.as_ref() != Some(&now);
        if all || changed {
            let row: Vec<String> = now
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let mark = if prev.as_ref().is_some_and(|p| p[i] != *v) {
                        '*'
                    } else {
                        ' '
                    };
                    format!("   {v:02X}{mark}")
                })
                .collect();
            println!("{n:>5} {}", row.join(" "));
            if sid_writes {
                for w in &frame.writes {
                    println!("       {w}");
                }
            }
        }
        prev = Some(now);
    }
    Ok(())
}

// ---------------------------------------------------------------- sid-re taint

fn cmd_taint(file: &Path, frames: u32, song: Option<u16>) -> Result<(), AppError> {
    let bytes = read_file(file)?;
    let (mut emu, header) = init_emulator(&bytes, song)?;
    let report = emu.run_taint(header.play_address, frames);

    println!("=== taint: {} ({frames} frames) ===\n", file.display());

    println!("SID register sinks (origin table base feeding each register):");
    if report.sinks.is_empty() {
        println!("  (none — no tracked write reached a SID register)");
    }
    for sink in &report.sinks {
        let label = sid_reg_label(0xD400 + u16::from(sink.reg)).unwrap_or("?");
        print!("  ${:04X} {label:<12}", 0xD400 + u16::from(sink.reg));
        let top: Vec<String> = sink
            .sources
            .iter()
            .take(3)
            .map(|(base, s)| {
                let tags = match (s.indexed, s.transformed) {
                    (true, true) => " [tbl,xf]",
                    (true, false) => " [tbl]",
                    (false, true) => " [xf]",
                    (false, false) => "",
                };
                format!("${base:04X}×{}{tags}", s.writes)
            })
            .collect();
        let via = sink
            .index_via
            .map(|p| format!("  ←idx via ${p:02X}"))
            .unwrap_or_default();
        if sink.const_writes > 0 {
            println!(" {}  (+{} const){via}", top.join("  "), sink.const_writes);
        } else {
            println!(" {}{via}", top.join("  "));
        }
    }

    println!("\nArithmetic combine sources (cells/tables folded into each register's value — B6):");
    let mut any_combine = false;
    for sink in &report.sinks {
        if sink.combine_srcs.is_empty() {
            continue;
        }
        any_combine = true;
        let label = sid_reg_label(0xD400 + u16::from(sink.reg)).unwrap_or("?");
        let srcs: Vec<String> = sink
            .combine_srcs
            .iter()
            .take(6)
            .map(|(a, n)| format!("${a:04X}×{n}"))
            .collect();
        println!(
            "  ${:04X} {label:<12} += {}",
            0xD400 + u16::from(sink.reg),
            srcs.join("  ")
        );
    }
    if !any_combine {
        println!("  (none — no SID value passed through memory-operand arithmetic)");
    }

    println!("\nStream pointers (zero-page cells read via indirect addressing):");
    if report.stream_ptrs.is_empty() {
        println!("  (none)");
    }
    for (zp, reads) in report.stream_ptrs.iter().take(12) {
        println!(
            "  ${zp:02X}/{:02X}  {reads} reads",
            (*zp as u8).wrapping_add(1)
        );
    }

    println!("\nStream-byte grammar tests (immediates compared/masked on sequence bytes):");
    if report.cmp_tests.is_empty() {
        println!("  (none)");
    }
    for ((op, imm), count) in report.cmp_tests.iter().take(16) {
        println!("  {op} #${imm:02X}   ×{count}");
    }

    println!("\nSong structure (per stream pointer: distinct patterns / orderlist visits):");
    if report.structure.is_empty() {
        println!("  (none)");
    }
    for s in report.structure.iter().take(6) {
        // Reused starts (visited >= 2) are the real patterns; one-off starts
        // are mostly a shared cursor jumping around (engines that multiplex
        // one stream pointer across voices look noisier than per-voice ones).
        let reused = s.starts.iter().filter(|(_, n)| *n >= 2).count();
        let top: Vec<String> = s
            .starts
            .iter()
            .take(6)
            .map(|(a, n)| format!("${a:04X}×{n}"))
            .collect();
        println!(
            "  ${:02X}/{:02X}  {} starts ({reused} reused), {} visits  {}",
            s.zp,
            (s.zp as u8).wrapping_add(1),
            s.patterns,
            s.segments,
            top.join(" ")
        );
    }

    println!("\nSelf-modified code cells (live-patched immediates, addr ×patches):");
    if report.self_mod.is_empty() {
        println!("  (none)");
    }
    for (addr, patches) in report.self_mod.iter().take(12) {
        println!("  ${addr:04X} ×{patches}");
    }

    println!(
        "\nTable-row vocabulary (distinct indices feeding each register — instrument ids / notes):"
    );
    let mut any_vocab = false;
    for sink in &report.sinks {
        if sink.index_vals.is_empty() {
            continue;
        }
        any_vocab = true;
        let label = sid_reg_label(0xD400 + u16::from(sink.reg)).unwrap_or("?");
        let vals: Vec<String> = sink
            .index_vals
            .iter()
            .take(10)
            .map(|(v, _)| format!("{v}"))
            .collect();
        let more = if sink.index_vals.len() > 10 {
            ", …"
        } else {
            ""
        };
        println!(
            "  ${:04X} {label:<12} {} rows: {}{more}",
            0xD400 + u16::from(sink.reg),
            sink.index_vals.len(),
            vals.join(",")
        );
    }
    if !any_vocab {
        println!("  (none)");
    }

    Ok(())
}

// ---------------------------------------------------------------- sid-re probe

fn cmd_probe(
    file: &Path,
    frames: u32,
    targets: usize,
    addr: Option<&str>,
    song: Option<u16>,
) -> Result<(), AppError> {
    let bytes = read_file(file)?;
    let (mut emu, header) = init_emulator(&bytes, song)?;
    let subtune = song.map_or(header.start_song, SubtuneIndex);
    let image = emu.ram_image();

    // `orig` always comes from the post-init image: that is the byte the
    // probe's replay actually mutates. The fetch log's recorded value is
    // read mid-play and can differ for self-modified stream state.
    let target = |a: u16| probe::ProbeTarget {
        addr: a,
        orig: image[usize::from(a)],
    };
    let specs: Vec<probe::ProbeTarget> = if let Some(list) = addr {
        parse_cells(list)?.into_iter().map(target).collect()
    } else {
        // Auto-discovery: the head of the taint fetch log — the first bytes
        // the player reads through its stream pointers — covers every byte
        // role (orderlist, note, duration, command, operand) of the song's
        // opening patterns.
        let report = emu.run_taint(header.play_address, frames);
        report
            .fetches
            .iter()
            .take(targets)
            .map(|&(_, a, _)| target(a))
            .collect()
    };
    if specs.is_empty() {
        return Err(AppError::Invalid(
            "no probe targets: the tune reads no stream bytes (or none were given)".into(),
        ));
    }

    println!(
        "=== probe: {} ({frames} frames, {} targets) ===\n",
        file.display(),
        specs.len()
    );
    let results = probe::probe(&header, &bytes, subtune, frames, &specs)?;

    println!("  addr   byte        verdict");
    for r in &results {
        let mutation = format!("${:02X}→${:02X}", r.orig, r.mutated);
        println!(
            "  ${:04X}  {mutation:<11} {}",
            r.addr,
            verdict_line(&r.verdict)
        );
    }

    let count =
        |pred: fn(&probe::Verdict) -> bool| results.iter().filter(|r| pred(&r.verdict)).count();
    println!(
        "\n  {} probed: {} param, {} timing, {} structural, {} crash, {} no-effect",
        results.len(),
        count(|v| matches!(v, probe::Verdict::Param { .. })),
        count(|v| matches!(v, probe::Verdict::Timing { .. })),
        count(|v| matches!(v, probe::Verdict::Structural { .. })),
        count(|v| matches!(v, probe::Verdict::Crash)),
        count(|v| matches!(v, probe::Verdict::NoEffect)),
    );
    Ok(())
}

/// Render a probe voice bitmask (bit 0 = voice 1) for the verdict line.
fn voices_label(voices: u8) -> String {
    match voices {
        0 => "global".to_string(),
        0b111 => "all".to_string(),
        _ => (0..3)
            .filter(|v| voices & (1 << v) != 0)
            .map(|v| format!("v{}", v + 1))
            .collect::<Vec<_>>()
            .join("+"),
    }
}

fn verdict_line(v: &probe::Verdict) -> String {
    match v {
        probe::Verdict::NoEffect => "no effect".into(),
        probe::Verdict::Crash => "CRASH (control-flow-bearing byte)".into(),
        probe::Verdict::Timing {
            shift,
            voices,
            first_frame,
        } => format!(
            "timing {shift:+} frames [{}] @f{first_frame}",
            voices_label(*voices)
        ),
        probe::Verdict::Param {
            categories,
            voices,
            first_frame,
        } => {
            let cats: Vec<String> = categories.iter().map(|c| format!("{c:?}")).collect();
            format!(
                "param {} [{}] @f{first_frame}",
                cats.join("+"),
                voices_label(*voices)
            )
        }
        probe::Verdict::Structural { first_frame } => {
            format!("STRUCTURAL @f{first_frame}")
        }
    }
}

// -------------------------------------------------------------- sid-re cluster

const CODE_SHINGLE_WIDTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CodeToken(u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CodeShingle([CodeToken; CODE_SHINGLE_WIDTH]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectedSubtunes(usize);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
struct CodeSimilarity(f64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InstructionBudget(usize);

#[derive(Debug)]
struct DriverFingerprint {
    path: PathBuf,
    selected_subtunes: SelectedSubtunes,
    entry: PlayAddress,
    instruction_count: InstructionBudget,
    shingles: HashSet<CodeShingle>,
}

struct ClusterOptions<'a> {
    census: Option<&'a Path>,
    root: Option<&'a Path>,
    native_class: &'a str,
    paths: &'a [PathBuf],
    instruction_budget: InstructionBudget,
    threshold: CodeSimilarity,
    workers: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
struct CensusSelection {
    results: Vec<CensusSelectionRow>,
}

#[derive(Debug, serde::Deserialize)]
struct CensusSelectionRow {
    path: PathBuf,
    #[serde(default)]
    native: CensusNativeResult,
}

#[derive(Debug, Default, serde::Deserialize)]
struct CensusNativeResult {
    class: Option<String>,
}

fn normalized_code_token(instruction: dis::Insn) -> CodeToken {
    let qualifier = match instruction.mode {
        dis::Mode::Immediate => instruction.bytes[1],
        dis::Mode::Absolute | dis::Mode::AbsoluteX | dis::Mode::AbsoluteY => instruction
            .target
            .and_then(|target| target.checked_sub(0xD400))
            .filter(|offset| *offset <= 0x1C)
            .and_then(|offset| u8::try_from(offset).ok())
            .map_or(0, |offset| 0x80 | offset),
        _ => 0,
    };
    CodeToken(u16::from_be_bytes([instruction.bytes[0], qualifier]))
}

fn reachable_code_fingerprint(
    ram: &[u8],
    entry: PlayAddress,
    instruction_budget: InstructionBudget,
) -> (InstructionBudget, HashSet<CodeShingle>) {
    let mut queue = VecDeque::from([entry.0]);
    let mut instructions = BTreeMap::new();
    while let Some(address) = queue.pop_front() {
        if instructions.len() >= instruction_budget.0 || instructions.contains_key(&address) {
            continue;
        }
        let instruction = dis::decode(ram, address);
        if instruction.mode == dis::Mode::Unknown {
            continue;
        }
        let next = address.wrapping_add(instruction.size());
        instructions.insert(address, instruction);
        match (instruction.mnemonic, instruction.mode) {
            ("BRK" | "RTI" | "RTS", _) | ("JMP", dis::Mode::Indirect) => {}
            ("JMP", _) => {
                if let Some(target) = instruction.target {
                    queue.push_back(target);
                }
            }
            ("JSR", _) => {
                queue.push_back(next);
                if let Some(target) = instruction.target {
                    queue.push_back(target);
                }
            }
            (_, dis::Mode::Relative) => {
                queue.push_back(next);
                if let Some(target) = instruction.target {
                    queue.push_back(target);
                }
            }
            _ => queue.push_back(next),
        }
    }

    let tokens: Vec<_> = instructions
        .values()
        .copied()
        .map(normalized_code_token)
        .collect();
    let shingles = tokens
        .windows(CODE_SHINGLE_WIDTH)
        .filter_map(|window| window.try_into().ok().map(CodeShingle))
        .collect();
    (InstructionBudget(instructions.len()), shingles)
}

fn post_init_play_entry(emulator: &Emulator, header: &Header) -> Result<PlayAddress, String> {
    if header.play_address.0 != 0 {
        return Ok(header.play_address);
    }
    let read_word = |address: u16| {
        PlayAddress(u16::from_le_bytes([
            emulator.read_ram(address),
            emulator.read_ram(address.wrapping_add(1)),
        ]))
    };
    let kernal_irq = read_word(0x0314);
    if kernal_irq.0 != 0 && kernal_irq != PlayAddress(0xEA31) {
        return Ok(kernal_irq);
    }
    let hardware_irq = read_word(0xFFFE);
    if hardware_irq.0 != 0 {
        return Ok(hardware_irq);
    }
    Err("post-init image has no supported play or IRQ entry".into())
}

fn code_similarity(left: &HashSet<CodeShingle>, right: &HashSet<CodeShingle>) -> CodeSimilarity {
    let intersection = left.intersection(right).count();
    let union = left.len() + right.len() - intersection;
    CodeSimilarity(if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    })
}

fn complete_link_groups(similarities: &[Vec<f64>], threshold: CodeSimilarity) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = (0..similarities.len()).map(|index| vec![index]).collect();
    loop {
        let mut best = None;
        for left in 0..groups.len() {
            for right in left + 1..groups.len() {
                let minimum = groups[left]
                    .iter()
                    .flat_map(|left_index| {
                        groups[right]
                            .iter()
                            .map(|right_index| similarities[*left_index][*right_index])
                    })
                    .fold(f64::INFINITY, f64::min);
                if minimum >= threshold.0
                    && best.is_none_or(|(best_similarity, _, _)| minimum > best_similarity)
                {
                    best = Some((minimum, left, right));
                }
            }
        }
        let Some((_, left, right)) = best else {
            break;
        };
        let merged = groups.remove(right);
        groups[left].extend(merged);
        groups[left].sort_unstable();
    }
    groups
}

fn census_inputs(
    census: &Path,
    corpus_root: Option<&Path>,
    native_class: &str,
) -> Result<Vec<(PathBuf, SelectedSubtunes)>, AppError> {
    let bytes = read_file(census)?;
    let report: CensusSelection =
        serde_json::from_slice(&bytes).map_err(|source| AppError::Census {
            path: census.display().to_string(),
            source,
        })?;
    let mut counts = BTreeMap::new();
    for row in report.results {
        if row.native.class.as_deref() != Some(native_class) {
            continue;
        }
        let path = if row.path.is_absolute() {
            row.path
        } else {
            corpus_root.unwrap_or_else(|| Path::new(".")).join(row.path)
        };
        *counts.entry(path).or_insert(0) += 1;
    }
    Ok(counts
        .into_iter()
        .map(|(path, count)| (path, SelectedSubtunes(count)))
        .collect())
}

fn fingerprint_file(
    path: &Path,
    selected_subtunes: SelectedSubtunes,
    instruction_budget: InstructionBudget,
) -> Result<DriverFingerprint, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read: {error}"))?;
    let (emulator, header) = init_emulator(&bytes, None).map_err(|error| error.to_string())?;
    let entry = post_init_play_entry(&emulator, &header)?;
    let (instruction_count, shingles) =
        reachable_code_fingerprint(&emulator.ram_image(), entry, instruction_budget);
    if shingles.is_empty() {
        return Err(format!(
            "only {} reachable instructions from {entry}",
            instruction_count.0
        ));
    }
    let selected_subtunes = if selected_subtunes.0 == 0 {
        SelectedSubtunes(usize::from(header.songs.0))
    } else {
        selected_subtunes
    };
    Ok(DriverFingerprint {
        path: path.to_path_buf(),
        selected_subtunes,
        entry,
        instruction_count,
        shingles,
    })
}

fn cmd_cluster(options: ClusterOptions<'_>) -> Result<(), AppError> {
    let ClusterOptions {
        census,
        root,
        native_class,
        paths,
        instruction_budget,
        threshold,
        workers,
    } = options;
    if instruction_budget.0 < CODE_SHINGLE_WIDTH || !(0.0..=1.0).contains(&threshold.0) {
        return Err(AppError::Invalid(
            "cluster needs --instruction-budget >= 4 and --threshold between 0 and 1".into(),
        ));
    }
    if census.is_some() && !paths.is_empty() {
        return Err(AppError::Invalid(
            "positional paths cannot be combined with --census".into(),
        ));
    }
    if let Some(worker_count) = workers
        && let Err(error) = rayon::ThreadPoolBuilder::new()
            .num_threads(worker_count)
            .build_global()
    {
        eprintln!("warning: could not configure thread pool: {error}");
    }

    let inputs = if let Some(census) = census {
        census_inputs(census, root, native_class)?
    } else {
        let mut roots: Vec<PathBuf> = root.map(Path::to_path_buf).into_iter().collect();
        roots.extend(paths.iter().cloned());
        sid_files(&roots)
            .into_iter()
            .map(|path| (path, SelectedSubtunes(0)))
            .collect()
    };
    if inputs.is_empty() {
        return Err(AppError::Invalid(format!(
            "no files selected for native class {native_class:?}"
        )));
    }
    if census.is_some() {
        let selected_subtunes: usize = inputs.iter().map(|(_, count)| count.0).sum();
        eprintln!(
            "fingerprinting {} files / {selected_subtunes} selected subtunes…",
            inputs.len()
        );
    } else {
        eprintln!(
            "fingerprinting {} files / all header subtunes…",
            inputs.len()
        );
    }

    let records: Vec<_> = inputs
        .par_iter()
        .map(|(path, count)| {
            (
                path.clone(),
                fingerprint_file(path, *count, instruction_budget),
            )
        })
        .collect();
    let mut fingerprints = Vec::new();
    let mut failures = Vec::new();
    for (path, result) in records {
        match result {
            Ok(fingerprint) => fingerprints.push(fingerprint),
            Err(error) => failures.push((path, error)),
        }
    }
    fingerprints.sort_by(|left, right| left.path.cmp(&right.path));
    failures.sort_by(|left, right| left.0.cmp(&right.0));
    if fingerprints.is_empty() {
        return Err(AppError::Invalid(
            "no post-init fingerprints could be built".into(),
        ));
    }

    let mut similarity_matrix = vec![vec![0.0; fingerprints.len()]; fingerprints.len()];
    for left in 0..fingerprints.len() {
        similarity_matrix[left][left] = 1.0;
        for right in left + 1..fingerprints.len() {
            let similarity =
                code_similarity(&fingerprints[left].shingles, &fingerprints[right].shingles).0;
            similarity_matrix[left][right] = similarity;
            similarity_matrix[right][left] = similarity;
        }
    }
    let mut groups = complete_link_groups(&similarity_matrix, threshold);
    groups.sort_by(|left, right| {
        let subtunes = |group: &[usize]| {
            group
                .iter()
                .map(|index| fingerprints[*index].selected_subtunes.0)
                .sum::<usize>()
        };
        subtunes(right)
            .cmp(&subtunes(left))
            .then_with(|| right.len().cmp(&left.len()))
            .then_with(|| fingerprints[left[0]].path.cmp(&fingerprints[right[0]].path))
    });

    for (group_index, group) in groups.iter().enumerate() {
        let subtunes: usize = group
            .iter()
            .map(|index| fingerprints[*index].selected_subtunes.0)
            .sum();
        let mut similarities = Vec::new();
        for left in 0..group.len() {
            for right in left + 1..group.len() {
                similarities.push(
                    code_similarity(
                        &fingerprints[group[left]].shingles,
                        &fingerprints[group[right]].shingles,
                    )
                    .0,
                );
            }
        }
        let range = if similarities.is_empty() {
            "singleton".to_string()
        } else {
            let minimum = similarities.iter().copied().fold(f64::INFINITY, f64::min);
            let maximum = similarities.iter().copied().fold(0.0, f64::max);
            format!("pair similarity {minimum:.3}..{maximum:.3}")
        };
        println!(
            "cluster {}: {} files / {subtunes} subtunes / {range}",
            group_index + 1,
            group.len()
        );
        for index in group {
            let fingerprint = &fingerprints[*index];
            println!(
                "  {:>2} subtunes  entry={}  {:>4} insns  {:>4} shingles  {}",
                fingerprint.selected_subtunes.0,
                fingerprint.entry,
                fingerprint.instruction_count.0,
                fingerprint.shingles.len(),
                fingerprint.path.display()
            );
        }
    }
    for (path, error) in &failures {
        println!("unclassified: {error}; {}", path.display());
    }
    eprintln!(
        "{} clusters from {} fingerprints; {} failures; threshold {:.3}",
        groups.len(),
        fingerprints.len(),
        failures.len(),
        threshold.0
    );
    Ok(())
}

// ----------------------------------------------------------------- sid-re scan

/// One signature byte: a concrete value or a `??` wildcard.
type PatternByte = Option<u8>;

fn parse_pattern(input: &str) -> Result<Vec<PatternByte>, AppError> {
    let err = |part: &str| AppError::Parse {
        what: "pattern",
        input: part.to_string(),
        expected: "space-separated hex bytes or ??, e.g. \"29 7F ?? AA\"",
    };
    let pat: Vec<PatternByte> = input
        .split_whitespace()
        .map(|tok| {
            if tok == "??" {
                Ok(None)
            } else {
                u8::from_str_radix(tok, 16).map(Some).map_err(|_| err(tok))
            }
        })
        .collect::<Result<_, _>>()?;
    if pat.is_empty() || pat.iter().all(Option::is_none) {
        return Err(AppError::Invalid(
            "pattern needs at least one concrete byte".into(),
        ));
    }
    Ok(pat)
}

/// Count (possibly overlapping) occurrences of `pat` in `haystack`.
fn count_matches(haystack: &[u8], pat: &[PatternByte]) -> usize {
    if haystack.len() < pat.len() {
        return 0;
    }
    haystack
        .windows(pat.len())
        .filter(|w| w.iter().zip(pat).all(|(&b, p)| p.is_none_or(|p| p == b)))
        .count()
}

fn sid_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        if root.is_file() {
            files.push(root.clone());
            continue;
        }
        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let is_sid = entry
                .path()
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("sid"));
            if entry.file_type().is_file() && is_sid {
                files.push(entry.into_path());
            }
        }
    }
    files.sort_unstable();
    files
}

fn cmd_scan(
    pattern: &str,
    root: Option<&Path>,
    paths: &[PathBuf],
    min_hits: usize,
    workers: Option<usize>,
) -> Result<(), AppError> {
    let pat = parse_pattern(pattern)?;

    if let Some(n) = workers
        && let Err(e) = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
    {
        eprintln!("warning: could not configure thread pool: {e}");
    }

    let mut roots: Vec<PathBuf> = root.map(Path::to_path_buf).into_iter().collect();
    roots.extend(paths.iter().cloned());
    if roots.is_empty() {
        return Err(AppError::Invalid(
            "no input: pass --root or positional paths".into(),
        ));
    }

    let files = sid_files(&roots);
    eprintln!("scanning {} files for [{pattern}]…", files.len());

    let mut hits: Vec<(usize, &PathBuf)> = files
        .par_iter()
        .filter_map(|path| match std::fs::read(path) {
            Ok(data) => {
                let n = count_matches(&data, &pat);
                (n >= min_hits).then_some((n, path))
            }
            Err(e) => {
                eprintln!("warning: reading {}: {e}", path.display());
                None
            }
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

    for (n, path) in &hits {
        println!("{n:>5}  {}", path.display());
    }
    let total_hits: usize = hits.iter().map(|(n, _)| n).sum();
    eprintln!(
        "\n{} of {} files matched (>= {min_hits} hits), {total_hits} occurrences total",
        hits.len(),
        files.len(),
    );
    Ok(())
}

#[derive(Debug)]
struct IrqScanSnapshot {
    vectors: [PlayAddress; 4],
    writes: Option<usize>,
}

#[derive(Debug)]
struct IrqScanRecord {
    path: PathBuf,
    subtune: SubtuneIndex,
    result: Result<IrqScanSnapshot, String>,
}

fn cmd_irq_scan(
    root: &Path,
    all_subtunes: bool,
    frames: u32,
    workers: Option<usize>,
) -> Result<(), AppError> {
    if let Some(worker_count) = workers
        && let Err(error) = rayon::ThreadPoolBuilder::new()
            .num_threads(worker_count)
            .build_global()
    {
        eprintln!("warning: could not configure thread pool: {error}");
    }

    let files = sid_files(&[root.to_path_buf()]);
    eprintln!(
        "scanning {} SID files for PSID play address zero…",
        files.len()
    );
    let mut records: Vec<IrqScanRecord> = files
        .par_iter()
        .flat_map_iter(|path| {
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return vec![IrqScanRecord {
                        path: path.clone(),
                        subtune: SubtuneIndex(0),
                        result: Err(format!("read: {error}")),
                    }];
                }
            };
            let header = match header::parse(&bytes) {
                Ok(header) if header.format == Format::Psid && header.play_address.0 == 0 => header,
                Ok(_) => return Vec::new(),
                Err(error) => {
                    return vec![IrqScanRecord {
                        path: path.clone(),
                        subtune: SubtuneIndex(0),
                        result: Err(format!("header: {error}")),
                    }];
                }
            };
            let subtunes: Vec<SubtuneIndex> = if all_subtunes {
                (1..=header.songs.0).map(SubtuneIndex).collect()
            } else {
                vec![header.start_song]
            };
            subtunes
                .into_iter()
                .map(|subtune| {
                    let result = (|| {
                        let mut emulator = Emulator::new();
                        emulator
                            .load(&header, &bytes)
                            .map_err(|error| error.to_string())?;
                        emulator
                            .call_init(header.init_address, subtune, header.songs)
                            .map_err(|error| error.to_string())?;
                        let word = |address: u16| {
                            PlayAddress(u16::from_le_bytes([
                                emulator.read_ram(address),
                                emulator.read_ram(address.wrapping_add(1)),
                            ]))
                        };
                        let vectors = [word(0x0314), word(0x0318), word(0xFFFE), word(0xFFFA)];
                        let writes = if frames == 0 {
                            None
                        } else {
                            Some(
                                emu::run(&header, &bytes, subtune, frames)
                                    .map_err(|error| error.to_string())?
                                    .total_writes(),
                            )
                        };
                        Ok(IrqScanSnapshot { vectors, writes })
                    })();
                    IrqScanRecord {
                        path: path.clone(),
                        subtune,
                        result,
                    }
                })
                .collect()
        })
        .collect();
    records.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.subtune.0.cmp(&right.subtune.0))
    });

    let mut successful = 0usize;
    let mut failed = 0usize;
    for record in &records {
        match &record.result {
            Ok(snapshot) => {
                successful += 1;
                let [irq, nmi, hardware_irq, hardware_nmi] = snapshot.vectors;
                let writes = snapshot
                    .writes
                    .map_or_else(String::new, |count| format!(" writes={count}"));
                println!(
                    "irq={irq} nmi={nmi} hw_irq={hardware_irq} \
                     hw_nmi={hardware_nmi}{writes} song={} {}",
                    record.subtune,
                    record.path.display()
                );
            }
            Err(error) => {
                failed += 1;
                println!(
                    "error={error:?} song={} {}",
                    record.subtune,
                    record.path.display()
                );
            }
        }
    }
    eprintln!(
        "{} play-zero init results: {successful} vector snapshots, {failed} failures",
        records.len()
    );
    Ok(())
}

// ------------------------------------------------------------ sid-re wavebands

fn cmd_wavebands(file: &Path, edges: &str) -> Result<(), AppError> {
    let edges: Vec<f64> = edges
        .split(',')
        .map(|e| {
            e.trim().parse::<f64>().map_err(|_| AppError::Parse {
                what: "edges",
                input: e.to_string(),
                expected: "comma-separated Hz values, e.g. 100,500,2000",
            })
        })
        .collect::<Result<_, _>>()?;
    if edges.is_empty() || edges.windows(2).any(|w| w[0] >= w[1]) {
        return Err(AppError::Invalid("edges must be ascending".into()));
    }

    let wav = parse_wav(&read_file(file)?).map_err(|error| AppError::Invalid(error.to_string()))?;
    if wav.samples.is_empty() {
        return Err(AppError::Invalid("WAV has no samples".into()));
    }

    let peak = wav.samples.iter().fold(0.0f64, |m, s| m.max(s.abs()));
    let rms = (wav.samples.iter().map(|s| s * s).sum::<f64>() / wav.samples.len() as f64).sqrt();
    let dbfs = |x: f64| {
        if x > 0.0 {
            20.0 * x.log10()
        } else {
            f64::NEG_INFINITY
        }
    };
    println!(
        "{}: {} Hz, {} samples ({:.1} s), peak {:.1} dBFS, RMS {:.1} dBFS, clipped {}",
        file.display(),
        wav.sample_rate.0,
        wav.samples.len(),
        wav.samples.len() as f64 / f64::from(wav.sample_rate.0),
        dbfs(peak),
        dbfs(rms),
        wav.clipped_samples.0,
    );

    let band_edges: Vec<FrequencyHz> = edges.iter().copied().map(FrequencyHz).collect();
    let bands = band_powers(&wav.samples, wav.sample_rate, &band_edges);
    let total: f64 = bands.iter().sum();
    let low = bands.get(1).copied().unwrap_or(0.0);
    println!("{:<16} {:>8} {:>8}", "band", "energy%", "/low");
    for (i, power) in bands.iter().enumerate() {
        let label = if i == 0 {
            format!("<{:.0}Hz", edges[0])
        } else if i == edges.len() {
            format!(">={:.0}Hz", edges[i - 1])
        } else {
            format!("{:.0}-{:.0}Hz", edges[i - 1], edges[i])
        };
        let pct = if total > 0.0 {
            power / total * 100.0
        } else {
            0.0
        };
        let vs_low = if low > 0.0 { power / low } else { 0.0 };
        println!("{label:<16} {pct:>7.1}% {vs_low:>8.3}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_and_cells_parse() {
        assert_eq!(parse_range("95E8:9700").unwrap(), (0x95E8, 0x9700));
        assert_eq!(parse_range("$10:$20").unwrap(), (0x10, 0x20));
        assert!(parse_range("20:10").is_err());
        assert!(parse_range("xyz").is_err());

        assert_eq!(
            parse_cells("E50D,E571-E573").unwrap(),
            vec![0xE50D, 0xE571, 0xE572, 0xE573]
        );
        assert!(parse_cells("").is_err());
        assert!(parse_cells("0000-1000").is_err()); // too many cells
    }

    #[test]
    fn pattern_parse_and_match() {
        let pat = parse_pattern("29 7F ?? AA").unwrap();
        assert_eq!(pat, vec![Some(0x29), Some(0x7F), None, Some(0xAA)]);
        assert!(parse_pattern("?? ??").is_err());
        assert!(parse_pattern("zz").is_err());

        let hay = [0x00, 0x29, 0x7F, 0x12, 0xAA, 0x29, 0x7F, 0x34, 0xAA];
        assert_eq!(count_matches(&hay, &pat), 2);
        assert_eq!(count_matches(&hay[..3], &pat), 0);
        let exact = parse_pattern("29 7F").unwrap();
        assert_eq!(count_matches(&hay, &exact), 2);
    }

    #[test]
    fn sid_register_labels() {
        assert_eq!(sid_reg_label(0xD400), Some("v1.freq_lo"));
        assert_eq!(sid_reg_label(0xD40B), Some("v2.ctrl"));
        assert_eq!(sid_reg_label(0xD412), Some("v3.ctrl"));
        assert_eq!(sid_reg_label(0xD40A), Some("v2.pw_hi"));
        assert_eq!(sid_reg_label(0xD418), Some("filt.mode_vol"));
        assert_eq!(sid_reg_label(0xD3FF), None);
        assert_eq!(sid_reg_label(0xD41D), None);
    }

    #[test]
    fn code_fingerprint_ignores_relocation_but_keeps_instruction_identity() {
        fn fixture(base: u16) -> Vec<u8> {
            let mut ram = vec![0; 0x10000];
            let entry = usize::from(base);
            let subroutine = base + 0x20;
            ram[entry..entry + 11].copy_from_slice(&[
                0xA9,
                0x07,
                0x8D,
                0x00,
                0xD4,
                0x20,
                subroutine as u8,
                (subroutine >> 8) as u8,
                0xD0,
                0xF6,
                0x60,
            ]);
            let subroutine = usize::from(subroutine);
            ram[subroutine..subroutine + 9]
                .copy_from_slice(&[0xE6, 0x10, 0xA5, 0x10, 0xC9, 0x08, 0xD0, 0xFA, 0x60]);
            ram
        }

        let (_, first) = reachable_code_fingerprint(
            &fixture(0x1000),
            PlayAddress(0x1000),
            InstructionBudget(64),
        );
        let (_, relocated) = reachable_code_fingerprint(
            &fixture(0x6000),
            PlayAddress(0x6000),
            InstructionBudget(64),
        );
        assert_eq!(code_similarity(&first, &relocated), CodeSimilarity(1.0));

        let mut different = vec![0; 0x10000];
        different[0x3000..0x300B].copy_from_slice(&[
            0xA2, 0x03, 0x86, 0x20, 0xE8, 0xE0, 0x0A, 0xD0, 0xF9, 0xCA, 0x60,
        ]);
        let (_, different) =
            reachable_code_fingerprint(&different, PlayAddress(0x3000), InstructionBudget(64));
        assert!(code_similarity(&first, &different).0 < 0.2);
    }

    #[test]
    fn complete_link_does_not_merge_a_weak_similarity_chain() {
        let similarities = vec![
            vec![1.0, 0.8, 0.3],
            vec![0.8, 1.0, 0.7],
            vec![0.3, 0.7, 1.0],
        ];

        let groups = complete_link_groups(&similarities, CodeSimilarity(0.6));

        assert_eq!(groups, vec![vec![0, 1], vec![2]]);
    }
}
