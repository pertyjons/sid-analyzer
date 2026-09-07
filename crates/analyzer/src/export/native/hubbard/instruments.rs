use super::*;
use crate::emu::PlaybackTiming;

/// One authored instrument from a Hubbard driver's instrument table.
///
/// The static half (`+0..+4`) maps 1:1 to a Pertylizer instrument; the three
/// trailing bytes (`+5/+6/+7`) parameterise the driver's hardwired per-frame
/// effects, decoded into [`InstrumentEffects`]. Contrary to the original design
/// sketch, Commando's driver has **no program tables / opcode grammar** — the
/// three bytes are scalar effect parameters plus a flag mask (see
/// `docs/drivers/hubbard.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeInstrument {
    /// 12-bit pulse width (`+0` low byte, `+1` high nibble).
    pub pulse_width: PulseWidth,
    /// Control register (`+2`): waveform select, ring, sync, gate.
    pub control: ControlBits,
    /// ADSR envelope (`+3` attack/decay, `+4` sustain/release).
    pub adsr: Adsr,
    /// The per-frame effects driven by `+5/+6/+7`.
    pub effects: InstrumentEffects,
}

/// The authored per-frame effects a Hubbard instrument drives, decoded from the
/// three effect-parameter bytes (`+5/+6/+7`).
///
/// The driver applies a fixed palette of hardwired effects each frame, gated and
/// scaled by these bytes — there is no program bytecode to interpret. Decoded
/// from Commando's per-frame instrument tick (`$51A3..$53A0`); other Hubbard
/// relocations are expected to share the engine but are not yet validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct InstrumentEffects {
    /// Vibrato depth as a right-shift of the adjacent-note (semitone) frequency
    /// width (`+5`); `None` when `+5 == 0` (vibrato disabled). A triangle LFO
    /// over a free-running frame counter drives it.
    pub vibrato_depth: Option<u8>,
    /// Continuous pulse-width modulation (`+6`, when `+7` bit 3 is clear and
    /// `+6 != 0`): `rate` = low 5 bits (period − 1 in frames), `step` = high 3
    /// bits (per-period sweep magnitude, bounced between limits).
    pub pwm: Option<Pwm>,
    /// One-shot pulse-width offset added once at note setup (`+6`, when `+7`
    /// bit 3 is set).
    pub pw_offset: Option<u8>,
    /// `+7` bit 0 — downward pitch sweep over the note's early frames (drum/snare).
    pub drum_drop: bool,
    /// `+7` bit 1 — upward pitch chirp on alternate frames.
    pub chirp_up: bool,
    /// `+7` bit 2 — alternate between the note and an octave-ish offset (a fast
    /// hardware arpeggio).
    pub arp: bool,
}

/// Pulse-width modulation parameters decoded from the `+6` byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pwm {
    /// Frames between sweep steps minus one (`+6 & 0x1F`).
    pub rate: u8,
    /// Pulse-width delta applied each period (`+6 & 0xE0`).
    pub step: u8,
}

/// `+7` bit 3: the `+6` byte is a one-shot pulse-width offset, not a PWM rate.
pub(super) const EFFECT_PW_OFFSET: u8 = 0x08;

/// Decode the three effect-parameter bytes `+5/+6/+7` into [`InstrumentEffects`].
pub(super) fn decode_effects(p5: u8, p6: u8, p7: u8) -> InstrumentEffects {
    let pw_offset_mode = p7 & EFFECT_PW_OFFSET != 0;
    InstrumentEffects {
        vibrato_depth: (p5 != 0).then_some(p5),
        pwm: (!pw_offset_mode && p6 != 0).then_some(Pwm {
            rate: p6 & 0x1F,
            step: p6 & 0xE0,
        }),
        pw_offset: (pw_offset_mode && p6 != 0).then_some(p6),
        drum_drop: p7 & 0x01 != 0,
        chirp_up: p7 & 0x02 != 0,
        arp: p7 & 0x04 != 0,
    }
}

