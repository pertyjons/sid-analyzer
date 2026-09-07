use crate::analysis::{Hertz, SystemClock};
use crate::hex_newtype;
use std::fmt;

/// Range of the SID phase accumulator (`2^24`).
const SID_ACCUMULATOR_RANGE: f64 = (1u32 << 24) as f64;

hex_newtype!(SidFreq, u16, "{:04X}");
hex_newtype!(PulseWidth, u16, "{:03X}");

impl SidFreq {
    /// `Fout = Fn · Φ2 / 2^24`.
    pub fn to_hertz(self, clock: SystemClock) -> Hertz {
        Hertz(f64::from(self.0) * f64::from(clock.phi2_hz()) / SID_ACCUMULATOR_RANGE)
    }
}

pub use crate::trace::Adsr;

/// Waveform-select bits from the voice control register.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[must_use]
pub struct Waveform {
    pub triangle: bool,
    pub sawtooth: bool,
    pub pulse: bool,
    pub noise: bool,
}

impl Waveform {
    #[must_use]
    pub fn is_silent(self) -> bool {
        !(self.triangle || self.sawtooth || self.pulse || self.noise)
    }

    /// Encode back into the high nibble of the voice control register
    /// (`$D404` / `$D40B` / `$D412`): T=`0x10`, S=`0x20`, P=`0x40`, N=`0x80`.
    #[must_use]
    pub fn to_control_byte(self) -> u8 {
        let mut b = 0u8;
        if self.triangle {
            b |= 0x10;
        }
        if self.sawtooth {
            b |= 0x20;
        }
        if self.pulse {
            b |= 0x40;
        }
        if self.noise {
            b |= 0x80;
        }
        b
    }

    /// Count of active waveform bits (0..=4).
    #[must_use]
    pub fn active_bits(self) -> u8 {
        u8::from(self.triangle)
            + u8::from(self.sawtooth)
            + u8::from(self.pulse)
            + u8::from(self.noise)
    }

    /// True when noise is the *only* active waveform bit. Such a frame carries
    /// no musical pitch — its frequency register sets the noise colour, not a
    /// note — so pitch analysis must skip it (otherwise the noise frame's freq
    /// invents a phantom arpeggio step, e.g. Hubbard's triangle/noise leads).
    #[must_use]
    pub fn is_noise_only(self) -> bool {
        self.noise && !self.triangle && !self.sawtooth && !self.pulse
    }
}

impl fmt::Display for Waveform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}{}{}",
            if self.triangle { 'T' } else { '-' },
            if self.sawtooth { 'S' } else { '-' },
            if self.pulse { 'P' } else { '-' },
            if self.noise { 'N' } else { '-' },
        )
    }
}

/// Voice control register ($D404 / $D40B / $D412) decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[must_use]
pub struct ControlBits {
    pub gate: bool,
    pub sync: bool,
    pub ring_mod: bool,
    pub test: bool,
    pub waveform: Waveform,
}

impl ControlBits {
    pub(crate) fn from_byte(b: u8) -> Self {
        Self {
            gate: b & 0x01 != 0,
            sync: b & 0x02 != 0,
            ring_mod: b & 0x04 != 0,
            test: b & 0x08 != 0,
            waveform: Waveform {
                triangle: b & 0x10 != 0,
                sawtooth: b & 0x20 != 0,
                pulse: b & 0x40 != 0,
                noise: b & 0x80 != 0,
            },
        }
    }
}

/// Snapshot of one SID voice's 7 registers, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[must_use]
pub struct VoiceState {
    pub freq: SidFreq,
    pub pulse_width: PulseWidth,
    pub control: ControlBits,
    pub adsr: Adsr,
}

impl VoiceState {
    /// Decode the 7 voice registers in canonical order:
    /// `[FREQLO, FREQHI, PWLO, PWHI, CR, AD, SR]`.
    pub fn from_regs(regs: &[u8; 7]) -> Self {
        Self {
            freq: SidFreq(u16::from_le_bytes([regs[0], regs[1]])),
            pulse_width: PulseWidth(u16::from_le_bytes([regs[2], regs[3]]) & 0x0FFF),
            control: ControlBits::from_byte(regs[4]),
            adsr: Adsr::from_bytes(regs[5], regs[6]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_to_control_byte_matches_register_bit_layout() {
        let triangle_only = Waveform {
            triangle: true,
            ..Default::default()
        };
        assert_eq!(triangle_only.to_control_byte(), 0x10);
        let pulse_noise = Waveform {
            pulse: true,
            noise: true,
            ..Default::default()
        };
        assert_eq!(pulse_noise.to_control_byte(), 0xC0);
        assert_eq!(Waveform::default().to_control_byte(), 0x00);
    }

    #[test]
    fn waveform_active_bits_counts_set_bits() {
        assert_eq!(Waveform::default().active_bits(), 0);
        let tri_saw = Waveform {
            triangle: true,
            sawtooth: true,
            ..Default::default()
        };
        assert_eq!(tri_saw.active_bits(), 2);
    }

    #[test]
    fn adsr_to_bytes_packs_ad_then_sr() {
        // Hubbard-snare-style ADSR: attack=0, decay=F, sustain=0, release=1.
        let a = Adsr {
            attack: 0x0,
            decay: 0xF,
            sustain: 0x0,
            release: 0x1,
        };
        assert_eq!(a.to_bytes(), (0x0F, 0x01));
    }
}
