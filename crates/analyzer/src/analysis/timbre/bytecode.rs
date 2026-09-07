//! Track B Phase G1 — bytecode patches (storage + extraction).
//!
//! A [`Patch`](super::patch::Patch) is a *static* fingerprint; a
//! [`BytecodePatch`] is the deterministic per-frame *program* that drives a
//! note — PWM sweep, arpeggio, waveform loop/sequence, hardware trick. The
//! hypothesis: a single program token subsumes the per-frame register churn
//! the baseline encoder's diff-suppression cannot compress, which is where
//! the static-patch encoder (ML9) came up null.
//!
//! G1 builds a program **per note**, from the Slice-1
//! [`NoteCharacteristics`] (which retains the full waveform sequence and
//! loop offsets), and clusters notes by [`BytecodePatch::program_key`],
//! which clusters on program equality and replaces the tuple key. The
//! `bytecode-compare` binary (G1's final step) then measures patches per
//! subtune and token compression against the go/no-go gate. No
//! encoder/decoder/training wiring lives here (that is G2/G3).
//!
//! Faithfulness notes (so the compression measurement isn't biased toward a
//! false GO): mid-note waveform switches and combined waveforms are captured
//! by the per-frame waveform sequence; `$D418` sample patches escape to
//! [`Op::PatchRaw`]; the test bit (no opcode) is dropped — a documented
//! opcode-set limitation. Keying each note's own program also avoids a v1
//! patch's members differing on non-key fields; waveform and pitch-loop phase
//! offsets are both preserved.

use super::characteristics::{ContourKind, HardwareTrick, NoteCharacteristics, SeqOrLoop};
use crate::analysis::voice::{Adsr, PulseWidth};

/// Pulse bit within the 4-bit waveform nibble (control-register bit 6 → 0x4).
const PULSE_NIBBLE_BIT: u8 = 0x4;

/// Hardware tricks expressible as a gate-on program opcode — the subset of
/// [`HardwareTrick`] that is a deterministic flag. Sample playback escapes
/// to [`Op::PatchRaw`]; combined/mid-note waveform tricks are captured by the
/// waveform sequence; the test bit has no opcode and is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BytecodeTrick {
    HardSync,
    RingMod,
    Voice3LfoSource,
}

impl BytecodeTrick {
    fn from_hardware(trick: HardwareTrick) -> Option<Self> {
        match trick {
            HardwareTrick::HardSync => Some(Self::HardSync),
            HardwareTrick::RingMod => Some(Self::RingMod),
            HardwareTrick::Voice3LfoSource => Some(Self::Voice3LfoSource),
            _ => None,
        }
    }
}

/// One bytecode operation. Gate-on ops run once before frame 0; sustain ops
/// advance one step per frame; [`Op::PatchRaw`] is the escape hatch for
/// `$D418` sample playback (no deterministic program).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    // --- Gate-on setup (once per note) ---
    SetAdsr(Adsr),
    SetWaveform(u8),
    FilterRoute,
    HardwareTrick(BytecodeTrick),
    // --- Sustain phase (one step per frame) ---
    WaveformLoop { offset: u8, body: Vec<u8> },
    WaveformSeq(Vec<u8>),
    PitchLoop { offset: u8, body: Vec<i8> },
    PwStatic(PulseWidth),
    PwRampUp { min: PulseWidth, max: PulseWidth },
    PwRampDown { min: PulseWidth, max: PulseWidth },
    PwTriangle { min: PulseWidth, max: PulseWidth },
    PwRandom { min: PulseWidth, max: PulseWidth },
    // --- Escape hatch ---
    PatchRaw,
}

