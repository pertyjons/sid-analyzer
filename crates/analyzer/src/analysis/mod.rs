pub mod effects;
pub mod filter;
pub mod inputs;
pub mod note;
pub mod osc3;
pub mod programs;
pub mod sid_program;
pub mod timbre;
pub mod voice;

use crate::emu::sid::{DigitalSid, EnvelopeFrameActivity, EnvelopeSnapshot, OscillatorSnapshot};
use crate::header::Clock;
use crate::hex_newtype;
use crate::trace::{ChipCycle, SidRegister, Trace};
use filter::FilterState;
use serde::{Deserialize, Serialize};
use std::fmt;
use voice::VoiceState;

#[cfg(test)]
use crate::trace::FrameIndex;
pub use sid_program::query::ProgramFrame as FrameState;

/// The two SID host-clock domains. Φ2 frequencies follow the canonical C64
/// crystal divisors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
#[must_use]
pub enum SystemClock {
    #[default]
    Pal,
    Ntsc,
}

impl SystemClock {
    /// Φ2 frequency in Hz: PAL = 985_248, NTSC = 1_022_727.
    #[must_use]
    pub fn phi2_hz(self) -> u32 {
        match self {
            Self::Pal => 985_248,
            Self::Ntsc => 1_022_727,
        }
    }

    /// Canonical raster-call rate in Hz from Φ2 divided by cycles per frame.
    #[must_use]
    pub fn frame_rate(self) -> f64 {
        match self {
            Self::Pal => 985_248.0 / 19_656.0,
            Self::Ntsc => 1_022_727.0 / 17_095.0,
        }
    }
}

impl From<Clock> for SystemClock {
    /// `Clock::Unknown` and `Clock::Both` map to PAL — the dominant case for
    /// HVSC and what the C64 KERNAL flag at `$02A6` defaults to.
    fn from(c: Clock) -> Self {
        match c {
            Clock::Ntsc => Self::Ntsc,
            _ => Self::Pal,
        }
    }
}

/// Audible pitch in hertz.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct Hertz(pub f64);

impl fmt::Display for Hertz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2} Hz", self.0)
    }
}

/// 1-based voice id (`1`, `2`, or `3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct VoiceId(pub u8);

impl VoiceId {
    /// Voice 1.
    pub const V1: Self = Self(1);
    /// Voice 2.
    pub const V2: Self = Self(2);
    /// Voice 3 — the "special" voice that's commonly read via `$D41B`
    /// to drive frequency modulation on voices 1/2 (voice-3-as-LFO).
    pub const V3: Self = Self(3);

    /// Build from a 0-based array index (0/1/2 → `VoiceId(1)`/`VoiceId(2)`/`VoiceId(3)`).
    pub fn from_index(i: usize) -> Self {
        Self((i as u8) + 1)
    }

    /// 0-based array index (`VoiceId(1)` → 0, ...). Clamped to `0..=2`
    /// so callers can use it as a `voices[..]` index without bounds
    /// surprises if the input is somehow malformed.
    #[must_use]
    pub fn to_index(self) -> usize {
        (self.0.saturating_sub(1) & 0x03) as usize
    }
}

impl fmt::Display for VoiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

hex_newtype!(Volume, u8, "{:X}");

#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct DigitalVoiceState {
    pub envelope_start: EnvelopeSnapshot,
    pub envelope: EnvelopeSnapshot,
    pub envelope_activity: EnvelopeFrameActivity,
    pub oscillator_start: OscillatorSnapshot,
    pub oscillator: OscillatorSnapshot,
}

/// The 29 SID registers we track ($D400..=$D41C).
const SID_REGS: usize = 0x1D;

/// First register of each voice's 7-byte block within the SID register file.
const VOICE_BASES: [usize; 3] = [0x00, 0x07, 0x0E];

/// Offset of the control register (waveform/gate) within a voice block.
const CONTROL_OFFSET: usize = 0x04;

/// Gate bit within the voice control register.
const GATE_BIT: u8 = 0x01;

/// First register of the 4-byte filter/mode block (`FCLO`, `FCHI`, `RES_FILT`, `MODE_VOL`).
const FILTER_BASE: usize = 0x15;

