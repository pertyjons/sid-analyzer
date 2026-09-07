use crate::emu::CallRate;
use crate::emu::capture::CapturedSidExecution;
use crate::header::SidModel;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const SID_REGISTER_LAST: SidRegister = SidRegister(0x1C);

/// Offset from `$D400`, constrained to `<= SID_REGISTER_LAST`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct SidRegister(pub u8);

impl SidRegister {
    /// Human label for the register, e.g. `v1.freq_lo`, `filt.mode_vol`.
    /// `None` for offsets past [`SID_REGISTER_LAST`].
    #[must_use]
    pub fn label(self) -> Option<&'static str> {
        const LABELS: [&str; 29] = [
            "v1.freq_lo",
            "v1.freq_hi",
            "v1.pw_lo",
            "v1.pw_hi",
            "v1.ctrl",
            "v1.ad",
            "v1.sr",
            "v2.freq_lo",
            "v2.freq_hi",
            "v2.pw_lo",
            "v2.pw_hi",
            "v2.ctrl",
            "v2.ad",
            "v2.sr",
            "v3.freq_lo",
            "v3.freq_hi",
            "v3.pw_lo",
            "v3.pw_hi",
            "v3.ctrl",
            "v3.ad",
            "v3.sr",
            "filt.cutoff_lo",
            "filt.cutoff_hi",
            "filt.res_routing",
            "filt.mode_vol",
            "paddle_x",
            "paddle_y",
            "v3.osc",
            "v3.env",
        ];
        LABELS.get(usize::from(self.0)).copied()
    }
}

impl fmt::Display for SidRegister {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "$D4{:02X}", self.0)
    }
}

/// ADSR envelope register nibbles: each field is 4-bit (0..=15).
///
/// Lives here (not in `analysis`) because both the emulation layer and the
/// analysis layer consume it — `emu::sid` must not depend on `analysis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[must_use]
pub struct Adsr {
    pub attack: u8,
    pub decay: u8,
    pub sustain: u8,
    pub release: u8,
}

impl Adsr {
    /// Encode back into the two ADSR register bytes (`AD`, `SR`) the
    /// way the SID register file stores them. High nibble = first
    /// field, low nibble = second.
    #[must_use]
    pub fn to_bytes(self) -> (u8, u8) {
        let ad = (self.attack << 4) | (self.decay & 0x0F);
        let sr = (self.sustain << 4) | (self.release & 0x0F);
        (ad, sr)
    }

    /// Decode the two ADSR register bytes (`AD`, `SR`) — the inverse of
    /// [`Self::to_bytes`].
    pub fn from_bytes(ad: u8, sr: u8) -> Self {
        Self {
            attack: ad >> 4,
            decay: ad & 0x0F,
            sustain: sr >> 4,
            release: sr & 0x0F,
        }
    }
}

impl fmt::Display for Adsr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:X}{:X}{:X}{:X}",
            self.attack, self.decay, self.sustain, self.release
        )
    }
}

/// Scheduled play-call index within a trace (0-based), not a raster-frame index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct FrameIndex(pub u32);

impl fmt::Display for FrameIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Absolute cycle on the SID's Φ2 clock, with cycle zero at init entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct ChipCycle(pub u64);

/// Absolute cycle count reported by the CPU emulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct CpuCycle(pub u64);

/// Elapsed cycles reported by the CPU emulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct CpuCycles(pub u64);

/// CPU-cycle offset at the start of an instruction within one init/play call.
/// Events with the same offset retain their insertion order in the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct SubFrameOffset(pub u32);

impl fmt::Display for SubFrameOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RegisterWrite {
    pub reg: SidRegister,
    pub value: u8,
    pub offset: SubFrameOffset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RegisterRead {
    pub reg: SidRegister,
    pub value: u8,
    pub offset: SubFrameOffset,
}

impl fmt::Display for RegisterWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cycle={:5} {} = {:02X}",
            self.offset.0, self.reg, self.value
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrameTrace {
    pub frame: FrameIndex,
    /// Absolute start of this scheduled play call.
    pub start_cycle: ChipCycle,
    /// CPU time spent executing the play routine.
    pub duration: CpuCycles,
    /// CPU time beyond the originally scheduled next call boundary.
    pub overrun: CpuCycles,
    /// Sampling boundary after the call's writes and idle advancement.
    pub end_cycle: ChipCycle,
    pub writes: Vec<RegisterWrite>,
    pub reads: Vec<RegisterRead>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    /// Canonical lossless SID-bus capture. Legacy read/write vectors below are
    /// compatibility projections from its ordered event stream.
    pub capture: CapturedSidExecution,
    /// SID model declared by the file header for digital-state replay.
    pub sid_model: SidModel,
    /// Writes captured during `init()`. Many tunes set master volume and
    /// filter routing here once and never touch them again, so analysis must
    /// seed its register file from these before walking play frames.
    pub init_writes: Vec<RegisterWrite>,
    pub init_reads: Vec<RegisterRead>,
    pub init_duration: CpuCycles,
    /// Init CPU time beyond the first nominal play-call boundary.
    pub init_overrun: CpuCycles,
    /// Whether scheduled call timing is eligible for digital-state ground truth.
    pub timing_exact: bool,
    /// Effective scheduled play-call rate, including a CIA timer period
    /// captured during init when available.
    pub call_rate: CallRate,
    pub frames: Vec<FrameTrace>,
}

impl Default for Trace {
    fn default() -> Self {
        Self {
            capture: CapturedSidExecution::default(),
            sid_model: SidModel::Unknown,
            init_writes: Vec::new(),
            init_reads: Vec::new(),
            init_duration: CpuCycles(0),
            init_overrun: CpuCycles(0),
            timing_exact: true,
            call_rate: CallRate::default(),
            frames: Vec::new(),
        }
    }
}

impl Trace {
    #[must_use]
    pub fn total_writes(&self) -> usize {
        self.init_writes.len() + self.frames.iter().map(|f| f.writes.len()).sum::<usize>()
    }

    /// Per-frame `$D41B`/`$D41C` read counts as a parallel slice. Used
    /// by `extract_timbre`'s Slice 3 post-pass.
    #[must_use]
    pub fn voice3_reads_per_frame(&self) -> Vec<u32> {
        self.frames
            .iter()
            .map(|f| {
                f.reads
                    .iter()
                    .filter(|read| matches!(read.reg.0, 0x1B | 0x1C))
                    .count() as u32
            })
            .collect()
    }

    #[must_use]
    pub fn osc3_read_count(&self) -> usize {
        self.init_reads
            .iter()
            .chain(self.frames.iter().flat_map(|frame| &frame.reads))
            .filter(|read| read.reg.0 == 0x1B)
            .count()
    }

    #[must_use]
    pub fn env3_read_count(&self) -> usize {
        self.init_reads
            .iter()
            .chain(self.frames.iter().flat_map(|frame| &frame.reads))
            .filter(|read| read.reg.0 == 0x1C)
            .count()
    }
}
