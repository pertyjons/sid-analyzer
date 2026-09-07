//! Empirical evaluation for `docs/ML9-FOLLOWUP-IDEAS.md` §G — bytecode
//! patches. For one tune at a time, dumps the per-frame parameter
//! evolution inside each significant patch's member notes and
//! classifies each sequence as one of:
//!
//! * **constant** — `SET_x` opcode once, then nothing.
//! * **one-step** — one `SET_x` change mid-note.
//! * **linear ramp** — `LOOP { x += delta }` style.
//! * **periodic** — `LOOP { cycle }` with explicit cycle body.
//! * **arbitrary** — needs an explicit per-frame opcode.
//!
//! The hypothesis (§G) is that real SID instruments are mostly the
//! first four categories. If most patches turn out to be `arbitrary`,
//! bytecode patches add complexity without compression. If most fit
//! the simple opcode set, bytecode is a strong representation.
//!
//! Usage:
//!
//! ```text
//! m7-patch-program-eval assets/music/Nemesis_the_Warlock.sid \
//!   --subtune 1 --frames 3000
//! ```

use clap::Parser;
use sid_analyzer::analysis::effects::{EffectThresholds, detect_effects};
use sid_analyzer::analysis::note::detect_notes;
use sid_analyzer::analysis::timbre::extract_timbre;
use sid_analyzer::analysis::voice::VoiceState;
use sid_analyzer::analysis::{SystemClock, analyze};
use sid_analyzer::emu;
use sid_analyzer::header::{self, SubtuneIndex};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "m7-patch-program-eval",
    about = "Per-frame parameter classification for ML9 §G bytecode-patch evaluation"
)]
struct Cli {
    /// Path to a `.sid` file.
    file: PathBuf,

    /// Subtune to analyze (1-based).
    #[arg(long, default_value_t = 1)]
    subtune: u16,

    /// Emulation frame budget.
    #[arg(long, default_value_t = 3000)]
    frames: u32,

    /// Only report patches with at least this many members.
    #[arg(long, default_value_t = 3)]
    min_members: u16,

    /// Per-patch, sample this many member notes (1 = first only).
    #[arg(long, default_value_t = 2)]
    samples: usize,

    /// Print the raw per-frame state alongside the classification.
    #[arg(long)]
    show_frames: bool,
}

/// Result of classifying a per-frame parameter sequence.
#[derive(Debug)]
enum Kind {
    Constant(u32),
    OneStep { from: u32, to: u32, at_frame: usize },
    LinearRamp { from: u32, to: u32, delta: i32 },
    Periodic { period: usize, cycle: Vec<u32> },
    Arbitrary { distinct: usize, range: (u32, u32) },
}

fn classify(values: &[u32]) -> Kind {
    if values.is_empty() {
        return Kind::Constant(0);
    }
    if values.iter().all(|&v| v == values[0]) {
        return Kind::Constant(values[0]);
    }
    // Linear ramp — constant non-zero delta between consecutive values.
    if values.len() >= 2 {
        let d0 = values[1] as i32 - values[0] as i32;
        if d0 != 0 && values.windows(2).all(|w| (w[1] as i32 - w[0] as i32) == d0) {
            return Kind::LinearRamp {
                from: values[0],
                to: *values.last().unwrap_or(&0),
                delta: d0,
            };
        }
    }
    // Periodic — smallest period p ≥ 2 where v[i] == v[i - p] for all i ≥ p.
    let max_period = (values.len() / 2).min(16);
    for period in 2..=max_period {
        if (period..values.len()).all(|i| values[i] == values[i - period]) {
            let cycle = values[..period].to_vec();
            return Kind::Periodic { period, cycle };
        }
    }
    // One-step — values[..k] all equal to values[0], values[k..] all
    // equal to values[k], for some k.
    for k in 1..values.len() {
        if values[..k].iter().all(|&v| v == values[0])
            && values[k..].iter().all(|&v| v == values[k])
        {
            return Kind::OneStep {
                from: values[0],
                to: values[k],
                at_frame: k,
            };
        }
    }
    let distinct: HashSet<u32> = values.iter().copied().collect();
    let min_v = values.iter().min().copied().unwrap_or(0);
    let max_v = values.iter().max().copied().unwrap_or(0);
    Kind::Arbitrary {
        distinct: distinct.len(),
        range: (min_v, max_v),
    }
}

fn render_kind(k: &Kind, hex_width: usize) -> String {
    let fmt = |v: u32| -> String {
        if hex_width == 4 {
            format!("{v:#06X}")
        } else {
            format!("{v:#X}")
        }
    };
    match k {
        Kind::Constant(v) => format!("constant {}", fmt(*v)),
        Kind::OneStep { from, to, at_frame } => {
            format!("one-step {} → {} at f={at_frame}", fmt(*from), fmt(*to))
        }
        Kind::LinearRamp { from, to, delta } => {
            format!("linear {} → {} (Δ {:+})", fmt(*from), fmt(*to), delta)
        }
        Kind::Periodic { period, cycle } => {
            let cycle_str: Vec<String> = cycle.iter().map(|&v| fmt(v)).collect();
            format!("periodic p={period} [{}]", cycle_str.join(","))
        }
        Kind::Arbitrary { distinct, range } => format!(
            "arbitrary ({distinct} distinct, {}..{})",
            fmt(range.0),
            fmt(range.1)
        ),
    }
}

#[derive(Default, Debug)]
struct ParamTally {
    constant: usize,
    one_step: usize,
    linear: usize,
    periodic: usize,
    arbitrary: usize,
}