/// Combined filter-mode/master-volume register; volume is the low nibble.
pub(crate) const MODE_VOL_REG: SidRegister = SidRegister(0x18);

/// Replay the trace writes in order, snapshotting decoded chip state at the
/// end of each frame. The register file is first seeded with `init` writes
/// — many tunes set master volume and filter routing there and rely on it
/// surviving across play frames.
///
/// The state machines are bit-exact for a supplied cycle/event timeline.
/// Whole-tune reconstruction is cycle-accurate only to the fidelity of the
/// captured timeline — bounded by instruction-start write stamping, assumed
/// idle time, unavailable CIA timing, the init→first-play host policy, and the
/// selected NTSC raster model.
#[must_use]
pub fn analyze(trace: &Trace) -> Vec<FrameState> {
    let mut sid = DigitalSid::with_model(trace.sid_model);
    for w in &trace.init_writes {
        sid.write(w.reg, w.value, ChipCycle(u64::from(w.offset.0)));
    }
    let mut out = Vec::with_capacity(trace.frames.len());
    for frame in &trace.frames {
        sid.begin_observation(frame.start_cycle);
        let oscillator_start = sid.oscillator_snapshots();
        let envelope_start = sid.envelope_snapshots();
        let mut regs = std::array::from_fn(|index| sid.register(SidRegister(index as u8)));
        // Walk writes in step order, watching each voice's control register for
        // gate edges. A rising edge (`0 -> 1`) re-attacks the envelope; the
        // first one when the gate entered the frame low is the ordinary note-on
        // the per-frame end state already reflects, so any rising edge beyond
        // that is a hard-restart retrigger this frame would otherwise hide.
        //
        // The entry gate is read from the live register file (so it reflects
        // init writes and the previous frame), not a separate carried value —
        // a voice gated high during `init` is then accounted for at frame 0.
        let entry_gate: [bool; 3] =
            std::array::from_fn(|v| regs[VOICE_BASES[v] + CONTROL_OFFSET] & GATE_BIT != 0);
        let mut cur_gate = entry_gate;
        let mut rising = [0u32; 3];
        for w in &frame.writes {
            let idx = w.reg.0 as usize;
            if idx < SID_REGS {
                sid.write(
                    w.reg,
                    w.value,
                    ChipCycle(frame.start_cycle.0 + u64::from(w.offset.0)),
                );
                regs[idx] = sid.register(w.reg);
            }
            for (v, base) in VOICE_BASES.iter().enumerate() {
                if idx == base + CONTROL_OFFSET {
                    let gate = w.value & GATE_BIT != 0;
                    if gate && !cur_gate[v] {
                        rising[v] += 1;
                    }
                    cur_gate[v] = gate;
                }
            }
        }
        let hard_restart = std::array::from_fn(|v| {
            let ordinary_onset = u32::from(!entry_gate[v] && cur_gate[v]);
            rising[v] > ordinary_onset
        });
        let last_event_cycle = frame
            .writes
            .last()
            .map(|write| frame.start_cycle.0 + u64::from(write.offset.0))
            .unwrap_or(frame.start_cycle.0);
        let sampling_cycle = ChipCycle(
            frame
                .end_cycle
                .0
                .max(frame.start_cycle.0 + frame.duration.0)
                .max(last_event_cycle),
        );
        sid.clock_to(sampling_cycle);
        let snapshots = sid.envelope_snapshots();
        let mut oscillator_snapshots = sid.oscillator_snapshots();
        // Rewrite the cumulative sync/MSB counters to per-frame deltas —
        // FrameState carries frame activity, not lifetime totals (see the
        // OscillatorSnapshot doc).
        for voice in 0..3 {
            oscillator_snapshots[voice].sync_resets = oscillator_snapshots[voice]
                .sync_resets
                .saturating_sub(oscillator_start[voice].sync_resets);
            oscillator_snapshots[voice].source_msb_edges = oscillator_snapshots[voice]
                .source_msb_edges
                .saturating_sub(oscillator_start[voice].source_msb_edges);
        }
        let activities = sid.finish_observation();
        let envelope_retrigger = std::array::from_fn(|voice| {
            let attacks = activities[voice]
                .events
                .iter()
                .filter(|event| event.kind == crate::emu::sid::EnvelopeEventKind::EnteredAttack)
                .count();
            hard_restart[voice] || attacks > usize::from(!entry_gate[voice] && cur_gate[voice])
        });
        out.push(FrameState {
            frame: frame.frame,
            call_duration: frame.duration,
            voices: [
                VoiceState::from_regs(&sub_array(&regs, VOICE_BASES[0])),
                VoiceState::from_regs(&sub_array(&regs, VOICE_BASES[1])),
                VoiceState::from_regs(&sub_array(&regs, VOICE_BASES[2])),
            ],
            filter: FilterState::from_regs(&sub_array(&regs, FILTER_BASE)),
            volume: Volume(regs[MODE_VOL_REG.0 as usize] & 0x0F),
            envelope_retrigger,
            hard_restart,
            digital_voices: std::array::from_fn(|voice| DigitalVoiceState {
                envelope_start: envelope_start[voice],
                envelope: snapshots[voice],
                envelope_activity: activities[voice].clone(),
                oscillator_start: oscillator_start[voice],
                oscillator: oscillator_snapshots[voice],
            }),
            digital_state_exact: trace.timing_exact,
            register_writes: frame.writes.clone(),
            register_reads: frame.reads.clone(),
        });
    }
    out
}