impl Op {
    /// Token cost under the G1 cost model: one opcode token, plus value
    /// tokens for fixed-arg ops, plus (for loops/seqs) an optional offset,
    /// the body elements, and one end marker. Approximates the token-stream
    /// layout closely enough to size compression; G2 pins the exact vocab
    /// slots. The per-use `PatchRef`
    /// and the `PatchRaw`-yields-no-compression accounting live in the
    /// measurement binary, not here.
    #[must_use]
    pub fn token_len(&self) -> usize {
        match self {
            Op::SetAdsr(_) => 1 + 4,
            Op::SetWaveform(_) => 1 + 1,
            Op::FilterRoute => 1,
            Op::HardwareTrick(_) => 1,
            Op::PwStatic(_) => 1 + 1,
            Op::PwRampUp { .. }
            | Op::PwRampDown { .. }
            | Op::PwTriangle { .. }
            | Op::PwRandom { .. } => 1 + 2,
            Op::WaveformLoop { body, .. } => 1 + 1 + body.len() + 1,
            Op::PitchLoop { body, .. } => 1 + 1 + body.len() + 1,
            Op::WaveformSeq(body) => 1 + body.len() + 1,
            Op::PatchRaw => 1,
        }
    }
}

/// PW bucket width mirroring `sid-data`'s 12-bit → 32-bucket quantizer
/// (4096 / 32). Masks to 12 bits first so an out-of-range `PulseWidth`
/// (the newtype has no constructor clamp) still lands in `0..32`, exactly
/// like sid-data's `pw_to_bucket`.
const PW_BUCKET_WIDTH: u16 = 128;

fn pw_bucket(pw: PulseWidth) -> u16 {
    (pw.0 & 0x0FFF) / PW_BUCKET_WIDTH
}

/// The deterministic per-frame program for one note. Ops are emitted in a
/// canonical order (gate-on setup first, then sustain), so two programs are
/// equal exactly when their `ops` match.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct BytecodePatch {
    pub ops: Vec<Op>,
}

impl BytecodePatch {
    /// Build the program for one note from its Slice-1
    /// [`NoteCharacteristics`]. A `$D418` sample note becomes a lone
    /// [`Op::PatchRaw`]. This only re-maps fields the analyzer already
    /// produced — no new pattern recognition.
    pub fn from_characteristics(c: &NoteCharacteristics) -> Self {
        let is_sample = c.role_tags.sample
            || c.hardware_tricks
                .iter()
                .any(|t| matches!(t, HardwareTrick::D418Sample { .. }));
        if is_sample {
            return Self {
                ops: vec![Op::PatchRaw],
            };
        }

        let mut ops = Vec::new();
        let first_nibble = c.first_waveform_byte() >> 4;

        // --- Gate-on setup ---
        ops.push(Op::SetAdsr(c.starting_adsr));
        ops.push(Op::SetWaveform(first_nibble));
        if c.filter_routed {
            ops.push(Op::FilterRoute);
        }
        // Canonical (sorted, deduped) trick order so `program_key` does not
        // depend on the order tricks were appended (Slice-1 vs Slice-3).
        let mut tricks: Vec<BytecodeTrick> = c
            .hardware_tricks
            .iter()
            .filter_map(|t| BytecodeTrick::from_hardware(*t))
            .collect();
        tricks.sort_unstable();
        tricks.dedup();
        for trick in tricks {
            ops.push(Op::HardwareTrick(trick));
        }

        // --- Sustain: waveform. The per-frame sequence captures mid-note
        //     switches and combined waveforms. Values are nibbles (matching
        //     SetWaveform). A single-valued sequence carries no new info. ---
        match &c.waveform_sequence {
            SeqOrLoop::Loop { body, offset } => {
                ops.push(Op::WaveformLoop {
                    offset: *offset,
                    body: body.iter().map(|b| b >> 4).collect(),
                });
            }
            SeqOrLoop::Raw(seq) => {
                let nibbles: Vec<u8> = seq.iter().map(|b| b >> 4).collect();
                if nibbles.iter().any(|&w| w != first_nibble) {
                    ops.push(Op::WaveformSeq(nibbles));
                }
            }
        }

        // --- Sustain: arpeggio (relative pitch loop), with its phase offset. ---
        if let Some((body, offset)) = &c.pitch_relative_loop {
            ops.push(Op::PitchLoop {
                offset: *offset,
                body: body.clone(),
            });
        }

        // --- Sustain: pulse-width form, only when the pulse waveform is
        //     actually used (otherwise PW is a stale, irrelevant register). ---
        if waveform_uses_pulse(first_nibble, &c.waveform_sequence) {
            let pw = c.pw_envelope;
            ops.push(match pw.kind {
                ContourKind::Static => Op::PwStatic(pw.min),
                ContourKind::RisingRamp => Op::PwRampUp {
                    min: pw.min,
                    max: pw.max,
                },
                ContourKind::FallingRamp => Op::PwRampDown {
                    min: pw.min,
                    max: pw.max,
                },
                ContourKind::Triangle => Op::PwTriangle {
                    min: pw.min,
                    max: pw.max,
                },
                ContourKind::Random => Op::PwRandom {
                    min: pw.min,
                    max: pw.max,
                },
            });
        }

        Self { ops }
    }

