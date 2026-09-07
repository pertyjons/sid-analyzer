//! `NoteCharacteristics` + supporting types + per-aspect extractors.

use crate::analysis::FrameState;
use crate::analysis::effects::{Effect, EffectSpan};
use crate::analysis::filter::{Cutoff, FilterMode, Resonance};
use crate::analysis::note::{Cents, MidiNote, NoteEvent, hertz_to_midi};
use crate::analysis::timbre::loops::{DEFAULT_TOLERANCE, detect_loop};
use crate::analysis::voice::{Adsr, ControlBits, PulseWidth, SidFreq, Waveform};
use crate::analysis::{Hertz, SystemClock, VoiceId};
use serde::Serialize;

/// Per-note timbral fingerprint produced by [`extract_characteristics`].
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[must_use]
pub struct NoteCharacteristics {
    // envelope
    /// Full ADSR register state at gate-on. `attack` is the bucketed
    /// view of this; the full bytes are kept so patch clustering can
    /// key on the exact envelope.
    pub starting_adsr: Adsr,
    pub attack: AttackClass,
    pub length_frames: u16,
    pub release_behavior: ReleaseClass,

    // spectral
    /// Distinct waveform combinations seen during the note, in order
    /// of first appearance.
    pub waveform_primary: Vec<Waveform>,
    pub waveform_switches: u8,
    pub noise_share: f32,
    /// Median oscillator frequency over the note's noise frames. Drivers jump
    /// the frequency register way up while the noise bit is set (a drum click
    /// or hihat tick riding a tonal body), so the noise brightness lives here,
    /// not at the body pitch — the percussion noise source pins its LFSR clock
    /// to this instead of tracking the played note.
    pub noise_freq_hz: Option<Hertz>,
    /// Mean length in frames of the note's noise runs (`0.0` = no noise
    /// frames) — sizes the percussion click envelope.
    pub noise_run_frames: f32,
    pub waveform_sequence: SeqOrLoop<u8>,
    /// Encoded waveform byte the note holds longest **while gated** (see
    /// [`dominant_gated_waveform`]). This is the note's timbral waveform — the
    /// patch key and exported oscillator use it via [`dominant_waveform_byte`].
    ///
    /// [`dominant_waveform_byte`]: Self::dominant_waveform_byte
    pub dominant_waveform: u8,
    pub pw_envelope: PwEnvelope,

    // pitch
    pub cents_drift_max: Cents,
    pub pitch_behavior: PitchBehavior,
    pub pitch_range_semitones: u8,
    /// Detected arpeggio loop as `(body, phase_offset)` — the relative
    /// semitone pattern and how many frames of prefix precede the cycle.
    pub pitch_relative_loop: Option<(Vec<i8>, u8)>,

    // filter
    pub filter_routed: bool,
    pub filter_automated: bool,
    pub filter_contour: FilterContour,
    /// Filter mode (LP/BP/HP) at the note's first frame.
    pub filter_mode: FilterMode,
    /// Filter resonance at the note's first frame.
    pub filter_resonance: Resonance,

    // hardware + derived
    pub hardware_tricks: Vec<HardwareTrick>,
    /// When ring-modulation is active, the frequency of the *modulating* voice
    /// (SID ring-mods voice N against voice N−1's oscillator). This is the ring
    /// carrier the export needs — a fixed pitch independent of the played note.
    pub ring_source_hz: Option<Hertz>,
    /// When hard sync is active, the frequency of the *master* voice (SID syncs
    /// voice N's accumulator to voice N−1's MSB edge) — same neighbour capture
    /// as [`Self::ring_source_hz`].
    pub sync_source_hz: Option<Hertz>,
    pub role_tags: RoleTags,
}

impl NoteCharacteristics {
    /// `true` if any frame had ≥ 2 waveform bits active simultaneously.
    /// Derived from `hardware_tricks` so storage stays single-source.
    #[must_use]
    pub fn has_combined_waveform(&self) -> bool {
        self.hardware_tricks
            .iter()
            .any(|t| matches!(t, HardwareTrick::CombinedWaveform(_)))
    }

    /// Encoded waveform byte of the note's first **audible** frame — the first
    /// frame carrying any waveform bit. A hard-restart gates the note a frame
    /// before the player writes the waveform, so the literal gate-on frame is
    /// often silent (`0x00`); ~32 % of HVSC notes start that way, and taking the
    /// first frame verbatim would mis-default 6 % of all notes to pulse when
    /// their real waveform is sawtooth/triangle. `0x00` only when the note never
    /// sounds (all frames silent).
    #[must_use]
    pub fn first_waveform_byte(&self) -> u8 {
        self.waveform_primary
            .iter()
            .map(|w| w.to_control_byte())
            .find(|&b| b != 0)
            .unwrap_or(0)
    }

    /// Encoded waveform byte the note holds longest **while gated** — its
    /// timbral character. A pulse body with a 1–2 frame triangle or noise attack
    /// tick exports as pulse, not the misleading first-audible attack waveform.
    /// The patch key and the exported oscillator both use this. Computed by
    /// [`dominant_gated_waveform`] at extraction; `0x00` only when the note never
    /// sounds.
    #[must_use]
    pub fn dominant_waveform_byte(&self) -> u8 {
        self.dominant_waveform
    }

    /// The note's **one-shot waveform program**: per-frame waveform masks
    /// (control-byte bits 4..=7 shifted down) from note start through the last
    /// waveform change, when the tail then holds a single mask — the Hubbard
    /// drum/stab idiom (`T N T P N N N N N P P P` then pulse to the end).
    /// Restricted to programs that touch **noise** (that is the audible win;
    /// tonal-only switches stay on the dominant waveform), fit the sid module's
    /// 16 seq steps, and never pass through a silent mask. Loops are the
    /// alternation idiom and are handled separately — this is `None` for them.
    #[must_use]
    pub fn waveform_program(&self) -> Option<Vec<u8>> {
        let SeqOrLoop::Raw(bytes) = &self.waveform_sequence else {
            return None;
        };
        let masks: Vec<u8> = bytes.iter().map(|b| (b >> 4) & 0x0F).collect();
        // Shrink the constant tail down to one held step (the module holds the
        // last seq step when `seq_loop` is off).
        let mut len = masks.len();
        while len > 1 && masks[len - 2] == masks[len - 1] {
            len -= 1;
        }
        if !(2..=16).contains(&len) {
            return None;
        }
        let prefix = &masks[..len];
        if prefix.contains(&0) {
            return None;
        }
        let has_noise = prefix.iter().any(|&m| m & 0x8 != 0);
        let mut distinct: Vec<u8> = prefix.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        (has_noise && distinct.len() >= 2).then(|| prefix.to_vec())
    }
}

/// Encoded waveform byte the note holds longest while **gated** (sounding):
/// counts only frames whose gate bit is set and whose waveform register is
/// non-zero, so a release / inter-note frame where the driver parks the register
/// at noise cannot hijack the pick (the bug that flipped ~57 % of Commando's
/// notes to noise). Ties resolve to the earliest-appearing waveform. Falls back
/// to the first audible waveform across the whole span when no gated audible
/// frame exists; `0x00` only when the note never sounds.
fn dominant_gated_waveform(span: &[FrameState], voice_idx: usize) -> u8 {
    let mut counts = [0u32; 256];
    let mut first_seen = [u32::MAX; 256];
    let mut order = 0u32;
    for state in span {
        let v = &state.voices[voice_idx];
        if !v.control.gate {
            continue;
        }
        let b = usize::from(v.control.waveform.to_control_byte());
        if b == 0 {
            continue;
        }
        if counts[b] == 0 {
            first_seen[b] = order;
            order += 1;
        }
        counts[b] += 1;
    }
    (0..256usize)
        .filter(|&b| counts[b] > 0)
        // Most-held wins; on a tie the earliest-appearing waveform.
        .max_by(|&a, &b| {
            counts[a]
                .cmp(&counts[b])
                .then(first_seen[b].cmp(&first_seen[a]))
        })
        .map_or_else(
            || {
                span.iter()
                    .map(|s| s.voices[voice_idx].control.waveform.to_control_byte())
                    .find(|&b| b != 0)
                    .unwrap_or(0)
            },
            |b| b as u8,
        )
}

/// Coarse attack-time bucket, derived from the ADSR.attack register
/// value at gate-on. Matches the canonical SID attack-rate table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum AttackClass {
    /// ADSR.attack ∈ {0, 1} — ≤ 8 ms.
    #[default]
    Instant,
    /// ADSR.attack ∈ {2..=4} — 16–38 ms (1–2 frames at 50 Hz).
    Fast,
    /// ADSR.attack ∈ {5..=8} — 56–100 ms (3–5 frames).
    Medium,
    /// ADSR.attack ∈ {9..=15} — 250 ms or longer.
    Slow,
}

impl AttackClass {
    /// Classify by the 4-bit ADSR.attack value (0..=15).
    #[must_use]
    pub fn from_adsr_attack(byte: u8) -> Self {
        match byte & 0x0F {
            0..=1 => Self::Instant,
            2..=4 => Self::Fast,
            5..=8 => Self::Medium,
            _ => Self::Slow,
        }
    }
}

/// What happened when the gate fell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum ReleaseClass {
    /// ADSR.release ∈ {0, 1} — note cuts essentially immediately.
    #[default]
    GateCut,
    /// ADSR.release ∈ {2..=8} — short audible tail (24–240 ms).
    NaturalDecay,
    /// ADSR.release ∈ {9..=15}, **or** the note never released
    /// (still active when the trace ends).
    Sustained,
}

impl ReleaseClass {
    #[must_use]
    pub fn from_release_byte(byte: u8, released: bool) -> Self {
        if !released {
            return Self::Sustained;
        }
        match byte & 0x0F {
            0..=1 => Self::GateCut,
            2..=8 => Self::NaturalDecay,
            _ => Self::Sustained,
        }
    }
}

