use crate::analysis::VoiceId;
use crate::hex_newtype;
use serde::Serialize;
use std::fmt;

hex_newtype!(Cutoff, u16, "{:03X}");
hex_newtype!(Resonance, u8, "{:X}");

/// Which sources are routed through the filter (low nibble of `$D417`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[must_use]
pub struct FilterRouting {
    pub voice1: bool,
    pub voice2: bool,
    pub voice3: bool,
    pub external: bool,
}

impl FilterRouting {
    /// `true` if any source is routed through the filter.
    #[must_use]
    pub fn any(self) -> bool {
        self.voice1 || self.voice2 || self.voice3 || self.external
    }

    /// `true` if the given voice is routed through the filter.
    #[must_use]
    pub fn contains(self, voice: VoiceId) -> bool {
        match voice.to_index() {
            0 => self.voice1,
            1 => self.voice2,
            2 => self.voice3,
            _ => false,
        }
    }
}

/// Filter mode bits (upper nibble of `$D418`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[must_use]
pub struct FilterMode {
    pub low_pass: bool,
    pub band_pass: bool,
    pub high_pass: bool,
    /// When set, voice 3 is muted (useful for tunes that use voice 3 for
    /// digi samples and don't want it audible).
    pub voice3_off: bool,
}

impl fmt::Display for FilterMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}{}{}",
            if self.low_pass { 'L' } else { '-' },
            if self.band_pass { 'B' } else { '-' },
            if self.high_pass { 'H' } else { '-' },
            if self.voice3_off { '3' } else { '-' },
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[must_use]
pub struct FilterState {
    pub cutoff: Cutoff,
    pub resonance: Resonance,
    pub routing: FilterRouting,
    pub mode: FilterMode,
}

impl FilterState {
    /// Decode the four filter/mode registers in order: `[FCLO, FCHI, RES_FILT, MODE_VOL]`.
    pub fn from_regs(regs: &[u8; 4]) -> Self {
        let fclo = regs[0];
        let fchi = regs[1];
        let res_filt = regs[2];
        let mode_vol = regs[3];
        Self {
            cutoff: Cutoff((u16::from(fchi) << 3) | u16::from(fclo & 0x07)),
            resonance: Resonance(res_filt >> 4),
            routing: FilterRouting {
                voice1: res_filt & 0x01 != 0,
                voice2: res_filt & 0x02 != 0,
                voice3: res_filt & 0x04 != 0,
                external: res_filt & 0x08 != 0,
            },
            mode: FilterMode {
                low_pass: mode_vol & 0x10 != 0,
                band_pass: mode_vol & 0x20 != 0,
                high_pass: mode_vol & 0x40 != 0,
                voice3_off: mode_vol & 0x80 != 0,
            },
        }
    }
}