/// Convert the driver-encoded [`InstrumentEffects`] to the driver-agnostic,
/// physical-unit [`AuthoredEffects`] carried on the patch table (rollout E1 —
/// stop discarding authored intent).
///
/// - **Vibrato**: the driver wobbles the pitch by the adjacent-semitone
///   frequency width right-shifted by the `+5` byte's **low nibble**, so the
///   peak deviation is `2^-(p5 & 0xF)` semitones — calibrated against the
///   trace: Monty's `$12` instrument measures 0.24 st (2⁻² = 0.25 ✓) and
///   Commando authors `$02`. The high nibble's role is NOT yet REd (Monty's
///   measured LFO rates differ per instrument: ~10 Hz vs ~5–6 Hz, so it
///   likely encodes the LFO speed); the rate here is the 8-call-period
///   default measured on the Nemesis V1 overlay (6.25 Hz PAL,
///   `docs/export.md`). Because the rate is approximate, the
///   exporter uses authored vibrato only as a *fallback* where the per-note
///   heuristic measured nothing. Follow-up: disassemble the `$51BF` vibrato
///   block for the high-nibble semantics, then flip the precedence.
/// - **PWM**: `+6` low 5 bits are the period − 1 in frames; the high 3 bits
///   are the raw register step added each period (kept as the driver byte).
///   The instrument's authored `+0/+1` pulse width rides along as `pw_init` —
///   the value the driver loads at note setup, i.e. the sweep's phase origin.
pub(super) fn authored_patch_effects(
    inst: &NativeInstrument,
    timing: PlaybackTiming,
) -> AuthoredEffects {
    let effects = &inst.effects;
    AuthoredEffects {
        vibrato: effects.vibrato_depth.map(|p5| AuthoredVibrato {
            depth_semitones: (2.0f32).powi(-i32::from(p5 & 0x0F)),
            rate_hz: (timing.calls_per_second() / 8.0) as f32,
        }),
        pwm: effects.pwm.map(|p| AuthoredPwm {
            period_frames: p.rate.saturating_add(1),
            step: p.step,
        }),
        pw_offset: effects.pw_offset,
        pw_init: Some(inst.pulse_width),
        chirp_up: effects.chirp_up,
        arp: effects.arp,
    }
}

pub(super) fn provenance_evidence(
    notes: &[NoteEvent],
    instruments: &[Option<u8>],
    authored: &[NativeInstrument],
    states: &[crate::analysis::FrameState],
    waveform_authored: bool,
) -> Vec<ProvenanceEvidence> {
    let mut adsr_samples = 0usize;
    let mut adsr_mismatches = 0usize;
    let mut waveform_samples = 0usize;
    let mut waveform_mismatches = 0usize;
    for (note, instrument) in notes.iter().zip(instruments) {
        let Some(index) = instrument.and_then(|index| authored.get(usize::from(index))) else {
            continue;
        };
        let Some(state) = states.get(note.start_frame.0 as usize) else {
            continue;
        };
        let voice = state.voices[note.voice.to_index()];
        adsr_samples += 1;
        adsr_mismatches += usize::from(voice.adsr != index.adsr);
        if waveform_authored {
            waveform_samples += 1;
            waveform_mismatches += usize::from(voice.control.waveform != index.control.waveform);
        }
    }
    let effect_count = authored
        .iter()
        .filter(|instrument| instrument.effects != InstrumentEffects::default())
        .count();
    let vibrato_count = authored
        .iter()
        .filter(|instrument| instrument.effects.vibrato_depth.is_some())
        .count();
    let pwm_count = authored
        .iter()
        .filter(|instrument| instrument.effects.pwm.is_some())
        .count();
    let mut evidence = vec![
        ProvenanceEvidence {
            field: "instrument.adsr".to_owned(),
            provenance: if adsr_mismatches == 0 && adsr_samples > 0 {
                FieldProvenance::AuthoredVerified
            } else {
                FieldProvenance::AuthoredDecoded
            },
            samples: adsr_samples,
            mismatches: adsr_mismatches,
        },
        ProvenanceEvidence {
            field: "instrument.waveform".to_owned(),
            provenance: if !waveform_authored {
                FieldProvenance::TraceMeasured
            } else if waveform_mismatches == 0 && waveform_samples > 0 {
                FieldProvenance::AuthoredVerified
            } else {
                FieldProvenance::AuthoredDecoded
            },
            samples: waveform_samples,
            mismatches: waveform_mismatches,
        },
        ProvenanceEvidence {
            field: "instrument.pulse_width".to_owned(),
            provenance: if authored.is_empty() {
                FieldProvenance::TraceMeasured
            } else {
                FieldProvenance::AuthoredDecoded
            },
            samples: authored.len(),
            mismatches: 0,
        },
        ProvenanceEvidence {
            field: "effect.vibrato_depth".to_owned(),
            provenance: FieldProvenance::AuthoredDecoded,
            samples: vibrato_count,
            mismatches: 0,
        },
        ProvenanceEvidence {
            field: "effect.vibrato_rate".to_owned(),
            provenance: FieldProvenance::AuthoredPartial,
            samples: vibrato_count,
            mismatches: 0,
        },
        ProvenanceEvidence {
            field: "effect.pwm".to_owned(),
            provenance: FieldProvenance::AuthoredDecoded,
            samples: pwm_count,
            mismatches: 0,
        },
        ProvenanceEvidence {
            field: "effect.flags".to_owned(),
            provenance: FieldProvenance::AuthoredDecoded,
            samples: effect_count,
            mismatches: 0,
        },
    ];
    if authored.is_empty() {
        for item in &mut evidence {
            if item.provenance != FieldProvenance::AuthoredPartial {
                item.provenance = FieldProvenance::TraceMeasured;
            }
        }
    }
    evidence
}