/// Shape of a continuous parameter (PW, cutoff) over the note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum ContourKind {
    /// Range below the static-jitter threshold.
    #[default]
    Static,
    /// Monotonically non-decreasing.
    RisingRamp,
    /// Monotonically non-increasing.
    FallingRamp,
    /// Two or more direction reversals (LFO/PWM modulation).
    Triangle,
    /// Changes without an obvious pattern.
    Random,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct PwEnvelope {
    pub min: PulseWidth,
    pub max: PulseWidth,
    pub kind: ContourKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct FilterContour {
    pub min: Cutoff,
    pub max: Cutoff,
    pub kind: ContourKind,
}

/// Coarse classification of pitch behavior over the note's lifetime.
///
/// The metric details that the plan-skiss-version of this enum
/// carried (vibrato depth, arpeggio period, etc.) live on the source
/// [`EffectSpan`](crate::analysis::effects::EffectSpan) objects —
/// query by voice + frame range. Keeping `PitchBehavior` flat avoids
/// duplicating data that the effects detector is the authority for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum PitchBehavior {
    #[default]
    Stable,
    Vibrato,
    Portamento,
    Arpeggio,
    OneShotSweep,
}

/// Either a raw sequence or one we successfully identified as a
/// short repeating loop. 1b-and-later type; included now so the
/// `NoteCharacteristics` signature is stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SeqOrLoop<T> {
    Raw(Vec<T>),
    Loop { body: Vec<T>, offset: u8 },
}

impl<T> Default for SeqOrLoop<T> {
    fn default() -> Self {
        Self::Raw(Vec::new())
    }
}

impl<T: Clone> SeqOrLoop<T> {
    /// `Some(body.clone())` when the sequence was detected as a loop;
    /// `None` for the `Raw` fallback.
    #[must_use]
    pub fn loop_body_cloned(&self) -> Option<Vec<T>> {
        match self {
            Self::Loop { body, .. } => Some(body.clone()),
            Self::Raw(_) => None,
        }
    }
}

/// 1c — kinds of SID-specific hardware tricks an analyzer can spot.
///
/// JSON shape uses an internally-tagged `kind` discriminator so every
/// element of a `hardware_tricks` array has a uniform `{"kind": "...", ...}`
/// shape, including unit variants. Downstream consumers can filter on
/// `kind` without special-casing the variants that happen to carry data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind")]
pub enum HardwareTrick {
    CombinedWaveform(WaveformCombo),
    TestBitUsage,
    MidNoteWaveformSwitch {
        from: u8,
        to: u8,
        frame_offset: u8,
    },
    /// `$D418`-driven sample playback overlapping the note. Field is
    /// the count of frames where a chip-global `Effect::Sample` span
    /// overlapped this note — a proxy for write density, since the
    /// exact write count is not carried on `EffectSpan`.
    D418Sample {
        overlap_frames: u16,
    },
    /// Hard-sync bit was set on this voice at some point during the note.
    HardSync,
    /// Ring-mod bit was set on this voice at some point during the note.
    RingMod,
    /// Set by Slice 3 (emu-bus tracks reads from `$D41B`).
    Voice3LfoSource,
}

/// Which pair of waveform bits were combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum WaveformCombo {
    /// Triangle + Sawtooth — "bell" timbre.
    TriSaw,
    /// Pulse + Triangle — hollow, ring-mod-like.
    PulseTri,
    /// Pulse + Sawtooth — thin, nasal.
    PulseSaw,
    /// Any combo involving Noise — "noise lock" (silence in practice
    /// on real hardware, but composers used it as a percussion gate).
    NoiseLock,
    /// Three or four bits active at once.
    Triple,
}

/// 1d — multi-tag role flags. A single note can be percussive *and*
/// bell-like; we don't force exact-one-class taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
pub struct RoleTags {
    pub percussive: bool,
    pub drum_subclass: Option<DrumSubclass>,
    pub bass: bool,
    pub lead: bool,
    pub pad: bool,
    pub stab: bool,
    pub bell: bool,
    pub sample: bool,
    pub sound_effect: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum DrumSubclass {
    Kick,
    Snare,
    HihatClosed,
    HihatOpen,
    Tom,
    PercMetallic,
}

impl std::fmt::Display for DrumSubclass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Kick => "kick",
            Self::Snare => "snare",
            Self::HihatClosed => "hh-cl",
            Self::HihatOpen => "hh-op",
            Self::Tom => "tom",
            Self::PercMetallic => "perc",
        })
    }
}

