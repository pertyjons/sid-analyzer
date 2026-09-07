use crate::analysis::note::hertz_to_midi;
use crate::analysis::voice::VoiceState;
use crate::analysis::{FrameState, SystemClock};
use std::io::{self, Write};

// Header text must line-by-line match the column widths emitted in
// `write_text`. The `header_and_row_align` test pins this invariant.
const HEADER: &str = "Frame | Note Freq Wave ADSR PW | Note Freq Wave ADSR PW | Note Freq Wave ADSR PW | Cut R Mode | V";

/// Render the decoded `FrameState` stream as a siddump-inspired per-frame
/// table. New notes (gate rising edge) show as `C-4`/`F#5`; sustained gates
/// show `...`; silent voices show `---`.
pub fn write_text(
    states: &[FrameState],
    clock: SystemClock,
    out: &mut dyn Write,
) -> io::Result<()> {
    writeln!(out, "{HEADER}")?;
    let mut prev_gate = [false; 3];
    for state in states {
        write!(out, "{:5}", state.frame)?;
        for (v, gate) in state.voices.iter().zip(prev_gate.iter_mut()) {
            write!(out, " | ")?;
            write_note_col(out, v, *gate, clock)?;
            write!(
                out,
                " {} {} {} {}",
                v.freq, v.control.waveform, v.adsr, v.pulse_width
            )?;
            *gate = v.control.gate;
        }
        writeln!(
            out,
            " | {} {} {} | {}",
            state.filter.cutoff, state.filter.resonance, state.filter.mode, state.volume,
        )?;
    }
    Ok(())
}

fn write_note_col(
    out: &mut dyn Write,
    v: &VoiceState,
    prev_gate: bool,
    clock: SystemClock,
) -> io::Result<()> {
    if v.control.gate && !prev_gate {
        match hertz_to_midi(v.freq.to_hertz(clock)) {
            Some((midi, _)) => write!(out, "{midi}"),
            None => out.write_all(b"---"),
        }
    } else if v.control.gate {
        out.write_all(b"...")
    } else {
        out.write_all(b"---")
    }
}