impl ParamTally {
    fn record(&mut self, k: &Kind) {
        match k {
            Kind::Constant(_) => self.constant += 1,
            Kind::OneStep { .. } => self.one_step += 1,
            Kind::LinearRamp { .. } => self.linear += 1,
            Kind::Periodic { .. } => self.periodic += 1,
            Kind::Arbitrary { .. } => self.arbitrary += 1,
        }
    }
    fn total(&self) -> usize {
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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fatal: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(&cli.file)?;
    let header = header::parse(&bytes)?;
    let trace = emu::run(&header, &bytes, SubtuneIndex(cli.subtune), cli.frames)?;
    let clock = SystemClock::from(header.flags.clock);
    let states = analyze(&trace);
    let notes = detect_notes(&states, clock);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());
    let voice3_reads = trace.voice3_reads_per_frame();
    let (_chars, patches, assignments) =
        extract_timbre(&notes, &states, &effects, &voice3_reads, clock);

    println!(
        "Tune: {} — {} (subtune {})",
        header.name, header.author, cli.subtune
    );
    println!(
        "{} frames analyzed, {} notes, {} patches",
        states.len(),
        notes.len(),
        patches.len()
    );
    println!();

    // Aggregate tallies across all member-note samples for this tune.
    let mut tally_wf = ParamTally::default();
    let mut tally_a = ParamTally::default();
    let mut tally_d = ParamTally::default();
    let mut tally_s = ParamTally::default();
    let mut tally_r = ParamTally::default();
    let mut tally_pw = ParamTally::default();

    for patch in &patches {
        if patch.member_count < cli.min_members {
            continue;
        }

        let member_idxs: Vec<usize> = assignments
            .iter()
            .enumerate()
            .filter_map(|(i, a)| if *a == Some(patch.id) { Some(i) } else { None })
            .collect();

        println!(
            "Patch {} — {} members, ADSR={:X},{:X},{:X},{:X}, first_wf={:#04X}",
            patch.id.0,
            patch.member_count,
            patch.adsr.attack,
            patch.adsr.decay,
            patch.adsr.sustain,
            patch.adsr.release,
            patch.waveform
        );
        // Voice-specific timbre lives on the per-voice profiles; show the first
        // playing voice's as the representative.
        if let Some(profile) = patch.voices.first() {
            if let Some(wfl) = &profile.waveform_loop {
                println!("  static patch already captures waveform_loop: {wfl:?}");
            }
            if let Some(arp) = &profile.arpeggio_loop {
                println!("  static patch already captures arpeggio_loop: {arp:?}");
            }
            println!(
                "  static patch pw_envelope: kind={:?} range=({:#06X},{:#06X})",
                profile.pw_envelope.kind, profile.pw_envelope.min.0, profile.pw_envelope.max.0
            );
        }
        println!("  role_tags: {:?}", patch.role_tags);

        for (sample_n, &idx) in member_idxs.iter().take(cli.samples).enumerate() {
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

            let k_wf = classify(&waveforms);
            let k_a = classify(&attacks);
            let k_d = classify(&decays);
            let k_s = classify(&sustains);
            let k_r = classify(&releases);
            let k_pw = classify(&pws);

            tally_wf.record(&k_wf);
            tally_a.record(&k_a);
            tally_d.record(&k_d);
            tally_s.record(&k_s);
            tally_r.record(&k_r);
            tally_pw.record(&k_pw);

            println!(
                "  sample #{} — V{} note at frames {}..{} (len {})",
                sample_n + 1,
                note.voice.0,
                start,
                end,
                end - start
            );
            println!("    waveform   : {}", render_kind(&k_wf, 1));
            println!("    attack     : {}", render_kind(&k_a, 1));
            println!("    decay      : {}", render_kind(&k_d, 1));
            println!("    sustain    : {}", render_kind(&k_s, 1));
            println!("    release    : {}", render_kind(&k_r, 1));
            println!("    pulse_width: {}", render_kind(&k_pw, 4));

            if cli.show_frames && voice_states.len() <= 64 {
                println!("    frames:");
                for (i, vs) in voice_states.iter().enumerate() {
                    println!(
                        "      f={i:3}  wf={:#04X}  adsr={:X},{:X},{:X},{:X}  pw={:#06X}",
                        vs.control.waveform.to_control_byte() >> 4,
                        vs.adsr.attack,
                        vs.adsr.decay,
                        vs.adsr.sustain,
                        vs.adsr.release,
                        vs.pulse_width.0
                    );
                }
            }
        }
        println!();
    }

    println!("=== aggregate (per-parameter % that fits a simple opcode pattern) ===");
    let row = |name: &str, t: &ParamTally| {
        println!(
            "  {name:<11}  constant={:>3}  one-step={:>3}  linear={:>3}  periodic={:>3}  arbitrary={:>3}  → simple {:>5.1}%",
            t.constant,
            t.one_step,
            t.linear,
            t.periodic,
            t.arbitrary,
            t.simple_pct()
        );
    };
    row("waveform", &tally_wf);
    row("attack", &tally_a);
    row("decay", &tally_d);
    row("sustain", &tally_s);
    row("release", &tally_r);
    row("pulse_width", &tally_pw);

    let total_simple = tally_wf.simple_pct()
        + tally_a.simple_pct()
        + tally_d.simple_pct()
        + tally_s.simple_pct()
        + tally_r.simple_pct()
        + tally_pw.simple_pct();
    println!(
        "  → mean simple-fit across parameters: {:.1}%",
        total_simple / 6.0
    );

    Ok(())
}