    /// Token cost of this program's `PatchDef` block (sum of op costs).
    #[must_use]
    pub fn token_len(&self) -> usize {
        self.ops.iter().map(Op::token_len).sum()
    }

    /// Compressed-representation size for the measurement: the `PatchDef`
    /// token cost, or `None` for a raw-sample patch — which keeps its full
    /// per-frame data and so yields **no** compression. The `bytecode-compare`
    /// binary must treat `None` as "not compressed" (count the note's baseline
    /// timbre tokens), never as "1 token replaced N frames". The
    /// baseline-relative accounting (per-use `PatchRef`, reused
    /// `Waveform`/`PwBucket` tokens) is finalized in that binary.
    #[must_use]
    pub fn compressed_token_len(&self) -> Option<usize> {
        if self.is_raw() {
            None
        } else {
            Some(self.token_len())
        }
    }

    /// `true` if this is the raw-sample escape — it carries no program and
    /// yields no compression (the measurement must not count it as such).
    #[must_use]
    pub fn is_raw(&self) -> bool {
        matches!(self.ops.as_slice(), [Op::PatchRaw])
    }

    /// Hashable canonical key for program-equality clustering, with PW
    /// quantized to buckets. NOTE: this is bucket-level equality, not a true
    /// ±1-bucket tolerance — values in the same bucket merge, values
    /// straddling a bucket boundary do not. A sound, hashable equivalence.
    /// `PulseWidth` is not `Hash`, hence the bucketed [`OpKey`].
    pub fn program_key(&self) -> ProgramKey {
        ProgramKey(self.ops.iter().map(OpKey::from_op).collect())
    }
}

/// `true` if the gate-on waveform or any frame of the sequence enables the
/// pulse waveform (so the pulse-width register is musically meaningful).
fn waveform_uses_pulse(first_nibble: u8, seq: &SeqOrLoop<u8>) -> bool {
    if first_nibble & PULSE_NIBBLE_BIT != 0 {
        return true;
    }
    let body = match seq {
        SeqOrLoop::Loop { body, .. } => body,
        SeqOrLoop::Raw(s) => s,
    };
    body.iter().any(|b| (b >> 4) & PULSE_NIBBLE_BIT != 0)
}

/// Hashable program identity for clustering — see [`BytecodePatch::program_key`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[must_use]
pub struct ProgramKey(Vec<OpKey>);

/// Bucketed, hashable mirror of [`Op`]: all PW variants collapse into one
/// `Pw { kind, min, max }` with bucket-quantized values.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum OpKey {
    SetAdsr(Adsr),
    SetWaveform(u8),
    FilterRoute,
    HardwareTrick(BytecodeTrick),
    WaveformLoop { offset: u8, body: Vec<u8> },
    WaveformSeq(Vec<u8>),
    PitchLoop { offset: u8, body: Vec<i8> },
    Pw { kind: u8, min: u16, max: u16 },
    PatchRaw,
}