/// Bytes per instrument record in the packed table (`instrument_index << 3`).
pub(super) const INSTRUMENT_STRIDE: u16 = 8;

/// Split an attack/decay byte and a sustain/release byte into an [`Adsr`].
pub(super) fn adsr_from(ad: u8, sr: u8) -> Adsr {
    Adsr {
        attack: ad >> 4,
        decay: ad & 0x0F,
        sustain: sr >> 4,
        release: sr & 0x0F,
    }
}

/// Decode instrument record `index` from the located table (packed or columnar).
pub(super) fn native_instrument(
    read: &impl Fn(u16) -> u8,
    table: InstrumentTable,
    index: u8,
) -> NativeInstrument {
    match table {
        InstrumentTable::Packed { base } => {
            let rec = base.wrapping_add(u16::from(index) * INSTRUMENT_STRIDE);
            let g = |off: u16| read(rec.wrapping_add(off));
            NativeInstrument {
                pulse_width: PulseWidth(u16::from(g(0)) | (u16::from(g(1) & 0x0F) << 8)),
                control: ControlBits::from_byte(g(2)),
                adsr: adsr_from(g(3), g(4)),
                effects: decode_effects(g(5), g(6), g(7)),
            }
        }
        InstrumentTable::Columnar { pwhi, ad, sr } => {
            let i = u16::from(index);
            let f = |base: u16| read(base.wrapping_add(i));
            NativeInstrument {
                // Pulse-width low is forced to 0 by the driver; only the high
                // nibble varies, fed straight from the PW-high field-table.
                pulse_width: PulseWidth(u16::from(f(pwhi) & 0x0F) << 8),
                // Waveform/control and the effects are driven by a separate
                // per-voice program pointer we do not recover, so both stay
                // trace-derived (default = no authored waveform / effects).
                control: ControlBits::default(),
                adsr: adsr_from(f(ad), f(sr)),
                effects: InstrumentEffects::default(),
            }
        }
    }
}

/// Decode the first `count` instrument records from the located table.
///
/// The table has no terminator, so `count` (the highest instrument index a
/// pattern selects, plus one) bounds the read. Returns empty when the layout has
/// no instrument table ([`HubbardLayout::inst_table`] `None`).
pub(crate) fn decode_instruments(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    count: usize,
) -> Vec<NativeInstrument> {
    let Some(table) = layout.inst_table else {
        return Vec::new();
    };
    (0..count)
        .map(|i| native_instrument(read, table, i as u8))
        .collect()
}