impl std::fmt::Display for RoleTags {
    /// Comma-joined active flags. A percussive note shows its
    /// drum-subclass name (kick/snare/…) in place of "percussive" if
    /// set, otherwise "perc?". A note with zero active flags renders
    /// as "-".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut tags: Vec<String> = Vec::new();
        if self.percussive {
            tags.push(
                self.drum_subclass
                    .map_or_else(|| "perc?".to_string(), |d| d.to_string()),
            );
        }
        if self.bass {
            tags.push("bass".into());
        }
        if self.lead {
            tags.push("lead".into());
        }
        if self.pad {
            tags.push("pad".into());
        }
        if self.stab {
            tags.push("stab".into());
        }
        if self.bell {
            tags.push("bell".into());
        }
        if self.sample {
            tags.push("sample".into());
        }
        if self.sound_effect {
            tags.push("fx".into());
        }
        if tags.is_empty() {
            f.write_str("-")
        } else {
            f.write_str(&tags.join(","))
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// extraction
// ─────────────────────────────────────────────────────────────────

/// Range below which a PW or cutoff sweep is considered static
/// jitter. SID PW is 12-bit (0..=4095); 32 ≈ < 1 % range.
const STATIC_RANGE_THRESHOLD: u16 = 32;

/// Build characteristics for a single note from the per-frame state
/// stream + the run's detected effect spans.
pub fn extract_characteristics(
    note: &NoteEvent,
    states: &[FrameState],
    spans: &[EffectSpan],
    clock: SystemClock,
) -> NoteCharacteristics {
    let voice_idx = note.voice.to_index();
    let range = note.frame_range(states.len());
    let (start_idx, end_idx) = (range.start, range.end);

    if start_idx >= end_idx {
        return NoteCharacteristics::default();
    }

    let span = &states[start_idx..end_idx];
    let start_voice = &span[0].voices[voice_idx];

    let starting_adsr = start_voice.adsr;
    let attack = AttackClass::from_adsr_attack(start_voice.adsr.attack);

    let released = note.end_frame.is_some();
    let release_byte = end_state_release(span, voice_idx);
    let release_behavior = ReleaseClass::from_release_byte(release_byte, released);

    let (waveform_primary, waveform_switches, noise_share) = spectral_summary(span, voice_idx);
    let (noise_freq_hz, noise_run_frames) = noise_interleave(span, voice_idx, clock);

    let pw_envelope = pw_envelope(span, voice_idx);

    let (filter_routed, filter_automated, filter_contour, filter_mode, filter_resonance) =
        filter_summary(span, note.voice);

    let cents_drift_max = cents_drift(note, span, voice_idx, clock);

    let per_frame_midi = per_frame_midi_notes(span, voice_idx, clock);
    let pitch_range_semitones = midi_range_semitones(&per_frame_midi);
    let relative_pitch = relative_pitch_stream(&per_frame_midi, note.midi);
    // A loop whose whole excursion is <= 1 semitone is quantization jitter — a
    // vibrato swing that occasionally rounds across a note boundary — not a chord
    // arpeggio. Real SID arps span thirds/fourths/octaves (>= 2 semitones).
    let pitch_relative_loop = detect_loop(&relative_pitch, DEFAULT_TOLERANCE)
        .filter(|(body, _)| body.iter().any(|&d| d.unsigned_abs() > 1));
    let waveform_sequence = waveform_sequence(span, voice_idx);
    let mut dominant_waveform = dominant_gated_waveform(span, voice_idx);
    if dominant_waveform == 0 {
        let sound_end = note
            .sound_end_frame(states)
            .map_or(states.len(), |frame| frame.0 as usize)
            .min(states.len())
            .max(end_idx);
        dominant_waveform = dominant_gated_waveform(&states[start_idx..sound_end], voice_idx);
    }
    let pitch_behavior = classify_pitch_behavior(
        note,
        spans,
        pitch_range_semitones,
        pitch_relative_loop.is_some(),
    );

    let hardware_tricks = detect_hardware_tricks(note, span, voice_idx, spans);
    let ring_source_hz = hardware_tricks
        .iter()
        .any(|t| matches!(t, HardwareTrick::RingMod))
        .then(|| neighbour_source_frequency(span, voice_idx, clock, |c| c.ring_mod))
        .flatten();
    let sync_source_hz = hardware_tricks
        .iter()
        .any(|t| matches!(t, HardwareTrick::HardSync))
        .then(|| neighbour_source_frequency(span, voice_idx, clock, |c| c.sync))
        .flatten();

    let mut characteristics = NoteCharacteristics {
        starting_adsr,
        attack,
        length_frames: u16::try_from(span.len()).unwrap_or(u16::MAX),
        release_behavior,
        waveform_primary,
        waveform_switches,
        noise_share,
        noise_freq_hz,
        noise_run_frames,
        pw_envelope,
        filter_routed,
        filter_automated,
        filter_contour,
        filter_mode,
        filter_resonance,
        cents_drift_max,
        pitch_behavior,
        pitch_range_semitones,
        pitch_relative_loop,
        waveform_sequence,
        dominant_waveform,
        hardware_tricks,
        ring_source_hz,
        sync_source_hz,
        role_tags: RoleTags::default(),
    };
    characteristics.role_tags = derive_role_tags(note, &characteristics);
    characteristics
}

fn end_state_release(span: &[FrameState], voice_idx: usize) -> u8 {
    span.last().map_or(0, |s| s.voices[voice_idx].adsr.release)
}

fn spectral_summary(span: &[FrameState], voice_idx: usize) -> (Vec<Waveform>, u8, f32) {
    let mut distinct: Vec<Waveform> = Vec::new();
    let mut switches = 0u8;
    let mut noise_frames = 0u32;

    let mut prev_byte: Option<u8> = None;

    for state in span {
        let wf = state.voices[voice_idx].control.waveform;
        let byte = wf.to_control_byte();

        if !distinct.contains(&wf) {
            distinct.push(wf);
        }

        if wf.noise {
            noise_frames += 1;
        }

        if let Some(prev) = prev_byte
            && prev != byte
        {
            switches = switches.saturating_add(1);
        }
        prev_byte = Some(byte);
    }

    let noise_share = noise_frames as f32 / span.len() as f32;
    (distinct, switches, noise_share)
}

/// Median noise-frame frequency + mean noise-run length over the span. The
/// median (not first/mean) because a Hubbard drum sweeps the noise register
/// down through the hit — the middle of the sweep is the hit's colour.
fn noise_interleave(
    span: &[FrameState],
    voice_idx: usize,
    clock: SystemClock,
) -> (Option<Hertz>, f32) {
    let mut regs: Vec<u16> = Vec::new();
    let mut runs: Vec<u32> = Vec::new();
    let mut run = 0u32;
    for state in span {
        let v = &state.voices[voice_idx];
        if v.control.waveform.noise && v.freq.0 > 0 {
            regs.push(v.freq.0);
            run += 1;
        } else {
            if run > 0 {
                runs.push(run);
            }
            run = 0;
        }
    }
    if run > 0 {
        runs.push(run);
    }
    if regs.is_empty() {
        return (None, 0.0);
    }
    regs.sort_unstable();
    let median = SidFreq(regs[regs.len() / 2]);
    let mean_run = runs.iter().sum::<u32>() as f32 / runs.len() as f32;
    (Some(median.to_hertz(clock)), mean_run)
}

fn pw_envelope(span: &[FrameState], voice_idx: usize) -> PwEnvelope {
    let values: Vec<u16> = span
        .iter()
        .map(|s| s.voices[voice_idx].pulse_width.0)
        .collect();
    let (min, max) = min_max(&values);
    PwEnvelope {
        min: PulseWidth(min),
        max: PulseWidth(max),
        kind: classify_contour(&values, (min, max), STATIC_RANGE_THRESHOLD),
    }
}

fn filter_summary(
    span: &[FrameState],
    voice: VoiceId,
) -> (bool, bool, FilterContour, FilterMode, Resonance) {
    let routed = span.iter().any(|s| s.filter.routing.contains(voice));
    let cutoffs: Vec<u16> = span.iter().map(|s| s.filter.cutoff.0).collect();
    let (min, max) = min_max(&cutoffs);
    let cutoff_changed = max - min >= STATIC_RANGE_THRESHOLD;
    let automated = routed && cutoff_changed;
    let contour = FilterContour {
        min: Cutoff(min),
        max: Cutoff(max),
        kind: classify_contour(&cutoffs, (min, max), STATIC_RANGE_THRESHOLD),
    };
    // Representative mode + resonance from the note's first frame.
    let (mode, resonance) = span
        .first()
        .map_or((FilterMode::default(), Resonance::default()), |s| {
            (s.filter.mode, s.filter.resonance)
        });
    (routed, automated, contour, mode, resonance)
}

fn cents_drift(
    note: &NoteEvent,
    span: &[FrameState],
    voice_idx: usize,
    clock: SystemClock,
) -> Cents {
    let initial_midi = note.midi.0;
    let mut max_abs = 0.0_f32;
    for state in span {
        let freq = state.voices[voice_idx].freq;
        let hz = freq.to_hertz(clock);
        if let Some((m, c)) = hertz_to_midi(hz) {
            let delta_cents =
                (i32::from(m.0) - i32::from(initial_midi)) as f32 * 100.0 + c.0 - note.cents.0;
            let abs = delta_cents.abs();
            if abs > max_abs {
                max_abs = abs;
            }
        }
    }
    Cents(max_abs)
}

fn min_max(values: &[u16]) -> (u16, u16) {
    if values.is_empty() {
        return (0, 0);
    }
    values
        .iter()
        .copied()
        .fold((u16::MAX, 0u16), |(lo, hi), v| (lo.min(v), hi.max(v)))
}

/// Heuristic shape classifier for monotonically-bounded streams.
///
/// - `Static` when range below `static_threshold`.
/// - `RisingRamp` / `FallingRamp` when strictly monotonic above the
///   threshold (allow tiny stationary stretches but no direction
///   reversals).
/// - `Triangle` when ≥ 2 direction reversals with comparable
///   amplitudes (LFO/PWM).
/// - `Random` otherwise.
///
/// Takes precomputed `(min, max)` so callers that already need those
/// values for the envelope struct don't pay for a second pass.
fn classify_contour(values: &[u16], min_max: (u16, u16), static_threshold: u16) -> ContourKind {
    if values.len() < 2 {
        return ContourKind::Static;
    }
    let (min, max) = min_max;
    if max - min < static_threshold {
        return ContourKind::Static;
    }

    let mut direction: i8 = 0;
    let mut reversals = 0u8;
    let mut went_up = false;
    let mut went_down = false;
    for pair in values.windows(2) {
        let cmp = pair[1] as i32 - pair[0] as i32;
        if cmp == 0 {
            continue;
        }
        let sign: i8 = if cmp > 0 { 1 } else { -1 };
        if sign > 0 {
            went_up = true;
        } else {
            went_down = true;
        }
        if direction != 0 && sign != direction {
            reversals = reversals.saturating_add(1);
        }
        direction = sign;
    }

    if reversals >= 2 {
        return ContourKind::Triangle;
    }
    if went_up && !went_down {
        return ContourKind::RisingRamp;
    }
    if went_down && !went_up {
        return ContourKind::FallingRamp;
    }
    ContourKind::Random
}

// ─────────────────────────────────────────────────────────────────
// 1b: pitch behavior + loop detection
// ─────────────────────────────────────────────────────────────────

/// Per-frame MIDI note for the given voice, or `None` for silent frames.
fn per_frame_midi_notes(
    span: &[FrameState],
    voice_idx: usize,
    clock: SystemClock,
) -> Vec<Option<MidiNote>> {
    span.iter()
        .map(|s| {
            let v = &s.voices[voice_idx];
            // A noise-only frame has no musical pitch — its freq register is
            // noise colour, not a note. `relative_pitch_stream` carries the
            // previous tonal value across the resulting `None`, so a triangle/
            // noise alternation lead reads as a held tone, not a phantom arp.
            if v.control.waveform.is_noise_only() {
                return None;
            }
            hertz_to_midi(v.freq.to_hertz(clock)).map(|(m, _)| m)
        })
        .collect()
}

/// Range in semitones between the lowest and highest MIDI notes
/// observed during the note. Ignores silent frames.
fn midi_range_semitones(per_frame: &[Option<MidiNote>]) -> u8 {
    let mut min = u8::MAX;
    let mut max = 0u8;
    let mut seen = false;
    for m in per_frame.iter().flatten() {
        seen = true;
        if m.0 < min {
            min = m.0;
        }
        if m.0 > max {
            max = m.0;
        }
    }
    if !seen { 0 } else { max - min }
}

/// Per-frame semitone offset from the note's starting MIDI pitch,
/// clamped to `i8`. Silent frames carry forward the previous value.
fn relative_pitch_stream(per_frame: &[Option<MidiNote>], base: MidiNote) -> Vec<i8> {
    let mut last_value: i8 = 0;
    per_frame
        .iter()
        .map(|m| {
            if let Some(midi) = m {
                let delta = i32::from(midi.0) - i32::from(base.0);
                last_value = delta.clamp(i8::MIN as i32, i8::MAX as i32) as i8;
            }
            last_value
        })
        .collect()
}

/// Build the per-frame waveform-byte sequence and look for a cycle.
/// Falls back to `Raw` when no loop is detected (including the
/// constant-waveform case — see `loops::detect_loop`).
fn waveform_sequence(span: &[FrameState], voice_idx: usize) -> SeqOrLoop<u8> {
    let bytes: Vec<u8> = span
        .iter()
        .map(|s| s.voices[voice_idx].control.waveform.to_control_byte())
        .collect();
    match detect_loop(&bytes, DEFAULT_TOLERANCE) {
        Some((body, offset)) => SeqOrLoop::Loop { body, offset },
        None => SeqOrLoop::Raw(bytes),
    }
}

/// Classify pitch behavior. Effect spans dominate when present; the
/// relative-pitch loop is a secondary signal for arpeggio.
fn classify_pitch_behavior(
    note: &NoteEvent,
    spans: &[EffectSpan],
    pitch_range_semitones: u8,
    relative_pitch_loops: bool,
) -> PitchBehavior {
    let note_start = note.start_frame.0;
    // The note's `end_frame` is exclusive; the span's is inclusive.
    let note_end = note.end_frame.map_or(u32::MAX, |f| f.0);

    let mut has_vibrato = false;
    let mut has_portamento = false;
    let mut has_arpeggio = false;

    for span in spans {
        if span.voice != Some(note.voice) {
            continue;
        }
        if span.start_frame.0 >= note_end || span.end_frame.0 < note_start {
            continue;
        }
        match span.effect {
            Effect::Vibrato => has_vibrato = true,
            Effect::Portamento => has_portamento = true,
            Effect::Arpeggio => has_arpeggio = true,
            _ => {}
        }
    }

    if has_arpeggio || relative_pitch_loops {
        return PitchBehavior::Arpeggio;
    }
    if has_vibrato {
        return PitchBehavior::Vibrato;
    }
    if has_portamento {
        return PitchBehavior::Portamento;
    }
    // No detected effect span — but if the note's MIDI note range
    // spans ≥ 2 semitones without forming a loop, it's a one-shot
    // sweep (typical kick-drum-style pitch drop).
    if pitch_range_semitones >= 2 {
        return PitchBehavior::OneShotSweep;
    }
    PitchBehavior::Stable
}

// ─────────────────────────────────────────────────────────────────
// 1c: hardware tricks
// ─────────────────────────────────────────────────────────────────

/// First frame offset after which a mid-note waveform switch is
/// considered "intentional" rather than gate-on setup. Hubbard's
/// brus-snare switches Pulse → Noise around frame 2-4.
const MID_NOTE_SWITCH_FRAME_THRESHOLD: u8 = 2;

fn detect_hardware_tricks(
    note: &NoteEvent,
    span: &[FrameState],
    voice_idx: usize,
    spans: &[EffectSpan],
) -> Vec<HardwareTrick> {
    let mut tricks: Vec<HardwareTrick> = Vec::new();

    let mut combos_seen: Vec<WaveformCombo> = Vec::new();
    let mut test_bit_seen = false;
    let mut hard_sync_seen = false;
    let mut ring_mod_seen = false;
    let mut first_mid_switch: Option<(u8, u8, u8)> = None;
    let mut prev_byte: Option<u8> = None;

    for (i, state) in span.iter().enumerate() {
        let voice = &state.voices[voice_idx];
        let wf = voice.control.waveform;

        if voice.control.test {
            test_bit_seen = true;
        }
        if voice.control.sync {
            hard_sync_seen = true;
        }
        if voice.control.ring_mod {
            ring_mod_seen = true;
        }

        if wf.active_bits() >= 2 {
            let combo = classify_waveform_combo(wf);
            if !combos_seen.contains(&combo) {
                combos_seen.push(combo);
            }
        }

        let byte = wf.to_control_byte();
        if let Some(prev) = prev_byte
            && prev != byte
            && i as u8 > MID_NOTE_SWITCH_FRAME_THRESHOLD
            && first_mid_switch.is_none()
        {
            first_mid_switch = Some((prev, byte, i as u8));
        }
        prev_byte = Some(byte);
    }

    for combo in combos_seen {
        tricks.push(HardwareTrick::CombinedWaveform(combo));
    }
    if test_bit_seen {
        tricks.push(HardwareTrick::TestBitUsage);
    }
    if hard_sync_seen {
        tricks.push(HardwareTrick::HardSync);
    }
    if ring_mod_seen {
        tricks.push(HardwareTrick::RingMod);
    }
    if let Some((from, to, frame_offset)) = first_mid_switch {
        tricks.push(HardwareTrick::MidNoteWaveformSwitch {
            from,
            to,
            frame_offset,
        });
    }
    if let Some(overlap) = d418_overlap_frames(note, spans) {
        tricks.push(HardwareTrick::D418Sample {
            overlap_frames: overlap,
        });
    }

    tricks
}

/// The ring-modulating voice's frequency: SID ring-mods voice N against voice
/// N−1 (with wraparound). Returns the first non-zero source frequency on a frame
/// where this voice's ring bit is set — a fixed carrier pitch the export uses,
/// independent of the played note.
/// The neighbour (voice N−1, the SID's ring/sync source) frequency on the first
/// frame where `active` reads true on this voice's control bits — the fixed
/// modulator/master pitch the export needs.
fn neighbour_source_frequency(
    span: &[FrameState],
    voice_idx: usize,
    clock: SystemClock,
    active: impl Fn(&ControlBits) -> bool,
) -> Option<Hertz> {
    let source_idx = (voice_idx + 2) % 3;
    for state in span {
        if active(&state.voices[voice_idx].control) {
            let f = state.voices[source_idx].freq;
            if f.0 > 0 {
                return Some(f.to_hertz(clock));
            }
        }
    }
    None
}

fn classify_waveform_combo(wf: Waveform) -> WaveformCombo {
    if wf.active_bits() >= 3 {
        return WaveformCombo::Triple;
    }
    if wf.noise {
        return WaveformCombo::NoiseLock;
    }
    match (wf.triangle, wf.sawtooth, wf.pulse) {
        (true, true, false) => WaveformCombo::TriSaw,
        (true, false, true) => WaveformCombo::PulseTri,
        (false, true, true) => WaveformCombo::PulseSaw,
        // Single-bit fallthrough — shouldn't be called with < 2 bits
        // active per the caller's guard, but stay defensive.
        _ => WaveformCombo::Triple,
    }
}

/// Frame-overlap between this note and any chip-global `Effect::Sample`
/// span. Multiple overlapping spans are summed. Returns `None` when no
/// overlap, otherwise the total in frames (saturating to `u16::MAX`).
fn d418_overlap_frames(note: &NoteEvent, spans: &[EffectSpan]) -> Option<u16> {
    let note_start = note.start_frame.0;
    // The note's `end_frame` is exclusive; the span's is inclusive, so the
    // intersection is computed half-open with the span end bumped past-one.
    let note_end = note.end_frame.map_or(u32::MAX, |f| f.0);

    let mut total: u32 = 0;
    for span in spans {
        if span.effect != Effect::Sample {
            continue;
        }
        let s = span.start_frame.0.max(note_start);
        let e = span.end_frame.0.saturating_add(1).min(note_end);
        if s < e {
            total = total.saturating_add(e - s);
        }
    }
    if total == 0 {
        None
    } else {
        Some(u16::try_from(total).unwrap_or(u16::MAX))
    }
}

// ─────────────────────────────────────────────────────────────────
// 1d: role-tag derivation
// ─────────────────────────────────────────────────────────────────

/// Pitch threshold (MIDI) below which we consider a note bass-range.
const BASS_PITCH_MAX: u8 = 48;
/// Length threshold above which a slow-attack note counts as a pad.
const PAD_MIN_LENGTH_FRAMES: u16 = 50;
/// Percussion length cap. Above this, even short attacks aren't percussion.
/// 24 frames ≈ 480 ms PAL — wide enough for long tom/snare decay tails; the
/// melodic-pitch guard in [`is_percussive`] keeps long leads out of the bucket.
const PERCUSSIVE_MAX_LENGTH_FRAMES: u16 = 24;
/// Pitch-range trigger for "sound effect" classification (semitones).
const SOUND_EFFECT_PITCH_RANGE: u8 = 24;

/// Boolean rollup of `hardware_tricks`, computed once at the top of
/// `derive_role_tags` so the rules can read fields directly instead
/// of walking the `Vec` over and over.
#[derive(Debug, Default, Clone, Copy)]
struct TrickFlags {
    has_tri_saw_combo: bool,
    hard_sync: bool,
    ring_mod: bool,
    d418_sample: bool,
}

impl TrickFlags {
    fn from_tricks(tricks: &[HardwareTrick]) -> Self {
        let mut f = Self::default();
        for t in tricks {
            match t {
                HardwareTrick::CombinedWaveform(WaveformCombo::TriSaw) => {
                    f.has_tri_saw_combo = true;
                }
                HardwareTrick::HardSync => f.hard_sync = true,
                HardwareTrick::RingMod => f.ring_mod = true,
                HardwareTrick::D418Sample { .. } => f.d418_sample = true,
                _ => {}
            }
        }
        f
    }
}

/// Derive multi-tag role flags + drum subclass from the characteristics.
///
/// Pure function. Tags are not mutually exclusive — a note can be
/// `percussive + bell` simultaneously (Hubbard brus-snare with RingMod
/// or TriSaw combo). Drum-subclass is only populated when `percussive`
/// is true.
#[must_use]
pub fn derive_role_tags(note: &NoteEvent, c: &NoteCharacteristics) -> RoleTags {
    let pitch = note.midi.0;
    let trick_flags = TrickFlags::from_tricks(&c.hardware_tricks);
    let percussive = is_percussive(c, &trick_flags);
    let drum_subclass = if percussive {
        derive_drum_subclass_with_flags(c, &trick_flags)
    } else {
        None
    };

    // Melodic-only rules — only meaningful when the note is not percussion.
    let (bass, stab, sound_effect_from_sweep) = if percussive {
        (false, false, false)
    } else {
        (
            pitch < BASS_PITCH_MAX && c.length_frames > 8,
            (8..=24).contains(&c.length_frames) && c.attack == AttackClass::Instant,
            matches!(c.pitch_behavior, PitchBehavior::OneShotSweep)
                && c.pitch_range_semitones >= 12,
        )
    };

    let lead = pitch >= BASS_PITCH_MAX && has_lead_modulation(c);
    let pad = c.length_frames > PAD_MIN_LENGTH_FRAMES
        && matches!(c.attack, AttackClass::Medium | AttackClass::Slow);
    let bell = c.has_combined_waveform() && (trick_flags.has_tri_saw_combo || trick_flags.ring_mod);
    let sample = trick_flags.d418_sample;
    let sound_effect =
        c.pitch_range_semitones > SOUND_EFFECT_PITCH_RANGE || sound_effect_from_sweep;

    RoleTags {
        percussive,
        drum_subclass,
        bass,
        lead,
        pad,
        stab,
        bell,
        sample,
        sound_effect,
    }
}

fn is_percussive(c: &NoteCharacteristics, flags: &TrickFlags) -> bool {
    if c.length_frames > PERCUSSIVE_MAX_LENGTH_FRAMES {
        return false;
    }
    if !matches!(c.attack, AttackClass::Instant | AttackClass::Fast) {
        return false;
    }
    if matches!(
        c.pitch_behavior,
        PitchBehavior::Vibrato | PitchBehavior::Portamento | PitchBehavior::Arpeggio
    ) {
        return false;
    }
    // A note that spans ≥ 2 semitones without being a one-shot downward drop is
    // melodic, not a drum hit. With the looser 24-frame cap this keeps long
    // ring/sync/noise *leads* (Nemesis V2, Knucklebusters) out of the drum
    // bucket; the one-shot pitch drop (kick/tom) stays allowed below.
    if c.pitch_range_semitones >= 2 && !matches!(c.pitch_behavior, PitchBehavior::OneShotSweep) {
        return false;
    }
    // Short + instant + non-modulated alone is a stab, not percussion —
    // Galway's fast pulse-leads satisfy those bounds too. Require at least
    // one positive percussive signal: noise (snare/hihat), a one-shot pitch
    // drop (kick/tom), or a metallic bit (RingMod/HardSync, Hubbard V2-perc).
    c.noise_share > 0.0
        || (matches!(c.pitch_behavior, PitchBehavior::OneShotSweep) && c.pitch_range_semitones >= 2)
        || flags.hard_sync
        || flags.ring_mod
}

fn has_lead_modulation(c: &NoteCharacteristics) -> bool {
    c.pw_envelope.kind != ContourKind::Static
        || matches!(
            c.pitch_behavior,
            PitchBehavior::Vibrato | PitchBehavior::Portamento
        )
}

/// Priority order (first match wins): `PercMetallic > HihatClosed >
/// HihatOpen > Snare > Kick > Tom`. The order is load-bearing —
/// e.g., Snare's `length_frames <= 12` would otherwise swallow
/// HihatOpen at length 12. Keep the cascade in this sequence when
/// adding new subclasses.
fn derive_drum_subclass_with_flags(
    c: &NoteCharacteristics,
    flags: &TrickFlags,
) -> Option<DrumSubclass> {
    let only_noise = c
        .waveform_primary
        .iter()
        .all(|w| w.noise && !w.triangle && !w.sawtooth && !w.pulse);

    // PercMetallic: hard-sync or ring-mod active + period-2 T/N alternation
    // detected in waveform_sequence (Nemesis V2 idiom).
    if (flags.hard_sync || flags.ring_mod) && period_two_alternation(&c.waveform_sequence) {
        return Some(DrumSubclass::PercMetallic);
    }
    // HihatClosed: very short gate + pure noise.
    if c.length_frames <= 4 && only_noise {
        return Some(DrumSubclass::HihatClosed);
    }
    // HihatOpen: medium-short gate + pure noise + no waveform switches.
    if (5..=15).contains(&c.length_frames) && only_noise && c.waveform_switches == 0 {
        return Some(DrumSubclass::HihatOpen);
    }
    // Snare: short gate + noise present (often with mid-note switch).
    if c.length_frames <= 12 && c.noise_share > 0.0 {
        return Some(DrumSubclass::Snare);
    }
    // Kick: short gate + downward pitch sweep > 1 octave + pulse/triangle primary.
    if c.length_frames <= 8
        && matches!(c.pitch_behavior, PitchBehavior::OneShotSweep)
        && c.pitch_range_semitones >= 12
        && c.waveform_primary.iter().any(|w| w.pulse || w.triangle)
    {
        return Some(DrumSubclass::Kick);
    }
    // Tom: short gate + smaller downward sweep.
    if c.length_frames <= 12
        && matches!(c.pitch_behavior, PitchBehavior::OneShotSweep)
        && c.pitch_range_semitones < 12
        && c.pitch_range_semitones >= 2
    {
        return Some(DrumSubclass::Tom);
    }
    None
}

/// True when the waveform sequence is a 2-step alternation (T-N-T-N or
/// any A-B-A-B pattern). Used by the PercMetallic rule.
fn period_two_alternation(seq: &SeqOrLoop<u8>) -> bool {
    matches!(seq, SeqOrLoop::Loop { body, .. } if body.len() == 2)
}

// ─────────────────────────────────────────────────────────────────
// tests
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Volume;
    use crate::analysis::filter::FilterState;
    use crate::analysis::note::{GmProgram, MidiNote, Velocity};
    use crate::analysis::voice::{Adsr, ControlBits, SidFreq, VoiceState};
    use crate::trace::FrameIndex;

    fn voice_with(freq: u16, wf: Waveform, adsr: Adsr, pulse_width: u16, gate: bool) -> VoiceState {
        VoiceState {
            freq: SidFreq(freq),
            pulse_width: PulseWidth(pulse_width),
            control: ControlBits {
                gate,
                sync: false,
                ring_mod: false,
                test: false,
                waveform: wf,
            },
            adsr,
        }
    }

    fn frame(frame: u32, voice1: VoiceState) -> FrameState {
        FrameState {
            frame: FrameIndex(frame),
            voices: [voice1, VoiceState::default(), VoiceState::default()],
            filter: FilterState::default(),
            volume: Volume(15),
            ..FrameState::default()
        }
    }

    fn frame_with_filter(frame_idx: u32, voice1: VoiceState, filter: FilterState) -> FrameState {
        FrameState {
            frame: FrameIndex(frame_idx),
            voices: [voice1, VoiceState::default(), VoiceState::default()],
            filter,
            volume: Volume(15),
            ..FrameState::default()
        }
    }

    fn pulse() -> Waveform {
        Waveform {
            pulse: true,
            ..Default::default()
        }
    }

    fn noise() -> Waveform {
        Waveform {
            noise: true,
            ..Default::default()
        }
    }

    fn tri_saw() -> Waveform {
        Waveform {
            triangle: true,
            sawtooth: true,
            ..Default::default()
        }
    }

    fn adsr_full(a: u8, d: u8, s: u8, r: u8) -> Adsr {
        Adsr {
            attack: a,
            decay: d,
            sustain: s,
            release: r,
        }
    }

    fn simple_note(start: u32, end: u32, voice: u8) -> NoteEvent {
        NoteEvent {
            voice: VoiceId(voice),
            start_frame: FrameIndex(start),
            end_frame: Some(FrameIndex(end)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        }
    }

    #[test]
    fn attack_class_thresholds_match_canonical_table() {
        assert_eq!(AttackClass::from_adsr_attack(0), AttackClass::Instant);
        assert_eq!(AttackClass::from_adsr_attack(1), AttackClass::Instant);
        assert_eq!(AttackClass::from_adsr_attack(2), AttackClass::Fast);
        assert_eq!(AttackClass::from_adsr_attack(4), AttackClass::Fast);
        assert_eq!(AttackClass::from_adsr_attack(5), AttackClass::Medium);
        assert_eq!(AttackClass::from_adsr_attack(8), AttackClass::Medium);
        assert_eq!(AttackClass::from_adsr_attack(9), AttackClass::Slow);
        assert_eq!(AttackClass::from_adsr_attack(15), AttackClass::Slow);
    }

    #[test]
    fn release_class_treats_unreleased_notes_as_sustained() {
        assert_eq!(
            ReleaseClass::from_release_byte(0, false),
            ReleaseClass::Sustained
        );
        assert_eq!(
            ReleaseClass::from_release_byte(15, false),
            ReleaseClass::Sustained
        );
    }

    #[test]
    fn release_class_buckets_by_release_byte() {
        assert_eq!(
            ReleaseClass::from_release_byte(0, true),
            ReleaseClass::GateCut
        );
        assert_eq!(
            ReleaseClass::from_release_byte(1, true),
            ReleaseClass::GateCut
        );
        assert_eq!(
            ReleaseClass::from_release_byte(2, true),
            ReleaseClass::NaturalDecay
        );
        assert_eq!(
            ReleaseClass::from_release_byte(8, true),
            ReleaseClass::NaturalDecay
        );
        assert_eq!(
            ReleaseClass::from_release_byte(9, true),
            ReleaseClass::Sustained
        );
        assert_eq!(
            ReleaseClass::from_release_byte(15, true),
            ReleaseClass::Sustained
        );
    }

    #[test]
    fn contour_classifier_static_below_threshold() {
        let v = vec![1000, 1005, 998, 1010, 1003];
        let mm = min_max(&v);
        assert_eq!(classify_contour(&v, mm, 32), ContourKind::Static);
    }

    #[test]
    fn contour_classifier_rising_ramp() {
        let v = vec![100, 200, 300, 400, 500];
        let mm = min_max(&v);
        assert_eq!(classify_contour(&v, mm, 32), ContourKind::RisingRamp);
    }

    #[test]
    fn contour_classifier_falling_ramp() {
        let v = vec![2000, 1500, 1000, 500, 100];
        let mm = min_max(&v);
        assert_eq!(classify_contour(&v, mm, 32), ContourKind::FallingRamp);
    }

    #[test]
    fn contour_classifier_triangle_detects_reversals() {
        let v = vec![1000, 1500, 2000, 1500, 1000, 1500, 2000];
        let mm = min_max(&v);
        assert_eq!(classify_contour(&v, mm, 32), ContourKind::Triangle);
    }

    #[test]
    fn extract_envelope_and_spectral_on_pulse_note() {
        let pulse_voice = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..10).map(|i| frame(i, pulse_voice)).collect();

        // Exclusive end: the note is active on all ten frames [0, 10).
        let note = simple_note(0, 10, 1);
        let chars = extract_characteristics(&note, &states, &[], SystemClock::Pal);

        assert_eq!(chars.attack, AttackClass::Instant);
        assert_eq!(chars.length_frames, 10);
        assert_eq!(chars.release_behavior, ReleaseClass::NaturalDecay);
        assert_eq!(chars.waveform_primary, vec![pulse()]);
        assert!(!chars.has_combined_waveform());
        assert_eq!(chars.waveform_switches, 0);
        assert!((chars.noise_share - 0.0).abs() < 1e-6);
        assert_eq!(chars.pw_envelope.kind, ContourKind::Static);
        assert!(!chars.filter_routed);
    }

    #[test]
    fn mid_note_waveform_switch_is_counted() {
        let v_pulse = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let v_noise = voice_with(0x1234, noise(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..6)
            .map(|i| {
                if i < 3 {
                    frame(i, v_pulse)
                } else {
                    frame(i, v_noise)
                }
            })
            .collect();

        let note = simple_note(0, 6, 1);
        let chars = extract_characteristics(&note, &states, &[], SystemClock::Pal);

        assert_eq!(chars.waveform_switches, 1);
        assert_eq!(chars.waveform_primary, vec![pulse(), noise()]);
        assert!((chars.noise_share - 3.0 / 6.0).abs() < 1e-6);
    }

    #[test]
    fn combined_waveform_flag_triggers_on_tri_saw() {
        let v = voice_with(0x1234, tri_saw(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..4).map(|i| frame(i, v)).collect();
        let chars = extract_characteristics(&simple_note(0, 3, 1), &states, &[], SystemClock::Pal);
        assert!(chars.has_combined_waveform());
    }

    #[test]
    fn pwm_modulation_is_classified_as_triangle_contour() {
        // 5 sweep cycles to clearly produce ≥2 reversals.
        let widths = [0x0400, 0x0800, 0x0C00, 0x0800, 0x0400, 0x0800, 0x0C00];
        let states: Vec<FrameState> = widths
            .iter()
            .enumerate()
            .map(|(i, &w)| {
                let v = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), w, true);
                frame(i as u32, v)
            })
            .collect();

        let note = simple_note(0, (widths.len() - 1) as u32, 1);
        let chars = extract_characteristics(&note, &states, &[], SystemClock::Pal);
        assert_eq!(chars.pw_envelope.kind, ContourKind::Triangle);
        assert_eq!(chars.pw_envelope.min, PulseWidth(0x0400));
        assert_eq!(chars.pw_envelope.max, PulseWidth(0x0C00));
    }

    #[test]
    fn filter_routing_and_automation_detected_only_when_voice_routed() {
        let v = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let route_v1 = crate::analysis::filter::FilterRouting {
            voice1: true,
            ..Default::default()
        };
        let states: Vec<FrameState> = [0x100, 0x200, 0x300, 0x400, 0x500]
            .iter()
            .enumerate()
            .map(|(i, &cutoff)| {
                let f = FilterState {
                    cutoff: Cutoff(cutoff),
                    routing: route_v1,
                    ..Default::default()
                };
                frame_with_filter(i as u32, v, f)
            })
            .collect();

        let chars = extract_characteristics(&simple_note(0, 4, 1), &states, &[], SystemClock::Pal);
        assert!(chars.filter_routed);
        assert!(chars.filter_automated);
        assert_eq!(chars.filter_contour.kind, ContourKind::RisingRamp);
    }

    #[test]
    fn cents_drift_max_tracks_largest_pitch_deviation() {
        // Start at MIDI 60 (~261.6 Hz). SidFreq 0x1CD6 ≈ 262 Hz @ PAL.
        let start_freq = 0x1CD6;
        let high_freq = 0x1F40; // ~50 cents up
        let mk = |f: u16| voice_with(f, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);

        let states: Vec<FrameState> = [start_freq, start_freq, high_freq, start_freq]
            .iter()
            .enumerate()
            .map(|(i, &f)| frame(i as u32, mk(f)))
            .collect();

        let note = NoteEvent {
            voice: VoiceId(1),
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(3)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        };
        let chars = extract_characteristics(&note, &states, &[], SystemClock::Pal);
        assert!(
            chars.cents_drift_max.0 > 100.0,
            "expected > 100 cents drift, got {}",
            chars.cents_drift_max.0
        );
    }

    #[test]
    fn unreleased_note_uses_trace_end_and_is_sustained() {
        let v = voice_with(0x1234, pulse(), adsr_full(2, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..5).map(|i| frame(i, v)).collect();
        let note = NoteEvent {
            voice: VoiceId(1),
            start_frame: FrameIndex(0),
            end_frame: None,
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        };
        let chars = extract_characteristics(&note, &states, &[], SystemClock::Pal);
        assert_eq!(chars.length_frames, 5);
        assert_eq!(chars.release_behavior, ReleaseClass::Sustained);
    }

    // ─────────────────────────────────────────────────────────────
    // Slice 1b: pitch behavior + loop detection
    // ─────────────────────────────────────────────────────────────

    use crate::analysis::effects::Effect;
    use crate::trace::FrameIndex as FrameIdx;

    fn span_for(voice: u8, effect: Effect, start: u32, end: u32) -> EffectSpan {
        EffectSpan {
            effect,
            voice: Some(VoiceId(voice)),
            start_frame: FrameIdx(start),
            end_frame: FrameIdx(end),
        }
    }

    /// SidFreq values for a C-major arpeggio rooted at MIDI 60 (C4)
    /// on PAL clock: `60 → 0x1167`, `64 → 0x15ED`, `67 → 0x1A13`.
    /// Hand-computed from `Fn = midi_hz * 2^24 / 985_248`.
    fn arpeggio_states() -> Vec<FrameState> {
        let freqs = [0x1167u16, 0x15ED, 0x1A13];
        (0..12)
            .map(|i| {
                let f = freqs[i as usize % 3];
                let v = voice_with(f, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
                frame(i, v)
            })
            .collect()
    }

    #[test]
    fn pitch_behavior_is_arpeggio_when_relative_pitch_loops() {
        let states = arpeggio_states();
        let note = NoteEvent {
            voice: VoiceId(1),
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(11)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        };
        let chars = extract_characteristics(&note, &states, &[], SystemClock::Pal);
        assert_eq!(chars.pitch_behavior, PitchBehavior::Arpeggio);
        // Loop body should be the 3-step pattern.
        let (body, _offset) = chars
            .pitch_relative_loop
            .as_ref()
            .expect("loop should be detected");
        assert_eq!(body.len(), 3);
        assert!(body.contains(&0));
        assert!(body.contains(&4));
        assert!(body.contains(&7));
        assert!(chars.pitch_range_semitones >= 7);
    }

    #[test]
    fn triangle_noise_alternation_is_not_a_phantom_arpeggio() {
        // Hubbard triangle/noise lead: every other frame is a pure noise burst
        // whose freq register reads as E4 (0x15EB ≈ MIDI 64, 19 semitones below
        // the B5 tone at 0x41B8). Before excluding noise frames, `relative_pitch`
        // was [0, -19, 0, -19, …] and detect_loop reported a phantom arpeggio.
        let triangle = Waveform {
            triangle: true,
            ..Default::default()
        };
        let states: Vec<FrameState> = (0..16)
            .map(|i| {
                let (freq, wf) = if i % 2 == 0 {
                    (0x41B8u16, triangle) // B5 triangle
                } else {
                    (0x15EBu16, noise()) // noise burst: freq is colour, not pitch
                };
                frame(i, voice_with(freq, wf, adsr_full(0, 9, 8, 6), 0x800, true))
            })
            .collect();
        let note = NoteEvent {
            voice: VoiceId(1),
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(15)),
            midi: MidiNote(83),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        };
        let chars = extract_characteristics(&note, &states, &[], SystemClock::Pal);
        assert!(
            chars.pitch_relative_loop.is_none(),
            "noise frames must not invent an arpeggio: {:?}",
            chars.pitch_relative_loop
        );
        // Only the tonal frames (all B5) count toward the pitch range.
        assert_eq!(chars.pitch_range_semitones, 0);
    }

    #[test]
    fn one_semitone_quantization_flicker_is_not_an_arpeggio() {
        // A vibrato swing whose bottom rounds down one semitone every few frames
        // (0x24E0 = C#5 = MIDI 73, 0x22CE = C5 = MIDI 72) makes relative_pitch
        // [0, 0, 0, -1, …] — a period-4 loop, but a <= 1 semitone excursion is
        // jitter, not a chord arpeggio. The guard must reject it.
        let states: Vec<FrameState> = (0..16)
            .map(|i| {
                let freq = if i % 4 == 3 { 0x22CEu16 } else { 0x24E0u16 };
                frame(
                    i,
                    voice_with(freq, pulse(), adsr_full(0, 9, 8, 6), 0x800, true),
                )
            })
            .collect();
        let note = NoteEvent {
            voice: VoiceId(1),
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(15)),
            midi: MidiNote(73),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        };
        let chars = extract_characteristics(&note, &states, &[], SystemClock::Pal);
        assert!(
            chars.pitch_relative_loop.is_none(),
            "a <= 1 semitone flicker must not be an arpeggio: {:?}",
            chars.pitch_relative_loop
        );
    }

    #[test]
    fn pitch_behavior_follows_effect_span_when_present() {
        // Constant freq → no detected loop in relative-pitch.
        let v = voice_with(0x1CD6, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..10).map(|i| frame(i, v)).collect();
        let spans = vec![span_for(1, Effect::Vibrato, 0, 9)];
        let chars =
            extract_characteristics(&simple_note(0, 9, 1), &states, &spans, SystemClock::Pal);
        assert_eq!(chars.pitch_behavior, PitchBehavior::Vibrato);
        assert!(chars.pitch_relative_loop.is_none());
    }

    #[test]
    fn portamento_span_classifies_pitch_behavior() {
        let v = voice_with(0x1CD6, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..10).map(|i| frame(i, v)).collect();
        let spans = vec![span_for(1, Effect::Portamento, 2, 8)];
        let chars =
            extract_characteristics(&simple_note(0, 9, 1), &states, &spans, SystemClock::Pal);
        assert_eq!(chars.pitch_behavior, PitchBehavior::Portamento);
    }

    #[test]
    fn one_shot_sweep_when_pitch_changes_without_effect_span() {
        // Kick-drum-style frequency drop: 0x1CD6 (C4) → 0x0CD6 (~C3).
        let high = voice_with(0x1CD6, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let low = voice_with(0x0CD6, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..6)
            .map(|i| if i < 2 { frame(i, high) } else { frame(i, low) })
            .collect();
        let note = NoteEvent {
            voice: VoiceId(1),
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(5)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        };
        let chars = extract_characteristics(&note, &states, &[], SystemClock::Pal);
        assert_eq!(chars.pitch_behavior, PitchBehavior::OneShotSweep);
        assert!(chars.pitch_range_semitones >= 2);
    }

    #[test]
    fn stable_pitch_when_no_change_and_no_spans() {
        let v = voice_with(0x1CD6, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..6).map(|i| frame(i, v)).collect();
        let chars = extract_characteristics(&simple_note(0, 5, 1), &states, &[], SystemClock::Pal);
        assert_eq!(chars.pitch_behavior, PitchBehavior::Stable);
        assert_eq!(chars.pitch_range_semitones, 0);
    }

    #[test]
    fn waveform_sequence_loops_on_pulse_noise_alternation() {
        // Period-2 T-N alternation — Hubbard's drum idiom.
        let v_pulse = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let v_noise = voice_with(0x1234, noise(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..8)
            .map(|i| {
                if i % 2 == 0 {
                    frame(i, v_pulse)
                } else {
                    frame(i, v_noise)
                }
            })
            .collect();
        let chars = extract_characteristics(&simple_note(0, 7, 1), &states, &[], SystemClock::Pal);
        match chars.waveform_sequence {
            SeqOrLoop::Loop { body, offset } => {
                assert_eq!(body.len(), 2);
                assert_eq!(offset, 0);
            }
            other => panic!("expected Loop, got {other:?}"),
        }
    }

    #[test]
    fn waveform_sequence_is_raw_for_constant_waveform() {
        let v = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..6).map(|i| frame(i, v)).collect();
        let chars = extract_characteristics(&simple_note(0, 6, 1), &states, &[], SystemClock::Pal);
        match chars.waveform_sequence {
            SeqOrLoop::Raw(seq) => assert_eq!(seq.len(), 6),
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Slice 1c: hardware tricks
    // ─────────────────────────────────────────────────────────────

    fn voice_with_test(base: VoiceState) -> VoiceState {
        VoiceState {
            control: ControlBits {
                test: true,
                ..base.control
            },
            ..base
        }
    }

    fn sample_span(start: u32, end: u32) -> EffectSpan {
        EffectSpan {
            effect: Effect::Sample,
            voice: None,
            start_frame: FrameIdx(start),
            end_frame: FrameIdx(end),
        }
    }

    #[test]
    fn combined_waveform_trick_classified_as_tri_saw() {
        let v = voice_with(0x1234, tri_saw(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..4).map(|i| frame(i, v)).collect();
        let chars = extract_characteristics(&simple_note(0, 3, 1), &states, &[], SystemClock::Pal);
        assert!(
            chars
                .hardware_tricks
                .contains(&HardwareTrick::CombinedWaveform(WaveformCombo::TriSaw))
        );
    }

    #[test]
    fn combined_waveform_pulse_triangle_classified() {
        let wf = Waveform {
            pulse: true,
            triangle: true,
            ..Default::default()
        };
        let v = voice_with(0x1234, wf, adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..4).map(|i| frame(i, v)).collect();
        let chars = extract_characteristics(&simple_note(0, 3, 1), &states, &[], SystemClock::Pal);
        assert!(
            chars
                .hardware_tricks
                .contains(&HardwareTrick::CombinedWaveform(WaveformCombo::PulseTri))
        );
    }

    #[test]
    fn combined_waveform_with_noise_classified_as_noise_lock() {
        let wf = Waveform {
            pulse: true,
            noise: true,
            ..Default::default()
        };
        let v = voice_with(0x1234, wf, adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..4).map(|i| frame(i, v)).collect();
        let chars = extract_characteristics(&simple_note(0, 3, 1), &states, &[], SystemClock::Pal);
        assert!(
            chars
                .hardware_tricks
                .contains(&HardwareTrick::CombinedWaveform(WaveformCombo::NoiseLock))
        );
    }

    #[test]
    fn triple_or_quadruple_waveform_classified_as_triple() {
        let wf = Waveform {
            triangle: true,
            sawtooth: true,
            pulse: true,
            noise: false,
        };
        let v = voice_with(0x1234, wf, adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..4).map(|i| frame(i, v)).collect();
        let chars = extract_characteristics(&simple_note(0, 3, 1), &states, &[], SystemClock::Pal);
        assert!(
            chars
                .hardware_tricks
                .contains(&HardwareTrick::CombinedWaveform(WaveformCombo::Triple))
        );
    }

    #[test]
    fn test_bit_usage_is_reported() {
        let v_normal = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let v_test = voice_with_test(v_normal);
        let states: Vec<FrameState> = (0..6)
            .map(|i| {
                if i == 4 {
                    frame(i, v_test)
                } else {
                    frame(i, v_normal)
                }
            })
            .collect();
        let chars = extract_characteristics(&simple_note(0, 5, 1), &states, &[], SystemClock::Pal);
        assert!(chars.hardware_tricks.contains(&HardwareTrick::TestBitUsage));
    }

    #[test]
    fn mid_note_switch_after_setup_frames_is_recorded() {
        // Hubbard's pulse → noise snare pattern, switch at frame 4.
        let v_pulse = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let v_noise = voice_with(0x1234, noise(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..8)
            .map(|i| {
                if i < 4 {
                    frame(i, v_pulse)
                } else {
                    frame(i, v_noise)
                }
            })
            .collect();
        let chars = extract_characteristics(&simple_note(0, 7, 1), &states, &[], SystemClock::Pal);

        let switch = chars
            .hardware_tricks
            .iter()
            .find(|t| matches!(t, HardwareTrick::MidNoteWaveformSwitch { .. }))
            .expect("mid-note switch should be recorded");
        match switch {
            HardwareTrick::MidNoteWaveformSwitch {
                from,
                to,
                frame_offset,
            } => {
                assert_eq!(*from, 0x40); // pulse byte
                assert_eq!(*to, 0x80); // noise byte
                assert_eq!(*frame_offset, 4);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn waveform_switch_within_setup_frames_is_not_recorded_as_mid_note() {
        // Switch at frame 1 — within the 2-frame setup window.
        let v_pulse = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let v_tri = voice_with(0x1234, tri_saw(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..6)
            .map(|i| {
                if i == 0 {
                    frame(i, v_pulse)
                } else {
                    frame(i, v_tri)
                }
            })
            .collect();
        let chars = extract_characteristics(&simple_note(0, 5, 1), &states, &[], SystemClock::Pal);
        // No MidNoteWaveformSwitch should appear (combined-waveform trick is
        // OK; just no mid-note switch entry).
        assert!(
            !chars
                .hardware_tricks
                .iter()
                .any(|t| matches!(t, HardwareTrick::MidNoteWaveformSwitch { .. }))
        );
    }

    #[test]
    fn d418_sample_overlap_recorded_when_span_intersects() {
        let v = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..20).map(|i| frame(i, v)).collect();
        // Note runs 0..=19; sample span covers frames 5..=14 → 10-frame overlap.
        let spans = vec![sample_span(5, 14)];
        let chars =
            extract_characteristics(&simple_note(0, 19, 1), &states, &spans, SystemClock::Pal);

        let overlap = chars
            .hardware_tricks
            .iter()
            .find_map(|t| match t {
                HardwareTrick::D418Sample { overlap_frames } => Some(*overlap_frames),
                _ => None,
            })
            .expect("D418Sample should be recorded");
        assert_eq!(overlap, 10);
    }

    #[test]
    fn no_tricks_on_plain_static_pulse_note() {
        let v = voice_with(0x1234, pulse(), adsr_full(0, 9, 8, 6), 0x800, true);
        let states: Vec<FrameState> = (0..8).map(|i| frame(i, v)).collect();
        let chars = extract_characteristics(&simple_note(0, 7, 1), &states, &[], SystemClock::Pal);
        assert!(chars.hardware_tricks.is_empty());
    }

    // ─────────────────────────────────────────────────────────────
    // Slice 1d: role-tag derivation + drum subclass
    // ─────────────────────────────────────────────────────────────

    fn mk_note(midi: u8) -> NoteEvent {
        NoteEvent {
            voice: VoiceId(1),
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(0)),
            midi: MidiNote(midi),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        }
    }

    fn percussive_chars() -> NoteCharacteristics {
        NoteCharacteristics {
            length_frames: 8,
            attack: AttackClass::Instant,
            pitch_behavior: PitchBehavior::Stable,
            ..Default::default()
        }
    }

    // ── percussive ───────────────────────────────────────────────

    #[test]
    fn role_percussive_requires_a_positive_signal() {
        // Short + Instant + Stable alone is a stab, not percussion —
        // Galway's pulse-leads satisfy those bounds too.
        let tags = derive_role_tags(&mk_note(60), &percussive_chars());
        assert!(
            !tags.percussive,
            "short + Instant + Stable + no noise/sweep/sync should NOT be percussive"
        );
    }

    #[test]
    fn role_percussive_triggered_by_noise() {
        let mut c = percussive_chars();
        c.noise_share = 0.5;
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.percussive);
    }

    #[test]
    fn role_percussive_triggered_by_ring_mod() {
        let mut c = percussive_chars();
        c.hardware_tricks = vec![HardwareTrick::RingMod];
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.percussive);
    }

    #[test]
    fn role_percussive_blocked_by_length() {
        let mut c = percussive_chars();
        c.length_frames = 32;
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(!tags.percussive);
    }

    #[test]
    fn role_percussive_blocked_by_slow_attack() {
        let mut c = percussive_chars();
        c.attack = AttackClass::Slow;
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(!tags.percussive);
    }

    #[test]
    fn role_percussive_blocked_by_vibrato() {
        let mut c = percussive_chars();
        c.pitch_behavior = PitchBehavior::Vibrato;
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(!tags.percussive);
    }

    #[test]
    fn role_percussive_allows_one_shot_sweep() {
        // Kick drums sweep pitch but are still percussive.
        let mut c = percussive_chars();
        c.pitch_behavior = PitchBehavior::OneShotSweep;
        c.pitch_range_semitones = 12;
        let tags = derive_role_tags(&mk_note(36), &c);
        assert!(tags.percussive);
    }

    // ── bass ─────────────────────────────────────────────────────

    #[test]
    fn role_bass_low_pitch_sustained() {
        let c = NoteCharacteristics {
            length_frames: 60,
            attack: AttackClass::Fast,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(40), &c);
        assert!(tags.bass);
    }

    #[test]
    fn role_bass_blocked_when_percussive() {
        // Low pitch + short + Instant + noise → percussive=true → bass=false.
        let c = NoteCharacteristics {
            length_frames: 10,
            attack: AttackClass::Instant,
            noise_share: 0.5,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(40), &c);
        assert!(tags.percussive);
        assert!(!tags.bass, "expected bass=false when percussive");
    }

    #[test]
    fn role_bass_blocked_when_high_pitch() {
        let c = NoteCharacteristics {
            length_frames: 60,
            attack: AttackClass::Fast,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(!tags.bass);
    }

    // ── lead ─────────────────────────────────────────────────────

    #[test]
    fn role_lead_high_pitch_with_pwm() {
        let c = NoteCharacteristics {
            length_frames: 30,
            attack: AttackClass::Fast,
            pw_envelope: PwEnvelope {
                kind: ContourKind::Triangle,
                ..Default::default()
            },
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(72), &c);
        assert!(tags.lead);
    }

    #[test]
    fn role_lead_blocked_without_modulation() {
        let c = NoteCharacteristics {
            length_frames: 30,
            attack: AttackClass::Fast,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(72), &c);
        assert!(!tags.lead);
    }

    #[test]
    fn role_lead_with_vibrato() {
        let c = NoteCharacteristics {
            length_frames: 30,
            pitch_behavior: PitchBehavior::Vibrato,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(72), &c);
        assert!(tags.lead);
    }

    #[test]
    fn role_lead_with_portamento() {
        let c = NoteCharacteristics {
            length_frames: 30,
            pitch_behavior: PitchBehavior::Portamento,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(72), &c);
        assert!(tags.lead);
    }

    // ── pad ──────────────────────────────────────────────────────

    #[test]
    fn role_pad_long_with_medium_attack() {
        let c = NoteCharacteristics {
            length_frames: 100,
            attack: AttackClass::Medium,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.pad);
    }

    #[test]
    fn role_pad_blocked_when_instant_attack() {
        let c = NoteCharacteristics {
            length_frames: 100,
            attack: AttackClass::Instant,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(!tags.pad);
    }

    #[test]
    fn role_pad_blocked_when_too_short() {
        let c = NoteCharacteristics {
            length_frames: 30,
            attack: AttackClass::Medium,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(!tags.pad);
    }

    // ── waveform pick ────────────────────────────────────────────

    fn wf(triangle: bool, sawtooth: bool, pulse: bool, noise: bool) -> Waveform {
        Waveform {
            triangle,
            sawtooth,
            pulse,
            noise,
        }
    }

    /// One frame on voice 1 with the given waveform and gate.
    fn wf_frame(idx: u32, w: Waveform, gate: bool) -> FrameState {
        frame(idx, voice_with(0x1000, w, Adsr::default(), 0, gate))
    }

    #[test]
    fn dominant_ignores_gate_off_noise_parking() {
        // The Commando bug: a 1-frame gated pulse note, then a gate-OFF frame
        // where the driver parks the waveform register at noise. The parked
        // frame must not win.
        let span = [
            wf_frame(0, wf(false, false, true, false), true),
            wf_frame(1, wf(false, false, false, true), false),
        ];
        assert_eq!(
            dominant_gated_waveform(&span, 0),
            0x40,
            "gated pulse, not parked noise"
        );
    }

    #[test]
    fn dominant_is_longest_held_gated_waveform() {
        // A 1-frame triangle attack tick, then a 5-frame pulse body, all gated.
        let mut span = vec![wf_frame(0, wf(true, false, false, false), true)];
        for i in 1..6 {
            span.push(wf_frame(i, wf(false, false, true, false), true));
        }
        assert_eq!(
            dominant_gated_waveform(&span, 0),
            0x40,
            "pulse body dominates"
        );
    }

    #[test]
    fn dominant_tie_breaks_to_earliest() {
        // Equal gated frames of pulse then noise: the earlier (pulse) wins.
        let span = [
            wf_frame(0, wf(false, false, true, false), true),
            wf_frame(1, wf(false, false, false, true), true),
        ];
        assert_eq!(dominant_gated_waveform(&span, 0), 0x40, "earliest on tie");
    }

    #[test]
    fn dominant_falls_back_to_first_audible_when_never_gated() {
        let span = [wf_frame(0, wf(false, true, false, false), false)];
        assert_eq!(
            dominant_gated_waveform(&span, 0),
            0x20,
            "fallback to first audible"
        );
    }

    #[test]
    fn hard_restart_gate_blip_uses_release_tail_waveform() {
        let mut states = vec![
            wf_frame(0, Waveform::default(), true),
            wf_frame(1, wf(false, false, true, false), false),
            wf_frame(2, wf(false, false, true, false), false),
            wf_frame(3, wf(false, false, true, false), false),
        ];
        for state in &mut states[..3] {
            state.digital_voices[0].envelope_activity.active_cycles = crate::trace::CpuCycles(1);
        }
        let chars = extract_characteristics(&simple_note(0, 1, 1), &states, &[], SystemClock::Pal);
        assert_eq!(chars.dominant_waveform_byte(), 0x40);
    }

    // ── stab ─────────────────────────────────────────────────────

    #[test]
    fn role_stab_medium_with_instant_attack() {
        let c = NoteCharacteristics {
            length_frames: 18,
            attack: AttackClass::Instant,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.stab);
    }

    #[test]
    fn role_stab_blocked_when_percussive() {
        // 12 frames + Instant alone is now a stab, not a snare. Add a
        // percussive signal (noise) so percussive fires and stab is blocked.
        let c = NoteCharacteristics {
            length_frames: 12,
            attack: AttackClass::Instant,
            noise_share: 0.3,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.percussive);
        assert!(!tags.stab);
    }

    #[test]
    fn role_stab_blocked_when_too_long() {
        let c = NoteCharacteristics {
            length_frames: 32,
            attack: AttackClass::Instant,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(!tags.stab);
    }

    // ── bell ─────────────────────────────────────────────────────

    #[test]
    fn role_bell_with_tri_saw_combo() {
        let c = NoteCharacteristics {
            length_frames: 30,
            hardware_tricks: vec![HardwareTrick::CombinedWaveform(WaveformCombo::TriSaw)],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.bell);
    }

    #[test]
    fn role_bell_with_ring_mod_and_combined() {
        let c = NoteCharacteristics {
            length_frames: 30,
            hardware_tricks: vec![
                HardwareTrick::CombinedWaveform(WaveformCombo::PulseTri),
                HardwareTrick::RingMod,
            ],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.bell);
    }

    #[test]
    fn role_bell_blocked_without_combined_flag() {
        // RingMod alone, no combined waveform → not bell.
        let c = NoteCharacteristics {
            length_frames: 30,
            hardware_tricks: vec![HardwareTrick::RingMod],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(!tags.bell);
    }

    // ── sample ───────────────────────────────────────────────────

    #[test]
    fn role_sample_when_d418_overlap_present() {
        let c = NoteCharacteristics {
            length_frames: 20,
            hardware_tricks: vec![HardwareTrick::D418Sample { overlap_frames: 10 }],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.sample);
    }

    #[test]
    fn role_sample_not_set_without_d418() {
        let c = NoteCharacteristics {
            length_frames: 20,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(!tags.sample);
    }

    // ── sound_effect ─────────────────────────────────────────────

    #[test]
    fn role_sound_effect_huge_pitch_range() {
        let c = NoteCharacteristics {
            length_frames: 60,
            pitch_range_semitones: 36,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.sound_effect);
    }

    #[test]
    fn role_sound_effect_long_one_shot_sweep() {
        // Long, melodic-range OneShotSweep — not percussive, range ≥ 12.
        let c = NoteCharacteristics {
            length_frames: 60,
            pitch_behavior: PitchBehavior::OneShotSweep,
            pitch_range_semitones: 14,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.sound_effect);
    }

    #[test]
    fn role_kick_drum_is_not_sound_effect() {
        // Short percussive sweep — kick drum, not sound effect.
        let c = NoteCharacteristics {
            length_frames: 6,
            attack: AttackClass::Instant,
            pitch_behavior: PitchBehavior::OneShotSweep,
            pitch_range_semitones: 14,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(36), &c);
        assert!(tags.percussive);
        assert!(
            !tags.sound_effect,
            "kick drum should not be tagged sound_effect"
        );
    }

    // ── multi-tag combinations ───────────────────────────────────

    #[test]
    fn hubbard_snare_is_percussive_and_bell() {
        // Short + Instant + RingMod + combined waveform → percussive + bell.
        let c = NoteCharacteristics {
            length_frames: 10,
            attack: AttackClass::Instant,
            pitch_behavior: PitchBehavior::Stable,
            noise_share: 0.3,
            hardware_tricks: vec![
                HardwareTrick::RingMod,
                HardwareTrick::CombinedWaveform(WaveformCombo::PulseTri),
            ],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert!(tags.percussive);
        assert!(tags.bell);
    }

    // ── drum subclasses ──────────────────────────────────────────

    fn pulse_wave() -> Waveform {
        Waveform {
            pulse: true,
            ..Default::default()
        }
    }

    fn noise_wave() -> Waveform {
        Waveform {
            noise: true,
            ..Default::default()
        }
    }

    #[test]
    fn drum_subclass_kick_short_with_octave_sweep() {
        let c = NoteCharacteristics {
            length_frames: 6,
            attack: AttackClass::Instant,
            pitch_behavior: PitchBehavior::OneShotSweep,
            pitch_range_semitones: 14,
            waveform_primary: vec![pulse_wave()],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(36), &c);
        assert_eq!(tags.drum_subclass, Some(DrumSubclass::Kick));
    }

    #[test]
    fn drum_subclass_snare_short_with_noise() {
        let c = NoteCharacteristics {
            length_frames: 10,
            attack: AttackClass::Instant,
            noise_share: 0.5,
            waveform_primary: vec![noise_wave(), pulse_wave()],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert_eq!(tags.drum_subclass, Some(DrumSubclass::Snare));
    }

    #[test]
    fn drum_subclass_hihat_closed_very_short_pure_noise() {
        let c = NoteCharacteristics {
            length_frames: 3,
            attack: AttackClass::Instant,
            noise_share: 1.0,
            waveform_primary: vec![noise_wave()],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(70), &c);
        assert_eq!(tags.drum_subclass, Some(DrumSubclass::HihatClosed));
    }

    #[test]
    fn drum_subclass_hihat_open_medium_pure_noise() {
        let c = NoteCharacteristics {
            length_frames: 12,
            attack: AttackClass::Instant,
            noise_share: 1.0,
            waveform_switches: 0,
            waveform_primary: vec![noise_wave()],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(72), &c);
        assert_eq!(tags.drum_subclass, Some(DrumSubclass::HihatOpen));
    }

    #[test]
    fn drum_subclass_tom_short_with_small_sweep() {
        let c = NoteCharacteristics {
            length_frames: 8,
            attack: AttackClass::Instant,
            pitch_behavior: PitchBehavior::OneShotSweep,
            pitch_range_semitones: 5,
            waveform_primary: vec![pulse_wave()],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(48), &c);
        assert_eq!(tags.drum_subclass, Some(DrumSubclass::Tom));
    }

    #[test]
    fn drum_subclass_perc_metallic_with_ring_mod_alternation() {
        let c = NoteCharacteristics {
            length_frames: 10,
            attack: AttackClass::Instant,
            waveform_sequence: SeqOrLoop::Loop {
                body: vec![0x10, 0x80],
                offset: 0,
            },
            hardware_tricks: vec![HardwareTrick::RingMod],
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(60), &c);
        assert_eq!(tags.drum_subclass, Some(DrumSubclass::PercMetallic));
    }

    #[test]
    fn drum_subclass_none_when_not_percussive() {
        let c = NoteCharacteristics {
            length_frames: 60,
            attack: AttackClass::Fast,
            ..Default::default()
        };
        let tags = derive_role_tags(&mk_note(40), &c);
        assert_eq!(tags.drum_subclass, None);
        assert!(!tags.percussive);
    }
}