impl OpKey {
    fn from_op(op: &Op) -> Self {
        match op {
            Op::SetAdsr(a) => OpKey::SetAdsr(*a),
            Op::SetWaveform(w) => OpKey::SetWaveform(*w),
            Op::FilterRoute => OpKey::FilterRoute,
            Op::HardwareTrick(t) => OpKey::HardwareTrick(*t),
            Op::WaveformLoop { offset, body } => OpKey::WaveformLoop {
                offset: *offset,
                body: body.clone(),
            },
            Op::WaveformSeq(body) => OpKey::WaveformSeq(body.clone()),
            Op::PitchLoop { offset, body } => OpKey::PitchLoop {
                offset: *offset,
                body: body.clone(),
            },
            Op::PwStatic(v) => OpKey::Pw {
                kind: 0,
                min: pw_bucket(*v),
                max: pw_bucket(*v),
            },
            Op::PwRampUp { min, max } => OpKey::Pw {
                kind: 1,
                min: pw_bucket(*min),
                max: pw_bucket(*max),
            },
            Op::PwRampDown { min, max } => OpKey::Pw {
                kind: 2,
                min: pw_bucket(*min),
                max: pw_bucket(*max),
            },
            Op::PwTriangle { min, max } => OpKey::Pw {
                kind: 3,
                min: pw_bucket(*min),
                max: pw_bucket(*max),
            },
            Op::PwRandom { min, max } => OpKey::Pw {
                kind: 4,
                min: pw_bucket(*min),
                max: pw_bucket(*max),
            },
            Op::PatchRaw => OpKey::PatchRaw,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::timbre::characteristics::PwEnvelope;
    use crate::analysis::voice::Waveform;

    fn adsr() -> Adsr {
        Adsr {
            attack: 1,
            decay: 2,
            sustain: 8,
            release: 4,
        }
    }

    /// Characteristics for a plain static pulse note.
    fn pulse_chars() -> NoteCharacteristics {
        NoteCharacteristics {
            starting_adsr: adsr(),
            waveform_primary: vec![Waveform {
                pulse: true,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn static_pulse_note_maps_to_setup_plus_pw_static() {
        let bc = BytecodePatch::from_characteristics(&pulse_chars());
        assert_eq!(
            bc.ops,
            vec![
                Op::SetAdsr(adsr()),
                Op::SetWaveform(0x04),
                Op::PwStatic(PulseWidth(0)),
            ]
        );
        assert!(!bc.is_raw());
    }

    #[test]
    fn non_pulse_note_emits_no_pw_op() {
        let mut c = pulse_chars();
        c.waveform_primary = vec![Waveform {
            triangle: true,
            ..Default::default()
        }];
        let bc = BytecodePatch::from_characteristics(&c);
        // No PW op: pulse is not used (#4).
        assert_eq!(bc.ops, vec![Op::SetAdsr(adsr()), Op::SetWaveform(0x01)]);
    }

    #[test]
    fn sample_note_becomes_raw() {
        let mut c = pulse_chars();
        c.role_tags.sample = true;
        assert!(BytecodePatch::from_characteristics(&c).is_raw());

        let mut c2 = pulse_chars();
        c2.hardware_tricks = vec![HardwareTrick::D418Sample { overlap_frames: 3 }];
        assert!(BytecodePatch::from_characteristics(&c2).is_raw());
    }

    #[test]
    fn raw_changing_waveform_becomes_waveform_seq() {
        // Pulse → noise one-shot (no detected loop): emits WaveformSeq of
        // nibbles, not silently dropped (#1).
        let mut c = pulse_chars();
        c.waveform_sequence = SeqOrLoop::Raw(vec![0x40, 0x40, 0x80]);
        let bc = BytecodePatch::from_characteristics(&c);
        assert!(
            bc.ops.contains(&Op::WaveformSeq(vec![0x04, 0x04, 0x08])),
            "ops were {:?}",
            bc.ops
        );
    }

    #[test]
    fn static_raw_waveform_emits_no_sequence_op() {
        // All frames equal the gate-on nibble → no sustain waveform op.
        let mut c = pulse_chars();
        c.waveform_sequence = SeqOrLoop::Raw(vec![0x40, 0x40, 0x40]);
        let bc = BytecodePatch::from_characteristics(&c);
        assert!(!bc.ops.iter().any(|op| matches!(op, Op::WaveformSeq(_))));
    }

    #[test]
    fn loop_keeps_offset_and_nibbles() {
        let mut c = pulse_chars();
        c.waveform_sequence = SeqOrLoop::Loop {
            body: vec![0x40, 0x80],
            offset: 2,
        };
        let bc = BytecodePatch::from_characteristics(&c);
        assert!(bc.ops.contains(&Op::WaveformLoop {
            offset: 2,
            body: vec![0x04, 0x08],
        }));
    }

    #[test]
    fn arpeggio_maps_to_pitch_loop() {
        let mut c = pulse_chars();
        c.pitch_relative_loop = Some((vec![0, 3, 7], 2));
        let bc = BytecodePatch::from_characteristics(&c);
        // Offset is preserved end-to-end (#3).
        assert!(bc.ops.contains(&Op::PitchLoop {
            offset: 2,
            body: vec![0, 3, 7],
        }));
    }

    #[test]
    fn raw_patch_has_no_compressed_size() {
        let mut c = pulse_chars();
        c.role_tags.sample = true;
        let bc = BytecodePatch::from_characteristics(&c);
        assert!(bc.is_raw());
        // #7: a raw patch must report no compressed size, not a misleading
        // "1 token". A real program has a defined size.
        assert_eq!(bc.compressed_token_len(), None);
        assert_eq!(
            BytecodePatch::from_characteristics(&pulse_chars()).compressed_token_len(),
            Some(9)
        );
    }

    #[test]
    fn trick_order_is_canonical() {
        let mut c = pulse_chars();
        c.hardware_tricks = vec![
            HardwareTrick::RingMod,
            HardwareTrick::TestBitUsage, // dropped (no opcode)
            HardwareTrick::HardSync,
        ];
        let bc = BytecodePatch::from_characteristics(&c);
        let tricks: Vec<&Op> = bc
            .ops
            .iter()
            .filter(|op| matches!(op, Op::HardwareTrick(_)))
            .collect();
        // Sorted: HardSync (decl order 0) before RingMod (1); TestBit dropped.
        assert_eq!(
            tricks,
            vec![
                &Op::HardwareTrick(BytecodeTrick::HardSync),
                &Op::HardwareTrick(BytecodeTrick::RingMod),
            ]
        );
    }

    #[test]
    fn token_len_sums_op_costs() {
        // SetAdsr(5) + SetWaveform(2) + PwStatic(2) = 9.
        assert_eq!(
            BytecodePatch::from_characteristics(&pulse_chars()).token_len(),
            9
        );
    }

    #[test]
    fn program_key_collapses_within_a_pw_bucket() {
        let mk = |pw: u16| {
            let mut c = pulse_chars();
            c.pw_envelope = PwEnvelope {
                min: PulseWidth(pw),
                max: PulseWidth(pw),
                kind: ContourKind::Static,
            };
            BytecodePatch::from_characteristics(&c).program_key()
        };
        // 896 and 1000 share bucket 7 (896/128 = 7, 1000/128 = 7).
        assert_eq!(mk(896), mk(1000));
    }

    #[test]
    fn program_key_separates_adjacent_buckets() {
        // Near-boundary non-collapse (#12): 1000 → bucket 7, 1024 → bucket 8.
        let mk = |pw: u16| {
            let mut c = pulse_chars();
            c.pw_envelope = PwEnvelope {
                min: PulseWidth(pw),
                max: PulseWidth(pw),
                kind: ContourKind::Static,
            };
            BytecodePatch::from_characteristics(&c).program_key()
        };
        assert_ne!(mk(1000), mk(1024));
    }

    #[test]
    fn program_key_separates_pw_kinds() {
        let base = pulse_chars();
        let mut tri = pulse_chars();
        tri.pw_envelope = PwEnvelope {
            min: PulseWidth(0),
            max: PulseWidth(0),
            kind: ContourKind::Triangle,
        };
        assert_ne!(
            BytecodePatch::from_characteristics(&base).program_key(),
            BytecodePatch::from_characteristics(&tri).program_key()
        );
    }
}