fn sub_array<const N: usize>(regs: &[u8; SID_REGS], start: usize) -> [u8; N] {
    std::array::from_fn(|i| regs[start + i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{FrameTrace, RegisterWrite, SidRegister, SubFrameOffset};

    /// Voice 2's control register (`$D40B`).
    const V2_CONTROL: u8 = (VOICE_BASES[1] + CONTROL_OFFSET) as u8;

    fn write(reg: u8, value: u8) -> RegisterWrite {
        RegisterWrite {
            reg: SidRegister(reg),
            value,
            offset: SubFrameOffset(0),
        }
    }

    fn frame(n: u32, writes: Vec<RegisterWrite>) -> FrameTrace {
        FrameTrace {
            frame: FrameIndex(n),
            writes,
            ..FrameTrace::default()
        }
    }

    #[test]
    fn within_frame_gate_dip_is_flagged_as_retrigger() {
        // Voice 2 held high across frames; frame 1 pulses gate 0x00 then 0x81
        // (a hard restart) — the end-of-frame snapshot stays gate-high, so only
        // the rising-edge count reveals the re-attack.
        let trace = Trace {
            init_writes: vec![],
            frames: vec![
                frame(0, vec![write(V2_CONTROL, 0x81)]),
                frame(1, vec![write(V2_CONTROL, 0x00), write(V2_CONTROL, 0x81)]),
                frame(2, vec![write(V2_CONTROL, 0x81)]),
            ],
            ..Default::default()
        };
        let states = analyze(&trace);
        assert!(
            !states[0].hard_restart[1],
            "ordinary note-on is not a retrigger"
        );
        assert!(
            states[1].hard_restart[1],
            "intra-frame gate dip is a retrigger"
        );
        assert!(
            states[1].envelope_retrigger[1],
            "envelope attack events drive exact retriggers"
        );
        assert!(!states[2].hard_restart[1], "held gate is not a retrigger");
    }

    #[test]
    fn init_seeded_gate_high_is_accounted_for_in_frame_zero() {
        // Voice 2 is gated high during init; frame 0 hard-restarts it (00 then
        // 81). The entry gate must be read from the init-seeded register file,
        // not assumed low, or the retrigger is missed.
        let trace = Trace {
            init_writes: vec![write(V2_CONTROL, 0x81)],
            frames: vec![frame(
                0,
                vec![write(V2_CONTROL, 0x00), write(V2_CONTROL, 0x81)],
            )],
            ..Default::default()
        };
        let states = analyze(&trace);
        assert!(
            states[0].hard_restart[1],
            "hard restart on an init-gated voice in frame 0 must be detected"
        );
    }
}
