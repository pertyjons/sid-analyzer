//! SID → Pertylizer (`.ptz`) v0 exporter.
//!
//! Translates the analyzed subtune into a schema-valid Pertylizer project
//! file. The faithful style uses Pertylizer's model-aware SID oscillator and
//! preserves chip-global topology where the project schema can represent it.
//! The emitted JSON mirrors the gold reference
//! `SID Export v0 Test.json` exactly — same module skeleton, the same
//! 2-string connection tuples, and every parameter key (defaults included).
//!
//! Mapping summary (full spec: `docs/export.md`):
//! - one Pertylizer instrument per *output track*, where a track gathers all of
//!   one SID voice's notes that share a single [`MergeShape`] (source kind,
//!   waveform, ring-mod insert, filter routing/type) — possibly drawn from
//!   several source [`Patch`]es (`osc → amp → out`, `env → amp.cv`). A voice is
//!   monophonic, so those notes never overlap and can share one instrument whose
//!   per-section differences (pulse width, filter cutoff, ADSR) are driven by
//!   automation lanes rather than by minting a fresh instrument per patch.
//!   Waveform and filter type are enums Pertylizer cannot automate, so they stay
//!   part of the shape — different-waveform patches never merge. The merge is
//!   **within one voice only**: two voices that cluster into the same SID patch
//!   each get their OWN instrument copy, never a shared one, so the automation
//!   lanes below cannot collide on a single shared instance (Pertylizer applies
//!   a lane to the one instance of its target instrument; the SID has
//!   independent per-voice registers). `voice` is part of every merge key, so
//!   this holds automatically;
//! - per-section ADSR differences become `Step` `AutomationTarget::Module` lanes
//!   on `env-1.{attack,decay,sustain,release}` (see [`build_adsr_lanes`]),
//!   stepping to each section's envelope at that section's first note;
//! - noise patches swap the oscillator for a `noise` module;
//! - ADSR nibbles → seconds via [`adsr_to_seconds`];
//! - one track per `(voice, patch)` pair, each bound to its own dedicated
//!   instrument (a SID voice is monophonic, so notes never overlap across
//!   the sub-tracks of one voice);
//! - stable filter states use one return-bus SID filter with per-track sends;
//!   dynamic cutoff/mode states retain per-instrument filters because the
//!   project schema cannot automate return-bus effect parameters;
//! - per-frame PWM and filter sweeps become `AutomationTarget::Module` lanes
//!   on `osc-1.pulse_width` / `flt-1.cutoff` (see [`build_automation`]),
//!   sampling the actual per-frame register over the group's note spans;
//! - ring modulation and hard sync connect the physical previous voice's live
//!   oscillator frequency to a second SID oscillator's MSB source;
//! - arpeggio patches (`patch.arpeggio_loop`) emit one held base note per event
//!   plus a SID-native `Arpeggiator` note processor (Custom offsets, `MilliHz` =
//!   resolved play-call rate, legato, gate 1.0 — see [`arp_processor_for`]) that
//!   cycles the relative-semitone pattern one step per call; the chip's
//!   arpeggio is one
//!   held gate, which the processor's legato reproduces. Non-clean plans (and
//!   `SynthOptions::arpeggiator_processor = false`) use the per-frame
//!   [`expand_arpeggio`] bake;
//! - detected `Vibrato` spans (see [`vibrato_from_span`]) become a per-note
//!   `NoteExpression.vibrato` (depth/rate derived from the per-frame frequency
//!   wobble), rather than a fixed LFO module;
//! - SID gate-held legato runs — a single held gate region whose frequency steps
//!   between several *sustained* pitches without re-gating — are split into one
//!   note per pitch plateau, tied with `Note.legato` so the engine re-pitches
//!   without re-attacking (see [`pitch_plateaus`]); a steady note or a pure
//!   slide stays a single note;
//! - detected `Portamento` spans (see [`glide_from_span`]) compose with that
//!   split: an *onset* slide that lands on the note it slides into becomes a
//!   per-note `Glide` (pitch = the slide's destination, `glide.from` = the
//!   signed-semitone origin), so the engine ramps into the pitch;
//! - trace exports use one full-length pattern per track; structured native
//!   exports reuse canonical driver blocks across compatible placements/tracks
//!   and keep full-length automation in separate lane-only patterns;
//! - every instrument shares a mix `volume` of `0.5 / active-voice-count` so the
//!   summed master bus keeps headroom below 0 dBFS — three correlated
//!   full-scale pulse voices would otherwise hard-clip it (see [`mix_volume`]).
//!
//! Now mapped beyond v0: static cents detune → oscillator `detune` (see
//! [`plan_detune_cents`]); combined-waveform bytes → one native SID oscillator
//! carrying the full waveform mask (see [`combined_secondary_waveform`]); the
//! per-frame tri↔noise / tri↔pulse *alternation* idiom → the `sid_oscillator`'s
//! native looping waveform sequence (see [`alternation_seq`]), replacing the
//! former two-LFO/two-oscillator gate. Still skipped: multi-SID.

mod enhance;
pub mod lowering;
mod modern;

use crate::analysis::Hertz;
use crate::analysis::SystemClock;
use crate::analysis::VoiceId;
use crate::analysis::effects::{Effect, EffectSpan, measure_slide, measure_vibrato};
use crate::analysis::filter::{FilterMode, Resonance};
use crate::analysis::note::{Cents, NoteEvent, hertz_to_midi};
use crate::analysis::sid_program::AnalyzedSidProgram;
use crate::analysis::sid_program::query::ProgramFrame;
use crate::analysis::timbre::{
    AuthoredEffects, AuthoredPwm, DrumSubclass, HardwareTrick, NoteCharacteristics, Patch,
    PatchVoiceProfile, RoleTags, median_hertz,
};
use crate::analysis::voice::{Adsr, PulseWidth, SidFreq};
use crate::emu::PlaybackTiming;
use crate::export::VoicePlacements;
use crate::export::descriptors::{self, ParamDescriptor};
use crate::export::forward;
use crate::export::json::EnrichedNote;
use crate::header::SidModel;
use crate::trace::FrameIndex;
use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::io::{self, Write};

struct SynthSource<'a> {
    header: &'a crate::header::Header,
    timing: PlaybackTiming,
    frame_count: usize,
    patches: Option<&'a [Patch]>,
    notes: Vec<EnrichedNote<'a>>,
    effects: &'a [EffectSpan],
    structure: Option<&'a [VoicePlacements]>,
    native: Option<&'a crate::analysis::sid_program::semantic::NativeSemanticOverlay>,
}

/// Default frame→tick scale by host clock, used when tempo detection (§A6)
/// can't run. PAL 40 ticks/frame → 125 BPM, NTSC 33 → 123.75 BPM through
/// `bpm = ticks_per_frame * frame_rate / 16` — the historical fixed values.
fn default_ticks_per_frame(clock: SystemClock) -> u32 {
    match clock {
        SystemClock::Pal => 40,
        SystemClock::Ntsc => 33,
    }
}

/// The §A6 time base: how SID frames map to Pertylizer ticks, the reported
/// tempo, and the editor-grid resolution.
///
/// `ticks_per_frame` is the absolute frame→tick scale every note/automation
/// position is built from; `bpm` is fixed by it (`bpm = ticks_per_frame *
/// frame_rate / 16`), so real-time playback is exact for *any* integer scale —
/// only the tempo *label* and grid alignment change. `grid_ticks_per_row` /
/// `grid_rows` are the cosmetic editor grid (a 16th-note grid that lines up
/// with beats), independent of where notes actually sit.
struct TimeBase {
    ticks_per_frame: u32,
    bpm: f32,
    grid_ticks_per_row: u32,
    grid_rows: u32,
}

/// Derive the [`TimeBase`] for a subtune (§A6).
///
/// Autocorrelates the note onsets for the dominant beat period, folds it to a
/// musical BPM, and picks the integer frame→tick scale closest to that tempo
/// (`bpm = ticks_per_frame * frame_rate / 16`). When too few onsets exist for a
/// reliable estimate it falls back to the clock default (PAL 40 → 125 BPM). The
/// editor grid is a fixed 16th-note grid sized to the subtune.
fn derive_time_base(export: &SynthSource<'_>, frame_count: u32) -> TimeBase {
    let onsets: Vec<u32> = export.notes.iter().map(|n| n.event.start_frame.0).collect();
    derive_time_base_from_timing(export.timing, &onsets, frame_count)
}

fn derive_time_base_from_timing(
    timing: PlaybackTiming,
    onsets: &[u32],
    frame_count: u32,
) -> TimeBase {
    let clock = timing.clock;
    let frame_rate = timing.calls_per_second();

    let ticks_per_frame = match detect_frames_per_beat(onsets, frame_count) {
        Some(frames_per_beat) => {
            let bpm = fold_bpm(60.0 * frame_rate / frames_per_beat);
            let scale = (bpm * TICKS_PER_FRAME_PER_BPM / frame_rate).round();
            (scale as i64).clamp(
                i64::from(MIN_TICKS_PER_FRAME),
                i64::from(MAX_TICKS_PER_FRAME),
            ) as u32
        }
        None => default_ticks_per_frame(clock),
    };
    let bpm = (f64::from(ticks_per_frame) * frame_rate / TICKS_PER_FRAME_PER_BPM) as f32;

    let pattern_length = frame_count.saturating_mul(ticks_per_frame);
    let grid_rows = pattern_length
        .div_ceil(GRID_TICKS_PER_ROW)
        .clamp(1, u32::from(u16::MAX));

    TimeBase {
        ticks_per_frame,
        bpm,
        grid_ticks_per_row: GRID_TICKS_PER_ROW,
        grid_rows,
    }
}

/// Fold a raw tempo into the musical `[BPM_FOLD_MIN, BPM_FOLD_MAX)` window by
/// doubling/halving, so an octave error in the detected beat period (a half- or
/// double-time pulse) still resolves to a sensible BPM. Non-finite or
/// non-positive input falls back to 125.
fn fold_bpm(mut bpm: f64) -> f64 {
    if !bpm.is_finite() || bpm <= 0.0 {
        return 125.0;
    }
    while bpm < BPM_FOLD_MIN {
        bpm *= 2.0;
    }
    while bpm >= BPM_FOLD_MAX {
        bpm /= 2.0;
    }
    bpm
}

/// Estimate the dominant beat period (in frames) from note onsets by
/// autocorrelation (§A6).
///
/// Builds a per-frame onset pulse train — lightly smoothed with a `[0.5,1,0.5]`
/// kernel so onsets a frame off still correlate — and finds the lag in
/// `[MIN_BEAT_FRAMES, MAX_BEAT_FRAMES]` with the strongest autocorrelation. The
/// lag may be a beat, a sub-beat, or a bar multiple; the caller folds the
/// resulting tempo by octaves, so any metrical level resolves to the same BPM.
/// Returns `None` when there are too few onsets for a reliable estimate.
fn detect_frames_per_beat(onsets: &[u32], frame_count: u32) -> Option<f64> {
    if onsets.len() < MIN_TEMPO_ONSETS || frame_count < 2 {
        return None;
    }
    let n = frame_count as usize;
    let mut pulse = vec![0.0f32; n];
    for &f in onsets {
        if let Some(slot) = pulse.get_mut(f as usize) {
            *slot += 1.0;
        }
    }
    // Light smoothing widens each onset to ±1 frame so jittered onsets still
    // align under the autocorrelation lag. (Every element is overwritten below,
    // so start from a fresh zeroed buffer rather than cloning `pulse`.)
    let mut sig = vec![0.0f32; n];
    for i in 0..n {
        let left = if i > 0 { pulse[i - 1] } else { 0.0 };
        let right = pulse.get(i + 1).copied().unwrap_or(0.0);
        sig[i] = pulse[i] + 0.5 * (left + right);
    }

    let lag_max = (n / 2).min(MAX_BEAT_FRAMES);
    if lag_max <= MIN_BEAT_FRAMES {
        return None;
    }
    let mut best_lag = 0usize;
    let mut best_score = 0.0f32;
    for lag in MIN_BEAT_FRAMES..=lag_max {
        let mut score = 0.0f32;
        for i in 0..(n - lag) {
            score += sig[i] * sig[i + lag];
        }
        if score > best_score {
            best_score = score;
            best_lag = lag;
        }
    }
    (best_score > 0.0).then_some(best_lag as f64)
}

/// SID attack time per 4-bit nibble, in milliseconds (canonical 6581/8580
/// table). Decay and release use 3× the same column.
const ATTACK_MS: [f32; 16] = [
    2.0, 8.0, 16.0, 24.0, 38.0, 56.0, 68.0, 80.0, 100.0, 250.0, 500.0, 800.0, 1000.0, 3000.0,
    5000.0, 8000.0,
];

/// MIDI velocity range; the divisor for normalising into Pertylizer's
/// `0.0..=1.0` note velocity.
const MIDI_VELOCITY_MAX: f32 = 127.0;

/// Per-voice headroom budget shared across the active SID voices to set each
/// instrument's mix `volume` (see [`mix_volume`]). Pertylizer hard-clips the
/// summed master at 0 dBFS; a single full-scale pulse voice — band-limited
/// narrow pulses overshoot to ~1.5× — already nears the ceiling, and the SID
/// plays up to its three highly-correlated hardware voices at once, so at unity
/// the sum reaches ~4–5× full scale and the master clips almost continuously
/// (the audible "skorr"). `0.5` leaves ~2 dB below 0 dBFS for the worst case
/// (three voices → `0.5/3 ≈ 0.167`), and proportionally more for sparser tunes.
const MIX_HEADROOM: f32 = 0.5;

/// The §A9 lo-fi coloring is a **master-bus chain** applied to the summed mix
/// ([`master_chain`]), not a per-instrument effect. This is physically truer to
/// the chip: the 6581 mixes its three voices and *then* colours the sum at the
/// analog output stage, so a single saturation + EQ over the mix matches that
/// signal flow (and avoids duplicating the tube on every track).
///
/// The tube saturation supplies the 6581 DAC's even-harmonic warmth; `tone` is
/// held open (`1.0`) so it does not roll the top (the export already sits darker
/// than the chip above 2 kHz — measured via `analyze_mix_bus` against a reSID
/// reference; see `docs/export.md` §A9).
const COLORING_TUBE_DRIVE: f32 = 0.25;
const COLORING_TUBE_TONE: f32 = 1.0;
const COLORING_TUBE_MIX: f32 = 0.5;

/// Master EQ following the tube: nudges the mix's spectral balance toward the
/// reSID reference, which carries markedly more mid and high energy than our
/// export. Re-measured on Auf Wiedersehen Monty (first 60 s, `analyze_mix_bus`
/// with master FX vs `sid-re wavebands` on a `sidplayfp -w` render, edges
/// 100/500/2000, normalized to the low band): reSID sub/mid/high ≈
/// 0.73 / 1.17 / 0.19 vs the export's ≈ 3.0 / 0.49 / 0.03 — bass-heavy and dark.
/// A full match is neither reachable nor wanted (reSID's top is partly raw-
/// waveform harmonics the band-limited oscs drop + digi the synth path can't
/// make), so this is a deliberate presence lift, ear-confirmed by Per against
/// the prior `-1.5 / +2 / +4` setting: the mid + high boosts add air, the
/// stronger low-shelf trim keeps the sub from dominating. The lift moves the
/// export to ≈ 2.5 / 0.59 / 0.05. Gains in dB, frequencies in Hz.
const COLORING_EQ_LOW_FREQ: f32 = 120.0;
const COLORING_EQ_LOW_GAIN: f32 = -3.0;
const COLORING_EQ_MID_FREQ: f32 = 1200.0;
const COLORING_EQ_MID_GAIN: f32 = 5.0;
const COLORING_EQ_MID_Q: f32 = 0.7;
const COLORING_EQ_HIGH_FREQ: f32 = 3000.0;
const COLORING_EQ_HIGH_GAIN: f32 = 8.0;

/// A final look-ahead limiter preserves the SID mix's average level while
/// containing the sharper peaks produced by the reconstructed oscillators.
/// `-5 dB` was selected from the permanent Nemesis intro window: it reduces
/// peak error from about 0.27 to 0.07 while keeping RMS within 0.001 of reSID.
const MASTER_LIMITER_CEILING_DB: f32 = -5.0;
const MASTER_LIMITER_LOOK_AHEAD_MS: f32 = 3.0;
const MASTER_LIMITER_RELEASE_MS: f32 = 100.0;

/// Role-aware channel trims measured on the permanent Nemesis voice windows.
/// Pertylizer's reconstructed pulse/noise sources produce materially more RMS
/// than reSID at the same nominal channel level, while triangle/bass paths are
/// already close. Keep the shared voice headroom as the base and trim only the
/// measured high-energy roles.
const LEAD_MIX_TRIM: f32 = 0.7;
const ARPEGGIATED_LEAD_MIX_TRIM: f32 = 0.45;
const DRUM_MIX_TRIM: f32 = 0.75;
const DRUM_DROP_MIX_TRIM: f32 = 0.35;
/// Native 6581 pulse+triangle is substantially quieter than a pure waveform.
const TRIANGLE_PULSE_MIX_TRIM: f32 = 0.32;

/// Static output-stage roll-off for roles whose reconstructed pulse/noise
/// spectra remain brighter than reSID after the shared master EQ. This is
/// separate from the programmable SID filter: it represents the measured
/// voice/output bandwidth correction and therefore never receives cutoff
/// automation.
const ARPEGGIATED_LEAD_OUTPUT_CUTOFF: Hertz = Hertz(4_000.0);
const DRUM_OUTPUT_CUTOFF: Hertz = Hertz(1_800.0);
const DRUM_DROP_OUTPUT_CUTOFF: Hertz = Hertz(2_000.0);
/// A role needs enough render evidence before the exporter adds a permanent
/// output-stage module. Sparse one-shots keep their native graph and avoid
/// multiplying nearly identical filters across fragmented effect plans.
const DRUM_OUTPUT_FILTER_MIN_NOTES: usize = 32;

/// Master volume for the coloured mix. The tube raises level ~+3 dB, so the
/// master fader is trimmed to keep the worst-case peak clear of 0 dBFS while
/// letting the now-louder per-instrument sum (no per-track makeup trim) lift the
/// mix off its previous ~−31 LUFS floor. Verified clip-free via `analyze_mix_bus`.
const MASTER_VOLUME: f32 = 0.8;

/// 11-bit SID cutoff range (`0..=2047`); the upper bound for clamping a raw
/// cutoff value before the curve lookup ([`cutoff_value_to_hz`]).
const SID_CUTOFF_MAX: f32 = 2047.0;
/// 4-bit SID resonance range (`0..=15`); the divisor for normalising into
/// Pertylizer's `0.0..=1.0` `resonance`.
const SID_RESONANCE_MAX: f32 = 15.0;
/// Ceiling on the normalised filter resonance. The 6581/8580 filter resonance is
/// mild — it never self-oscillates — but Pertylizer's `resonance = 1.0` drives
/// its filter into a sharp self-resonant whistle the SID cannot make. Capping
/// the mapped value keeps even register 15 a moderate, SID-like Q.
const RESONANCE_MAX_NORM: f32 = 0.2;

/// The 6581 low-pass does not fully reject the dry voice at very low cutoff.
/// Blend a bounded dry path beside Pertylizer's ideal low-pass, tapering it to
/// zero before the measured curve reaches the mid band.
const FILTER_LEAK_MAX: f32 = 0.4;
const FILTER_LEAK_FULL_HZ: f32 = 420.0;
const FILTER_LEAK_END_HZ: f32 = 2_000.0;

/// Pertylizer filter model that best matches the 6581 lowpass. A/B vs synthetic
/// reSID saw-at-A2 filter profiles: at the common mid-cutoff operating point
/// (register `$400`, ≈4.6 kHz)
/// `acid` scores 2.45 dB vs `standard` 6.40 (and beats `fluid`/`screamer`/
/// `karlsen`); it also leads at low cutoff (register `$200`: 19.9 vs 25.4 dB).
/// Two model-independent gaps remain (open follow-ups): at low cutoff the real
/// 6581 is leaky and passes 3–5 kHz the export's cutoff table cuts, and at very
/// high cutoff `acid`'s resonance adds a peak near the corner.
const SID_FILTER_MODEL: &str = "acid";

/// Maximum allowed reconstruction error, in normalised lane units (`0.0..=1.0`),
/// when decimating a per-frame automation series to interpolation points. A
/// linear ramp or triangle modulation collapses to its turning points; values
/// drifting within this tolerance coalesce. `0.01` ≈ 1% of pulse-width range,
/// or ~0.1 octave of cutoff (the lane is 10 octaves wide).
const AUTOMATION_EPSILON: f32 = 0.01;
/// A one-register tolerance for the phase-critical ring/sync source frequency.
const SID_FREQ_AUTOMATION_EPSILON: f32 = 1.0 / 65535.0;

#[derive(Debug, Clone, Copy)]
struct ParamLaneSampling {
    continuous: bool,
    epsilon: f32,
}

impl ParamLaneSampling {
    const PER_NOTE: Self = Self {
        continuous: false,
        epsilon: AUTOMATION_EPSILON,
    };
    const CONTINUOUS: Self = Self {
        continuous: true,
        epsilon: AUTOMATION_EPSILON,
    };
    const SID_FREQUENCY: Self = Self {
        continuous: false,
        epsilon: SID_FREQ_AUTOMATION_EPSILON,
    };
}

/// Fitted `Exponential` strengths with magnitude at or below this are treated
/// as effectively straight and emitted as `Linear` instead. An `|strength|` of
/// 3 is an exponent of `1 ± 0.06` — visually indistinguishable from a line — so
/// only clearly curved eases earn an `Exponential` label.
const NEAR_LINEAR_STRENGTH: u8 = 3;

/// `rng-1` (ring-mod) parameters. SID ring-mods a voice against the *previous
/// voice's* oscillator at a fixed pitch, so the carrier is `key_track = 0`
/// (note-independent) at the modulating voice's frequency (passed in from
/// `ring_source_hz`; `RING_MOD_CARRIER_FREQ` is only the fallback when the
/// source pitch is unknown). A sine carrier at `1.0×` (`freq_ratio = 0.5`) was
/// measured the closest match to the SID ring sidebands; `mix = 1.0` is fully
/// ring-modulated.
const RING_MOD_CARRIER_FREQ: f32 = 440.0;

/// Minimum number of frames a pitch must hold to survive as its own legato note
/// when a gate-held melodic run is split. Shorter dwells are transient slide
/// frames and are absorbed into the neighbouring plateau (see
/// [`pitch_plateaus`]). 4 frames ≈ 80 ms at PAL.
const MIN_LEGATO_FRAMES: u32 = 4;

/// Pitch deviations below this many semitones are inaudible, so a vibrato or
/// glide that small is dropped and the note plays at its plain pitch.
const MIN_PITCH_EFFECT_SEMITONES: f32 = 0.05;

/// 4-bit `$D418` master-volume range (`0..=15`); the divisor for normalising
/// the per-frame volume nibble into Pertylizer's `0.0..=1.0` `MasterVolume`
/// (§A5).
const SID_MAX_VOLUME: f32 = 15.0;

/// Point-count ceiling for the §A5 master-volume lane. A musical volume swell
/// or fade decimates to a handful of points; a tune that hammers `$D418` for
/// 4-bit PCM digi (the §A7 sample trick) instead swings the register every
/// frame, defeating decimation. Past this many points the "contour" is digi
/// noise, not a musical envelope, so the lane is dropped rather than modulating
/// the whole mix with PCM.
const MAX_VOLUME_LANE_POINTS: usize = 512;

/// Pertylizer's quarter-note resolution (PPQN). One beat is this many ticks,
/// fixed by the schema's tick model.
const TICKS_PER_QUARTER: u32 = 960;

/// Editor-grid resolution for the §A6 musical grid: 240 ticks/row =
/// `TICKS_PER_QUARTER / 4` = four rows per quarter note (16th-note rows), the
/// Pertylizer default. Purely cosmetic — note timing is absolute ticks — but it
/// makes the grid line up with beats instead of one row per SID frame.
const GRID_TICKS_PER_ROW: u32 = TICKS_PER_QUARTER / 4;

/// Musical tempo-folding window (§A6): a detected tempo is doubled/halved into
/// `[BPM_FOLD_MIN, BPM_FOLD_MAX)` so an octave error in the beat period (picking
/// a half- or double-time pulse) still lands on a sensible BPM.
const BPM_FOLD_MIN: f64 = 75.0;
const BPM_FOLD_MAX: f64 = 150.0;

/// Bounds on the derived integer ticks-per-frame (§A6). The frame→tick scale is
/// coupled to BPM by `bpm = ticks_per_frame * frame_rate / 16`, so this keeps
/// the reported tempo in a sane band (~50..300 BPM) and the tick resolution
/// ample (≥16 ticks/frame). Real-time playback is exact for any value in range.
const MIN_TICKS_PER_FRAME: u32 = 16;
const MAX_TICKS_PER_FRAME: u32 = 96;

/// Tempo detection (§A6) needs at least this many note onsets for the
/// autocorrelation to be meaningful; below it the exporter falls back to the
/// clock-default frame→tick scale (PAL 40 → 125 BPM).
const MIN_TEMPO_ONSETS: usize = 16;

/// Beat-period search window for the onset autocorrelation (§A6), in frames.
/// At PAL (50 fps) 10 frames ≈ 300 BPM and 100 frames ≈ 30 BPM, comfortably
/// bracketing real beats; octave errors are folded afterwards.
const MIN_BEAT_FRAMES: usize = 10;
const MAX_BEAT_FRAMES: usize = 100;

/// Onset-to-tick scale relating ticks-per-frame to BPM: one quarter note is
/// `TICKS_PER_QUARTER` ticks, one minute is `60 * frame_rate` frames, so
/// `ticks_per_frame = TICKS_PER_QUARTER * bpm / (60 * frame_rate)
/// = bpm * 16 / frame_rate` at 960 PPQN. Named for the `960/60 = 16` constant.
const TICKS_PER_FRAME_PER_BPM: f64 = TICKS_PER_QUARTER as f64 / 60.0;

/// Maximum leading frames an onset chirp may span (§A8). Hubbard's attack
/// chirps settle within ~4–8 frames (sub-100 ms at PAL); a longer pitch
/// movement is a musical slide, left to the portamento / legato paths.
const MAX_CHIRP_FRAMES: u32 = 8;

/// An onset chirp must open at least this far (semitones) from the settled
/// pitch to be audible and to avoid firing on ordinary tuning jitter (§A8).
const MIN_CHIRP_SEMITONES: f32 = 1.0;

/// A frame whose pitch is within this many semitones of the settled note counts
/// as "arrived", ending the chirp window (§A8).
const CHIRP_SETTLE_SEMITONES: f32 = 0.5;

/// An onset glide (portamento or §A8 chirp) may repitch a note at most this far
/// (semitones) from its own gated/detected pitch. Beyond an octave the "slide" is
/// not a melodic gesture but a percussive onset drop — a Hubbard drum/zap gates
/// the written note for the attack click, then sweeps the frequency down to a far
/// lower body and holds it. The trace captures that body as a slide destination,
/// but the note Hubbard wrote is the gated pitch; repitching it to the drop body
/// turns a lead note into a clashing low blip. So keep the gated pitch when the
/// destination diverges this far.
const MAX_ONSET_GLIDE_SEMITONES: i32 = 12;

/// The drum-drop body pitch, if the note's first sustained plateau sits more than
/// [`MAX_ONSET_GLIDE_SEMITONES`] below its gated pitch — the percussive far-drop
/// the trace holds (see the constant's note). Shared by [`push_expressive_notes`]
/// (which emits the note at this pitch) and [`build_track_plans`] (which routes it
/// onto a dedicated percussion track), so the two never disagree on what a
/// drum-drop is.
fn plateau_drop_dest(first_pitch: Option<u32>, gate_midi: u8) -> Option<u32> {
    first_pitch.filter(|d| (*d as i32) < i32::from(gate_midi) - MAX_ONSET_GLIDE_SEMITONES)
}

/// Classify a note as a drum-drop and return its body pitch, mirroring exactly the
/// conditions under which [`push_expressive_notes`] takes its drum-drop branch: a
/// vibrato'd note is a sustained tone (never a drum-drop), otherwise the first
/// sustained plateau must drop past [`plateau_drop_dest`]'s threshold. `None` when
/// the note is melodic.
fn drum_drop_dest(
    event: &NoteEvent,
    states: &[ProgramFrame],
    effects: &[EffectSpan],
    timing: PlaybackTiming,
    voice: VoiceId,
    voice_index: usize,
    frame_count: u32,
) -> Option<u32> {
    let clock = timing.clock;
    let start = event.start_frame.0;
    let end = authored_note_end_frame(event, frame_count);
    let vibrato = overlapping_span(effects, Effect::Vibrato, voice, start, end)
        .and_then(|span| vibrato_from_span(states, span, timing, voice_index, event.start_frame));
    if vibrato.is_some() {
        return None;
    }
    let first_pitch = pitch_plateaus(states, clock, voice_index, start, end)
        .first()
        .map(|p| u32::from(p.pitch));
    plateau_drop_dest(first_pitch, event.midi.0)
}

/// Decoded ADSR in Pertylizer terms: attack/decay/release in seconds and a
/// `0.0..=1.0` sustain level.
struct AdsrSeconds {
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,
}

/// Pertylizer's `sid_oscillator.pw_reg` descriptor (raw 12-bit register,
/// linear `0..=4095`), sourced from `descriptors.json` rather than a
/// hand-copied band.
fn pulse_width_param() -> ParamDescriptor {
    descriptors::param("sid_oscillator", "pw_reg")
}

/// Pertylizer's raw 16-bit SID oscillator-frequency register descriptor.
fn oscillator_frequency_param() -> ParamDescriptor {
    descriptors::param("sid_oscillator", "freq_reg")
}

/// Pertylizer's `filter.cutoff` descriptor (range + logarithmic curve).
fn cutoff_param() -> ParamDescriptor {
    descriptors::param("filter", "cutoff")
}

/// Pertylizer's `envelope.attack` descriptor (range + exponential curve).
/// `decay` and `release` share the same range and curve, so this one stands
/// for all three time stages (the drift test verifies they agree).
fn env_time_param() -> ParamDescriptor {
    descriptors::param("envelope", "attack")
}

/// Map a SID [`Adsr`] (four 0..=15 nibbles) to Pertylizer seconds + a
/// normalised sustain. Attack uses [`ATTACK_MS`]; decay and release use 3×
/// the attack-column value for their own nibble. The long SID decay/release
/// nibbles (15 → 24 s) exceed Pertylizer's envelope range, so each time stage
/// is clamped into the `envelope.attack` descriptor's `[min, max]`.
fn adsr_to_seconds(adsr: Adsr) -> AdsrSeconds {
    let at = ATTACK_MS[(adsr.attack & 0x0F) as usize];
    let de = ATTACK_MS[(adsr.decay & 0x0F) as usize] * 3.0;
    let re = ATTACK_MS[(adsr.release & 0x0F) as usize] * 3.0;
    let env = env_time_param();
    AdsrSeconds {
        attack: env.clamp(at / 1000.0),
        decay: env.clamp(de / 1000.0),
        sustain: f32::from(adsr.sustain & 0x0F) / 15.0,
        release: env.clamp(re / 1000.0),
    }
}

/// Pick the single Pertylizer oscillator waveform string for a SID
/// `waveform` byte, by priority pulse > sawtooth > triangle. Noise is
/// handled by the caller (separate module), so this never returns "noise".
fn waveform_string(waveform: u8) -> &'static str {
    if waveform & 0x40 != 0 {
        "pulse"
    } else if waveform & 0x20 != 0 {
        "sawtooth"
    } else if waveform & 0x10 != 0 {
        "triangle"
    } else {
        "pulse"
    }
}

/// The secondary tonal bit of a combined-waveform byte. Pertylizer's native SID
/// oscillator receives the complete bit mask; this value is retained as merge
/// evidence and for waveform-specific output calibration.
fn combined_secondary_waveform(waveform: u8) -> Option<&'static str> {
    if waveform & 0x80 != 0 {
        return None;
    }
    let tri = waveform & 0x10 != 0;
    let saw = waveform & 0x20 != 0;
    let pulse = waveform & 0x40 != 0;
    if u8::from(tri) + u8::from(saw) + u8::from(pulse) < 2 {
        return None;
    }
    Some(if pulse {
        if saw { "sawtooth" } else { "triangle" }
    } else {
        // Sawtooth is the dominant, so triangle is the only lower set bit.
        "triangle"
    })
}

fn combined_waveform_support(model: SidModel, mask: u8, exact: bool) -> &'static str {
    if mask.count_ones() < 2 {
        return "not_combined";
    }
    if exact {
        return "native_exact";
    }
    match model {
        SidModel::Mos6581 | SidModel::Mos8580 => "native_model_approximation",
        _ => "native_6581_fallback",
    }
}

/// Write a Pertylizer `.ptz` project file for the analyzed subtune.
///
/// Requires `export.patches` to be `Some` (timbre extraction ran). When it
/// is `None`, falls back to one generic pulse instrument per SID voice so
/// the exporter is never a hard error.
///
/// `states` is the per-frame chip state (one entry per frame, parallel to
/// `export.frame_count`). It is the source for the per-frame pulse-width and
/// cutoff automation lanes; the compact [`SynthSource`] borrows those states
/// from the analyzed program's cached physical query layer.
/// Per-export fidelity census: counts of what the export approximated or dropped
/// relative to the SID trace, so a schema-valid `.ptz` that silently loses
/// SID data is visible and corpus-wide regressions are measurable. Printed to
/// stderr by [`write_synth`]; `Serialize` so a sidecar JSON can carry it too.
#[derive(Serialize, Default)]
pub struct Census {
    frames: u32,
    instruments: usize,
    tracks: usize,
    notes_total: usize,
    notes_percussion: usize,
    /// Notes that fell to a raw (no-patch) generic-pulse plan — low fidelity.
    notes_raw: usize,
    patches_total: usize,
    /// Patches carrying decoded driver effects (E1) rather than heuristics.
    patches_authored: usize,
    /// Frames with at least one voice routed through the (approximated) filter.
    filter_routed_frames: u32,
    /// Frames with a voice holding ≥2 waveform bits (6581 combine approximation).
    combined_waveform_frames: u32,
    /// Per-frame `$D418` volume changes; `d418_digi_dropped` flags the digi-hammer
    /// case where the volume lane was rejected rather than modulating the mix.
    d418_volume_changes: usize,
    d418_digi_dropped: bool,
    d418_pcm_streams: usize,
    d418_pcm_events: u64,
    vibrato_spans: usize,
    /// Notes whose measured stable prefix cannot use Pertylizer's fade-in-only
    /// per-note vibrato and therefore lower to an exact track-pitch lane.
    vibrato_pitch_automation_notes: usize,
    portamento_spans: usize,
    arpeggio_spans: usize,
    ring_mod_notes: usize,
    /// Notes exported with a *sustained* oscillator hard sync (`sid_oscillator`
    /// `hard_sync = 1`, driven by the note's patch profile). This is the
    /// export-fidelity number: it counts sync that becomes an audible whole-note
    /// timbre in the output.
    ///
    /// It is deliberately **not** the raw count of frames that set the sync bit
    /// (see [`hard_sync_attack_frames`](Self::hard_sync_attack_frames)). A common
    /// Hubbard idiom writes the sync bit for a *single* onset frame as the attack
    /// transient of a noise-body drum (Commando's snares: `pulse+sync` for one
    /// frame → `noise` — 2-frame notes tagged `drum_subclass: Snare`). Those are
    /// routed through the percussion path (a noise-click instrument), *not* the
    /// tonal oscillator, so they correctly do **not** set `hard_sync` and are
    /// **not** counted here. `hard_sync_notes == 0` with
    /// `hard_sync_attack_frames > 0` therefore means "sync is present but only as
    /// percussion-attack transients, faithfully handled by the drum path" — an
    /// expected, non-gap state, not a dropped sustained sync.
    hard_sync_notes: usize,
    /// Gated frames whose control byte sets the sync bit, summed over all voices —
    /// the raw trace measure of hard-sync *usage*, independent of how (or whether)
    /// it is exported. Its purpose is to disambiguate
    /// [`hard_sync_notes`](Self::hard_sync_notes): `> 0` here while that is `0`
    /// identifies the snare-attack-transient case above (so a future reader does
    /// not re-diagnose it as a dropped tonal sync).
    hard_sync_attack_frames: u32,
    hardware_sync_active_frames: u64,
    hardware_sync_inactive_frames: u64,
    hardware_ring_active_frames: u64,
    hardware_ring_inactive_frames: u64,
    combined_waveform_exact_frames: u64,
    combined_waveform_fallback_frames: u64,
    combined_waveform_policies: BTreeMap<String, u64>,
    waveform_programs: WaveformProgramCensus,
    chip_programs: crate::analysis::programs::ProgramCensus,
    osc3: crate::analysis::osc3::Osc3Attribution,
    osc3_contours_exported: u32,
    /// User-facing musical patterns. Automation-only lane containers are
    /// reported separately and never inflate this number.
    patterns: usize,
    automation_patterns: usize,
    serialized_patterns: usize,
    placements: usize,
    note_graphs: usize,
    automation_points: usize,
    serialized_size: u64,
    /// Forward-model note-fidelity residuals (slice 1, report-only): per-note
    /// predicted-vs-trace pitch residuals plus the uncovered-gated-frames
    /// coverage measure. See `export::forward` and `docs/forward-model-gate.md`.
    note_fidelity: forward::ResidualCensus,
    envelope: EnvelopeCensus,
    native: Option<NativeCensus>,
    options: SynthOptions,
    physical_voices: PhysicalVoiceCensus,
    continuous_program: crate::analysis::sid_program::semantic::ContinuousProgramComplexity,
    #[serde(skip_serializing_if = "Option::is_none")]
    lowering: Option<lowering::AutomaticSelection>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum SynthStyle {
    #[default]
    SidFaithful,
    ModernAnalog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct EnhancementAmount(u8);

impl EnhancementAmount {
    pub const MIN: u8 = 1;
    pub const MAX: u8 = 10;

    pub fn new(value: u8) -> Result<Self, EnhancementAmountError> {
        (Self::MIN..=Self::MAX)
            .contains(&value)
            .then_some(Self(value))
            .ok_or(EnhancementAmountError)
    }

    #[must_use]
    pub fn get(self) -> u8 {
        self.0
    }

    fn strength(self) -> f32 {
        f32::from(self.0) / f32::from(Self::MAX)
    }
}

impl std::str::FromStr for EnhancementAmount {
    type Err = EnhancementAmountError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<u8>()
            .ok()
            .and_then(|value| Self::new(value).ok())
            .ok_or(EnhancementAmountError)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("enhance level must be an integer from 1 to 10")]
pub struct EnhancementAmountError;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct SynthOptions {
    pub forward_gate: bool,
    pub arpeggiator_processor: bool,
    pub style: SynthStyle,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enhancement: Option<EnhancementAmount>,
}

impl Default for SynthOptions {
    fn default() -> Self {
        Self {
            forward_gate: true,
            arpeggiator_processor: true,
            style: SynthStyle::SidFaithful,
            enhancement: None,
        }
    }
}

#[derive(Serialize)]
struct NativeCensus {
    driver: String,
    extractor: String,
    validation: crate::export::native::NativeValidationReport,
    provenance: Vec<crate::export::native::ProvenanceEvidence>,
    recovered_structure: Option<crate::export::RecoveredStructure>,
}

/// What a native extractor delivered for one export. Note decoding and source
/// structure recovery are separate capabilities: an extractor can read the
/// driver's own note tables while recovering no orderlist at all, which yields
/// one full-song pattern per physical voice. Corpus reports must not conflate
/// the two, so this never collapses to a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum NativeCapability {
    /// Trace-derived export; no native extractor ran.
    None,
    /// Notes came from the driver's own tables; no recovered structure.
    Decoded,
    /// Notes and the driver's own order/pattern structure were recovered.
    Structured,
}

/// Stable, compact view of a [`Census`] for corpus-scale comparison.
///
/// [`Census`] itself is a deep record whose fields track the exporter's
/// internals; this is the subset a baseline report compares across tunes and
/// across runs. Kept separate so the census can grow without churning the
/// report format.
#[derive(Debug, Clone, Serialize)]
pub struct CensusSummary {
    pub frames: u32,
    pub instruments: usize,
    pub tracks: usize,
    pub notes_total: usize,
    pub patterns: usize,
    pub automation_patterns: usize,
    pub serialized_patterns: usize,
    pub placements: usize,
    pub note_graphs: usize,
    pub automation_points: usize,
    pub serialized_size: u64,
    pub d418_pcm_streams: usize,
    pub d418_pcm_events: u64,
    pub program_occurrences: u64,
    pub reusable_program_groups: u64,
    pub program_serialized_size: u64,
    pub native: NativeCapability,
    pub native_driver: Option<String>,
    pub native_extractor: Option<String>,
    pub notes_ok: u32,
    pub notes_fail: u32,
    pub notes_degraded: u32,
    pub notes_unverifiable: u32,
    pub mean_cents: f32,
    /// Largest per-note mean residual among the recorded worst offenders.
    pub worst_mean_cents: f32,
    /// Gated tonal frames no emitted note covers, summed over all three voices.
    pub uncovered_gated_frames: u32,
}

impl Census {
    /// Compact view for corpus reports. See [`CensusSummary`].
    #[must_use]
    pub fn summary(&self) -> CensusSummary {
        let native = match &self.native {
            None => NativeCapability::None,
            Some(n) if n.recovered_structure.is_some() => NativeCapability::Structured,
            Some(_) => NativeCapability::Decoded,
        };
        let worst_mean_cents = self
            .note_fidelity
            .worst
            .iter()
            .map(|w| w.mean_cents)
            .fold(0.0f32, f32::max);
        CensusSummary {
            frames: self.frames,
            instruments: self.instruments,
            tracks: self.tracks,
            notes_total: self.notes_total,
            patterns: self.patterns,
            automation_patterns: self.automation_patterns,
            serialized_patterns: self.serialized_patterns,
            placements: self.placements,
            note_graphs: self.note_graphs,
            automation_points: self.automation_points,
            serialized_size: self.serialized_size,
            d418_pcm_streams: self.d418_pcm_streams,
            d418_pcm_events: self.d418_pcm_events,
            program_occurrences: self.continuous_program.occurrences.0,
            reusable_program_groups: self.continuous_program.reusable_groups.0,
            program_serialized_size: self.continuous_program.serialized_size.0,
            native,
            native_driver: self.native.as_ref().map(|n| n.driver.clone()),
            native_extractor: self.native.as_ref().map(|n| n.extractor.clone()),
            notes_ok: self.note_fidelity.notes_ok,
            notes_fail: self.note_fidelity.notes_fail,
            notes_degraded: self.note_fidelity.events_degraded,
            notes_unverifiable: self.note_fidelity.notes_unverifiable,
            mean_cents: self.note_fidelity.mean_cents,
            worst_mean_cents,
            uncovered_gated_frames: self.note_fidelity.uncovered_gated_frames.iter().sum(),
        }
    }
}

#[derive(Serialize, Default)]
struct PhysicalVoiceCensus {
    cross_plan_overlap_pairs: usize,
    cross_plan_overlap_calls: u64,
    source_release_overlap_pairs: usize,
    source_release_overlap_calls: u64,
}

struct PhysicalVoiceOwnership {
    starts: [Vec<FrameIndex>; 3],
}

impl PhysicalVoiceOwnership {
    fn from_plans(plans: &[TrackPlan<'_>]) -> Self {
        let mut starts: [Vec<FrameIndex>; 3] = std::array::from_fn(|_| Vec::new());
        for plan in plans {
            let voice_starts = &mut starts[plan.voice_index];
            voice_starts.extend(plan.timeline.iter().map(|(_, event)| event.start_frame));
        }
        for voice_starts in &mut starts {
            voice_starts.sort_unstable_by_key(|frame| frame.0);
            voice_starts.dedup();
        }
        Self { starts }
    }

    fn end(&self, event: &NoteEvent) -> Option<FrameIndex> {
        let starts = &self.starts[event.voice.to_index()];
        let next = starts.partition_point(|frame| *frame <= event.start_frame);
        starts.get(next).copied()
    }
}

fn physical_voice_census(
    plans: &[TrackPlan<'_>],
    states: &[ProgramFrame],
    ownership: &PhysicalVoiceOwnership,
) -> PhysicalVoiceCensus {
    let mut census = PhysicalVoiceCensus::default();
    for left_index in 0..plans.len() {
        for right_index in left_index + 1..plans.len() {
            let left = &plans[left_index];
            let right = &plans[right_index];
            if left.voice != right.voice {
                continue;
            }
            for &(_, left_note) in &left.timeline {
                let left_end = left_note
                    .sound_end_frame(states)
                    .or(left_note.end_frame)
                    .map_or(states.len() as u32, |frame| frame.0);
                for &(_, right_note) in &right.timeline {
                    let right_end = right_note
                        .sound_end_frame(states)
                        .or(right_note.end_frame)
                        .map_or(states.len() as u32, |frame| frame.0);
                    let start = left_note.start_frame.0.max(right_note.start_frame.0);
                    let end = left_end.min(right_end);
                    if start < end {
                        census.source_release_overlap_pairs += 1;
                        census.source_release_overlap_calls += u64::from(end - start);
                    }
                    let left_owned_end = ownership
                        .end(left_note)
                        .map_or(left_end, |frame| left_end.min(frame.0));
                    let right_owned_end = ownership
                        .end(right_note)
                        .map_or(right_end, |frame| right_end.min(frame.0));
                    let owned_end = left_owned_end.min(right_owned_end);
                    if start < owned_end {
                        census.cross_plan_overlap_pairs += 1;
                        census.cross_plan_overlap_calls += u64::from(owned_end - start);
                    }
                }
            }
        }
    }
    census
}

#[derive(Serialize, Default)]
struct EnvelopeCensus {
    calls_gate_off_nonzero: u64,
    calls_gate_on_zero: u64,
    attack_entries: u64,
    release_entries: u64,
    reached_zero: u64,
    gate_blips_never_left_zero: u64,
    active_cycles: u64,
    inexact_voice_calls_excluded: u64,
    measured_velocity_notes: u64,
    static_velocity_notes: u64,
    automated_notes: u64,
    censored_sound_ends: u64,
    release_extended_notes: u64,
    intra_call_retriggers: u64,
    stable_release_tails: u64,
    moving_release_tails: u64,
    waveform_changed_tails_deferred: u64,
}

#[derive(Serialize, Default)]
struct WaveformProgramCensus {
    programs: usize,
    frequency_override_steps: usize,
    deterministic_noise_seeded: usize,
    /// Programs whose voice clocked a combined noise waveform: the hardware
    /// LFSR was corrupted by destructive write-back the model does not
    /// reproduce, so the seed is measured-only, not deterministic.
    noise_seed_poisoned: usize,
    test_transitions_deferred: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChipArticulation {
    release: Option<FrameIndex>,
    sound_end: Option<FrameIndex>,
    peak_level: crate::emu::sid::EnvLevel,
    retriggers: u32,
    exact: bool,
}

fn chip_articulation(event: &NoteEvent, states: &[ProgramFrame]) -> ChipArticulation {
    let start = event.start_frame.0 as usize;
    let release = event.release_frame();
    let sound_end = event.sound_end_frame(states);
    let end = sound_end
        .or(release)
        .map_or(states.len(), |frame| frame.0 as usize + 1)
        .min(states.len());
    let voice = event.voice.to_index();
    let slice = states.get(start..end).unwrap_or(&[]);
    let peak_level = slice
        .iter()
        .map(|state| state.digital_voices[voice].envelope_activity.peak_level)
        .max()
        .unwrap_or_default();
    let attack_in = |state: &ProgramFrame| {
        state.digital_voices[voice]
            .envelope_activity
            .events
            .iter()
            .any(|event| event.kind == crate::emu::sid::EnvelopeEventKind::EnteredAttack)
    };
    let attacks = slice
        .iter()
        .flat_map(|state| &state.digital_voices[voice].envelope_activity.events)
        .filter(|event| event.kind == crate::emu::sid::EnvelopeEventKind::EnteredAttack)
        .count();
    // The onset attack arrives with the gate rise, which for row-leads-gate
    // drivers lands up to GATE_ON_LEAD_MAX frames after the authored start
    // (see [`gate_on_start`]) — search that window when the note begins
    // gate-low. A note already gate-high at its start with no attack there
    // started legato: every attack it sees is a genuine retrigger.
    let onset_attack = if slice.first().is_some_and(attack_in) {
        true
    } else if slice
        .first()
        .is_some_and(|state| !state.voices[voice].control.gate)
    {
        slice
            .iter()
            .take(1 + GATE_ON_LEAD_MAX as usize)
            .any(attack_in)
    } else {
        false
    };
    let retriggers = attacks.saturating_sub(usize::from(onset_attack)) as u32;
    ChipArticulation {
        release,
        sound_end,
        peak_level,
        retriggers,
        exact: !slice.is_empty() && slice.iter().all(|state| state.digital_state_exact),
    }
}

fn articulation_is_automatable(event: &NoteEvent, states: &[ProgramFrame]) -> bool {
    let articulation = chip_articulation(event, states);
    articulation.exact && articulation.sound_end.is_some() && articulation.peak_level.0 > 0
}

fn export_velocity(event: &NoteEvent, states: &[ProgramFrame]) -> f32 {
    let articulation = chip_articulation(event, states);
    if articulation.exact && articulation.peak_level.0 > 0 {
        f32::from(articulation.peak_level.0) / 255.0
    } else {
        f32::from(event.velocity.0) / MIDI_VELOCITY_MAX
    }
}

/// Collect the [`Census`] from the finished export inputs (no extra passes over
/// the driver — all counts come from the trace `states`, the `export` notes /
/// patches / effects, and the built `plans`).
fn collect_census(
    export: &SynthSource<'_>,
    states: &[ProgramFrame],
    plans: &[TrackPlan<'_>],
    ownership: &PhysicalVoiceOwnership,
    instrument_count: usize,
    options: SynthOptions,
) -> Census {
    use crate::analysis::effects::Effect;
    let mut c = Census {
        frames: states.len() as u32,
        instruments: instrument_count,
        tracks: plans.len(),
        notes_total: export.notes.len(),
        osc3: crate::analysis::osc3::attribute_osc3(states),
        native: export.native.map(|native| NativeCensus {
            driver: native.driver.clone(),
            extractor: native.extractor.clone(),
            validation: native.validation.clone(),
            provenance: native
                .fields
                .iter()
                .map(|field| crate::export::native::ProvenanceEvidence {
                    field: field.field.clone(),
                    provenance: native_field_provenance(field.provenance),
                    samples: field.samples.0 as usize,
                    mismatches: field.mismatches.0 as usize,
                })
                .collect(),
            recovered_structure: native.recovered_structure.clone(),
        }),
        options,
        physical_voices: physical_voice_census(plans, states, ownership),
        chip_programs: crate::analysis::programs::analyze_programs(
            &export
                .notes
                .iter()
                .map(|note| note.event)
                .collect::<Vec<_>>(),
            states,
        ),
        ..Census::default()
    };
    if let Some(patches) = export.patches {
        c.patches_total = patches.len();
        c.patches_authored = patches
            .iter()
            .filter(|p| p.authored_effects.is_some())
            .count();
    }
    for s in states {
        let r = s.filter.routing;
        if r.voice1 || r.voice2 || r.voice3 {
            c.filter_routed_frames += 1;
        }
        if s.voices
            .iter()
            .any(|v| v.control.waveform.active_bits().count_ones() >= 2)
        {
            c.combined_waveform_frames += 1;
        }
        for (voice_index, voice) in s.voices.iter().enumerate() {
            let mask = voice.control.waveform.to_control_byte() >> 4;
            if mask.count_ones() >= 2 {
                let model = match export.header.flags.sid_model {
                    SidModel::Mos8580 => "8580",
                    _ => "6581",
                };
                let exact = s.digital_voices[voice_index]
                    .oscillator
                    .combined_waveform_exact;
                let support = combined_waveform_support(export.header.flags.sid_model, mask, exact);
                *c.combined_waveform_policies
                    .entry(format!("{model}:{mask:X}:{support}"))
                    .or_default() += 1;
            }
        }
        c.hard_sync_attack_frames += s
            .voices
            .iter()
            .filter(|v| v.control.sync && v.control.gate)
            .count() as u32;
        if !s.digital_state_exact {
            c.envelope.inexact_voice_calls_excluded += 3;
            continue;
        }
        for (voice_index, (voice, digital)) in s.voices.iter().zip(&s.digital_voices).enumerate() {
            if voice.control.sync {
                if digital.oscillator.sync_resets > 0 {
                    c.hardware_sync_active_frames += 1;
                } else {
                    c.hardware_sync_inactive_frames += 1;
                }
            }
            if voice.control.ring_mod {
                let source = (voice_index + 2) % 3;
                if s.digital_voices[source].oscillator.source_msb_edges > 0
                    && voice.control.waveform.triangle
                {
                    c.hardware_ring_active_frames += 1;
                } else {
                    c.hardware_ring_inactive_frames += 1;
                }
            }
            if voice.control.waveform.active_bits().count_ones() >= 2 {
                if digital.oscillator.combined_waveform_exact {
                    c.combined_waveform_exact_frames += 1;
                } else {
                    c.combined_waveform_fallback_frames += 1;
                }
            }
            let activity = &digital.envelope_activity;
            if !voice.control.gate && digital.envelope.level.0 > 0 {
                c.envelope.calls_gate_off_nonzero += 1;
            }
            if voice.control.gate && digital.envelope.level.0 == 0 {
                c.envelope.calls_gate_on_zero += 1;
            }
            c.envelope.active_cycles += activity.active_cycles.0;
            let mut entered_attack = false;
            let mut entered_release = false;
            let mut left_zero = false;
            for event in &activity.events {
                match event.kind {
                    crate::emu::sid::EnvelopeEventKind::EnteredAttack => {
                        entered_attack = true;
                        c.envelope.attack_entries += 1;
                    }
                    crate::emu::sid::EnvelopeEventKind::EnteredRelease => {
                        entered_release = true;
                        c.envelope.release_entries += 1;
                    }
                    crate::emu::sid::EnvelopeEventKind::ReachedZero => {
                        c.envelope.reached_zero += 1;
                    }
                    crate::emu::sid::EnvelopeEventKind::LeftZero => left_zero = true,
                    crate::emu::sid::EnvelopeEventKind::EnteredDecaySustain => {}
                }
            }
            if entered_attack && entered_release && !left_zero && activity.peak_level.0 == 0 {
                c.envelope.gate_blips_never_left_zero += 1;
            }
        }
    }
    c.d418_volume_changes = states
        .windows(2)
        .filter(|w| w[0].volume != w[1].volume)
        .count();
    c.d418_digi_dropped = c.d418_volume_changes > MAX_VOLUME_LANE_POINTS;
    for e in export.effects {
        match e.effect {
            Effect::Vibrato => c.vibrato_spans += 1,
            Effect::Portamento => c.portamento_spans += 1,
            Effect::Arpeggio => c.arpeggio_spans += 1,
            _ => {}
        }
    }
    for plan in plans {
        let n = plan.timeline.len();
        c.vibrato_pitch_automation_notes += plan
            .timeline
            .iter()
            .filter(|(_, event)| {
                measured_vibrato_for_event(
                    event,
                    states,
                    export.effects,
                    export.timing,
                    plan.voice,
                    plan.voice_index,
                    states.len() as u32,
                )
                .is_some_and(|vibrato| vibrato.delay > 0.0)
            })
            .count();
        let rep = plan.segments.iter().find_map(|s| s.patch);
        if plan.drum_drop || rep.is_some_and(is_sid_percussion) {
            c.notes_percussion += n;
        } else if plan.segments.iter().all(|s| s.patch.is_none()) {
            c.notes_raw += n;
        }
        if plan.waveform_program.is_some() {
            c.waveform_programs.programs += 1;
            let frequencies = program_noise_frequencies(plan, states);
            c.waveform_programs.frequency_override_steps += frequencies
                .iter()
                .filter(|&&frequency| frequency > 0)
                .count();
            if plan_noise_state_poisoned(plan, states) {
                c.waveform_programs.noise_seed_poisoned += 1;
            } else {
                c.waveform_programs.deterministic_noise_seeded += 1;
            }
        }
    }
    c.waveform_programs.test_transitions_deferred = states
        .windows(2)
        .map(|pair| {
            pair[0]
                .voices
                .iter()
                .zip(&pair[1].voices)
                .filter(|(before, after)| before.control.test != after.control.test)
                .count() as u64
        })
        .sum();
    if c.osc3.confidence_percent >= 70 {
        c.osc3_contours_exported = u32::from(match c.osc3.target {
            crate::analysis::osc3::Osc3Target::Pitch => true,
            crate::analysis::osc3::Osc3Target::PulseWidth => {
                plans.iter().any(|plan| plan.shape.waveform == "pulse")
            }
            crate::analysis::osc3::Osc3Target::FilterCutoff => {
                plans.iter().any(|plan| plan.shape.filter_kind.is_some())
            }
            crate::analysis::osc3::Osc3Target::Volume => !c.d418_digi_dropped,
            _ => false,
        });
    }
    for note in &export.notes {
        let articulation = chip_articulation(note.event, states);
        if articulation.exact && articulation.peak_level.0 > 0 {
            c.envelope.measured_velocity_notes += 1;
        } else {
            c.envelope.static_velocity_notes += 1;
        }
        c.envelope.intra_call_retriggers += u64::from(articulation.retriggers);
        if articulation.release.is_some() && articulation.sound_end.is_none() {
            c.envelope.censored_sound_ends += 1;
        }
        if articulation
            .release
            .zip(articulation.sound_end)
            .is_some_and(|(release, sound_end)| sound_end > release)
        {
            c.envelope.release_extended_notes += 1;
            if release_waveform_is_stable(note.event, states) {
                let voice = note.event.voice.to_index();
                let release = articulation.release.unwrap_or(note.event.start_frame);
                let sound_end = articulation.sound_end.unwrap_or(release);
                let moving = states
                    .get(release.0 as usize..sound_end.0 as usize)
                    .unwrap_or(&[])
                    .windows(2)
                    .any(|pair| pair[0].voices[voice].freq != pair[1].voices[voice].freq);
                if moving {
                    c.envelope.moving_release_tails += 1;
                } else {
                    c.envelope.stable_release_tails += 1;
                }
            } else {
                c.envelope.waveform_changed_tails_deferred += 1;
            }
        }
        if let Some(ch) = note.characteristics {
            if ch.hardware_tricks.contains(&HardwareTrick::RingMod) {
                c.ring_mod_notes += 1;
            }
            if ch.hardware_tricks.contains(&HardwareTrick::HardSync) {
                c.hard_sync_notes += 1;
            }
        }
    }
    c.envelope.automated_notes = plans
        .iter()
        .flat_map(|plan| &plan.timeline)
        .filter(|(_, event)| articulation_is_automatable(event, states))
        .count() as u64;
    c
}

fn native_field_provenance(
    provenance: crate::analysis::sid_program::evidence::Provenance,
) -> crate::export::native::FieldProvenance {
    use crate::analysis::sid_program::evidence::Provenance;
    use crate::export::native::FieldProvenance;
    match provenance {
        Provenance::AuthoredVerified => FieldProvenance::AuthoredVerified,
        Provenance::AuthoredDecoded => FieldProvenance::AuthoredDecoded,
        Provenance::AuthoredPartial => FieldProvenance::AuthoredPartial,
        Provenance::TraceMeasured | Provenance::RenderMeasured => FieldProvenance::TraceMeasured,
        Provenance::TraceCorrected => FieldProvenance::TraceCorrected,
        Provenance::Inferred | Provenance::Approximated | Provenance::ExactEmulation => {
            FieldProvenance::Inferred
        }
        Provenance::Unsupported | Provenance::Unknown => FieldProvenance::Unsupported,
    }
}

impl Census {
    /// A compact multi-line stderr summary.
    fn stderr_summary(&self) -> String {
        let pct = |n: u32| {
            if self.frames > 0 {
                100.0 * f64::from(n) / f64::from(self.frames)
            } else {
                0.0
            }
        };
        format!(
            "fidelity census: {frames} frames · {inst} instruments · {tracks} tracks · {notes} notes\n  \
             notes: {perc} percussion · {raw} raw/low-fi · patches {pa}/{pt} authored\n  \
             frames: {fr} filter-routed ({frp:.0}%) · {cw} combined-waveform ({cwp:.0}%)\n  \
             $D418 volume changes {vc}{digi} · PCM {pcm_streams} streams/{pcm_events} events · spans: {vib} vibrato ({vib_auto} pitch automated) · {porta} portamento · {arp} arpeggio\n  \
             hardware: {rm} ring-mod · {hs} hard-sync notes ({hsa} sync-attack frames) · measured sync {msa}/{msi} active/inactive · ring {mra}/{mri}\n  \
             envelope: {atk} attacks · {rel} releases · {zero} reached-zero · velocity {mv}/{sv} measured/static · {auto} automated · {censored} censored · {inexact} inexact voice-calls excluded\n  \
             combined waveforms: {cwe} exact · {cwf} explicit fallback frames\n  \
             OSC3: {osc3:?} · {osc3_reads} reads · {osc3_evidence} evidence · {osc3_conf}% confidence\n  \
             structure: {patterns} musical patterns + {automation_patterns} automation-only ({serialized_patterns} serialized) · {placements} placements · {graphs} note graphs · {points} automation points · {serialized_size} bytes",
            frames = self.frames,
            inst = self.instruments,
            tracks = self.tracks,
            notes = self.notes_total,
            perc = self.notes_percussion,
            raw = self.notes_raw,
            pa = self.patches_authored,
            pt = self.patches_total,
            fr = self.filter_routed_frames,
            frp = pct(self.filter_routed_frames),
            cw = self.combined_waveform_frames,
            cwp = pct(self.combined_waveform_frames),
            vc = self.d418_volume_changes,
            digi = if self.d418_digi_dropped {
                " (DIGI — volume lane dropped)"
            } else {
                ""
            },
            pcm_streams = self.d418_pcm_streams,
            pcm_events = self.d418_pcm_events,
            vib = self.vibrato_spans,
            vib_auto = self.vibrato_pitch_automation_notes,
            porta = self.portamento_spans,
            arp = self.arpeggio_spans,
            rm = self.ring_mod_notes,
            hs = self.hard_sync_notes,
            hsa = self.hard_sync_attack_frames,
            msa = self.hardware_sync_active_frames,
            msi = self.hardware_sync_inactive_frames,
            mra = self.hardware_ring_active_frames,
            mri = self.hardware_ring_inactive_frames,
            atk = self.envelope.attack_entries,
            rel = self.envelope.release_entries,
            zero = self.envelope.reached_zero,
            mv = self.envelope.measured_velocity_notes,
            sv = self.envelope.static_velocity_notes,
            auto = self.envelope.automated_notes,
            censored = self.envelope.censored_sound_ends,
            inexact = self.envelope.inexact_voice_calls_excluded,
            cwe = self.combined_waveform_exact_frames,
            cwf = self.combined_waveform_fallback_frames,
            osc3 = self.osc3.target,
            osc3_reads = self.osc3.reads,
            osc3_evidence = self.osc3.evidence,
            osc3_conf = self.osc3.confidence_percent,
            patterns = self.patterns,
            automation_patterns = self.automation_patterns,
            serialized_patterns = self.serialized_patterns,
            placements = self.placements,
            graphs = self.note_graphs,
            points = self.automation_points,
            serialized_size = self.serialized_size,
        ) + &format!(
            "\n  physical voices: {} rendered overlap pairs · {} overlap calls; {} source release overlaps choked ({} calls)\n  {}",
            self.physical_voices.cross_plan_overlap_pairs,
            self.physical_voices.cross_plan_overlap_calls,
            self.physical_voices.source_release_overlap_pairs,
            self.physical_voices.source_release_overlap_calls,
            self.note_fidelity.summary(),
        )
    }
}

pub fn write_synth(program: &AnalyzedSidProgram, out: &mut dyn Write) -> io::Result<Census> {
    write_synth_with_options(program, out, SynthOptions::default())
}

pub fn write_synth_with_options(
    program: &AnalyzedSidProgram,
    out: &mut dyn Write,
    options: SynthOptions,
) -> io::Result<Census> {
    let capabilities =
        lowering::TargetCapabilities::from_pinned_mirrors().map_err(io::Error::other)?;
    write_selected_synth(program, &capabilities, &[], out, options, true)
}

pub fn write_synth_quiet(program: &AnalyzedSidProgram, out: &mut dyn Write) -> io::Result<Census> {
    let capabilities =
        lowering::TargetCapabilities::from_pinned_mirrors().map_err(io::Error::other)?;
    write_selected_synth(
        program,
        &capabilities,
        &[],
        out,
        SynthOptions::default(),
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_selected_synth(
    program: &AnalyzedSidProgram,
    capabilities: &lowering::TargetCapabilities,
    evaluations: &[lowering::CandidateRenderEvaluation],
    out: &mut dyn Write,
    options: SynthOptions,
    print_census: bool,
) -> io::Result<Census> {
    let selection = lowering::select_automatic(
        program,
        capabilities,
        lowering::StateBudget::default(),
        evaluations,
        crate::audio::abtest::AudioBudget::default(),
    );
    if !selection.render.uncovered.is_empty() {
        return Err(io::Error::other(format!(
            "no state/render-valid representation covers {:?}",
            selection.render.uncovered
        )));
    }
    let semantic = &program.semantic;
    let export = SynthSource {
        header: &program.source.header,
        timing: program.source.timing,
        frame_count: program.frame_count(),
        patches: Some(&semantic.patches),
        notes: EnrichedNote::enrich(
            &semantic.notes,
            Some(&semantic.patch_assignments),
            Some(&semantic.characteristics),
        ),
        effects: &semantic.effects,
        structure: semantic.structure.as_deref(),
        native: semantic.native.as_ref(),
    };
    let mut census = write_synth_source(&export, program.frames(), out, options, false)?;
    let digi_streams = crate::analysis::sid_program::observable::summarize_d418_streams(program)
        .map_err(io::Error::other)?;
    census.d418_pcm_streams = digi_streams.len();
    census.d418_pcm_events = digi_streams
        .iter()
        .map(|stream| stream.source_events.0)
        .sum();
    census.continuous_program = program.semantic.continuous.complexity;
    census.lowering = Some(selection);
    if print_census {
        eprintln!("{}", census.stderr_summary());
    }
    Ok(census)
}

fn write_synth_source(
    export: &SynthSource<'_>,
    states: &[ProgramFrame],
    out: &mut dyn Write,
    options: SynthOptions,
    print_census: bool,
) -> io::Result<Census> {
    if options.style == SynthStyle::ModernAnalog && options.enhancement.is_some() {
        return Err(io::Error::other(
            "enhancement cannot be combined with the modern analog style",
        ));
    }
    let frame_count = export.frame_count as u32;
    let time_base = derive_time_base(export, frame_count);
    // The first SID's model selects the measured cutoff curve (6581 vs 8580);
    // multi-SID tunes are out of scope, so the primary chip's model is enough.
    let model = export.header.flags.sid_model;
    let plans = build_track_plans(export, states);
    let ownership = PhysicalVoiceOwnership::from_plans(&plans);
    let mut instruments = build_instruments(&plans, states, model, export.timing);
    let shared_filter = (options.style == SynthStyle::SidFaithful)
        .then(|| shared_filter_spec(&plans, states, model))
        .flatten();
    if shared_filter.is_some() {
        strip_instrument_filter_copies(&plans, &mut instruments);
    }
    let mut census = collect_census(
        export,
        states,
        &plans,
        &ownership,
        instruments.len(),
        options,
    );
    // When a driver-native extractor recovered the real song structure, ship the
    // driver's reused pattern blocks; otherwise emit the flat whole-song layout.
    // Both paths feed the forward-model residual census (report-only, slice 1).
    let fidelity = &mut census.note_fidelity;
    let mut song = match export.structure {
        Some(structure) if !structure.is_empty() => build_song_structured(
            export,
            states,
            &time_base,
            frame_count,
            &plans,
            &ownership,
            structure,
            fidelity,
            options,
        ),
        _ => build_song(
            export,
            states,
            &time_base,
            frame_count,
            &plans,
            &ownership,
            fidelity,
            options,
        ),
    };
    let mut global = match options.style {
        SynthStyle::SidFaithful => GlobalProjectState::default(),
        SynthStyle::ModernAnalog => {
            modern::apply_modern_analog(&plans, &mut instruments, &mut song);
            modern::global_state()
        }
    };
    if let Some(filter) = shared_filter {
        apply_shared_filter_routing(&plans, filter, &mut song);
        global.return_bus_effects.push(ReturnBusEffectsState {
            id: 0,
            effects: vec![shared_filter_module(filter)],
        });
    }
    if let Some(amount) = options.enhancement {
        enhance::apply(&plans, &mut instruments, &mut global, amount);
    }
    census.patterns = song
        .patterns
        .iter()
        .filter(|pattern| !pattern.notes.is_empty())
        .count();
    census.automation_patterns = song
        .patterns
        .iter()
        .filter(|pattern| pattern.notes.is_empty() && !pattern.automation.is_empty())
        .count();
    census.serialized_patterns = song.patterns.len();
    census.placements = song.arrangement.len();
    census.note_graphs = song.note_graphs.len();
    census.automation_points = song
        .patterns
        .iter()
        .flat_map(|pattern| &pattern.automation)
        .map(|lane| lane.points.len())
        .sum();
    census.note_fidelity.finalize(states, export.timing.clock);

    let project = ProjectFile {
        file_type: "project",
        version: "1.0",
        instruments,
        active_instrument_id: 1,
        song,
        global,
    };

    let serialized = serde_json::to_vec_pretty(&project).map_err(io::Error::other)?;
    census.serialized_size = serialized.len() as u64;
    if print_census {
        eprintln!("{}", census.stderr_summary());
    }
    out.write_all(&serialized)?;
    Ok(census)
}

/// The graph shape shared by every [`Segment`] merged into one instrument: it
/// fixes the module skeleton [`Instrument::build`] emits (source kind, waveform,
/// ring-mod insert, filter presence/type), so two segments that share it produce
/// byte-identical graphs and differ only in *automatable* parameters (pulse
/// width, cutoff, ADSR). Waveform and filter type are enums Pertylizer cannot
/// automate, so they stay part of the shape — patches that differ in either
/// never merge.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct MergeShape {
    is_noise: bool,
    waveform: &'static str,
    /// The second tonal component of a combined-waveform byte (see
    /// [`combined_secondary_waveform`]), or `None` for a single waveform. Part of
    /// the merge key so a combined patch (e.g. pulse+triangle) never folds into a
    /// single-waveform patch of the same dominant class (plain pulse).
    secondary_waveform: Option<&'static str>,
    has_ring: bool,
    has_sync: bool,
    /// `None` when this voice does not route through the filter, else the derived
    /// filter type string (`"lowpass"`, `"bandpass"`, …).
    filter_kind: Option<&'static str>,
    /// The authored continuous-PWM program of a pulse patch (E3 program
    /// replay), normalised to `None` for a trivial step. Part of the merge key:
    /// the program becomes one static `script` module per instrument
    /// ([`authored_pwm_script`]), so a sweeping patch and a non-sweeping (or
    /// differently-sweeping) patch must not fold into the same instrument.
    authored_pwm: Option<AuthoredPwm>,
    /// The one-shot waveform program ([`PatchVoiceProfile::waveform_program`])
    /// as `(steps, len)` — fixed-size so the shape stays `Copy`. Part of the
    /// merge key: the program is a static sid-module sequence per instrument,
    /// so differently-programmed patches must not fold.
    waveform_program: Option<([u8; 16], u8)>,
    /// The repeating waveform-mask program carried by the patch profile. This
    /// is distinct from a per-note one-shot program and must remain part of the
    /// merge identity or a static patch can erase the loop.
    waveform_loop: Option<([u8; 16], u8)>,
    tonal_arp: Option<([i8; 64], u8)>,
}

const SID_NOISE_SEED: u32 = 0x007F_FFF8;

/// Whether any frame the plan's notes cover carries a poisoned LFSR state
/// (a combined noise waveform was clocked — sticky, see
/// [`crate::emu::sid::OscillatorSnapshot::noise_state_poisoned`]).
fn plan_noise_state_poisoned(plan: &TrackPlan, states: &[ProgramFrame]) -> bool {
    plan.timeline.iter().any(|&(_, event)| {
        let range = event.frame_range(states.len());
        states[range].iter().any(|state| {
            state.digital_voices[plan.voice_index]
                .oscillator
                .noise_state_poisoned
        })
    })
}

fn program_noise_frequencies(plan: &TrackPlan, states: &[ProgramFrame]) -> [u16; 16] {
    let mut frequencies = [0u16; 16];
    let Some(program) = plan.waveform_program.as_deref() else {
        return frequencies;
    };
    for (step, &mask) in program.iter().enumerate().take(16) {
        if mask & 0x8 == 0 {
            continue;
        }
        let mut samples: Vec<u16> = plan
            .timeline
            .iter()
            .filter_map(|(_, event)| {
                states
                    .get(event.start_frame.0 as usize + step)
                    .map(|state| state.voices[plan.voice_index].freq.0)
            })
            .filter(|&frequency| frequency > 0)
            .collect();
        samples.sort_unstable();
        if let Some(&frequency) = samples.get(samples.len() / 2) {
            frequencies[step] = frequency;
        }
    }
    frequencies
}

/// [`MergeShape`]'s fixed-size form of a plan group's one-shot waveform program.
fn shape_program(program: Option<&[u8]>) -> Option<([u8; 16], u8)> {
    let program = program?;
    let mut steps = [0u8; 16];
    steps[..program.len()].copy_from_slice(program);
    Some((steps, program.len() as u8))
}

fn shape_tonal_arp(profile: Option<&PatchVoiceProfile>) -> Option<([i8; 64], u8)> {
    let offsets = profile?.arpeggio_loop.as_deref()?;
    if offsets.is_empty() || offsets.len() > 64 {
        return None;
    }
    let mut steps = [0i8; 64];
    steps[..offsets.len()].copy_from_slice(offsets);
    Some((steps, offsets.len() as u8))
}

/// One source [`Patch`] feeding a [`TrackPlan`]: its per-voice timbre profile and
/// the notes it backs. A merged plan holds several segments (same voice, same
/// [`MergeShape`]); a raw or arpeggio plan holds exactly one.
struct Segment<'a> {
    patch: Option<&'a Patch>,
    profile: Option<&'a PatchVoiceProfile>,
    events: Vec<&'a NoteEvent>,
}

/// One planned output track: all of a SID voice's notes that share a single
/// [`MergeShape`], drawn from one or more source patches (`segments`), plus the
/// dedicated instrument id minted for it.
///
/// A SID voice is monophonic, so every note across the plan's segments plays in
/// sequence without overlap — they share one instrument whose per-section
/// differences (pulse width, cutoff, ADSR) are driven by automation lanes (see
/// [`build_plan_automation`]). The merge is **within one voice only**: Pertylizer
/// applies an instrument's automation to its single shared instance, so two
/// voices sharing a patch still get separate instrument copies (their lanes would
/// otherwise collide — the chip has independent per-voice registers, the synth
/// does not). `voice` is part of every merge key, so this holds automatically.
struct TrackPlan<'a> {
    instrument_id: u32,
    voice: VoiceId,
    voice_index: usize,
    shape: MergeShape,
    /// A dedicated percussion plan holding this voice's drum-drop notes
    /// ([`drum_drop_dest`]), split off so they sound a pulse drum body instead of
    /// inheriting the lead patch's (often triangle) timbre. Drives
    /// [`build_instruments`] to mint a [`drum_drop_instrument`].
    drum_drop: bool,
    /// Trace-measured noise accent aggregated over the drum-drop notes
    /// (median noise-frame register, mean run length); `None` for melodic
    /// plans and for drops whose notes carried no noise frames.
    drum_noise: Option<NoiseAccent>,
    /// The one-shot waveform program shared by every note in this plan
    /// ([`NoteCharacteristics::waveform_program`], part of the group key) —
    /// exported as a held sid-module sequence on the plan's instrument.
    waveform_program: Option<Vec<u8>>,
    /// Source patches feeding this plan; `segments[0]` is the representative the
    /// instrument's static parameters come from (automation overrides per note).
    segments: Vec<Segment<'a>>,
    /// Every note across all segments in global time order, tagged with its
    /// owning segment index. Drives note emission and the ADSR step lanes: a
    /// voice can re-use two patches in alternation, so the ADSR sequence follows
    /// the timeline, not the segment order.
    timeline: Vec<(usize, &'a NoteEvent)>,
}

/// The [`MergeShape`] a `(patch, profile)` pair emits. Generic pulse for a raw
/// group (no patch), matching [`generic_pulse_instrument`].
fn shape_of(
    patch: Option<&Patch>,
    profile: Option<&PatchVoiceProfile>,
    program: Option<&[u8]>,
) -> MergeShape {
    match (patch, profile) {
        (Some(patch), Some(profile)) => {
            let is_noise = patch.waveform & 0x80 != 0;
            let waveform = if is_noise {
                "noise"
            } else {
                waveform_string(patch.waveform)
            };
            MergeShape {
                is_noise,
                waveform,
                secondary_waveform: (!is_noise)
                    .then(|| combined_secondary_waveform(patch.waveform))
                    .flatten(),
                has_ring: profile.hardware_tricks.contains(&HardwareTrick::RingMod),
                has_sync: profile.hardware_tricks.contains(&HardwareTrick::HardSync),
                filter_kind: profile
                    .filter_routed
                    .then(|| filter_kind(profile.filter_mode)),
                authored_pwm: (waveform == "pulse")
                    .then(|| patch.authored_effects.as_ref().and_then(|e| e.pwm))
                    .flatten()
                    .filter(|p| p.step != 0),
                waveform_program: shape_program(program),
                waveform_loop: shape_program(observed_waveform_program(profile).as_deref()),
                tonal_arp: shape_tonal_arp(Some(profile)),
            }
        }
        _ => MergeShape {
            is_noise: false,
            waveform: "pulse",
            secondary_waveform: None,
            has_ring: false,
            has_sync: false,
            filter_kind: None,
            authored_pwm: None,
            waveform_program: None,
            waveform_loop: None,
            tonal_arp: None,
        },
    }
}

/// Maximum start-frame gap between an unassigned note and the assigned note
/// whose patch it may adopt — a coarse backstop (~10 s PAL / ~8 s NTSC) against
/// reskinning a note stranded across a long silence. Timbre compatibility, not
/// distance, is the primary gate.
const ADOPTION_MAX_DISTANCE_FRAMES: u32 = 500;

/// Per-field ADSR nibble tolerance for adoption. Attack shapes the onset and
/// sustain sets the held level — the two most audible fields — so they screen
/// tighter; the decay/release rates get more slack.
const ADOPTION_ADSR_SHAPE_TOL: u8 = 3;
const ADOPTION_ADSR_RATE_TOL: u8 = 5;

/// The patch id an unassigned note should adopt: the temporally nearest assigned
/// candidate whose timbre is compatible ([`timbre_compatible`]) and that lies
/// within [`ADOPTION_MAX_DISTANCE_FRAMES`]. Returns `None` — routing the note to
/// a raw generic-pulse plan — when it carries no characteristics to compare or
/// no candidate qualifies, so a genuine one-shot / SFX note is never reskinned
/// as the wrong instrument.
fn adopt_patch(
    ev: &NoteEvent,
    characteristics: Option<&NoteCharacteristics>,
    assigned: &[(u32, u16, &Patch, &PatchVoiceProfile)],
) -> Option<u16> {
    let note = characteristics?;
    assigned
        .iter()
        .filter(|&&(f, _, patch, profile)| {
            ev.start_frame.0.abs_diff(f) <= ADOPTION_MAX_DISTANCE_FRAMES
                && timbre_compatible(note, patch, profile)
        })
        .min_by_key(|&&(f, _, _, _)| ev.start_frame.0.abs_diff(f))
        .map(|&(_, k, _, _)| k)
}

/// Whether an unassigned note's timbre is close enough to a candidate patch to
/// play on its instrument. Waveform family and filter routing must match: the
/// exported [`MergeShape`] keys the oscillator and filter on the patch, so a
/// mismatch would re-skin the note to the wrong sound. The ADSR envelope only
/// has to be *close* ([`adsr_compatible`]) — an identical envelope would already
/// have clustered into this patch, so sub-threshold notes are near, not equal.
fn timbre_compatible(
    note: &NoteCharacteristics,
    patch: &Patch,
    profile: &PatchVoiceProfile,
) -> bool {
    let note_wave = note.dominant_waveform_byte();
    // Noise vs tonal must agree; two tonal notes must share a waveform-select
    // bit (`$10` tri / `$20` saw / `$40` pulse) — pulse ↔ pulse+triangle is fine,
    // pulse ↔ triangle is not.
    if (note_wave & 0x80) != (patch.waveform & 0x80) {
        return false;
    }
    if note_wave & 0x80 == 0 && note_wave & patch.waveform & 0x70 == 0 {
        return false;
    }
    if note.filter_routed != profile.filter_routed {
        return false;
    }
    adsr_compatible(note.starting_adsr, patch.adsr)
}

/// Two ADSR envelopes within the adoption tolerance — attack/sustain (shape)
/// tight, decay/release (rates) looser.
fn adsr_compatible(a: Adsr, b: Adsr) -> bool {
    a.attack.abs_diff(b.attack) <= ADOPTION_ADSR_SHAPE_TOL
        && a.sustain.abs_diff(b.sustain) <= ADOPTION_ADSR_SHAPE_TOL
        && a.decay.abs_diff(b.decay) <= ADOPTION_ADSR_RATE_TOL
        && a.release.abs_diff(b.release) <= ADOPTION_ADSR_RATE_TOL
}

/// Group every voice's notes into [`TrackPlan`]s. Notes first cluster into the
/// per-`(voice, patch)` groups the timbre layer produced (first-appearance, i.e.
/// time, order); groups on the same voice that share a [`MergeShape`] then fold
/// into one merged plan, while raw groups and arpeggio patches (which expand
/// notes per their own offsets) stay standalone. Plans are ordered by their
/// earliest note and minted stable 1-based instrument ids, so instruments are
/// never shared across tracks.
fn build_track_plans<'a>(export: &SynthSource<'a>, states: &[ProgramFrame]) -> Vec<TrackPlan<'a>> {
    struct Proto<'a> {
        voice: VoiceId,
        voice_index: usize,
        patch: Option<&'a Patch>,
        profile: Option<&'a PatchVoiceProfile>,
        shape: MergeShape,
        mergeable: bool,
        drum_drop: bool,
        drum_noise: Option<NoiseAccent>,
        waveform_program: Option<Vec<u8>>,
        events: Vec<&'a NoteEvent>,
    }

    let frame_count = export.frame_count as u32;
    let mut protos: Vec<Proto<'a>> = Vec::new();
    for vi in 0..3 {
        let voice = VoiceId::from_index(vi);

        // Drum-drops split off their owning patch group onto a dedicated
        // percussion plan, so the pulse drum body never carries the lead patch's
        // timbre (patch clustering binds them to the melodic instrument).
        let mut drum_events: Vec<&'a NoteEvent> = Vec::new();
        let mut drum_chars: Vec<&'a NoteCharacteristics> = Vec::new();
        let mut melodic: Vec<(&'a NoteEvent, Option<u16>, Option<&'a NoteCharacteristics>)> =
            Vec::new();
        for n in export.notes.iter().filter(|n| n.event.voice == voice) {
            if drum_drop_dest(
                n.event,
                states,
                export.effects,
                export.timing,
                voice,
                vi,
                frame_count,
            )
            .is_some()
            {
                drum_events.push(n.event);
                if let Some(ch) = n.characteristics {
                    drum_chars.push(ch);
                }
                continue;
            }
            melodic.push((n.event, n.patch_id.map(|p| p.0), n.characteristics));
        }

        // Sub-threshold timbre clusters (fewer than `MIN_PATCH_MEMBERS` notes)
        // leave their notes unassigned. Rather than spawn a stray generic-pulse
        // "raw" plan — plus its own full-length automation carrier — each such
        // note may adopt the patch of the temporally nearest *timbre-compatible*
        // assigned note on the same voice, so a one-off fingerprint plays with
        // its phrase's instrument. The gate (`adopt_patch`) requires matching
        // waveform family + filter routing and a close ADSR envelope, within a
        // bounded time gap, so a genuine one-shot / SFX note is never reskinned
        // as the wrong instrument; when nothing qualifies the note keeps `None`
        // and falls through to a genuine raw plan.
        //
        // Arpeggio patches are excluded as adoption targets: a flat raw note
        // folded into one would inherit its `arpeggio_loop` and explode into
        // chord steps (an audible change), so it must only join a non-arp patch.
        // Only the unassigned-note case needs the adoption candidates, so skip
        // the whole scan when every note already clustered (the common case).
        let assigned: Vec<(u32, u16, &'a Patch, &'a PatchVoiceProfile)> =
            if melodic.iter().any(|&(_, k, _)| k.is_none()) {
                melodic
                    .iter()
                    .filter_map(|&(ev, k, _)| {
                        let k = k?;
                        let patch = export
                            .patches
                            .and_then(|ps| ps.get(k as usize))
                            .filter(|p| p.id.0 == k)?;
                        let profile = patch.profile(voice)?;
                        let arp = profile
                            .arpeggio_loop
                            .as_deref()
                            .is_some_and(|a| !a.is_empty());
                        (!arp).then_some((ev.start_frame.0, k, patch, profile))
                    })
                    .collect()
            } else {
                Vec::new()
            };
        // Group by patch, then sub-group by each note's own one-shot waveform
        // program: one cluster can span several driver wavetables (snare vs
        // kick stabs share ADSR+waveform), and rendering a majority program on
        // every member plays noise where the chip held pulse — measured 9.5 →
        // 11.5 dB regression on the AWM drums window before this split. Per-note
        // grouping keeps every emitted program note-verified.
        type GroupKey = (Option<u16>, Option<Vec<u8>>);
        let mut order: Vec<GroupKey> = Vec::new();
        let mut groups: HashMap<GroupKey, Vec<&'a NoteEvent>> = HashMap::new();
        for (ev, k, ch) in melodic {
            let patch_key = k.or_else(|| adopt_patch(ev, ch, &assigned));
            let program = patch_key
                .is_some()
                .then(|| ch.and_then(NoteCharacteristics::waveform_program))
                .flatten();
            let key = (patch_key, program);
            let bucket = groups.entry(key.clone()).or_default();
            if bucket.is_empty() {
                order.push(key);
            }
            bucket.push(ev);
        }

        for key in order {
            let events = groups.remove(&key).unwrap_or_default();
            let (patch_key, program) = key;
            // Patch ids are 0-based and assigned in order, so a `Some(pid)`
            // group keys straight into `export.patches`.
            let patch = patch_key.and_then(|pid| {
                export
                    .patches
                    .and_then(|patches| patches.get(pid as usize))
                    .filter(|p| p.id.0 == pid)
            });
            let profile = patch.and_then(|p| p.profile(voice));
            let mergeable = profile.is_some_and(|p| {
                p.arpeggio_loop
                    .as_deref()
                    .is_none_or(|offsets| offsets.is_empty() || offsets.len() <= 64)
            });
            let mut shape = shape_of(patch, profile, program.as_deref());
            if profile.is_none()
                && let Some(filter) = events
                    .first()
                    .and_then(|event| states.get(event.start_frame.0 as usize))
                    .map(|state| state.filter)
                    .filter(|filter| filter.routing.contains(voice))
            {
                shape.filter_kind = Some(filter_kind(filter.mode));
            }
            if profile.is_none() {
                for event in &events {
                    let range = event.frame_range(states.len());
                    for state in &states[range] {
                        shape.has_ring |= state.voices[vi].control.ring_mod;
                        shape.has_sync |= state.voices[vi].control.sync;
                    }
                }
            }
            protos.push(Proto {
                voice,
                voice_index: vi,
                patch,
                profile,
                shape,
                mergeable,
                drum_drop: false,
                drum_noise: None,
                waveform_program: program,
                events,
            });
        }

        // One percussion plan per voice that has drum-drops: a standalone pulse
        // body (no source patch → `drum_drop_instrument`), ordered with the rest
        // by its earliest note. The drop notes' measured noise stats aggregate
        // the same way a patch profile's do (median frequency, mean run).
        if !drum_events.is_empty() {
            let noise_freq = median_hertz(drum_chars.iter().filter_map(|c| c.noise_freq_hz));
            let runs: Vec<f32> = drum_chars
                .iter()
                .map(|c| c.noise_run_frames)
                .filter(|&r| r > 0.0)
                .collect();
            let mean_run = if runs.is_empty() {
                0.0
            } else {
                runs.iter().sum::<f32>() / runs.len() as f32
            };
            protos.push(Proto {
                voice,
                voice_index: vi,
                patch: None,
                profile: None,
                shape: shape_of(None, None, None),
                mergeable: false,
                drum_drop: true,
                drum_noise: NoiseAccent::from_capture(noise_freq, mean_run, export.timing),
                waveform_program: None,
                events: drum_events,
            });
        }
    }

    // Fold mergeable protos sharing `(voice, shape)` into one plan; everything
    // else becomes a singleton. `merged[(vi, shape)]` indexes into `plans`.
    let mut plans: Vec<TrackPlan<'a>> = Vec::new();
    let mut merged: HashMap<(usize, MergeShape), usize> = HashMap::new();
    for proto in protos {
        let segment = Segment {
            patch: proto.patch,
            profile: proto.profile,
            events: proto.events,
        };
        if proto.mergeable {
            let slot = *merged
                .entry((proto.voice_index, proto.shape))
                .or_insert_with(|| {
                    plans.push(TrackPlan {
                        instrument_id: 0,
                        voice: proto.voice,
                        voice_index: proto.voice_index,
                        shape: proto.shape,
                        // Mergeable protos are never drum-drops (those are forced
                        // standalone), so a merged plan is always melodic.
                        drum_drop: false,
                        drum_noise: None,
                        waveform_program: proto.waveform_program.clone(),
                        segments: Vec::new(),
                        timeline: Vec::new(),
                    });
                    plans.len() - 1
                });
            plans[slot].segments.push(segment);
        } else {
            plans.push(TrackPlan {
                instrument_id: 0,
                voice: proto.voice,
                voice_index: proto.voice_index,
                shape: proto.shape,
                drum_drop: proto.drum_drop,
                drum_noise: proto.drum_noise,
                waveform_program: proto.waveform_program,
                segments: vec![segment],
                timeline: Vec::new(),
            });
        }
    }

    // Build each plan's time-ordered timeline, then order plans by their earliest
    // note and mint stable 1-based instrument ids (HashMap order is irrelevant).
    for plan in &mut plans {
        let mut timeline: Vec<(usize, &'a NoteEvent)> = Vec::new();
        for (si, seg) in plan.segments.iter().enumerate() {
            for ev in &seg.events {
                timeline.push((si, ev));
            }
        }
        timeline.sort_by_key(|(_, ev)| ev.start_frame.0);
        plan.timeline = timeline;
    }
    plans.retain(|p| !p.timeline.is_empty());
    plans.sort_by_key(|p| {
        p.timeline
            .first()
            .map_or(u32::MAX, |(_, ev)| ev.start_frame.0)
    });
    for (i, plan) in plans.iter_mut().enumerate() {
        plan.instrument_id = i as u32 + 1;
    }

    plans
}

/// The oscillator `detune` param range (± cents), from the module spec.
const DETUNE_LIMIT_CENTS: f32 = 100.0;

/// Representative static fine-detune (cents) for a plan's oscillator, mapped to
/// the module `detune` param. The chip tunes a whole voice, so a deliberately
/// detuned voice carries a consistent per-note cents offset; the median across
/// the plan's notes captures that while rejecting the per-note pitch-analysis
/// scatter a mean would smear. Clamped to the module's ±[`DETUNE_LIMIT_CENTS`]
/// range; `0.0` for a plan with no notes.
fn plan_detune_cents(plan: &TrackPlan) -> f32 {
    let mut cents: Vec<f32> = plan.timeline.iter().map(|(_, ev)| ev.cents.0).collect();
    if cents.is_empty() {
        return 0.0;
    }
    cents.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = cents.len() / 2;
    let median = if cents.len().is_multiple_of(2) {
        (cents[mid - 1] + cents[mid]) / 2.0
    } else {
        cents[mid]
    };
    median.clamp(-DETUNE_LIMIT_CENTS, DETUNE_LIMIT_CENTS)
}

/// Build one instrument per [`TrackPlan`] — a patch-derived instrument for a
/// clustered group, a generic pulse instrument for a `None` (raw) group. Never
/// reuses an instrument across plans. Falls back to a single generic instrument
/// when there are no tracks at all, so `active_instrument_id: 1` stays valid.
///
/// Every instrument shares the same mix `volume` ([`mix_volume`]) so the summed
/// master bus keeps headroom below 0 dBFS.
fn build_instruments(
    plans: &[TrackPlan],
    states: &[ProgramFrame],
    model: SidModel,
    timing: PlaybackTiming,
) -> Vec<Instrument> {
    let clock = timing.clock;
    if plans.is_empty() {
        return vec![generic_pulse_instrument(1, VoiceId::V1, model, clock)];
    }
    let volume = mix_volume(plans);
    plans
        .iter()
        .map(|plan| {
            // Static timbre comes from the representative segment; the per-section
            // differences across the other segments ride automation lanes.
            let rep = plan.segments.first();
            let mut instrument = if plan.drum_drop {
                drum_drop_instrument(
                    plan.instrument_id,
                    plan.voice,
                    plan.drum_noise,
                    model,
                    clock,
                )
            } else {
                match rep.and_then(|s| s.patch.zip(s.profile)) {
                    // Dynamic waveform evidence is the instrument program: an
                    // instrument-level loop or note-level one-shot renders on
                    // the exact SID sequence path below. The generic
                    // body+click model is only the final fallback when neither
                    // observed representation exists.
                    Some((patch, profile))
                        if use_generic_percussion(
                            patch,
                            profile,
                            plan.waveform_program.as_deref(),
                        ) =>
                    {
                        sid_percussion_instrument(
                            plan.instrument_id,
                            format!("V{} perc", plan.voice.0),
                            InstrumentRole::from_role_tags(&patch.role_tags),
                            adsr_to_seconds(patch.adsr),
                            NoiseAccent::from_capture(
                                profile.noise_freq_hz,
                                profile.noise_run_frames,
                                timing,
                            ),
                            model,
                            clock,
                        )
                    }
                    Some((patch, profile)) => instrument_from_patch(
                        plan.instrument_id,
                        plan.voice,
                        patch,
                        profile,
                        plan.waveform_program.as_deref(),
                        program_noise_frequencies(plan, states),
                        model,
                        timing,
                        plan_detune_cents(plan),
                        plan_authored_pwm(plan, states),
                    ),
                    None => raw_trace_instrument(plan, states, model, clock),
                }
            };
            if plan_uses_measured_envelope(plan, states) {
                enable_measured_envelope(&mut instrument);
            }
            if let Some(cutoff_hz) = plan_output_cutoff(plan) {
                add_output_lowpass(&mut instrument, cutoff_hz);
            }
            enforce_oscillator_restart_policy(plan, states, &mut instrument);
            enforce_measured_hardware_effects(plan, states, &mut instrument);
            instrument.volume = volume * plan_mix_trim(plan);
            // Name by the merge shape (matching the track/pattern), not the lone
            // representative patch — a merged instrument spans several patches.
            instrument.name = plan_name(plan);
            instrument
        })
        .collect()
}

fn enforce_oscillator_restart_policy(
    plan: &TrackPlan,
    states: &[ProgramFrame],
    instrument: &mut Instrument,
) {
    let seeded: Vec<u32> = plan
        .timeline
        .iter()
        .filter_map(|(_, event)| {
            let program = crate::analysis::programs::analyze_note(event, states);
            if program.oscillator_start != crate::analysis::programs::OscillatorStart::TestReset {
                return None;
            }
            states.get(event.start_frame.0 as usize).and_then(|state| {
                let oscillator = state.digital_voices[plan.voice_index].oscillator;
                (!oscillator.noise_state_poisoned).then_some(oscillator.noise_shift_register)
            })
        })
        .collect();
    if seeded.len() != plan.timeline.len() || seeded.windows(2).any(|pair| pair[0] != pair[1]) {
        return;
    }
    let Some(seed) = seeded.first().copied() else {
        return;
    };
    for module in &mut instrument.patch.modules {
        if module.id == "sid-1"
            && let Parameters::SidOscillator(params) = &mut module.parameters
        {
            params.noise_seed = seed as f32;
        }
    }
}

fn enforce_measured_hardware_effects(
    plan: &TrackPlan,
    states: &[ProgramFrame],
    instrument: &mut Instrument,
) {
    let mut exact = true;
    let mut sync_active = false;
    let mut ring_active = false;
    let mut static_silent_sources = Vec::new();
    for &(_, event) in &plan.timeline {
        if let Some(evidence) = crate::analysis::programs::analyze_note(event, states).cross_voice
            && evidence.role == crate::analysis::programs::ModulatorRole::SilentModulator
            && evidence.source_frequency_stable
        {
            static_silent_sources.push(evidence.source_frequency.0);
        }
        let start = event.start_frame.0 as usize;
        let end = event
            .sound_end_frame(states)
            .or(event.end_frame)
            .map_or(states.len(), |frame| frame.0 as usize + 1)
            .min(states.len());
        for state in states.get(start..end).unwrap_or(&[]) {
            exact &= state.digital_state_exact;
            let oscillator = state.digital_voices[plan.voice_index].oscillator;
            let source = (plan.voice_index + 2) % 3;
            let source_edges = state.digital_voices[source].oscillator.source_msb_edges;
            sync_active |= source_edges > 0 && oscillator.sync_resets > 0;
            ring_active |=
                source_edges > 0 && state.voices[plan.voice_index].control.waveform.triangle;
        }
    }
    if !exact {
        return;
    }
    let static_source = (!static_silent_sources.is_empty()
        && static_silent_sources
            .windows(2)
            .all(|pair| pair[0] == pair[1]))
    .then(|| static_silent_sources[0]);
    for module in &mut instrument.patch.modules {
        if module.id == "sid-1"
            && let Parameters::SidOscillator(params) = &mut module.parameters
        {
            if params.hard_sync > 0.0 && !sync_active {
                params.hard_sync = 0.0;
            }
            if params.ring_mod > 0.0 && !ring_active {
                params.ring_mod = 0.0;
            }
        }
        if module.id == "sid-2"
            && let (Some(frequency), Parameters::SidOscillator(params)) =
                (static_source, &mut module.parameters)
        {
            params.freq_reg = f32::from(frequency);
            params.track_pitch = 0.0;
        }
    }
}

fn enable_measured_envelope(instrument: &mut Instrument) {
    instrument.velocity_amp_sensitivity = 0.0;
    for module in &mut instrument.patch.modules {
        if let Parameters::Envelope(envelope) = &mut module.parameters {
            envelope.attack = 0.0;
            envelope.decay = 0.0;
            envelope.sustain = 1.0;
            envelope.release = 0.0;
            envelope.vel_sens = 0.0;
        }
    }
}

/// Per-instrument mix volume that keeps the summed master bus below full scale:
/// the [`MIX_HEADROOM`] budget divided by the number of distinct SID voices that
/// carry notes (1..=3). Tracks of the same voice never overlap (a SID voice is
/// monophonic), so simultaneity is bounded by the active voice count, not the
/// track count — a tune split into many patch-tracks still only sounds three
/// voices at once.
fn mix_volume(plans: &[TrackPlan]) -> f32 {
    let mut voices = [false; 3];
    for plan in plans {
        if let Some(slot) = voices.get_mut(plan.voice_index) {
            *slot = true;
        }
    }
    let active = voices.iter().filter(|&&v| v).count().max(1) as f32;
    // The §A9 coloring now lives on the master bus ([`master_chain`]), not per
    // instrument, so no per-track makeup trim — the master fader compensates the
    // tube's level instead ([`MASTER_VOLUME`]).
    MIX_HEADROOM / active
}

/// Per-plan correction layered on top of [`mix_volume`]. The role comes from
/// analyzed timbre evidence, so this applies to equivalent patches in every
/// tune rather than to a title, voice number, or instrument ID.
fn plan_mix_trim(plan: &TrackPlan) -> f32 {
    if plan.drum_drop {
        return DRUM_DROP_MIX_TRIM;
    }
    let role_trim = match plan_role(plan) {
        InstrumentRole::Lead if plan_arpeggiated(plan) => ARPEGGIATED_LEAD_MIX_TRIM,
        InstrumentRole::Lead => LEAD_MIX_TRIM,
        InstrumentRole::Drum(_) => DRUM_MIX_TRIM,
        _ => 1.0,
    };
    if plan.shape.waveform == "pulse" && plan.shape.secondary_waveform == Some("triangle") {
        role_trim * TRIANGLE_PULSE_MIX_TRIM
    } else {
        role_trim
    }
}

fn plan_output_cutoff(plan: &TrackPlan) -> Option<Hertz> {
    if plan.drum_drop {
        return Some(DRUM_DROP_OUTPUT_CUTOFF);
    }
    match plan_role(plan) {
        InstrumentRole::Lead if plan_arpeggiated(plan) => Some(ARPEGGIATED_LEAD_OUTPUT_CUTOFF),
        InstrumentRole::Drum(_) if plan.timeline.len() >= DRUM_OUTPUT_FILTER_MIN_NOTES => {
            Some(DRUM_OUTPUT_CUTOFF)
        }
        _ => None,
    }
}

/// Insert a non-resonant low-pass immediately before the patch output. The
/// programmable SID filter, when present, remains earlier in the voice graph.
fn add_output_lowpass(instrument: &mut Instrument, cutoff: Hertz) {
    let Some(output_connection) = instrument
        .patch
        .connections
        .iter_mut()
        .find(|connection| connection.to == ["out-1", "in"])
    else {
        return;
    };
    output_connection.to = ["flt-2", "in"];
    instrument.patch.modules.push(Module {
        id: "flt-2",
        kind: "filter",
        position: Position { x: 640.0, y: 32.0 },
        scripts: None,
        parameters: Parameters::Filter(FilterParams {
            cutoff: cutoff.0 as f32,
            cv_amt: 0.0,
            drive: 1.0,
            env_amt: 0.0,
            key_track: 0.0,
            model: "standard",
            morph: 0.0,
            resonance: 0.0,
            kind: "lowpass",
        }),
    });
    instrument.patch.connections.push(Connection {
        from: ["flt-2", "out"],
        to: ["out-1", "in"],
    });
}

/// The pulse-width bounce band of the Hubbard continuous-PWM effect: the sweep
/// reflects when the register's high nibble reaches `$08` / `$0E`
/// (`docs/drivers/hubbard.md`, the `$524C` block).
const PWM_BOUNCE_LO: f32 = 2048.0; // $800
/// Bounce span `$800..$E00` in raw register units.
const PWM_BOUNCE_SPAN: f32 = 1536.0; // $E00 - $800

/// YAMS source reproducing the driver's continuous PWM as a *program* instead
/// of a baked per-frame lane (rollout E3, program replay). The driver adds
/// `step` to the pulse width every `period_frames` frames and reflects inside
/// the `$800..$E00` bounce band; the script regenerates that exact staircase
/// triangle from `age` (per-voice seconds since note-on — the driver resets
/// its sweep at note setup) and emits the *offset* from the instrument's base
/// `pw_reg`, which the `sid_oscillator.pwm` input adds back in raw register
/// units. Every constant is a named `let` so the generated script reads as
/// the driver program it is; YAMS needs no division (the rate is pre-divided)
/// and the script stays a pure function of `age`.
fn authored_pwm_script(pwm: AuthoredPwm, pw_reg: u16, timing: PlaybackTiming) -> String {
    let steps_per_second = timing.calls_per_second() as f32 / f32::from(pwm.period_frames.max(1));
    let step = f32::from(pwm.step);
    // Phase origin: start the triangle at the instrument's base pulse width so
    // the offset is 0 at note-on (the driver starts from the authored PW).
    let phi0 = (f32::from(pw_reg) - PWM_BOUNCE_LO).clamp(0.0, PWM_BOUNCE_SPAN);
    format!(
        "let steps_per_sec = {rate}\n\
         let step_units = {step}\n\
         let band_floor = {floor}\n\
         let band_span = {span}\n\
         let origin_units = {phi0}\n\
         let base_pw = {pw0}\n\
         let t = floor(age * steps_per_sec)\n\
         let x = origin_units + step_units * t\n\
         let tri = band_span - abs((x % (2 * band_span)) - band_span)\n\
         out = band_floor + tri - base_pw",
        rate = steps_per_second,
        step = step,
        floor = PWM_BOUNCE_LO,
        span = PWM_BOUNCE_SPAN,
        phi0 = phi0,
        pw0 = f32::from(pw_reg),
    )
}

/// Per-frame pulse-width band tolerance for the authored-PWM program, in raw
/// units on top of two `step`s of slop (a bounce can overshoot the band edge
/// by up to a step without an audible duty difference).
const PW_RESIDUAL_BASE_TOL: f32 = 64.0;

/// Accepted ratio window between the trace's measured per-frame pulse-width
/// movement and the program's `step / period` rate. Inside it the program
/// sweeps like the driver; a flat register (driver gated the effect off —
/// the Monty V1 conditional-PWM case) falls below, and a sweep an order of
/// magnitude faster than the decoded parameters (a mis-decoded `+6` variant)
/// lands above.
const PW_RATE_RATIO: std::ops::RangeInclusive<f32> = 0.4..=2.5;

/// Minimum consecutive pulse-frame pairs before the rate check has enough
/// evidence to judge (mirrors the old "too short to show a step" skip).
const PW_RATE_MIN_PAIRS: u32 = 8;

/// The authored continuous-PWM program of a plan, or `None` when the plan must
/// keep the baked `pw_reg` automation lane. The program is part of the
/// [`MergeShape`] key ([`shape_of`] normalises it: pulse waveform, non-trivial
/// step), so every segment of a merged plan shares it by construction; this
/// re-checks the instrument variants that bypass the plain single-`sid` build
/// (drum drop, waveform alternation) and then **verifies the program with the
/// forward model** (slice 5, replacing the bespoke min-range confirm),
/// phase-blind like the vibrato check — the script restarts its staircase at
/// note-on while e.g. Commando's driver free-runs the sweep across notes, so
/// phase is unknowable and benign. Two measured conditions per event:
///
/// - **Band** ([`forward::pw_band_residual`]): the traced register must live
///   inside the script's hardcoded `$800..$E00` bounce band. Monty's variant
///   sweeps `$400..$E80` — the script would fold that duty cycle wrong, so
///   the program is rejected and the exact baked lane keeps playing.
/// - **Rate** ([`forward::pw_step_rate`]): the traced per-frame movement
///   must be within [`PW_RATE_RATIO`] of the program's `step/period`. A flat
///   register (the driver engages `+6` conditionally — Monty's V1 lead,
///   measured 25 dB vs the lane's 12 dB when emitted unconditionally) falls
///   below; a mis-decoded `+6` variant sweeping an order of magnitude faster
///   lands above.
///
/// The exporter emits the program *instead of* the lane, so both
/// [`build_instruments`] and [`build_plan_automation`] must take the same
/// branch.
fn plan_authored_pwm(plan: &TrackPlan, states: &[ProgramFrame]) -> Option<AuthoredPwm> {
    if plan.drum_drop {
        return None;
    }
    let profile = plan.segments.first()?.profile?;
    if observed_waveform_program(profile).is_some() {
        return None;
    }
    let pwm = plan.shape.authored_pwm?;

    let spec = forward::PwBandSpec {
        floor: PWM_BOUNCE_LO,
        span: PWM_BOUNCE_SPAN,
        tol: 2.0 * f32::from(pwm.step) + PW_RESIDUAL_BASE_TOL,
    };
    let program_rate = f32::from(pwm.step) / f32::from(pwm.period_frames.max(1));
    let mut verified_any = false;
    for &(_, event) in &plan.timeline {
        let range = event.frame_range(states.len());
        let frames = range.len() as u32;
        if frames == 0 {
            continue;
        }
        let start = range.start as u32;
        match forward::pw_band_residual(&spec, start, frames, plan.voice_index, states) {
            Some(r) if !r.passes() => return None,
            Some(_) => verified_any = true,
            None => continue,
        }
        if let Some((rate, pairs)) = forward::pw_step_rate(start, frames, plan.voice_index, states)
            && pairs >= PW_RATE_MIN_PAIRS
            && !PW_RATE_RATIO.contains(&(rate / program_rate.max(f32::EPSILON)))
        {
            return None;
        }
    }
    verified_any.then_some(pwm)
}

#[allow(clippy::too_many_arguments)]
fn instrument_from_patch(
    id: u32,
    voice: VoiceId,
    patch: &Patch,
    profile: &PatchVoiceProfile,
    program: Option<&[u8]>,
    program_frequencies: [u16; 16],
    model: SidModel,
    timing: PlaybackTiming,
    detune: f32,
    authored_pwm: Option<AuthoredPwm>,
) -> Instrument {
    let clock = timing.clock;
    let adsr = adsr_to_seconds(patch.adsr);

    // Static pulse-width centre from THIS voice's own pulse-width envelope
    // (raw 12-bit register); per-frame movement is reproduced by a `pw_reg`
    // automation lane (see [`build_automation`]), not a fixed-rate LFO.
    //
    // A patch carrying the authored-PWM *program* instead uses the authored
    // `+0/+1` pulse width: the driver loads it at note setup, so it is the
    // sweep's phase origin — regenerating the sweep from the trace's
    // mid-envelope lands the duty cycle wrong (measured 25 dB vs the baked
    // lane's 12 dB against reSID on the Monty V1 lead).
    let authored_pw_init = authored_pwm
        .is_some()
        .then(|| patch.authored_effects.as_ref()?.pw_init)
        .flatten();
    let pw_reg = if let Some(pw_init) = authored_pw_init {
        pw_init.0 & 0x0FFF
    } else if patch.waveform & 0x40 != 0 {
        let lo = u32::from(profile.pw_envelope.min.0);
        let hi = u32::from(profile.pw_envelope.max.0);
        ((lo + hi) / 2).min(4095) as u16
    } else {
        2048
    };
    // A looping alternation wins (it cycles for the whole note); otherwise the
    // plan group's one-shot waveform program plays once and holds its last
    // step — the Hubbard drum/stab wavetable (`T N T P N N N N N P P P` →
    // pulse), whose noise steps the static dominant waveform silently dropped
    // (the dark-drums half of the 2026-07-05 ear-pass finding). The program is
    // part of the plan's group key, so every note on this instrument measured
    // exactly this program.
    let (seq, seq_hold) = match observed_waveform_program(profile) {
        Some(alternation) => (Some(alternation), false),
        None => (program.map(<[u8]>::to_vec), true),
    };
    let ring_mod = profile.hardware_tricks.contains(&HardwareTrick::RingMod);
    let hard_sync = profile.hardware_tricks.contains(&HardwareTrick::HardSync);
    let source = SidSource {
        // With a live sequence the static mask is ignored (the module cycles the
        // seq steps); keep the patch's waveform for the seq-less path.
        mask: (patch.waveform >> 4) & 0x0F,
        pw_reg,
        seq,
        seq_frequencies: program_frequencies,
        seq_hold,
        ring_mod,
        hard_sync,
        model,
        clock,
    };
    let filter = FilterSpec::from_profile(profile, model);
    let neighbour_freq_reg = neighbour_freq_reg_from_profile(profile, clock);
    let name = if source.seq.is_some() {
        format!(
            "V{} patch {} ({} seq)",
            voice.0,
            patch.id.0,
            source.waveform_label()
        )
    } else {
        format!(
            "V{} patch {} ({})",
            voice.0,
            patch.id.0,
            source.waveform_label()
        )
    };
    let role = InstrumentRole::from_role_tags(&patch.role_tags);
    let pwm_script = authored_pwm.map(|pwm| authored_pwm_script(pwm, pw_reg, timing));
    Instrument::build(
        id,
        name,
        role,
        source,
        adsr,
        filter,
        neighbour_freq_reg,
        detune,
        pwm_script,
    )
}

/// The waveform-mask sequence of an alternating voice — the SID waveform-switch
/// idiom (the Hubbard tri↔noise lead, e.g. Nemesis V2) — mapped to the
/// `sid_oscillator`'s native `seq_*` steps (bit 0 = triangle, 1 = saw, 2 =
/// pulse, 3 = noise). Every captured period step is retained, including held
/// frames, so Daglish's `T T N N` loop remains four frames rather than being
/// accelerated to `T N`. [`SidOscillatorParams::set_seq`] advances it once per
/// driver frame and loops for the whole note.
///
/// Periods that fit the target's 16 steps are preserved verbatim. Longer
/// captured bodies fall back to their distinct audible masks: patch aggregation
/// can retain a long representative window for a short alternation (Nemesis V2
/// yields 22 frames), and dropping that sequence would regress the established
/// two-step representation. Ring/sync ride the single oscillator; ring affects
/// only triangle frames, exactly like the chip.
fn alternation_seq(waveform_loop: Option<&[u8]>) -> Option<Vec<u8>> {
    let loop_body = waveform_loop?;
    if loop_body.len() < 2 {
        return None;
    }
    let masks: Vec<u8> = loop_body.iter().map(|byte| (byte >> 4) & 0x0F).collect();
    let mut audible = masks.iter().copied().filter(|mask| *mask != 0);
    let first = audible.next()?;
    if !audible.any(|mask| mask != first) {
        return None;
    }
    if masks.len() <= 16 {
        return Some(masks);
    }

    let mut distinct = Vec::new();
    for mask in masks {
        if mask != 0 && !distinct.contains(&mask) {
            distinct.push(mask);
        }
    }
    Some(distinct)
}

fn observed_waveform_program(profile: &PatchVoiceProfile) -> Option<Vec<u8>> {
    alternation_seq(profile.waveform_loop.as_deref())
}

/// The captured neighbour-voice (ring modulator / sync master) frequency as a
/// raw SID register, when the profile carries ring-mod or hard sync. Ring and
/// sync share the same physical source on the chip (voice N−1), so one
/// register serves both inputs; ring's capture wins when both are present.
fn neighbour_freq_reg_from_profile(profile: &PatchVoiceProfile, clock: SystemClock) -> Option<u16> {
    let has_ring = profile.hardware_tricks.contains(&HardwareTrick::RingMod);
    let has_sync = profile.hardware_tricks.contains(&HardwareTrick::HardSync);
    if !has_ring && !has_sync {
        return None;
    }
    let hz = profile
        .ring_source_hz
        .or(profile.sync_source_hz)
        .map_or(RING_MOD_CARRIER_FREQ, |h| h.0 as f32);
    Some(sid_freq_reg_from_hz(hz, clock))
}

// Filter tables: Copyright (C) 2004 Dag Lem, GPL-2.0-or-later.
// Adapted by Per Jonsson in 2026; see THIRD_PARTY_NOTICES.md.
/// Measured 6581 filter cutoff curve: `(FC register, cutoff Hz)` spline knots
/// from Dag Lem's reSID (`f0_points_6581` in `filter.cc`), obtained by feeding
/// the chip an external signal and measuring the response. The curve is steeply
/// non-linear and dark across the lower registers, with the characteristic
/// discontinuity at FC 1024 (the "filter dip": 6000 Hz → 4600 Hz) — both are
/// reproduced. reSID's duplicated endpoint knots are dropped here; we
/// piecewise-linearly interpolate the distinct, FC-ascending points.
///
/// Source: <https://github.com/simonowen/resid/blob/master/filter.cc>
/// (reSID 0.16, Dag Lem). Background: libsidplayfp wiki, "SID internals — 6581
/// Filter overview" <https://sourceforge.net/p/sidplay-residfp/wiki/SID%20internals%20-%206581%20Filter%20overview/>.
const F0_6581: [(u16, f32); 27] = [
    (0, 220.0),
    (128, 230.0),
    (256, 250.0),
    (384, 300.0),
    (512, 420.0),
    (640, 780.0),
    (768, 1600.0),
    (832, 2300.0),
    (896, 3200.0),
    (960, 4300.0),
    (992, 5000.0),
    (1008, 5400.0),
    (1016, 5700.0),
    (1023, 6000.0),
    (1024, 4600.0),
    (1032, 4800.0),
    (1056, 5300.0),
    (1088, 6000.0),
    (1120, 6600.0),
    (1152, 7200.0),
    (1280, 9500.0),
    (1408, 12000.0),
    (1536, 14500.0),
    (1664, 16000.0),
    (1792, 17100.0),
    (1920, 17700.0),
    (2047, 18000.0),
];

/// Measured 8580 filter cutoff curve: `(FC register, cutoff Hz)` knots from
/// reSID's `f0_points_8580`. The 8580 filter is near-linear and brighter than
/// the 6581. Source as for [`F0_6581`].
const F0_8580: [(u16, f32); 17] = [
    (0, 0.0),
    (128, 800.0),
    (256, 1600.0),
    (384, 2500.0),
    (512, 3300.0),
    (640, 4100.0),
    (768, 4800.0),
    (896, 5600.0),
    (1024, 6500.0),
    (1152, 7500.0),
    (1280, 8400.0),
    (1408, 9200.0),
    (1536, 9800.0),
    (1664, 10500.0),
    (1792, 11000.0),
    (1920, 11700.0),
    (2047, 12500.0),
];

/// The measured cutoff curve for `model`. `Both`/`Unknown` fall back to the
/// 6581 — the classic chip whose dark, non-linear filter defines the era's
/// character (and the safer choice to avoid an over-bright export).
fn cutoff_table(model: SidModel) -> &'static [(u16, f32)] {
    match model {
        SidModel::Mos8580 => &F0_8580,
        _ => &F0_6581,
    }
}

/// Piecewise-linear interpolation of a measured `(FC, Hz)` curve at `fc`. The
/// knots are FC-ascending, so each segment has a non-zero FC span (no division
/// by zero); values outside the knot range clamp to the end points.
fn interp_cutoff_curve(table: &[(u16, f32)], fc: f32) -> f32 {
    let first = table[0];
    let last = table[table.len() - 1];
    if fc <= f32::from(first.0) {
        return first.1;
    }
    if fc >= f32::from(last.0) {
        return last.1;
    }
    for pair in table.windows(2) {
        let (lo, hi) = (pair[0], pair[1]);
        let hx = f32::from(hi.0);
        if fc <= hx {
            let lx = f32::from(lo.0);
            let t = (fc - lx) / (hx - lx);
            return lo.1 + (hi.1 - lo.1) * t;
        }
    }
    last.1
}

/// Map a raw 11-bit SID cutoff value (`0.0..=2047.0`) to Hz via the measured
/// reSID curve for `model` ([`cutoff_table`]), clamped to Pertylizer's filter
/// `cutoff` bounds. Shared by the static `flt-1.cutoff` value and the cutoff
/// automation lane so the two agree.
fn cutoff_value_to_hz(fc: f32, model: SidModel) -> f32 {
    let fc = fc.clamp(0.0, SID_CUTOFF_MAX);
    cutoff_param().clamp(interp_cutoff_curve(cutoff_table(model), fc))
}

/// Invert Pertylizer's `filter.cutoff` mapping to the normalized `0.0..=1.0`
/// automation-lane value, through the `filter.cutoff` descriptor's range and
/// (logarithmic) response curve.
fn normalize_cutoff_hz(hz: f32) -> f32 {
    cutoff_param().normalize(hz)
}

/// Map a raw 12-bit SID pulse-width register to the normalized `0.0..=1.0`
/// automation-lane value through the `sid_oscillator.pw_reg` descriptor
/// (linear over `0..=4095` — the lane carries the register itself, no
/// band-mapping).
fn normalize_pulse_width(pw: u16) -> f32 {
    pulse_width_param().normalize(f32::from(pw))
}

/// A `flt-1` filter inserted between the source and the amplifier for a
/// filter-routed `(voice, patch)` group. `cutoff_hz`/`resonance`/`kind` are
/// derived from the global filter state during the frames where *this voice* is
/// routed (see [`voice_filter_spec`]); the remaining `flt-1` params take static
/// defaults (see [`Instrument::build`]).
/// The Pertylizer filter `type` string for a SID filter mode. SID low-pass +
/// high-pass enabled together forms a notch (the schema has a dedicated `notch`
/// type), so check that combination before the single-bit arms; default to
/// lowpass when no mode bit is set. Independent of chip model.
fn filter_kind(mode: FilterMode) -> &'static str {
    if mode.low_pass && mode.high_pass {
        "notch"
    } else if mode.low_pass {
        "lowpass"
    } else if mode.band_pass {
        "bandpass"
    } else if mode.high_pass {
        "highpass"
    } else {
        "lowpass"
    }
}

#[derive(Clone, Copy)]
struct FilterSpec {
    cutoff_hz: f32,
    resonance: f32,
    kind: &'static str,
    leak: f32,
}

impl FilterSpec {
    /// Build a filter spec from a cutoff range, resonance, and mode. The cutoff
    /// uses the range midpoint; the mode maps by priority
    /// `low_pass+high_pass → notch`, then `low_pass > band_pass > high_pass`,
    /// defaulting to lowpass when no mode bit is set.
    fn from_state(
        min_cutoff: u16,
        max_cutoff: u16,
        resonance: Resonance,
        mode: FilterMode,
        model: SidModel,
    ) -> Self {
        let fc = (f32::from(min_cutoff) + f32::from(max_cutoff)) / 2.0;
        let cutoff_hz = cutoff_value_to_hz(fc, model);
        let resonance =
            (f32::from(resonance.0) / SID_RESONANCE_MAX * RESONANCE_MAX_NORM).clamp(0.0, 1.0);
        let leak = filter_leak(cutoff_hz, filter_kind(mode), model);
        Self {
            cutoff_hz,
            resonance,
            kind: filter_kind(mode),
            leak,
        }
    }

    /// Filter insert for a voice's profile: `Some` iff that voice routes through
    /// the filter, built from its (voice-correct) cutoff contour, mode and
    /// resonance. `None` leaves the instrument unfiltered.
    fn from_profile(profile: &PatchVoiceProfile, model: SidModel) -> Option<Self> {
        profile.filter_routed.then(|| {
            Self::from_state(
                profile.filter_contour.min.0,
                profile.filter_contour.max.0,
                profile.filter_resonance,
                profile.filter_mode,
                model,
            )
        })
    }
}

/// Select the exact shared-return representation when every routed note sees
/// one stable chip-filter state. Routing may change between notes and voices;
/// cutoff/resonance/mode may not, because the pinned Pertylizer schema has no
/// automation target for a return-bus effect parameter. Dynamic filter states
/// keep the established per-instrument representation instead of losing their
/// sweep.
fn shared_filter_spec(
    plans: &[TrackPlan<'_>],
    states: &[ProgramFrame],
    model: SidModel,
) -> Option<FilterSpec> {
    let mut reference: Option<(u16, u8, FilterMode)> = None;
    let mut routed_frames = 0usize;
    for plan in plans {
        let expected_routing = plan.shape.filter_kind.is_some();
        for &(_, event) in &plan.timeline {
            for state in &states[event.frame_range(states.len())] {
                let routed = state.filter.routing.contains(plan.voice);
                if routed != expected_routing {
                    return None;
                }
                if !routed {
                    continue;
                }
                routed_frames += 1;
                let observed = (
                    state.filter.cutoff.0,
                    state.filter.resonance.0,
                    state.filter.mode,
                );
                if reference.is_some_and(|known| known != observed) {
                    return None;
                }
                reference = Some(observed);
            }
        }
    }
    let (cutoff, resonance, mode) = reference.filter(|_| routed_frames > 0)?;
    let spec = FilterSpec::from_state(cutoff, cutoff, Resonance(resonance), mode, model);
    Some(spec)
}

fn strip_instrument_filter_copies(plans: &[TrackPlan<'_>], instruments: &mut [Instrument]) {
    for plan in plans.iter().filter(|plan| plan.shape.filter_kind.is_some()) {
        let Some(instrument) = instruments
            .iter_mut()
            .find(|instrument| instrument.id == plan.instrument_id)
        else {
            continue;
        };
        if !instrument
            .patch
            .modules
            .iter()
            .any(|module| module.id == "flt-1")
        {
            continue;
        }
        instrument
            .patch
            .modules
            .retain(|module| module.id != "flt-1" && module.id != "mix-1");
        instrument.patch.connections.retain(|connection| {
            !matches!(connection.from[0], "flt-1" | "mix-1")
                && !matches!(connection.to[0], "flt-1" | "mix-1")
        });
        if !instrument.patch.connections.iter().any(|connection| {
            connection.from == ["sid-1", "out"] && connection.to == ["amp-1", "in"]
        }) {
            instrument.patch.connections.push(Connection {
                from: ["sid-1", "out"],
                to: ["amp-1", "in"],
            });
        }
    }
}

fn shared_filter_module(filter: FilterSpec) -> Module {
    Module {
        id: "flt-1",
        kind: "filter",
        position: Position { x: 0.0, y: 0.0 },
        scripts: None,
        parameters: Parameters::Filter(FilterParams {
            cutoff: filter.cutoff_hz,
            resonance: filter.resonance,
            kind: filter.kind,
            model: SID_FILTER_MODEL,
            drive: 1.0,
            cv_amt: 0.0,
            env_amt: 0.0,
            key_track: 0.0,
            morph: 0.0,
        }),
    }
}

fn apply_shared_filter_routing(plans: &[TrackPlan<'_>], filter: FilterSpec, song: &mut Song) {
    for (plan, track) in plans.iter().zip(song.tracks.iter_mut()) {
        if plan.shape.filter_kind.is_some() {
            track.volume = 0.0;
            track.sends.push(TrackSend {
                target: 0,
                level: 1.0,
                pre_fader: true,
                enabled: true,
            });
            if filter.leak > 0.0 {
                track.sends.push(TrackSend {
                    target: 1,
                    level: filter.leak,
                    pre_fader: true,
                    enabled: true,
                });
            }
        }
    }
    song.return_busses.push(ReturnBus {
        id: 0,
        name: "SID filter",
        volume: 1.0,
        pan: 0.0,
        mute: false,
        solo: false,
        color: TrackColor {
            r: 205,
            g: 120,
            b: 55,
        },
        description: "Shared MOS SID filter for dynamically routed voices",
        sends: Vec::new(),
    });
    if filter.leak > 0.0 {
        song.return_busses.push(ReturnBus {
            id: 1,
            name: "SID filter leak",
            volume: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            color: TrackColor {
                r: 170,
                g: 100,
                b: 45,
            },
            description: "Measured 6581 low-cutoff dry leakage",
            sends: Vec::new(),
        });
    }
    song.next_return_bus_id = Some(if filter.leak > 0.0 { 2 } else { 1 });
}

fn filter_leak(cutoff_hz: f32, kind: &str, model: SidModel) -> f32 {
    if model != SidModel::Mos6581 || kind != "lowpass" {
        return 0.0;
    }
    let position = ((FILTER_LEAK_END_HZ - cutoff_hz) / (FILTER_LEAK_END_HZ - FILTER_LEAK_FULL_HZ))
        .clamp(0.0, 1.0);
    position * FILTER_LEAK_MAX
}

fn generic_pulse_instrument(
    id: u32,
    voice: VoiceId,
    model: SidModel,
    clock: SystemClock,
) -> Instrument {
    let adsr = adsr_to_seconds(Adsr {
        attack: 0,
        decay: 8,
        sustain: 8,
        release: 8,
    });
    let source = SidSource::simple(0x4, 2048, model, clock);
    Instrument::build(
        id,
        format!("V{} (SID pulse)", voice.0),
        InstrumentRole::Untagged,
        source,
        adsr,
        None,
        None,
        0.0,
        None,
    )
}

fn raw_trace_instrument(
    plan: &TrackPlan,
    states: &[ProgramFrame],
    model: SidModel,
    clock: SystemClock,
) -> Instrument {
    let Some(event) = plan.timeline.first().map(|(_, event)| *event) else {
        return generic_pulse_instrument(plan.instrument_id, plan.voice, model, clock);
    };
    let Some(state) = states.get(event.start_frame.0 as usize) else {
        return generic_pulse_instrument(plan.instrument_id, plan.voice, model, clock);
    };
    let voice = &state.voices[plan.voice_index];
    let mask = voice.control.waveform.to_control_byte() >> 4;
    if mask == 0 {
        return generic_pulse_instrument(plan.instrument_id, plan.voice, model, clock);
    }
    let filter = state.filter.routing.contains(plan.voice).then(|| {
        FilterSpec::from_state(
            state.filter.cutoff.0,
            state.filter.cutoff.0,
            state.filter.resonance,
            state.filter.mode,
            model,
        )
    });
    let mut source = SidSource::simple(mask, voice.pulse_width.0, model, clock);
    source.ring_mod = plan.shape.has_ring;
    source.hard_sync = plan.shape.has_sync;
    let source_voice = (plan.voice_index + 2) % 3;
    let mut source_frequencies: Vec<SidFreq> = plan
        .timeline
        .iter()
        .flat_map(|(_, event)| &states[event.frame_range(states.len())])
        .map(|state| state.voices[source_voice].freq)
        .filter(|frequency| *frequency != SidFreq(0))
        .collect();
    source_frequencies.sort_unstable();
    let neighbour_freq_reg = source_frequencies
        .get(source_frequencies.len() / 2)
        .map(|frequency| frequency.0)
        .filter(|_| plan.shape.has_ring || plan.shape.has_sync);
    Instrument::build(
        plan.instrument_id,
        format!("V{} raw SID", plan.voice.0),
        InstrumentRole::Untagged,
        source,
        adsr_to_seconds(voice.adsr),
        filter,
        neighbour_freq_reg,
        0.0,
        None,
    )
}

/// Mixer gain of the noise-click accent relative to the pulse body (`1.0`). The
/// 6581 fires a short burst of noise at the gate before the pulse body settles
/// (the `---N` frame in the Auf Wiedersehen Monty drum-drop trace); the click is
/// what gives the drum its punch. Tuned by ear against the bar-21 Monty render
/// (reSID `analyze_section`): at `0.7` the click was a near-inaudible transient
/// (crest +1.8 dB over no click); `1.5` lands a real attack (crest +4 dB,
/// high-band +49 %) without clipping. The render also showed noise *colour* (LFSR
/// vs white) barely moved the click — level is the lever — so the body keeps the
/// SID-faithful LFSR. Ear-tunable alongside [`DRUM_CLICK_ADSR`].
const DRUM_CLICK_LEVEL: f32 = 1.5;

/// Envelope of the noise-click accent: a fast percussive transient (~1-2 frames
/// of noise, no sustain), gating a dedicated noise source in
/// [`drum_drop_instrument`]. The 0.04 s decay was the ear-tuned click length.
const DRUM_CLICK_ADSR: AdsrSeconds = AdsrSeconds {
    attack: 0.0,
    decay: 0.04,
    sustain: 0.0,
    release: 0.02,
};

/// Trace-measured noise accent for a percussion instrument: the LFSR clock
/// pinned to the driver's noise-frame register and the click decay sized by
/// the measured noise-run length. Without this the noise click tracks the
/// note's (low) body pitch — a drum body at E-2 clocks the LFSR ~19× slower
/// than the `$684C` the driver actually writes on its noise frames, which is
/// the "export renders dark" half of the drums-window gap; the fixed 2-frame
/// [`DRUM_CLICK_ADSR`] against Auf Wiedersehen Monty's measured 5-frame noise
/// runs is the other half.
#[derive(Clone, Copy)]
struct NoiseAccent {
    freq_reg: u16,
    decay: f32,
}

/// Click decay bounds: never shorter than the ear-tuned [`DRUM_CLICK_ADSR`]
/// default, capped so a long noise wash cannot turn the accent into a
/// sustained hiss over the pulse body.
const CLICK_DECAY_RANGE: (f32, f32) = (0.04, 0.12);

impl NoiseAccent {
    /// Build from captured per-note/per-patch noise stats; `None` when the
    /// trace showed no noise frames (keeps the pitch-tracking default).
    fn from_capture(freq: Option<Hertz>, run_frames: f32, timing: PlaybackTiming) -> Option<Self> {
        let freq = freq?;
        let decay = (run_frames * timing.seconds_per_call() as f32)
            .clamp(CLICK_DECAY_RANGE.0, CLICK_DECAY_RANGE.1);
        Some(Self {
            freq_reg: sid_freq_reg_from_hz(freq.0 as f32, timing.clock),
            decay,
        })
    }
}

/// Envelope of the drum-drop pulse body: percussive, not sustained. The chip's
/// drum voice is a sharp hit that decays fast — measured on the bar-21 Auf
/// Wiedersehen Monty reSID voice-2 solo, the RMS peaks then falls to ~10 % in
/// ~0.15 s. The old full-sustain body (`08F8`, sustain 15) instead held the low
/// A#1 pulse as a boomy drone that dominated the section (RMS share 0.26,
/// low-band 0.13 vs the chip's 0.07) and "took over" the mix. A short decay to
/// zero sustain makes the body punch and clears the low end (RMS share → 0.13,
/// low-band → 0.04). The noise click ([`DRUM_CLICK_ADSR`]) still rides on top.
const DRUM_BODY_ADSR: AdsrSeconds = AdsrSeconds {
    attack: 0.002,
    decay: 0.16,
    sustain: 0.0,
    release: 0.05,
};

/// A SID percussion instrument: a pulse body plus a noise-click attack summed in
/// a mixer — a two-source graph (the rest of the exporter's instruments are
/// single-source). The 6581 drum voice is a short noise burst over a pulse body:
/// the click gives the "snap" that reads as percussion, the body carries the
/// tuned tone. Shared by every SID drum the chip builds this way — the drum-drop
/// zap ([`drum_drop_instrument`]) and the tuned Snare/Tom hits
/// ([`is_sid_percussion`]) — differing only in the body envelope, name and role.
///
/// Graph:
/// ```text
///   sid-1 (pulse body, 50% PW)  → amp-1 ←cv─ env-1 (body_adsr)       ┐
///                                                                     ├→ mix-1 → out-1
///   sid-2 (SID noise click)     → amp-2 ←cv─ env-2 (DRUM_CLICK_ADSR) ┘
/// ```
/// `body_adsr` is percussive (sustain 0): a drum is a short hit, not a sustained
/// tone. The pulse body tracks the note pitch, so a tuned tom fill keeps its
/// pitches; the noise click ([`DRUM_CLICK_ADSR`]) rides on top.
fn sid_percussion_instrument(
    id: u32,
    name: String,
    role: InstrumentRole,
    body_adsr: AdsrSeconds,
    noise: Option<NoiseAccent>,
    model: SidModel,
    clock: SystemClock,
) -> Instrument {
    // Noise-click attack: the chip's own 23-bit LFSR noise (measured 0.76 dB
    // from the reSID reference) gated by a fast percussive envelope, mixed
    // under the body. With a trace-measured [`NoiseAccent`] the LFSR clock is
    // pinned to the driver's noise-frame register (the chip retunes the
    // frequency way up on noise frames — tracking the low body pitch instead
    // renders the click dark) and the click envelope follows the measured
    // noise-run length.
    let click_source = match noise {
        Some(a) => {
            let mut params = SidOscillatorParams::tonal(0x8, PulseWidth(2048), model, clock);
            params.track_pitch = 0.0;
            params.freq_reg = f32::from(a.freq_reg);
            Module {
                id: "sid-2",
                kind: "sid_oscillator",
                position: Position { x: 32.0, y: 200.0 },
                scripts: None,
                parameters: Parameters::SidOscillator(Box::new(params)),
            }
        }
        None => SidSource::simple(0x8, 2048, model, clock)
            .module_at("sid-2", Position { x: 32.0, y: 200.0 }),
    };
    let click_adsr = match noise {
        Some(a) => AdsrSeconds {
            decay: a.decay,
            ..DRUM_CLICK_ADSR
        },
        None => DRUM_CLICK_ADSR,
    };
    let modules = vec![
        // Pulse body (the sustained waveform the chip holds; the onset pitch-drop
        // the note carries sweeps it down into the drum body).
        SidSource::simple(0x4, 2048, model, clock).module(),
        envelope_module("env-1", Position { x: 32.0, y: 384.0 }, body_adsr),
        amplifier_module("amp-1", Position { x: 240.0, y: 32.0 }),
        click_source,
        envelope_module("env-2", Position { x: 32.0, y: 520.0 }, click_adsr),
        amplifier_module("amp-2", Position { x: 240.0, y: 200.0 }),
        Module {
            id: "mix-1",
            kind: "mixer",
            position: Position { x: 448.0, y: 32.0 },
            scripts: None,
            parameters: Parameters::Mixer(MixerParams {
                input_1: 1.0,
                input_2: DRUM_CLICK_LEVEL,
                input_3: 1.0,
                input_4: 1.0,
                input_5: 1.0,
                input_6: 1.0,
                input_7: 1.0,
                input_8: 1.0,
                master: 1.0,
            }),
        },
        Module {
            id: "out-1",
            kind: "stereo_output",
            position: Position { x: 800.0, y: 32.0 },
            scripts: None,
            parameters: Parameters::StereoOutput(StereoOutputParams {
                dither: 0.0,
                limit: 1.0,
                master: 1.0,
                mute: 0.0,
                pan: 0.0,
            }),
        },
    ];
    let connections = vec![
        Connection {
            from: ["sid-1", "out"],
            to: ["amp-1", "in"],
        },
        Connection {
            from: ["env-1", "out"],
            to: ["amp-1", "cv"],
        },
        Connection {
            from: ["sid-2", "out"],
            to: ["amp-2", "in"],
        },
        Connection {
            from: ["env-2", "out"],
            to: ["amp-2", "cv"],
        },
        Connection {
            from: ["amp-1", "out"],
            to: ["mix-1", "in1"],
        },
        Connection {
            from: ["amp-2", "out"],
            to: ["mix-1", "in2"],
        },
        Connection {
            from: ["mix-1", "out"],
            to: ["out-1", "in"],
        },
    ];
    Instrument::assemble(id, name, role, modules, connections)
}

/// The instrument for a voice's drum-drop notes ([`drum_drop_dest`]): a SID
/// percussion voice with the drum-drop body envelope. The onset pitch-drop the
/// notes carry sweeps the pulse body down into the drum body.
fn drum_drop_instrument(
    id: u32,
    voice: VoiceId,
    noise: Option<NoiseAccent>,
    model: SidModel,
    clock: SystemClock,
) -> Instrument {
    sid_percussion_instrument(
        id,
        format!("V{} drum (drop)", voice.0),
        InstrumentRole::Drum(DrumSubclass::Tom),
        DRUM_BODY_ADSR,
        noise,
        model,
        clock,
    )
}

/// A patch the chip builds as tuned SID percussion: a drum-subclass cluster with
/// a **tonal** (pulse/triangle) body — a snare or tom that is a pitched pulse hit
/// carrying a one-frame noise-click attack. These route to
/// [`sid_percussion_instrument`] so the click is reproduced; the generic
/// single-oscillator path renders only the bare pitched pulse, so a tuned tom
/// fill reads as a little descending melody instead of a drum. A pure-noise drum
/// (`0x80`) already sounds right on the generic noise path and needs no separate
/// click, so it is excluded.
fn is_sid_percussion(patch: &Patch) -> bool {
    patch.drum_subclass().is_some() && patch.waveform & 0x80 == 0
}

fn use_generic_percussion(
    patch: &Patch,
    profile: &PatchVoiceProfile,
    note_program: Option<&[u8]>,
) -> bool {
    is_sid_percussion(patch)
        && note_program.is_none()
        && observed_waveform_program(profile).is_none()
}

/// The patch source: one native `sid_oscillator` at the head of the audio
/// path. The waveform mask carries tonal, noise and **combined** selections in
/// one module — combined waveforms meet on the module's internal bus exactly
/// like the chip (measured at the reSID floor for pulse+tri and every 8580
/// combo; PoC matrix, `docs/export.md`), replacing the
/// old dominant-waveform + summed-`osc-2` approximation (47 dB off). An
/// optional per-frame waveform-mask sequence reproduces the SID
/// waveform-alternation idiom natively (`seq_*` params), replacing the old
/// two-LFO amplifier gate.
struct SidSource {
    /// Waveform mask — control-byte bits 4..=7 shifted down, i.e. the module's
    /// seq-step encoding: bit 0 = triangle, 1 = sawtooth, 2 = pulse, 3 = noise.
    mask: u8,
    /// Raw 12-bit pulse-width register (2048 = square).
    pw_reg: u16,
    /// Per-frame waveform-mask loop (2..=16 steps) for the `seq_*` params;
    /// `None` renders the static `mask`.
    seq: Option<Vec<u8>>,
    seq_frequencies: [u16; 16],
    /// `true` = one-shot program: play `seq` once and hold the last step
    /// (`seq_loop` off) — the Hubbard drum/stab wavetable. `false` = the
    /// alternation idiom, looping for the whole note.
    seq_hold: bool,
    /// SID RING bit — needs a neighbour source on the `ring` input.
    ring_mod: bool,
    /// SID SYNC bit — needs a neighbour source on the `sync` input.
    hard_sync: bool,
    model: SidModel,
    clock: SystemClock,
}

impl SidSource {
    /// A static source with no sequence/ring/sync — the drum-drop and generic
    /// fallback shape.
    fn simple(mask: u8, pw_reg: u16, model: SidModel, clock: SystemClock) -> Self {
        Self {
            mask,
            pw_reg,
            seq: None,
            seq_frequencies: [0; 16],
            seq_hold: false,
            ring_mod: false,
            hard_sync: false,
            model,
            clock,
        }
    }

    /// Dominant-waveform label for instrument names (pulse > saw > tri, noise
    /// when it is the only selection) — naming only, the module renders the
    /// full mask.
    fn waveform_label(&self) -> &'static str {
        if self.mask & 0x4 != 0 {
            "pulse"
        } else if self.mask & 0x2 != 0 {
            "sawtooth"
        } else if self.mask & 0x1 != 0 {
            "triangle"
        } else if self.mask & 0x8 != 0 {
            "noise"
        } else {
            "pulse"
        }
    }

    fn module(&self) -> Module {
        self.module_at("sid-1", Position { x: 32.0, y: 32.0 })
    }

    /// Like [`module`](Self::module) but with an explicit id and position (the
    /// drum-drop click source lives at `sid-2`).
    fn module_at(&self, id: &'static str, position: Position) -> Module {
        let mut params =
            SidOscillatorParams::tonal(self.mask, PulseWidth(self.pw_reg), self.model, self.clock);
        params.ring_mod = f32::from(self.ring_mod);
        params.hard_sync = f32::from(self.hard_sync);
        if let Some(seq) = &self.seq {
            params.set_seq(seq);
            params.set_seq_frequencies(&self.seq_frequencies);
            if self.seq_hold {
                // One-shot program: play once, hold the last step.
                params.seq_loop = 0.0;
            }
        }
        Module {
            id,
            kind: "sid_oscillator",
            position,
            scripts: None,
            parameters: Parameters::SidOscillator(Box::new(params)),
        }
    }
}

/// The ring/sync **source** oscillator: `track_pitch` off, held at the
/// captured neighbour-voice register; only its `msb` output is consumed
/// (its `out` is never connected), mirroring how the chip taps voice N−1's
/// accumulator MSB regardless of that voice's own waveform.
fn neighbour_source_module(
    id: &'static str,
    freq_reg: u16,
    model: SidModel,
    clock: SystemClock,
) -> Module {
    let mut params = SidOscillatorParams::tonal(0x2, PulseWidth(2048), model, clock);
    params.track_pitch = 0.0;
    params.freq_reg = f32::from(freq_reg);
    Module {
        id,
        kind: "sid_oscillator",
        position: Position { x: 32.0, y: 200.0 },
        scripts: None,
        parameters: Parameters::SidOscillator(Box::new(params)),
    }
}

/// The 16-bit SID frequency register for a pitch in Hz: `reg = hz · 2²⁴ / Φ2`.
fn sid_freq_reg_from_hz(hz: f32, clock: SystemClock) -> u16 {
    let reg = (f64::from(hz) * f64::from(1u32 << 24) / f64::from(clock.phi2_hz())).round();
    if reg <= 0.0 {
        0
    } else if reg >= 65535.0 {
        65535
    } else {
        reg as u16
    }
}

/// The musical identity an instrument plays, derived from the M7 [`RoleTags`] of
/// the representative patch (§A3). Drives the Pertylizer instrument `category`,
/// the human-readable name, and the voice-allocation mode — replacing the old
/// hardcoded `category: 4` / `Polyphonic` / `max_voices: 4`.
///
/// `category` follows the engine's `InstrumentCategory::as_u8()` encoding
/// (`Uncategorized=0, Drums=1, Bass=2, Pad=3, Lead=4, Arp=5, Keys=6, FX=7`).
/// A SID voice is monophonic and the per-voice merge guarantees one
/// instrument's notes never overlap, so `Mono` / `max_voices: 1` is always
/// safe; we follow the doc's role-based intent and keep poly only where it
/// reads as the musical default (pads, drums, untagged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstrumentRole {
    Bass,
    Lead,
    Pad,
    Stab,
    Bell,
    Drum(DrumSubclass),
    /// No role tag set — fall back to the waveform-based identity.
    Untagged,
}

impl InstrumentRole {
    /// Pick the most specific role for a patch. A note can carry several tags
    /// (it is a multi-tag set); we resolve to one identity by priority:
    /// percussive/drum first (it is the most audibly distinct), then the
    /// pitched roles. Returns [`Untagged`] when nothing applies.
    fn from_role_tags(tags: &RoleTags) -> Self {
        if let Some(sub) = tags.drum_subclass {
            return Self::Drum(sub);
        }
        if tags.bass {
            Self::Bass
        } else if tags.lead {
            Self::Lead
        } else if tags.pad {
            Self::Pad
        } else if tags.stab {
            Self::Stab
        } else if tags.bell {
            Self::Bell
        } else if tags.percussive {
            // Percussive but no resolved subclass — treat as a generic drum.
            Self::Drum(DrumSubclass::PercMetallic)
        } else {
            Self::Untagged
        }
    }

    /// The `InstrumentCategory::as_u8()` value for this role.
    fn category(self) -> u32 {
        match self {
            Self::Drum(_) => 1,
            Self::Bass => 2,
            Self::Pad => 3,
            Self::Lead | Self::Stab => 4,
            // Bell timbres are mallet/keys-like; map to Keys. Untagged stays
            // Uncategorized.
            Self::Bell => 6,
            Self::Untagged => 0,
        }
    }

    /// `Mono` (with `max_voices: 1`) for the inherently-monophonic lead/bass/
    /// stab/bell single-note roles; `Polyphonic` for pads, drums and untagged.
    fn allocation_mode(self) -> &'static str {
        match self {
            Self::Bass | Self::Lead | Self::Stab | Self::Bell => "Mono",
            Self::Pad | Self::Drum(_) | Self::Untagged => "Polyphonic",
        }
    }

    fn max_voices(self) -> u32 {
        match self.allocation_mode() {
            "Mono" => 1,
            _ => 4,
        }
    }

    /// The role's musical name, if it has one. `None` for [`Untagged`], whose
    /// name falls back to the waveform-based [`plan_name`].
    fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::Bass => "Bass",
            Self::Lead => "Lead",
            Self::Pad => "Pad",
            Self::Stab => "Stab",
            Self::Bell => "Bell",
            Self::Drum(DrumSubclass::Kick) => "Kick",
            Self::Drum(DrumSubclass::Snare) => "Snare",
            Self::Drum(DrumSubclass::HihatClosed) => "Hihat (closed)",
            Self::Drum(DrumSubclass::HihatOpen) => "Hihat (open)",
            Self::Drum(DrumSubclass::Tom) => "Tom",
            Self::Drum(DrumSubclass::PercMetallic) => "Perc",
            Self::Untagged => return None,
        })
    }
}

impl Instrument {
    #[allow(clippy::too_many_arguments)]
    fn build(
        id: u32,
        name: String,
        role: InstrumentRole,
        source: SidSource,
        adsr: AdsrSeconds,
        filter: Option<FilterSpec>,
        neighbour_freq_reg: Option<u16>,
        detune_cents: f32,
        pwm_script: Option<String>,
    ) -> Self {
        let (ring_mod, hard_sync, model, clock) = (
            source.ring_mod,
            source.hard_sync,
            source.model,
            source.clock,
        );
        let mut modules = vec![
            source.module(),
            Module {
                id: "env-1",
                kind: "envelope",
                position: Position { x: 32.0, y: 384.0 },
                scripts: None,
                parameters: Parameters::Envelope(EnvelopeParams {
                    atk_curve: 0.0,
                    attack: adsr.attack,
                    dec_curve: -0.5,
                    decay: adsr.decay,
                    rel_curve: -0.5,
                    release: adsr.release,
                    sustain: adsr.sustain,
                    vel_sens: 1.0,
                }),
            },
            Module {
                id: "amp-1",
                kind: "amplifier",
                position: Position { x: 448.0, y: 32.0 },
                scripts: None,
                parameters: Parameters::Amplifier(AmplifierParams {
                    cv_bipolar: 0.0,
                    level: 1.0,
                    pan: 0.0,
                }),
            },
            Module {
                id: "out-1",
                kind: "stereo_output",
                position: Position { x: 800.0, y: 32.0 },
                scripts: None,
                parameters: Parameters::StereoOutput(StereoOutputParams {
                    dither: 0.0,
                    limit: 1.0,
                    master: 1.0,
                    mute: 0.0,
                    pan: 0.0,
                }),
            },
        ];
        // Main audio chain, in order: sid-1 → [flt-1 → leak mixer] → amp-1. Combined
        // waveforms, ring-mod and hard sync all live *inside* the source
        // module now. The leak mixer is present only for low-cutoff 6581
        // low-pass voices.
        //
        // Filter: SID has ONE global filter shared across the voices routed to
        // it; giving each instrument its own `flt-1` copy is a v0
        // approximation.
        let mut connections = match filter {
            Some(filter) if filter.leak > 0.0 => vec![
                Connection {
                    from: ["sid-1", "out"],
                    to: ["flt-1", "in"],
                },
                Connection {
                    from: ["flt-1", "out"],
                    to: ["mix-1", "in1"],
                },
                Connection {
                    from: ["sid-1", "out"],
                    to: ["mix-1", "in2"],
                },
                Connection {
                    from: ["mix-1", "out"],
                    to: ["amp-1", "in"],
                },
            ],
            Some(_) => vec![
                Connection {
                    from: ["sid-1", "out"],
                    to: ["flt-1", "in"],
                },
                Connection {
                    from: ["flt-1", "out"],
                    to: ["amp-1", "in"],
                },
            ],
            None => vec![Connection {
                from: ["sid-1", "out"],
                to: ["amp-1", "in"],
            }],
        };
        connections.push(Connection {
            from: ["env-1", "out"],
            to: ["amp-1", "cv"],
        });
        connections.push(Connection {
            from: ["amp-1", "out"],
            to: ["out-1", "in"],
        });

        // Ring-mod / hard sync: the chip taps voice N−1's oscillator MSB. A
        // second `sid_oscillator` held at the captured neighbour register
        // (`track_pitch` off) feeds its `msb` into the ring/sync inputs — the
        // designed neighbour-source topology, verified to land the Nemesis
        // 988±165 Hz sidebands exactly (PoC matrix).
        if let Some(freq_reg) = neighbour_freq_reg.filter(|_| ring_mod || hard_sync) {
            modules.push(neighbour_source_module("sid-2", freq_reg, model, clock));
            if ring_mod {
                connections.push(Connection {
                    from: ["sid-2", "msb"],
                    to: ["sid-1", "ring"],
                });
            }
            if hard_sync {
                connections.push(Connection {
                    from: ["sid-2", "msb"],
                    to: ["sid-1", "sync"],
                });
            }
        }
        if let Some(filter) = filter {
            modules.push(Module {
                id: "flt-1",
                kind: "filter",
                position: Position { x: 240.0, y: 32.0 },
                scripts: None,
                parameters: Parameters::Filter(FilterParams {
                    cutoff: filter.cutoff_hz,
                    resonance: filter.resonance,
                    kind: filter.kind,
                    model: SID_FILTER_MODEL,
                    drive: 1.0,
                    cv_amt: 0.0,
                    env_amt: 0.0,
                    key_track: 0.0,
                    morph: 0.0,
                }),
            });
            if filter.leak > 0.0 {
                modules.push(Module {
                    id: "mix-1",
                    kind: "mixer",
                    position: Position { x: 352.0, y: 32.0 },
                    scripts: None,
                    parameters: Parameters::Mixer(MixerParams {
                        input_1: 1.0,
                        input_2: filter.leak,
                        input_3: 1.0,
                        input_4: 1.0,
                        input_5: 1.0,
                        input_6: 1.0,
                        input_7: 1.0,
                        input_8: 1.0,
                        master: 1.0 / (1.0 + filter.leak),
                    }),
                });
            }
        }
        // Authored continuous PWM as a *program*: a `script` module regenerates
        // the driver's staircase-triangle sweep ([`authored_pwm_script`]) and
        // feeds the `sid_oscillator.pwm` CV input (additive, raw register
        // units) — replacing the baked per-frame `pw_reg` lane
        // ([`build_plan_automation`] skips it for these plans).
        if let Some(script) = pwm_script {
            modules.push(Module {
                id: "scr-1",
                kind: "script",
                position: Position { x: 32.0, y: 560.0 },
                scripts: Some(BTreeMap::from([("1", script)])),
                parameters: Parameters::Script(ScriptParams {}),
            });
            connections.push(Connection {
                from: ["scr-1", "out1"],
                to: ["sid-1", "pwm"],
            });
        }
        let mut instrument = Self::assemble(id, name, role, modules, connections);
        // Static fine detune (median per-note cents, ±[`DETUNE_LIMIT_CENTS`]):
        // the sid module derives its register from the played note, so the
        // cents offset rides the instrument transpose (fractional semitones)
        // instead of an oscillator detune param.
        instrument.transpose = detune_cents / 100.0;
        instrument
    }

    /// Wrap a finished module graph in the Pertylizer instrument/channel-strip
    /// boilerplate. The mix `volume` is a placeholder here — [`build_instruments`]
    /// overwrites it with the shared [`mix_volume`]. Shared by the single-source
    /// [`build`] and the two-source [`drum_drop_instrument`].
    fn assemble(
        id: u32,
        name: String,
        role: InstrumentRole,
        modules: Vec<Module>,
        connections: Vec<Connection>,
    ) -> Self {
        Self {
            id,
            name: name.clone(),
            channel: 1,
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            key_range: [0, 127],
            transpose: 0.0,
            oversampling: 1,
            category: role.category(),
            description: "sid-analyzer v0 export.".to_string(),
            allocation_mode: role.allocation_mode(),
            stealing_strategy: "Oldest",
            max_voices: role.max_voices(),
            velocity_amp_sensitivity: 1.0,
            velocity_filter_sensitivity: 0.0,
            patch: PatchBlock {
                name,
                version: "1.0",
                description: "SID-flavoured voice -> amplifier -> stereo output.",
                modules,
                connections,
                settings: PatchSettings::default(),
            },
        }
    }
}

/// An `env-N` envelope module carrying `adsr` (the decay/release curves match
/// [`Instrument::build`]'s defaults).
fn envelope_module(id: &'static str, position: Position, adsr: AdsrSeconds) -> Module {
    Module {
        id,
        kind: "envelope",
        position,
        scripts: None,
        parameters: Parameters::Envelope(EnvelopeParams {
            atk_curve: 0.0,
            attack: adsr.attack,
            dec_curve: -0.5,
            decay: adsr.decay,
            rel_curve: -0.5,
            release: adsr.release,
            sustain: adsr.sustain,
            vel_sens: 1.0,
        }),
    }
}

/// An `amp-N` amplifier module at unity level, centre pan (gated by an envelope
/// wired to its `cv` input).
fn amplifier_module(id: &'static str, position: Position) -> Module {
    Module {
        id,
        kind: "amplifier",
        position,
        scripts: None,
        parameters: Parameters::Amplifier(AmplifierParams {
            cv_bipolar: 0.0,
            level: 1.0,
            pan: 0.0,
        }),
    }
}

/// The §A9 master-bus coloring chain applied to the full mix: a light **tube**
/// `distortion` (6581 DAC warmth), a 3-band `eq` that nudges the spectral
/// balance toward the reSID reference, and a limiter that contains reconstructed
/// oscillator peaks without lowering the mix fader. Emitted into the project's
/// `global.master_effects`, in order.
fn master_chain() -> Vec<Module> {
    vec![
        Module {
            id: "dst-1",
            kind: "distortion",
            position: Position { x: 0.0, y: 0.0 },
            scripts: None,
            parameters: Parameters::Distortion(DistortionParams {
                kind: "tube",
                drive: COLORING_TUBE_DRIVE,
                tone: COLORING_TUBE_TONE,
                mix: COLORING_TUBE_MIX,
                bit_depth: 8.0,
            }),
        },
        Module {
            id: "equ-1",
            kind: "eq",
            position: Position { x: 200.0, y: 0.0 },
            scripts: None,
            parameters: Parameters::Eq(EqParams {
                low_freq: COLORING_EQ_LOW_FREQ,
                low_gain: COLORING_EQ_LOW_GAIN,
                mid_freq: COLORING_EQ_MID_FREQ,
                mid_gain: COLORING_EQ_MID_GAIN,
                mid_q: COLORING_EQ_MID_Q,
                high_freq: COLORING_EQ_HIGH_FREQ,
                high_gain: COLORING_EQ_HIGH_GAIN,
                mix: 1.0,
            }),
        },
        Module {
            id: "lmt-1",
            kind: "limiter",
            position: Position { x: 400.0, y: 0.0 },
            scripts: None,
            parameters: Parameters::Limiter(LimiterParams {
                ceiling: MASTER_LIMITER_CEILING_DB,
                look_ahead: MASTER_LIMITER_LOOK_AHEAD_MS,
                release: MASTER_LIMITER_RELEASE_MS,
                mix: 1.0,
            }),
        },
    ]
}

/// Assemble the song: one pattern + track + arrangement entry per
/// [`TrackPlan`] (a `(voice, shape)` group with at least one note). Each track
/// binds its plan's dedicated `instrument_id`, so two plans sharing a patch
/// drive separate instruments and their automation lanes never collide.
#[allow(clippy::too_many_arguments)]
fn build_song(
    export: &SynthSource<'_>,
    states: &[ProgramFrame],
    time_base: &TimeBase,
    frame_count: u32,
    plans: &[TrackPlan],
    ownership: &PhysicalVoiceOwnership,
    fidelity: &mut forward::ResidualCensus,
    options: SynthOptions,
) -> Song {
    let tpr = time_base.ticks_per_frame;
    let pattern_length = frame_count.saturating_mul(tpr);

    let mut patterns: Vec<Pattern> = Vec::new();
    let mut tracks: Vec<Track> = Vec::new();
    let mut arrangement: Vec<PatternPlacement> = Vec::new();

    for (i, plan) in plans.iter().enumerate() {
        let id = i as u32;
        let instrument = plan.instrument_id;
        let name = plan_name(plan);

        // A clean arp plan converts to one held base note per event + a pattern
        // `Arpeggiator` processor (the SID-native model); otherwise the arp bake.
        let arp_proc = arp_processor_for(
            plan,
            states,
            export.timing,
            frame_count,
            options.arpeggiator_processor,
        );

        let detune = plan_detune_cents(plan);

        // Emit notes in timeline order, expanding each event through its own
        // segment's profile: an arpeggio patch holds one note while the SID
        // cycles chord tones ~1 per frame, so those expand into per-frame
        // sub-notes (or one held note when `arp_proc` carries the arp); every
        // other note takes the per-note expression path.
        let mut notes: Vec<Note> = Vec::new();
        for &(si, ev) in &plan.timeline {
            let arpeggio = plan.segments[si]
                .profile
                .and_then(|p| p.arpeggio_loop.as_deref())
                .filter(|offsets| !offsets.is_empty());
            match arpeggio {
                Some(_) if arp_proc.is_some() => {
                    let id = notes.len() as u32;
                    notes.push(build_note(id, ev, tpr, frame_count, states));
                }
                Some(offsets) => {
                    push_arp_event(
                        &mut notes,
                        ev,
                        offsets,
                        tpr,
                        frame_count,
                        states,
                        export.timing,
                        plan.voice_index,
                        detune,
                        options.forward_gate,
                        fidelity,
                    );
                }
                None => {
                    let percussion = plan.segments[si].patch.is_some_and(is_sid_percussion);
                    push_melodic_event(
                        &mut notes,
                        ev,
                        states,
                        export.effects,
                        export.timing,
                        plan.voice,
                        plan.voice_index,
                        frame_count,
                        tpr,
                        plan.segments[si]
                            .patch
                            .and_then(|p| p.authored_effects.as_ref()),
                        percussion,
                        detune,
                        options.forward_gate,
                        fidelity,
                        ownership.end(ev),
                    );
                }
            }
        }
        record_note_fidelity(
            fidelity,
            plan,
            &notes,
            arp_proc.as_ref(),
            export.effects,
            tpr,
            export.timing,
            states,
        );
        let next_note_id = notes.len() as u32;

        let automation = build_plan_automation(
            plan,
            ownership,
            states,
            export.effects,
            export.timing,
            export.header.flags.sid_model,
            frame_count,
            tpr,
        );

        patterns.push(Pattern {
            id,
            name: name.clone(),
            length: pattern_length,
            notes,
            automation,
            next_note_id,
            processors: arp_proc.into_iter().collect(),
            note_graph: None,
        });
        tracks.push(Track {
            id,
            name,
            instrument,
            volume: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            color: track_color(plan.voice_index),
            mode: "Polyphonic",
            sends: Vec::new(),
        });
        arrangement.push(PatternPlacement {
            pattern_id: id,
            track_id: id,
            start: 0,
            transpose: 0.0,
            gain: 1.0,
            length_override: None,
            loop_mode: "clip",
        });
    }

    // §A5: the `$D418` master-volume contour is a single chip-global register,
    // so it rides one `AutomationTarget::Global` lane. Every pattern starts at
    // tick 0 and spans the whole subtune, so attaching it to the first pattern
    // covers the tune without duplicating the lane across patterns.
    if let Some(first) = patterns.first_mut()
        && let Some(lane) = build_master_volume_lane(states, frame_count, tpr)
    {
        first.automation.push(lane);
    }

    assemble_song(export, time_base, patterns, tracks, arrangement)
}

/// Wrap the built patterns/tracks/arrangement in a [`Song`] with the shared
/// metadata footer (tempo, time signature, §A6 musical row grid). Used by both
/// the flat [`build_song`] and the structure-preserving [`build_song_structured`].
fn assemble_song(
    export: &SynthSource<'_>,
    time_base: &TimeBase,
    mut patterns: Vec<Pattern>,
    tracks: Vec<Track>,
    mut arrangement: Vec<PatternPlacement>,
) -> Song {
    canonicalize_truncated_patterns(&mut patterns, &mut arrangement);
    compact_song_automation(&mut patterns);
    let note_graphs = pool_note_graphs(&mut patterns);
    let next_pattern_id = patterns.len() as u32;
    let next_track_id = tracks.len() as u32;
    let next_note_graph_id = note_graphs.len() as u32;
    Song {
        name: clean(&export.header.name, "SID export"),
        author: clean(&export.header.author, "sid-analyzer"),
        patterns,
        next_pattern_id,
        tracks,
        next_track_id,
        return_busses: Vec::new(),
        next_return_bus_id: None,
        arrangement,
        note_graphs,
        next_note_graph_id,
        tempo_changes: Vec::new(),
        time_signature_changes: Vec::new(),
        default_tempo: time_base.bpm,
        default_time_signature: TimeSignature {
            numerator: 4,
            denominator: 4,
        },
        row_resolution: RowResolution {
            // The §A6 musical grid: a 16th-note row grid sized to the subtune,
            // not one row per SID frame. `rows` is a u16 in the schema (max
            // 65535); note timing is absolute ticks (`pattern.length`,
            // `Note.start`, which stay u32), so capping the grid row count only
            // truncates the editor display, not the played content.
            rows: time_base.grid_rows,
            ticks_per_row: time_base.grid_ticks_per_row,
        },
    }
}

fn compact_song_automation(patterns: &mut [Pattern]) {
    for lane in patterns
        .iter_mut()
        .flat_map(|pattern| &mut pattern.automation)
    {
        let mut compact: Vec<AutomationPoint> = Vec::with_capacity(lane.points.len());
        for point in lane.points.drain(..) {
            if compact
                .last()
                .is_some_and(|previous| previous.tick == point.tick)
            {
                compact.pop();
            } else if compact.last().is_some_and(|previous| {
                previous.curve == CurveType::Step
                    && point.curve == CurveType::Step
                    && (previous.value - point.value).abs() <= AUTOMATION_EPSILON
            }) {
                continue;
            }
            compact.push(point);
        }
        lane.points = compact;
    }
}

fn pool_note_graphs(patterns: &mut [Pattern]) -> Vec<NoteGraph> {
    let mut ids: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut graphs = Vec::new();
    for pattern in patterns {
        if pattern.processors.len() != 1 {
            continue;
        }
        let key = serde_json::to_vec(&pattern.processors[0]).unwrap_or_default();
        let graph_id = match ids.get(&key) {
            Some(&id) => id,
            None => {
                let id = graphs.len() as u32;
                let processor = pattern.processors[0].clone();
                graphs.push(NoteGraph {
                    id,
                    name: format!("SID arpeggiator {}", id + 1),
                    nodes: BTreeMap::from([(0, NoteModuleConfig::Processor(processor))]),
                    connections: Vec::new(),
                });
                ids.insert(key, id);
                id
            }
        };
        pattern.processors.clear();
        pattern.note_graph = Some(graph_id);
    }
    graphs
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct MusicalNoteIdentity {
    start: u32,
    duration: u32,
    pitch: u32,
    legato: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CanonicalVibrato {
    depth: u32,
    rate: u32,
    delay: u32,
    shape: VibratoShape,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CanonicalGlide {
    from: u32,
    time: u32,
    interp: GlideInterp,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct NoteLocalExpression {
    velocity: u32,
    vibrato: Option<CanonicalVibrato>,
    glide: Option<CanonicalGlide>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CanonicalNote {
    musical: MusicalNoteIdentity,
    expression: NoteLocalExpression,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CanonicalArpeggiator {
    custom: Vec<i8>,
    rate_millihz: u32,
    octaves: u8,
    legato: bool,
    gate: u32,
}

#[derive(PartialEq, Eq, Hash)]
struct CanonicalPatternBody {
    notes: Vec<CanonicalNote>,
    processors: Vec<CanonicalArpeggiator>,
}

impl CanonicalNote {
    fn from_note(note: &Note) -> Self {
        let vibrato = note
            .expression
            .as_ref()
            .and_then(|expression| expression.vibrato.as_ref())
            .map(|vibrato| CanonicalVibrato {
                depth: vibrato.depth.to_bits(),
                rate: vibrato.rate.to_bits(),
                delay: vibrato.delay.to_bits(),
                shape: vibrato.shape,
            });
        let glide = note.glide.map(|glide| {
            let GlideFrom::Semitones(from) = glide.from;
            CanonicalGlide {
                from: from.to_bits(),
                time: glide.time.to_bits(),
                interp: glide.interp,
            }
        });
        Self {
            musical: MusicalNoteIdentity {
                start: note.start,
                duration: note.duration,
                pitch: note.pitch,
                legato: note.legato,
            },
            expression: NoteLocalExpression {
                velocity: note.velocity.to_bits(),
                vibrato,
                glide,
            },
        }
    }
}

impl CanonicalPatternBody {
    fn new(notes: &[Note], processors: &[NoteProcessor]) -> Self {
        Self {
            notes: notes.iter().map(CanonicalNote::from_note).collect(),
            processors: processors
                .iter()
                .map(|processor| {
                    let NoteProcessor::Arpeggiator(arpeggiator) = processor;
                    let ArpRate::MilliHz(rate_millihz) = &arpeggiator.rate;
                    CanonicalArpeggiator {
                        custom: arpeggiator.custom.clone(),
                        rate_millihz: *rate_millihz,
                        octaves: arpeggiator.octaves,
                        legato: arpeggiator.legato,
                        gate: arpeggiator.gate.to_bits(),
                    }
                })
                .collect(),
        }
    }
}

fn lift_pattern_gain(notes: &mut [Note]) -> f32 {
    let peak = notes
        .iter()
        .map(|note| note.velocity)
        .fold(0.0f32, f32::max);
    if peak <= 0.0 || peak >= 1.0 {
        return 1.0;
    }
    for note in notes {
        note.velocity = (note.velocity / peak).clamp(0.0, 1.0);
    }
    peak
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PatternPrefixKey {
    processor: u64,
    notes: u64,
    note_count: usize,
}

#[derive(Clone, Copy)]
struct PatternPrefixCandidate {
    pattern_id: u32,
    next_note_start: u32,
    note_count: usize,
}

fn hash_value(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn hash_notes(notes: &[CanonicalNote]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for note in notes {
        note.hash(&mut hasher);
    }
    hasher.finish()
}

fn canonicalize_truncated_patterns(
    patterns: &mut Vec<Pattern>,
    arrangement: &mut [PatternPlacement],
) {
    let mut prefixes: HashMap<PatternPrefixKey, Vec<PatternPrefixCandidate>> = HashMap::new();
    let mut canonical: HashMap<u32, CanonicalPatternBody> = HashMap::new();

    for pattern in patterns.iter() {
        if pattern.notes.len() < 2
            || !pattern.automation.is_empty()
            || pattern.notes.iter().any(|note| note.track.is_some())
        {
            continue;
        }
        let body = CanonicalPatternBody::new(&pattern.notes, &pattern.processors);
        let processor = hash_value(&body.processors);
        let mut notes = DefaultHasher::new();
        for (index, note) in body.notes.iter().enumerate().take(body.notes.len() - 1) {
            note.hash(&mut notes);
            prefixes
                .entry(PatternPrefixKey {
                    processor,
                    notes: notes.finish(),
                    note_count: index + 1,
                })
                .or_default()
                .push(PatternPrefixCandidate {
                    pattern_id: pattern.id,
                    next_note_start: pattern.notes[index + 1].start,
                    note_count: pattern.notes.len(),
                });
        }
        canonical.insert(pattern.id, body);
    }

    let mut remap: HashMap<u32, u32> = HashMap::new();
    for pattern in patterns.iter() {
        if pattern.notes.is_empty()
            || !pattern.automation.is_empty()
            || pattern.notes.iter().any(|note| note.track.is_some())
        {
            continue;
        }
        let body = CanonicalPatternBody::new(&pattern.notes, &pattern.processors);
        let key = PatternPrefixKey {
            processor: hash_value(&body.processors),
            notes: hash_notes(&body.notes),
            note_count: body.notes.len(),
        };
        let Some(candidates) = prefixes.get(&key) else {
            continue;
        };
        let candidate = candidates
            .iter()
            .filter(|candidate| {
                let Some(candidate_body) = canonical.get(&candidate.pattern_id) else {
                    return false;
                };
                candidate_body.processors == body.processors
                    && candidate_body.notes.starts_with(&body.notes)
                    && arrangement
                        .iter()
                        .filter(|placement| placement.pattern_id == pattern.id)
                        .all(|placement| {
                            placement
                                .length_override
                                .is_some_and(|length| length <= candidate.next_note_start)
                        })
            })
            .max_by_key(|candidate| candidate.note_count);
        if let Some(candidate) = candidate {
            remap.insert(pattern.id, candidate.pattern_id);
        }
    }
    if remap.is_empty() {
        return;
    }

    for placement in arrangement.iter_mut() {
        while let Some(&pattern_id) = remap.get(&placement.pattern_id) {
            placement.pattern_id = pattern_id;
        }
    }
    patterns.retain(|pattern| !remap.contains_key(&pattern.id));
    let new_ids: HashMap<u32, u32> = patterns
        .iter()
        .enumerate()
        .map(|(index, pattern)| (pattern.id, index as u32))
        .collect();
    for pattern in patterns.iter_mut() {
        pattern.id = new_ids.get(&pattern.id).copied().unwrap_or(pattern.id);
    }
    for placement in arrangement {
        placement.pattern_id = new_ids
            .get(&placement.pattern_id)
            .copied()
            .unwrap_or(placement.pattern_id);
    }
}

/// Structure-preserving variant of [`build_song`], used when a driver-native
/// extractor recovered the real per-voice pattern placements
/// ([`SynthSource::structure`]). Instead of one whole-song pattern per voice, it ships
/// the driver's own reused blocks: one short [`Pattern`] per distinct driver
/// pattern (notes re-based to placement-relative ticks and *untransposed*),
/// placed once per orderlist step — each [`PatternPlacement`] carrying that step's
/// transpose (the engine adds it back to the pitch) and its own `length_override`
/// (so a timing-gated tune's per-instance span never has to match).
///
/// Reuse is content-addressed across tracks: leading silence moves to the
/// placement start, transposition and uniform velocity move to the placement,
/// and identical canonical note bodies share a `Pattern` regardless of source
/// pattern number or physical voice. Musical identity and note-local expression
/// are keyed separately; expression must still match exactly because the target
/// schema has no placement-local vibrato or glide override.
///
/// Per-frame automation (PWM / cutoff / ADSR / master volume) rides one
/// full-length lane pattern per track on the absolute timeline. The sequencer
/// engine evaluates every active placement's lanes by absolute position and keys
/// them to the target instrument's module — verified in `sequencer_engine` — so a
/// single full-length automation placement drives the instrument across every
/// reused note block without being baked into (and thus blocking the reuse of)
/// the blocks themselves.
#[allow(clippy::too_many_arguments)]
fn build_song_structured(
    export: &SynthSource<'_>,
    states: &[ProgramFrame],
    time_base: &TimeBase,
    frame_count: u32,
    plans: &[TrackPlan],
    ownership: &PhysicalVoiceOwnership,
    structure: &[VoicePlacements],
    fidelity: &mut forward::ResidualCensus,
    options: SynthOptions,
) -> Song {
    let tpr = time_base.ticks_per_frame;
    let full_len = frame_count.saturating_mul(tpr);
    let model = export.header.flags.sid_model;

    let mut patterns: Vec<Pattern> = Vec::new();
    let mut tracks: Vec<Track> = Vec::new();
    let mut arrangement: Vec<PatternPlacement> = Vec::new();
    let mut next_pattern_id: u32 = 0;
    let mut block_ids: HashMap<CanonicalPatternBody, u32> = HashMap::new();
    // The automation pattern that hosts the §A5 master-volume (Global) lane.
    let mut master_volume_host: Option<u32> = None;

    for (track_idx, plan) in plans.iter().enumerate() {
        let track_id = track_idx as u32;
        let instrument = plan.instrument_id;
        let name = plan_name(plan);

        // A clean arp plan converts to held base notes + a pattern `Arpeggiator`
        // processor (replicated onto every block of the plan, since the offset
        // table is plan-constant); otherwise the per-frame bake. `None` when the
        // flag is off, the plan is not an arp plan, or the clamp would fire.
        let arp_proc = arp_processor_for(
            plan,
            states,
            export.timing,
            frame_count,
            options.arpeggiator_processor,
        );

        let detune = plan_detune_cents(plan);
        let mut expanded_notes: Vec<Note> = Vec::new();
        for &(si, ev) in &plan.timeline {
            let arpeggio = plan.segments[si]
                .profile
                .and_then(|p| p.arpeggio_loop.as_deref())
                .filter(|offsets| !offsets.is_empty());
            match arpeggio {
                Some(_) if arp_proc.is_some() => {
                    let id = expanded_notes.len() as u32;
                    expanded_notes.push(build_note(id, ev, tpr, frame_count, states));
                }
                Some(offsets) => {
                    push_arp_event(
                        &mut expanded_notes,
                        ev,
                        offsets,
                        tpr,
                        frame_count,
                        states,
                        export.timing,
                        plan.voice_index,
                        detune,
                        options.forward_gate,
                        fidelity,
                    );
                }
                None => {
                    let percussion = plan.segments[si].patch.is_some_and(is_sid_percussion);
                    push_melodic_event(
                        &mut expanded_notes,
                        ev,
                        states,
                        export.effects,
                        export.timing,
                        plan.voice,
                        plan.voice_index,
                        frame_count,
                        tpr,
                        plan.segments[si]
                            .patch
                            .and_then(|p| p.authored_effects.as_ref()),
                        percussion,
                        detune,
                        options.forward_gate,
                        fidelity,
                        ownership.end(ev),
                    );
                }
            }
        }
        record_note_fidelity(
            fidelity,
            plan,
            &expanded_notes,
            arp_proc.as_ref(),
            export.effects,
            tpr,
            export.timing,
            states,
        );

        let voice_placements = structure
            .iter()
            .find(|v| v.voice == plan.voice)
            .map(|v| v.placements.as_slice())
            .filter(|p| !p.is_empty());

        match voice_placements {
            Some(placements) => {
                for (pi, pl) in placements.iter().enumerate() {
                    let lo = pl.start_frame.0;
                    let hi = placements
                        .get(pi + 1)
                        .map_or(frame_count, |n| n.start_frame.0);
                    if lo >= hi {
                        continue;
                    }

                    let lo_tick = lo.saturating_mul(tpr);
                    let hi_tick = hi.saturating_mul(tpr);
                    let window_start = expanded_notes.partition_point(|note| note.start < lo_tick);
                    let window_end = expanded_notes.partition_point(|note| note.start < hi_tick);
                    let mut raw = expanded_notes[window_start..window_end].to_vec();
                    if raw.is_empty() {
                        continue;
                    }

                    // Re-base to placement-relative ticks and remove the step's
                    // transpose (the placement re-applies it via the engine).
                    // Untransposing lets a pattern played at several transposes
                    // share one block — but only when it round-trips losslessly:
                    // a note whose pitch is below the transpose would clamp at 0
                    // and come back wrong. In that (rare) case bake the actual
                    // pitches and leave the placement untransposed, so the sound
                    // is always exact (just one less block of reuse). Glide is a
                    // signed offset from the note's own pitch — transpose- and
                    // position-invariant — so it is untouched.
                    let base_tick = lo_tick;
                    let tr = i32::from(pl.transpose);
                    let liftable = tr != 0 && raw.iter().all(|n| n.pitch as i32 >= tr);
                    let applied = if liftable { tr } else { 0 };
                    let leading_ticks = raw
                        .iter()
                        .map(|note| note.start.saturating_sub(base_tick))
                        .min()
                        .unwrap_or(0);
                    for (k, n) in raw.iter_mut().enumerate() {
                        n.start = n
                            .start
                            .saturating_sub(base_tick)
                            .saturating_sub(leading_ticks);
                        n.pitch = (n.pitch as i32 - applied) as u32;
                        n.id = k as u32;
                    }
                    let placement_gain = lift_pattern_gain(&mut raw);
                    let next_note_id = raw.len() as u32;
                    let span = hi
                        .saturating_sub(lo)
                        .saturating_mul(tpr)
                        .saturating_sub(leading_ticks);

                    let processors: Vec<NoteProcessor> = arp_proc.clone().into_iter().collect();
                    let key = CanonicalPatternBody::new(&raw, &processors);
                    let pattern_id = match block_ids.get(&key) {
                        Some(&id) => id,
                        None => {
                            let id = next_pattern_id;
                            next_pattern_id += 1;
                            patterns.push(Pattern {
                                id,
                                name: format!(
                                    "{} #{} {applied:+} · p{id}",
                                    plan_base_name(plan),
                                    pl.pattern_number
                                ),
                                length: span,
                                notes: raw,
                                automation: Vec::new(),
                                next_note_id,
                                processors,
                                note_graph: None,
                            });
                            block_ids.insert(key, id);
                            id
                        }
                    };
                    arrangement.push(PatternPlacement {
                        pattern_id,
                        track_id,
                        start: base_tick.saturating_add(leading_ticks),
                        transpose: applied as f32,
                        gain: placement_gain,
                        length_override: Some(span),
                        loop_mode: "clip",
                    });
                }
            }
            None => {
                // No driver structure for this voice (e.g. an empty orderlist):
                // fall back to one whole-song note pattern, as the flat path does.
                let raw = expanded_notes;
                let next_note_id = raw.len() as u32;
                let id = next_pattern_id;
                next_pattern_id += 1;
                patterns.push(Pattern {
                    id,
                    name: name.clone(),
                    length: full_len,
                    notes: raw,
                    automation: Vec::new(),
                    next_note_id,
                    processors: arp_proc.clone().into_iter().collect(),
                    note_graph: None,
                });
                arrangement.push(PatternPlacement {
                    pattern_id: id,
                    track_id,
                    start: 0,
                    transpose: 0.0,
                    gain: 1.0,
                    length_override: None,
                    loop_mode: "clip",
                });
            }
        }

        // Per-frame automation rides one full-length lane pattern on this track,
        // applied globally by the engine regardless of which note block plays.
        let automation = build_plan_automation(
            plan,
            ownership,
            states,
            export.effects,
            export.timing,
            model,
            frame_count,
            tpr,
        );
        if !automation.is_empty() {
            let id = next_pattern_id;
            next_pattern_id += 1;
            if master_volume_host.is_none() {
                master_volume_host = Some(id);
            }
            patterns.push(Pattern {
                id,
                name: format!("{name} auto"),
                length: full_len,
                notes: Vec::new(),
                automation,
                next_note_id: 0,
                processors: Vec::new(),
                note_graph: None,
            });
            arrangement.push(PatternPlacement {
                pattern_id: id,
                track_id,
                start: 0,
                transpose: 0.0,
                gain: 1.0,
                length_override: None,
                loop_mode: "clip",
            });
        }

        tracks.push(Track {
            id: track_id,
            name,
            instrument,
            volume: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            color: track_color(plan.voice_index),
            mode: "Polyphonic",
            sends: Vec::new(),
        });
    }

    // §A5: the chip-global `$D418` master-volume contour is one `Global` lane.
    // Host it on an existing full-length automation pattern, or — if no plan had
    // automation — on a dedicated full-length pattern placed on the first track.
    if let Some(lane) = build_master_volume_lane(states, frame_count, tpr) {
        match master_volume_host.and_then(|id| patterns.iter_mut().find(|p| p.id == id)) {
            Some(host) => host.automation.push(lane),
            None if !tracks.is_empty() => {
                let id = next_pattern_id;
                patterns.push(Pattern {
                    id,
                    name: "Master volume".to_string(),
                    length: full_len,
                    notes: Vec::new(),
                    automation: vec![lane],
                    next_note_id: 0,
                    processors: Vec::new(),
                    note_graph: None,
                });
                arrangement.push(PatternPlacement {
                    pattern_id: id,
                    track_id: 0,
                    start: 0,
                    transpose: 0.0,
                    gain: 1.0,
                    length_override: None,
                    loop_mode: "clip",
                });
            }
            None => {}
        }
    }

    assemble_song(export, time_base, patterns, tracks, arrangement)
}

/// Build the §A5 master-volume contour lane: sample the per-frame `$D418`
/// master-volume nibble across the whole subtune, normalise into `0.0..=1.0`,
/// and decimate to interpolation points (same RDP + curve fit as the module
/// lanes). Targets `AutomationTarget::Global` `MasterVolume`.
///
/// Returns `None` when the volume never moves (a constant the static master
/// volume already covers) or when the decimated series exceeds
/// [`MAX_VOLUME_LANE_POINTS`] — a sign the register is being hammered for
/// `$D418` PCM digi (§A7) rather than a musical swell, which is not a contour
/// worth emitting.
fn build_master_volume_lane(
    states: &[ProgramFrame],
    frame_count: u32,
    tpr: u32,
) -> Option<AutomationLane> {
    if frame_count == 0 {
        return None;
    }
    let values: Vec<f32> = (0..frame_count)
        .map(|f| {
            states
                .get(f as usize)
                .map_or(0.0, |s| f32::from(s.volume.0) / SID_MAX_VOLUME)
        })
        .collect();

    let base = values[0];
    if values
        .iter()
        .all(|v| (v - base).abs() <= AUTOMATION_EPSILON)
    {
        return None;
    }

    let mut points = Vec::new();
    push_span_points(&values, 0, tpr, &mut points);
    if points.len() < 2 || points.len() > MAX_VOLUME_LANE_POINTS {
        return None;
    }
    Some(AutomationLane {
        target: AutomationTarget::Global(GlobalTarget::master_volume()),
        points,
    })
}

/// The §A3 role of a plan, from the representative segment's patch role tags
/// (`segments[0]` is the representative the instrument's static parameters come
/// from). A raw plan with no backing patch is [`InstrumentRole::Untagged`].
fn plan_role(plan: &TrackPlan) -> InstrumentRole {
    plan.segments
        .first()
        .and_then(|s| s.patch)
        .map_or(InstrumentRole::Untagged, |p| {
            InstrumentRole::from_role_tags(&p.role_tags)
        })
}

/// The instrument/track/pattern name for a plan. A role-tagged plan (§A3) is
/// named musically (`V{n} Bass`, `V{n} Kick`, …); an untagged plan falls back
/// to `V{n} {waveform}` with `flt` and `rng` suffixes for the filter / ring-mod
/// inserts, or `V{n} raw` for a fallback group with no backing patch. The
/// voice prefix is kept so two voices sharing a role stay distinct.
/// Whether any of the plan's segments carries an arpeggio loop (arp patches stay
/// standalone — note-expanded, never merged).
fn plan_arpeggiated(plan: &TrackPlan) -> bool {
    plan.segments.iter().any(|s| {
        s.profile
            .and_then(|p| p.arpeggio_loop.as_deref())
            .is_some_and(|o| !o.is_empty())
    })
}

/// Compact timbre descriptor distinguishing two instruments that share a
/// voice+role: waveform (with any combine partner), filter kind, ring-mod and
/// arpeggio. Leads with the waveform so it reads on its own (`pulse+tri bandpass`).
fn plan_timbre(plan: &TrackPlan) -> String {
    let mut parts: Vec<String> = Vec::new();
    if plan.shape.is_noise {
        parts.push("noise".to_string());
    } else {
        parts.push(match plan.shape.secondary_waveform {
            Some(secondary) => format!("{}+{}", plan.shape.waveform, secondary),
            None => plan.shape.waveform.to_string(),
        });
    }
    if let Some(kind) = plan.shape.filter_kind {
        parts.push(kind.to_string());
    }
    if plan.shape.has_ring {
        parts.push("ring".to_string());
    }
    if plan.shape.has_sync {
        parts.push("sync".to_string());
    }
    if plan_arpeggiated(plan) {
        parts.push("arp".to_string());
    }
    parts.join(" ")
}

/// Short `V{voice} {role}` (or `V{voice} {waveform}`) label with no timbre
/// disambiguator or id — the prefix for pattern names, where the block number,
/// transpose and pattern id carry uniqueness.
fn plan_base_name(plan: &TrackPlan) -> String {
    if plan.drum_drop {
        return format!("V{} drum (drop)", plan.voice.0);
    }
    if plan.segments.iter().all(|s| s.patch.is_none()) {
        return format!("V{} raw", plan.voice.0);
    }
    match plan_role(plan).name() {
        Some(role) => format!("V{} {role}", plan.voice.0),
        None => format!("V{} {}", plan.voice.0, plan.shape.waveform),
    }
}

/// The unique instrument (and track) name: `V{voice} [role ]{timbre} · i{id}`.
/// Carrying the engine instrument id makes the name and `list_instruments` id
/// agree, so two instruments of the same voice+role never collide.
fn plan_name(plan: &TrackPlan) -> String {
    let id = plan.instrument_id;
    if plan.drum_drop {
        return format!("V{} drum (drop) · i{id}", plan.voice.0);
    }
    if plan.segments.iter().all(|s| s.patch.is_none()) {
        return format!("V{} raw · i{id}", plan.voice.0);
    }
    let base = match plan_role(plan).name() {
        Some(role) => format!("V{} {role}", plan.voice.0),
        None => format!("V{}", plan.voice.0),
    };
    format!("{base} {} · i{id}", plan_timbre(plan))
}

/// Lower one emitted [`Note`] to the forward model's plain-data spec
/// ([`forward::NoteSpec`]): absolute ticks → frames, MIDI + instrument detune →
/// cents, and the serializer expression/glide/processor structs → their spec
/// counterparts. Lives here (not in `forward`) so the serializer structs keep
/// their private fields.
fn note_spec(
    note: &Note,
    detune_cents: f32,
    arp_proc: Option<&NoteProcessor>,
    tpr: u32,
) -> forward::NoteSpec {
    let tpr = tpr.max(1);
    forward::NoteSpec {
        start_frame: note.start / tpr,
        frames: (note.duration / tpr).max(1),
        pitch_cents: note.pitch as f32 * 100.0 + detune_cents,
        glide: note.glide.map(|g| {
            let GlideFrom::Semitones(s) = g.from;
            forward::GlideSpec {
                from_cents: s * 100.0,
                time_ms: g.time,
            }
        }),
        vibrato: note
            .expression
            .as_ref()
            .and_then(|e| e.vibrato.as_ref())
            .map(|v| forward::VibratoSpec {
                depth_cents: v.depth * 100.0,
                rate_hz: v.rate,
                delay_ms: v.delay,
                triangle: matches!(v.shape, VibratoShape::Triangle),
            }),
        arp: arp_proc.map(|NoteProcessor::Arpeggiator(a)| {
            let ArpRate::MilliHz(m) = a.rate;
            forward::ArpSpec {
                offsets: a.custom.clone(),
                rate_millihz: m,
            }
        }),
        track_pitch_cents: Vec::new(),
        // A vibrato prediction drifts against the chip's LFO phase; the
        // worst case (anti-phase) diverges by 2×depth, so that is the slack —
        // the check then verifies the carrier (center pitch + glide), never
        // failing a note on LFO phase alone.
        slack_cents: note
            .expression
            .as_ref()
            .and_then(|e| e.vibrato.as_ref())
            .map_or(0.0, |v| 2.0 * v.depth * 100.0),
    }
}

/// Record every note of one plan batch into the forward-model residual census
/// (slice 1, report-only). `notes` must still be in the **absolute** tick and
/// pitch domain — the structured path calls this before its placement re-base.
#[allow(clippy::too_many_arguments)]
fn record_note_fidelity(
    fidelity: &mut forward::ResidualCensus,
    plan: &TrackPlan,
    notes: &[Note],
    arp_proc: Option<&NoteProcessor>,
    effects: &[EffectSpan],
    tpr: u32,
    timing: PlaybackTiming,
    states: &[ProgramFrame],
) {
    let label = plan_name(plan);
    let detune = plan_detune_cents(plan);
    let percussion = plan.drum_drop
        || plan
            .segments
            .iter()
            .find_map(|s| s.patch)
            .is_some_and(is_sid_percussion);
    for note in notes {
        let mut spec = note_spec(note, detune, arp_proc, tpr);
        if plan.timeline.iter().any(|&(_, event)| {
            event.start_frame.0 <= spec.start_frame
                && spec.start_frame < authored_note_end_frame(event, states.len() as u32)
                && measured_vibrato_for_event(
                    event,
                    states,
                    effects,
                    timing,
                    plan.voice,
                    plan.voice_index,
                    states.len() as u32,
                )
                .is_some_and(|vibrato| vibrato.delay > 0.0)
        }) {
            spec.track_pitch_cents = traced_track_pitch_offsets(
                spec.start_frame,
                spec.frames,
                spec.pitch_cents,
                plan.voice_index,
                states,
                timing,
            );
        }
        fidelity.record(&label, plan.voice_index, percussion, &spec, timing, states);
    }
}

/// Build every automation lane for a plan: delayed-vibrato track pitch, the
/// per-frame `pulse_width` / `cutoff` lanes sampled from chip state, and the
/// per-section ADSR step lanes ([`build_adsr_lanes`]).
///
/// The per-frame lanes sample the actual [`ProgramFrame`], so the SID's exact PW
/// modulation and filter sweeps are reproduced rather than approximated (what
/// the `AutomationTarget::Module` schema variant unlocks). The two registers
/// differ in scope, so they sample differently (the `continuous` flag on
/// [`build_param_lane`]):
/// - **pulse width** is per-voice — another voice/patch owns the register
///   between this plan's notes — so it is sampled per note span;
/// - **filter cutoff** is a single *global* register the play routine sweeps
///   continuously, so it is sampled across one contiguous range; gating it to
///   note spans would freeze the sweep and re-step it at every note boundary.
///
/// Raw plans can still carry track pitch because it does not target an
/// instrument module; their module and ADSR lanes remain empty.
#[allow(clippy::too_many_arguments)]
fn build_plan_automation(
    plan: &TrackPlan,
    ownership: &PhysicalVoiceOwnership,
    states: &[ProgramFrame],
    effects: &[EffectSpan],
    timing: PlaybackTiming,
    model: SidModel,
    frame_count: u32,
    tpr: u32,
) -> Vec<AutomationLane> {
    let instrument = plan.instrument_id;
    let voice_index = plan.voice_index;
    let events: Vec<&NoteEvent> = plan.timeline.iter().map(|&(_, ev)| ev).collect();
    let mut lanes = Vec::new();

    if let Some(lane) =
        build_delayed_vibrato_pitch_lane(plan, ownership, states, effects, timing, frame_count, tpr)
    {
        lanes.push(lane);
    }

    // An authored continuous-PWM program replaces this lane entirely: the
    // instrument carries a `script` module regenerating the driver's sweep
    // ([`plan_authored_pwm`] — the same gate [`build_instruments`] used), so a
    // baked lane on top would double-modulate the register.
    if !plan.shape.is_noise
        && plan.shape.waveform == "pulse"
        && plan_authored_pwm(plan, states).is_none()
        && let Some(lane) = build_param_lane(
            ModuleTarget::new(instrument, "sid_oscillator", "pw_reg"),
            &events,
            frame_count,
            tpr,
            ParamLaneSampling::PER_NOTE,
            |f| sample_pw(states, voice_index, f),
            normalize_pulse_width,
        )
    {
        lanes.push(lane);
    }
    // Emit the cutoff lane exactly when the shape carries a filter, so the
    // target's `flt-1` always exists — never a dangling target, never a filter
    // without its sweep.
    if plan.shape.filter_kind.is_some()
        && let Some(lane) = build_param_lane(
            ModuleTarget::new(instrument, "filter", "cutoff"),
            &events,
            frame_count,
            tpr,
            ParamLaneSampling::CONTINUOUS,
            |f| sample_cutoff(states, f),
            |raw| normalize_cutoff_hz(cutoff_value_to_hz(f32::from(raw), model)),
        )
    {
        lanes.push(lane);
    }
    if (plan.shape.has_ring || plan.shape.has_sync)
        && let Some(lane) = build_param_lane(
            ModuleTarget::with_instance(instrument, "sid_oscillator", 2, "freq_reg"),
            &events,
            frame_count,
            tpr,
            ParamLaneSampling::SID_FREQUENCY,
            |frame| {
                states
                    .get(frame as usize)
                    .map_or(0, |state| state.voices[(voice_index + 2) % 3].freq.0)
            },
            |freq| oscillator_frequency_param().normalize(f32::from(freq)),
        )
    {
        lanes.push(lane);
    }

    if plan_uses_measured_envelope(plan, states) {
        for instance in 1..=plan_amplifier_count(plan) {
            if let Some(lane) =
                build_envelope_lane(plan, ownership, states, frame_count, tpr, instance)
            {
                lanes.push(lane);
            }
        }
    } else {
        lanes.extend(build_adsr_lanes(plan, tpr));
    }
    lanes
}

const TRACK_PITCH_RANGE_CENTS: Cents = Cents(4_800.0);
const PITCH_AUTOMATION_EPSILON: f32 = 0.0001;

fn normalize_track_pitch(offset: Cents) -> f32 {
    (0.5 + offset.0 / (2.0 * TRACK_PITCH_RANGE_CENTS.0)).clamp(0.0, 1.0)
}

fn traced_track_pitch_offsets(
    start: u32,
    frames: u32,
    base_cents: f32,
    voice_index: usize,
    states: &[ProgramFrame],
    timing: PlaybackTiming,
) -> Vec<f32> {
    (start..start.saturating_add(frames))
        .map(|frame| {
            states
                .get(frame as usize)
                .and_then(|state| fine_pitch(state.voices[voice_index].freq, timing.clock))
                .map_or(0.0, |pitch| {
                    Cents(pitch * 100.0 - base_cents)
                        .0
                        .clamp(-TRACK_PITCH_RANGE_CENTS.0, TRACK_PITCH_RANGE_CENTS.0)
                })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn build_delayed_vibrato_pitch_lane(
    plan: &TrackPlan,
    ownership: &PhysicalVoiceOwnership,
    states: &[ProgramFrame],
    effects: &[EffectSpan],
    timing: PlaybackTiming,
    frame_count: u32,
    tpr: u32,
) -> Option<AutomationLane> {
    let delayed: Vec<&NoteEvent> = plan
        .timeline
        .iter()
        .map(|&(_, event)| event)
        .filter(|event| {
            measured_vibrato_for_event(
                event,
                states,
                effects,
                timing,
                plan.voice,
                plan.voice_index,
                frame_count,
            )
            .is_some_and(|vibrato| vibrato.delay > 0.0)
        })
        .collect();
    if delayed.is_empty() {
        return None;
    }

    let detune = plan_detune_cents(plan);
    let mut points = Vec::new();
    for (index, event) in delayed.iter().enumerate() {
        let start = event.start_frame.0;
        let mut end = event
            .sound_end_frame(states)
            .map_or_else(
                || authored_note_end_frame(event, frame_count),
                |frame| frame.0,
            )
            .min(states.len() as u32);
        if let Some(ownership_end) = ownership.end(event) {
            end = end.min(ownership_end.0);
        }
        if end <= start {
            continue;
        }

        let base_cents = f32::from(event.midi.0) * 100.0 + detune;
        let values: Vec<f32> = (start..end)
            .map(|frame| {
                states
                    .get(frame as usize)
                    .and_then(|state| fine_pitch(state.voices[plan.voice_index].freq, timing.clock))
                    .map_or(0.5, |pitch| {
                        normalize_track_pitch(Cents(pitch * 100.0 - base_cents))
                    })
            })
            .collect();
        push_span_points_with_epsilon(&values, start, tpr, PITCH_AUTOMATION_EPSILON, &mut points);
        if delayed
            .get(index + 1)
            .is_none_or(|next| next.start_frame.0 != end)
        {
            points.push(AutomationPoint {
                tick: end.saturating_mul(tpr),
                value: 0.5,
                curve: CurveType::Step,
            });
        }
    }

    (!points.is_empty()).then_some(AutomationLane {
        target: AutomationTarget::Track(TrackTarget::pitch()),
        points,
    })
}

fn plan_uses_measured_envelope(plan: &TrackPlan, states: &[ProgramFrame]) -> bool {
    !plan.timeline.is_empty()
        && (!plan.timeline.iter().all(|(_, event)| {
            crate::analysis::programs::analyze_note(event, states).envelope
                == crate::analysis::programs::EnvelopeRecipe::NormalAdsr
        }) || !plan
            .timeline
            .iter()
            .all(|(_, event)| articulation_is_automatable(event, states)))
}

fn plan_amplifier_count(plan: &TrackPlan) -> u16 {
    let percussion = plan.drum_drop
        || plan
            .segments
            .first()
            .and_then(|segment| segment.patch)
            .is_some_and(is_sid_percussion)
            && plan.waveform_program.is_none();
    if percussion { 2 } else { 1 }
}

fn build_envelope_lane(
    plan: &TrackPlan,
    ownership: &PhysicalVoiceOwnership,
    states: &[ProgramFrame],
    frame_count: u32,
    tpr: u32,
    instance: u16,
) -> Option<AutomationLane> {
    let mut points = Vec::new();
    let voice = plan.voice_index;
    for &(_, event) in &plan.timeline {
        let start = event.start_frame.0;
        let mut end = event
            .sound_end_frame(states)
            .map_or_else(
                || authored_note_end_frame(event, frame_count),
                |frame| frame.0,
            )
            .min(states.len() as u32);
        if let Some(ownership_end) = ownership.end(event) {
            end = end.min(ownership_end.0);
        }
        if end <= start {
            continue;
        }
        let values: Vec<f32> = (start..end)
            .map(|frame| {
                states[frame as usize].digital_voices[voice]
                    .envelope_activity
                    .end_level
                    .0 as f32
                    / 255.0
            })
            .collect();
        push_span_points(&values, start, tpr, &mut points);
        points.push(AutomationPoint {
            tick: end.saturating_mul(tpr),
            value: 0.0,
            curve: CurveType::Step,
        });
    }
    (!points.is_empty()).then_some(AutomationLane {
        target: AutomationTarget::Module(ModuleTarget::with_instance(
            plan.instrument_id,
            "amplifier",
            instance,
            "level",
        )),
        points,
    })
}

/// Tolerance (normalised lane units) below which two ADSR values are treated as
/// equal when coalescing the step lanes.
const ADSR_EPSILON: f32 = 1e-4;

/// Build the per-section ADSR step lanes for a merged plan: one lane each for
/// `attack` / `decay` / `sustain` / `release` on the envelope, stepping to each
/// section's value at that section's note. A note plays with the envelope value
/// present at its gate (verified against the engine), so a `Step` point placed
/// at each note where the value changes is enough.
///
/// A lane is emitted only when its value actually varies across the timeline —
/// when it is constant, the instrument's static envelope already covers it. The
/// step sequence follows the timeline (not the segment order), so a voice that
/// alternates between two patches switches its ADSR back and forth correctly.
fn build_adsr_lanes(plan: &TrackPlan, tpr: u32) -> Vec<AutomationLane> {
    // Per-segment decoded ADSR; a raw segment (no patch) contributes nothing.
    let seg_adsr: Vec<Option<AdsrSeconds>> = plan
        .segments
        .iter()
        .map(|s| s.patch.map(|p| adsr_to_seconds(p.adsr)))
        .collect();

    type EnvParam = (&'static str, fn(&AdsrSeconds) -> f32);
    let params: [EnvParam; 4] = [
        ("attack", |a| normalize_env_seconds(a.attack)),
        ("decay", |a| normalize_env_seconds(a.decay)),
        ("sustain", |a| a.sustain.clamp(0.0, 1.0)),
        ("release", |a| normalize_env_seconds(a.release)),
    ];

    params
        .into_iter()
        .filter_map(|(param_id, value_of)| {
            build_adsr_lane(
                ModuleTarget::new(plan.instrument_id, "envelope", param_id),
                plan,
                &seg_adsr,
                tpr,
                value_of,
            )
        })
        .collect()
}

/// One ADSR step lane: walk the timeline, and at each note whose section value
/// differs from the previous one, emit a `Step` point at that note's start.
/// Returns `None` unless at least two distinct values occur (a constant lane is
/// already covered by the instrument's static envelope).
fn build_adsr_lane(
    target: ModuleTarget,
    plan: &TrackPlan,
    seg_adsr: &[Option<AdsrSeconds>],
    tpr: u32,
    value_of: impl Fn(&AdsrSeconds) -> f32,
) -> Option<AutomationLane> {
    let mut points: Vec<AutomationPoint> = Vec::new();
    let mut last: Option<f32> = None;
    for &(si, ev) in &plan.timeline {
        let Some(adsr) = seg_adsr.get(si).and_then(Option::as_ref) else {
            continue;
        };
        let value = value_of(adsr);
        if last.is_none_or(|prev| (prev - value).abs() > ADSR_EPSILON) {
            points.push(AutomationPoint {
                tick: ev.start_frame.0.saturating_mul(tpr),
                value,
                curve: CurveType::Step,
            });
            last = Some(value);
        }
    }
    (points.len() >= 2).then_some(AutomationLane {
        target: AutomationTarget::Module(target),
        points,
    })
}

/// Invert Pertylizer's envelope `attack`/`decay`/`release` mapping to the
/// normalized `0.0..=1.0` automation-lane value, through the `envelope.attack`
/// descriptor's range and (exponential) response curve — mirroring the engine's
/// `ResponseCurve::Exponential::normalize`. Sustain uses a linear curve and is
/// mapped directly by the caller.
fn normalize_env_seconds(secs: f32) -> f32 {
    env_time_param().normalize(secs)
}

/// Per-frame pulse width of `voice_index` at `frame` (raw 12-bit), or `0`
/// when the frame is out of range.
fn sample_pw(states: &[ProgramFrame], voice_index: usize, frame: u32) -> u16 {
    states
        .get(frame as usize)
        .map_or(0, |s| s.voices[voice_index].pulse_width.0)
}

/// Per-frame global filter cutoff at `frame` (raw 11-bit), or `0` when the
/// frame is out of range.
fn sample_cutoff(states: &[ProgramFrame], frame: u32) -> u16 {
    states.get(frame as usize).map_or(0, |s| s.filter.cutoff.0)
}

/// Build one [`AutomationLane`] for `target` by sampling `raw_at(frame)` over
/// the frame ranges of `events`, normalising each sample into `0.0..=1.0`, and
/// decimating the per-frame series to a sparse set of interpolation points.
///
/// `sampling.continuous` selects how the register relates to this group's notes
/// ([`collapse_spans`]): a per-voice register (pulse width) is sampled once per
/// note span, since other patches own it in the gaps between this group's
/// notes; a *global* register swept continuously by the play routine (the
/// filter cutoff) is sampled across one contiguous range so the sweep is not
/// frozen and re-stepped at every note boundary.
///
/// Within each range, [`rdp_indices`] keeps the anchors needed for a **linear**
/// reconstruction within [`AUTOMATION_EPSILON`]; [`fit_segment_curves`] then
/// picks a per-segment curve (`Linear`/`SCurve`/`Exponential`/`Step`) and merges
/// adjacent anchors when one curve covers the wider span within tolerance. The
/// final anchor of each range is emitted as `Step` so the held value does not
/// ramp across the following gap. A per-frame ramp thus collapses to its
/// endpoints, a triangle to its turning points, and a smooth ease to a single
/// curved segment.
///
/// Returns `None` when the series never moves beyond the tolerance (a constant
/// the module's static parameter already covers).
fn build_param_lane<R, N>(
    target: ModuleTarget,
    events: &[&NoteEvent],
    frame_count: u32,
    tpr: u32,
    sampling: ParamLaneSampling,
    raw_at: R,
    normalize: N,
) -> Option<AutomationLane>
where
    R: Fn(u32) -> u16,
    N: Fn(u16) -> f32,
{
    let ranges: Vec<(u32, u32)> = events
        .iter()
        .map(|ev| (ev.start_frame.0, authored_note_end_frame(ev, frame_count)))
        .collect();

    let mut points: Vec<AutomationPoint> = Vec::new();
    let mut baseline: Option<f32> = None;
    let mut moved = false;

    for (start, end) in collapse_spans(&ranges, sampling.continuous) {
        let values: Vec<f32> = (start..end).map(|f| normalize(raw_at(f))).collect();

        let base = *baseline.get_or_insert(values[0]);
        if values.iter().any(|v| (v - base).abs() > sampling.epsilon) {
            moved = true;
        }

        push_span_points_with_epsilon(&values, start, tpr, sampling.epsilon, &mut points);
    }

    (moved && points.len() >= 2).then_some(AutomationLane {
        target: AutomationTarget::Module(target),
        points,
    })
}

/// Decimate one span's per-frame value series into interpolation points and
/// append them to `out`. [`rdp_indices`] keeps the anchors needed for a linear
/// reconstruction within [`AUTOMATION_EPSILON`]; [`fit_segment_curves`] then
/// assigns each a curve and merges adjacent anchors. Frame `start + idx` is
/// scaled to ticks by `tpr`. Shared by the per-module lanes ([`build_param_lane`])
/// and the §A5 master-volume lane ([`build_master_volume_lane`]).
fn push_span_points(values: &[f32], start: u32, tpr: u32, out: &mut Vec<AutomationPoint>) {
    push_span_points_with_epsilon(values, start, tpr, AUTOMATION_EPSILON, out);
}

fn push_span_points_with_epsilon(
    values: &[f32],
    start: u32,
    tpr: u32,
    epsilon: f32,
    out: &mut Vec<AutomationPoint>,
) {
    let anchors = rdp_indices(values, epsilon);
    for (idx, curve) in fit_segment_curves(values, &anchors, epsilon) {
        out.push(AutomationPoint {
            tick: (start + idx as u32).saturating_mul(tpr),
            value: values[idx],
            curve,
        });
    }
}

/// Resolve the half-open frame ranges to sample for an automation lane.
///
/// Drops empty ranges (`end <= start`). When `continuous`, collapses the
/// survivors into one contiguous range `[earliest start, latest end)` so a
/// globally-swept register is sampled without gaps between this group's notes;
/// otherwise returns one range per note. Returns empty when nothing is valid.
fn collapse_spans(ranges: &[(u32, u32)], continuous: bool) -> Vec<(u32, u32)> {
    let valid: Vec<(u32, u32)> = ranges.iter().copied().filter(|&(s, e)| e > s).collect();
    if !continuous {
        return valid;
    }
    match (
        valid.iter().map(|&(s, _)| s).min(),
        valid.iter().map(|&(_, e)| e).max(),
    ) {
        (Some(start), Some(end)) => vec![(start, end)],
        _ => Vec::new(),
    }
}

/// Ramer–Douglas–Peucker simplification of a uniformly-sampled value series,
/// measuring **vertical** (value-axis) distance rather than perpendicular
/// distance: the samples are a function `value(frame)` reconstructed by linear
/// interpolation, so the error that matters is `|sample − interpolated|`, and
/// the frame axis must not mix into the metric (it dwarfs the `0..1` values).
///
/// Returns the sorted indices to keep — always including the first and last —
/// such that linear interpolation between them stays within `epsilon` of every
/// original sample. Iterative (explicit stack) to stay safe on long held notes.
fn rdp_indices(values: &[f32], epsilon: f32) -> Vec<usize> {
    let n = values.len();
    if n <= 2 {
        return (0..n).collect();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;

    let mut stack = vec![(0usize, n - 1)];
    while let Some((lo, hi)) = stack.pop() {
        if hi <= lo + 1 {
            continue;
        }
        let v_lo = values[lo];
        let v_hi = values[hi];
        let span = (hi - lo) as f32;
        let mut max_dev = 0.0f32;
        let mut split = lo;
        for (offset, &v) in values[(lo + 1)..hi].iter().enumerate() {
            let m = lo + 1 + offset;
            let t = (m - lo) as f32 / span;
            let interp = v_lo + (v_hi - v_lo) * t;
            let dev = (v - interp).abs();
            if dev > max_dev {
                max_dev = dev;
                split = m;
            }
        }
        if max_dev > epsilon {
            keep[split] = true;
            stack.push((lo, split));
            stack.push((split, hi));
        }
    }

    (0..n).filter(|&i| keep[i]).collect()
}

/// Assign each emitted point an interpolation curve and greedily merge anchors.
///
/// `anchors` are [`rdp_indices`] into `values` (a single span's per-frame
/// series). Walking left to right, the segment from the current anchor is
/// extended to the farthest later anchor for which some curve reconstructs
/// every intervening sample within `epsilon` ([`fit_curve`]); the dropped
/// anchors in between vanish. Each emitted point carries the curve used to
/// reach the next emitted point. The span's final anchor is `Step` (its curve
/// is moot — no next point — and `Step` holds the value across the gap to the
/// next span rather than ramping into it).
fn fit_segment_curves(values: &[f32], anchors: &[usize], epsilon: f32) -> Vec<(usize, CurveType)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < anchors.len() {
        let a = anchors[i];
        if i + 1 >= anchors.len() {
            out.push((a, CurveType::Step));
            break;
        }
        // The minimal segment to the next anchor always fits within epsilon
        // (RDP bounds its linear residual), so this is a safe starting point.
        let (mut curve, _) = fit_curve(values, a, anchors[i + 1]);
        let mut next = i + 1;
        let mut j = i + 2;
        while j < anchors.len() {
            let (cand, residual) = fit_curve(values, a, anchors[j]);
            if residual > epsilon {
                break;
            }
            curve = cand;
            next = j;
            j += 1;
        }
        out.push((a, curve));
        i = next;
    }
    out
}

/// Best-fitting curve for the segment `values[a..=b]` and its max residual.
///
/// Tries the engine's four interpolation shapes against every intervening
/// sample and returns the one with the lowest worst-case error. `Linear` is the
/// default and wins ties, so a curve is chosen only when it strictly improves
/// the fit (`Step` for held-then-jump shapes, `SCurve`/`Exponential` for eases).
fn fit_curve(values: &[f32], a: usize, b: usize) -> (CurveType, f32) {
    let mut best = (
        CurveType::Linear,
        segment_max_residual(values, a, b, CurveType::Linear),
    );
    for cand in [CurveType::Step, CurveType::SCurve] {
        let residual = segment_max_residual(values, a, b, cand);
        if residual < best.1 {
            best = (cand, residual);
        }
    }
    if let Some(strength) = fit_exponential_strength(values, a, b) {
        let cand = CurveType::Exponential(strength);
        let residual = segment_max_residual(values, a, b, cand);
        if residual < best.1 {
            best = (cand, residual);
        }
    }
    best
}

/// Worst-case `|sample − reconstruction|` over the interior of `values[a..=b]`
/// for `curve`. Endpoints are exact (`a` is the point value; `b` is the next
/// emitted point), so only interior samples can deviate.
fn segment_max_residual(values: &[f32], a: usize, b: usize, curve: CurveType) -> f32 {
    if b <= a + 1 {
        return 0.0;
    }
    let from = values[a];
    let to = values[b];
    let span = (b - a) as f32;
    let mut max = 0.0f32;
    for (offset, &v) in values[(a + 1)..b].iter().enumerate() {
        let t = (offset + 1) as f32 / span;
        let recon = curve.interpolate(from, to, t);
        max = max.max((v - recon).abs());
    }
    max
}

/// Estimate an `Exponential` strength fitting the ease of `values[a..=b]`.
///
/// Reads the shape at the segment midpoint: where the curve sits relative to
/// the linear diagonal fixes the branch (slow start → positive strength, fast
/// start → negative) and inverting the engine's power law gives the exponent.
/// Returns `None` for flat or non-monotonic segments (no exponential ease to
/// fit) and for near-linear fits (`|strength| ≤ NEAR_LINEAR_STRENGTH`), so
/// barely-curved segments stay `Linear`; the caller verifies the actual
/// residual before accepting it.
fn fit_exponential_strength(values: &[f32], a: usize, b: usize) -> Option<i8> {
    if b <= a + 1 {
        return None;
    }
    let from = values[a];
    let delta = values[b] - from;
    if delta.abs() < 1e-6 {
        return None;
    }
    let span = (b - a) as f32;
    let mid = a + (b - a) / 2;
    let t = (mid - a) as f32 / span;
    if t <= 0.0 || t >= 1.0 {
        return None;
    }
    // Ideal `exp_t` at the midpoint; outside (0,1) the segment overshoots its
    // endpoints (not a simple monotonic ease), so there is nothing to fit.
    let r = (values[mid] - from) / delta;
    if !(0.0..=1.0).contains(&r) {
        return None;
    }
    let r = r.clamp(1e-4, 1.0 - 1e-4);
    let strength = if r < t {
        let p = (r.ln() / t.ln()).max(1.0);
        ((p - 1.0) / 0.02).round()
    } else if r > t {
        let q = ((1.0 - r).ln() / (1.0 - t).ln()).max(1.0);
        (-(q - 1.0) / 0.02).round()
    } else {
        return None;
    };
    let strength = strength.clamp(-127.0, 127.0) as i8;
    (strength.unsigned_abs() > NEAR_LINEAR_STRENGTH).then_some(strength)
}

/// The exclusive end frame of a note: its `end_frame`, or the trace end
/// (`frame_count`) when the note was still playing; never before `start_frame`.
fn authored_note_end_frame(event: &NoteEvent, frame_count: u32) -> u32 {
    event
        .end_frame
        .map_or(frame_count, |f| f.0)
        .max(event.start_frame.0)
}

/// A gate must stay low this many frames to count as a note-off rather than a
/// one-frame hard-restart dip (the gate flicker some drivers use to re-attack
/// the envelope without ending the note).
const MIN_GATE_OFF_FRAMES: u32 = 2;

/// Where a note's **gate** actually goes off, clamped to `[start, authored_end)`.
///
/// The authored note length ([`note_end_frame`]) is the row spacing — the frame
/// the *next* note starts. But a SID note is frequently **staccato**: the driver
/// gates it on for only a few frames, then off, letting the envelope's release
/// ring out into the gap. Holding the Pertylizer note at its sustain level for
/// the whole row loses that pluck (it survived unnoticed because low-sustain
/// notes decay anyway, and the native decode reads the authored row length, not
/// the chip's runtime gate). Clamping the note to its gate-on length lets the
/// instrument's release reproduce the pluck — a legato/held note (gate high the
/// whole row) is returned unchanged.
///
/// The native note's `start` can precede the real gate-on by a frame or two, so
/// pre-gate frames are skipped first; then the first low run of at least
/// [`MIN_GATE_OFF_FRAMES`] is the note-off. A note the trace never gates (no
/// high frame found) is returned unchanged, so unanalysable notes are never
/// shortened.
fn gate_on_end(states: &[ProgramFrame], voice_index: usize, start: u32, authored_end: u32) -> u32 {
    let gate_at = |f: u32| {
        states
            .get(f as usize)
            .is_some_and(|s| s.voices[voice_index].control.gate)
    };
    let mut gate_on = start;
    while gate_on < authored_end && !gate_at(gate_on) {
        gate_on += 1;
    }
    if gate_on >= authored_end {
        return authored_end;
    }
    let mut low_run = 0u32;
    for f in gate_on..authored_end {
        if gate_at(f) {
            low_run = 0;
        } else {
            low_run += 1;
            if low_run >= MIN_GATE_OFF_FRAMES {
                return (f + 1 - low_run).max(gate_on + 1);
            }
        }
    }
    authored_end
}

/// How many frames a note's authored `start` may precede the real gate-on before
/// the snap is abandoned. The native decode reads the authored row frame, which
/// typically leads the chip's actual gate-on by 1–2 frames (§4 export-fidelity
/// review); a longer lead means the start is not a simple pre-gate offset (it may
/// be a legato/tie or an analysis artifact), so it is trusted as authored.
const GATE_ON_LEAD_MAX: u32 = 3;

/// Where a note's **gate** actually goes on, snapped forward from its authored
/// `start`.
///
/// The authored note start ([`NoteEvent::start_frame`]) is the driver's row
/// frame, which frequently precedes the chip's real gate-on by a frame or two:
/// the driver writes the note's registers, then gates it on a tick later.
/// Starting the Pertylizer note at the authored frame attacks the envelope early,
/// so the ramp is already climbing before the gate — every native attack comes
/// out softer than the chip's, plus a small (~40 ms) onset drift. Snapping the
/// start to the first gate-high frame sharpens the attack and removes the drift.
///
/// The search is bounded to [`GATE_ON_LEAD_MAX`] frames: a note already gate-high
/// at its start (legato/held) is returned unchanged, and a note whose gate stays
/// low longer — or which the trace never gates within the window — keeps its
/// authored start, so no note is ever relocated far from where it was authored.
fn gate_on_start(
    states: &[ProgramFrame],
    voice_index: usize,
    start: u32,
    authored_end: u32,
) -> u32 {
    let gate_at = |f: u32| {
        states
            .get(f as usize)
            .is_some_and(|s| s.voices[voice_index].control.gate)
    };
    let limit = (start + GATE_ON_LEAD_MAX).min(authored_end);
    let mut f = start;
    while f < limit && !gate_at(f) {
        f += 1;
    }
    let gate_start = if f < limit { f } else { start };
    measured_onset_start(states, voice_index, gate_start, authored_end)
}

/// After the gate rises, the ADSR delay bug can hold the envelope at zero for
/// up to ~1.7 frames (the 15-bit rate counter wrapping through `0x7FFF`), and
/// the slowest attack rates take over a frame to their first step — the
/// measured audible onset is the envelope's first active frame, not the gate
/// edge. Bounds the lag search accordingly.
const ONSET_LAG_MAX: u32 = 2;

/// Advance a gate-snapped note start (bounded by [`ONSET_LAG_MAX`]) to the
/// first frame whose measured envelope is audible. Only exact digital frames
/// with a silent envelope are skipped, and only when a genuinely audible
/// frame is found inside the window — a legato/held note (envelope already
/// active) or a release tail ringing across the gate frame is returned
/// unchanged.
fn measured_onset_start(
    states: &[ProgramFrame],
    voice_index: usize,
    gate_start: u32,
    authored_end: u32,
) -> u32 {
    let silent_exact = |f: u32| {
        states.get(f as usize).is_some_and(|s| {
            s.digital_state_exact
                && s.digital_voices[voice_index]
                    .envelope_activity
                    .active_cycles
                    .0
                    == 0
        })
    };
    let limit = (gate_start + ONSET_LAG_MAX).min(authored_end);
    let mut f = gate_start;
    while f < limit && silent_exact(f) {
        f += 1;
    }
    // `f == authored_end` would move the start onto the successor's frame —
    // a fully swallowed short note keeps its gate-snapped start instead.
    let audible = f > gate_start
        && f < authored_end
        && states.get(f as usize).is_some_and(|s| {
            s.digital_voices[voice_index]
                .envelope_activity
                .active_cycles
                .0
                > 0
        });
    if audible { f } else { gate_start }
}

fn build_note(
    id: u32,
    event: &NoteEvent,
    tpr: u32,
    frame_count: u32,
    states: &[ProgramFrame],
) -> Note {
    let authored_end = authored_note_end_frame(event, frame_count);
    let start_frame = gate_on_start(
        states,
        event.voice.to_index(),
        event.start_frame.0,
        authored_end,
    );
    let end_frame = gate_on_end(states, event.voice.to_index(), start_frame, authored_end);
    let span_frames = (end_frame - start_frame).max(1);
    Note {
        id,
        start: start_frame.saturating_mul(tpr),
        duration: span_frames.saturating_mul(tpr),
        pitch: u32::from(event.midi.0),
        velocity: export_velocity(event, states),
        track: None,
        expression: None,
        glide: None,
        legato: false,
    }
}

/// The single arpeggio offset table a `plan` plays, if any. Arp segments merge
/// only when [`MergeShape::tonal_arp`] is identical, so the first non-empty loop
/// represents every note in the plan. `None` for a non-arpeggio plan.
fn arp_plan_offsets<'a>(plan: &TrackPlan<'a>) -> Option<&'a [i8]> {
    plan.segments.iter().find_map(|s| {
        s.profile
            .and_then(|p| p.arpeggio_loop.as_deref())
            .filter(|o| !o.is_empty())
    })
}

/// The forward-model spec of one arp event as the `Arpeggiator` processor would
/// render it: the held base note over its gate-snapped span (mirroring
/// [`build_note`]) with the processor stepping `offsets` from the note's onset.
/// Shared by the processor acceptance ([`arp_processor_for`]) and its tests.
fn arp_event_spec(
    event: &NoteEvent,
    offsets: &[i8],
    detune_cents: f32,
    states: &[ProgramFrame],
    timing: PlaybackTiming,
    frame_count: u32,
    voice_index: usize,
) -> forward::NoteSpec {
    let authored_end = authored_note_end_frame(event, frame_count);
    let start = gate_on_start(states, voice_index, event.start_frame.0, authored_end);
    let end = gate_on_end(states, voice_index, start, authored_end);
    forward::NoteSpec {
        start_frame: start,
        frames: (end.saturating_sub(start)).max(1),
        pitch_cents: f32::from(event.midi.0) * 100.0 + detune_cents,
        glide: None,
        vibrato: None,
        arp: Some(forward::ArpSpec {
            offsets: offsets.to_vec(),
            rate_millihz: (timing.calls_per_second() * 1000.0).round() as u32,
        }),
        track_pitch_cents: Vec::new(),
        slack_cents: 0.0,
    }
}

/// The arp processor to attach to a plan's pattern(s), or `None` to keep baking.
///
/// Acceptance is **measured** (slice 3, `docs/forward-model-gate.md`): every
/// event's predicted rendering — held base note + processor offsets restarting
/// at the note onset — must pass the forward model against the trace. That one
/// check subsumes the retired `arp_plan_clean` heuristics: a non-arp note folded
/// into the plan misses the chord tones *at the predicted times*, and a
/// sub-cycle stab (Galway's ~2-frame hits) diverges because the chip's
/// free-running phase plays tones the restarted processor never reaches. Events
/// with nothing verifiable can't disprove the processor and are skipped; a plan
/// with no verifiable event at all keeps the bake.
///
/// The accepted recipe is the SID-native `Arpeggiator` (Custom offsets,
/// `MilliHz` = resolved play-call rate, legato, gate 1.0) on one held base note
/// per event —
/// A/B vs a voice-muted reSID reference: identical to the bake within 0.03 dB
/// (bake 9.18, processor 9.21 dB on Commando), collapsing ~1600 sub-notes to
/// ~14 processors. [`SynthOptions::arpeggiator_processor`] controls the
/// explicit diagnostic bake path.
fn arp_processor_for(
    plan: &TrackPlan,
    states: &[ProgramFrame],
    timing: PlaybackTiming,
    frame_count: u32,
    enabled: bool,
) -> Option<NoteProcessor> {
    if !enabled {
        return None;
    }
    let offsets = arp_plan_offsets(plan)?;
    let detune = plan_detune_cents(plan);
    let mut verified_any = false;
    for &(_, ev) in &plan.timeline {
        let spec = arp_event_spec(
            ev,
            offsets,
            detune,
            states,
            timing,
            frame_count,
            plan.voice_index,
        );
        match forward::note_residual(&spec, plan.voice_index, timing, states) {
            Some(r) if !r.passes() => return None,
            Some(_) => verified_any = true,
            None => {}
        }
    }
    verified_any.then(|| NoteProcessor::Arpeggiator(Arpeggiator::sid_native(offsets, timing)))
}

/// Expand one held `NoteEvent` of an arpeggio patch into per-frame sub-notes —
/// the middle rung of the arp ladder (processor → this bake → pitch runs).
///
/// For each frame `f` in `[start_frame, end_frame)`, the played pitch is the
/// base MIDI note plus `offsets[(f - start_frame) % offsets.len()]`, clamped
/// to the MIDI range. Consecutive frames with the same pitch are coalesced
/// into one sub-note (`start = run_first_frame * tpr`,
/// `duration = run_len * tpr`), so the count stays bounded. Sub-notes are
/// pushed onto `notes` with ids continuing from its current length; velocity
/// matches `build_note`.
///
/// The `arpeggio_loop` is a **patch** property (one representative note's chord
/// shape), applied to every note clustered into that patch — a flat/percussion
/// note folded in by clustering would gain phantom chord steps here. The old
/// per-frame trace-span clamp that papered over that is retired: the caller
/// ([`push_arp_event`]) verifies this expansion against the trace and degrades
/// a diverging event to [`bake_pitch_runs`], which reads the chip's real
/// pitches instead of guessing which offset to pull back.
#[allow(clippy::too_many_arguments)]
fn expand_arpeggio(
    notes: &mut Vec<Note>,
    event: &NoteEvent,
    offsets: &[i8],
    tpr: u32,
    frame_count: u32,
    states: &[ProgramFrame],
) {
    let start_frame = event.start_frame.0;
    let end_frame = authored_note_end_frame(event, frame_count);
    if end_frame <= start_frame {
        // Degenerate zero-length note — fall back to a single sub-note so the
        // event is never silently dropped.
        let id = notes.len() as u32;
        notes.push(build_note(id, event, tpr, frame_count, states));
        return;
    }

    let base_midi = i16::from(event.midi.0);
    let velocity = export_velocity(event, states);
    let len = offsets.len() as u32;

    let pitch_at = |f: u32| -> u32 {
        let offset = i16::from(offsets[((f - start_frame) % len) as usize]);
        (base_midi + offset).clamp(0, 127) as u32
    };

    // The whole arpeggio rides one SID gate, so every sub-note but the FIRST
    // is a legato continuation (`legato = true`) — the engine's boundary
    // coalesce reads the *incoming* note's flag (a legato successor extends
    // the active voice cross-pitch without re-gating), killing the machine-gun
    // re-attack the per-step sub-notes would otherwise produce. The flag used
    // to sit on the predecessor (Pertylizer's `Note.legato` doc reads that
    // way) — but the engine consumes it on the successor, so every tie
    // re-gated and every seq program restarted mid-figure (2026-07-06 review
    // finding; the doc/engine mismatch is reported upstream).
    let mut run_first = start_frame;
    let mut run_pitch = pitch_at(start_frame);
    for f in (start_frame + 1)..end_frame {
        let pitch = pitch_at(f);
        if pitch != run_pitch {
            let id = notes.len() as u32;
            notes.push(arpeggio_subnote(
                id,
                run_first,
                f - run_first,
                run_pitch,
                velocity,
                tpr,
                run_first != start_frame,
            ));
            run_first = f;
            run_pitch = pitch;
        }
    }
    let id = notes.len() as u32;
    notes.push(arpeggio_subnote(
        id,
        run_first,
        end_frame - run_first,
        run_pitch,
        velocity,
        tpr,
        run_first != start_frame,
    ));
}

/// Slice-3 enforcement wrapper around [`expand_arpeggio`] (the dirty-arp
/// bake): emit the expansion, verify it against the trace, and on failure
/// replace it with [`bake_pitch_runs`] — a folded-in flat/percussion note then
/// plays the chip's real pitches instead of phantom chord tones (the retired
/// clamp's job, now measured instead of guessed). The explicit
/// [`SynthOptions::forward_gate`] diagnostic switch can retain the raw expansion
/// for A/B; an unverifiable event keeps its expansion.
#[allow(clippy::too_many_arguments)]
fn push_arp_event(
    notes: &mut Vec<Note>,
    event: &NoteEvent,
    offsets: &[i8],
    tpr: u32,
    frame_count: u32,
    states: &[ProgramFrame],
    timing: PlaybackTiming,
    voice_index: usize,
    detune_cents: f32,
    forward_gate: bool,
    fidelity: &mut forward::ResidualCensus,
) {
    let clock = timing.clock;
    let mark = notes.len();
    expand_arpeggio(notes, event, offsets, tpr, frame_count, states);
    if !forward_gate {
        return;
    }
    let specs: Vec<forward::NoteSpec> = notes[mark..]
        .iter()
        .map(|n| note_spec(n, detune_cents, None, tpr))
        .collect();
    if forward::batch_passes(&specs, voice_index, timing, states) == Some(false) {
        notes.truncate(mark);
        bake_pitch_runs(
            notes,
            event,
            detune_cents,
            tpr,
            frame_count,
            states,
            clock,
            voice_index,
        );
        fidelity.events_degraded += 1;
    }
    drop_ungated_notes(notes, mark, tpr, states, voice_index, fidelity);
}

/// One coalesced arpeggio sub-note spanning `run_len` frames from `run_first`.
/// `legato` marks a continuation of the previous sub-note (true for every step
/// but the first) — the engine coalesce reads the incoming note's flag.
#[allow(clippy::too_many_arguments)]
fn arpeggio_subnote(
    id: u32,
    run_first: u32,
    run_len: u32,
    pitch: u32,
    velocity: f32,
    tpr: u32,
    legato: bool,
) -> Note {
    Note {
        id,
        start: run_first.saturating_mul(tpr),
        duration: run_len.max(1).saturating_mul(tpr),
        pitch,
        velocity,
        track: None,
        expression: None,
        glide: None,
        legato,
    }
}

/// Frames without a new minimum frequency that end a fall's post-gate tail: once
/// the descending sweep has held its lowest register this long it has bottomed
/// out (the accumulator sits near DC), so the note stops there.
const FALL_PLATEAU_FRAMES: u32 = 8;

/// Extend a slide-through (fall) note through its post-gate tail and describe it
/// as one note gliding from the onset pitch down to where the sweep bottoms out.
///
/// A Hubbard fall keeps sliding *after* gate-off: the frequency runs on down to
/// near-DC (inaudible) while the long release fades, so the note never "hangs" on
/// the chip. Gating it only over `[start, gated_end)` and holding the gate-off
/// pitch would leave the max-release tail ringing at a clearly audible pitch, so
/// the note is extended to the frame the sweep bottoms out (`gate_off` frames with
/// a still-falling frequency, up to the next note or [`FALL_PLATEAU_FRAMES`]) and
/// glided all the way there. Returns `(end_frame, dest_pitch, glide)`, or `None`
/// when the note does not fall by an audible amount. A per-frame raw-frequency
/// lane would be more faithful, but `sid_oscillator.track_pitch` is not
/// modulatable, so a per-note glide is the workable rendering.
fn fall_extent(
    states: &[ProgramFrame],
    timing: PlaybackTiming,
    voice_index: usize,
    onset_midi: u8,
    start_frame: u32,
    gated_end: u32,
    frame_count: u32,
) -> Option<(u32, u32, Glide)> {
    let clock = timing.clock;
    let gate_off = |f: u32| {
        states
            .get(f as usize)
            .is_some_and(|s| !s.voices[voice_index].control.gate)
    };

    // `build_note`'s gated end can land on the last gated frame rather than the
    // first released one; advance to the actual gate-off before scanning the tail
    // so the post-gate fall is not skipped.
    let mut gated_end = gated_end;
    while gated_end < frame_count && !gate_off(gated_end) {
        gated_end += 1;
    }

    let mut min_freq = freq_at(states, voice_index, gated_end.saturating_sub(1))?;
    let mut min_frame = gated_end.saturating_sub(1);
    let mut since_new_min = 0u32;
    let mut f = gated_end;
    while f < frame_count && gate_off(f) && since_new_min < FALL_PLATEAU_FRAMES {
        let Some(freq) = freq_at(states, voice_index, f) else {
            break;
        };
        if freq.0 < min_freq.0 {
            min_freq = freq;
            min_frame = f;
            since_new_min = 0;
        } else {
            since_new_min += 1;
        }
        f += 1;
    }

    let (dest, _) = hertz_to_midi(min_freq.to_hertz(clock))?;
    let drop = f32::from(onset_midi) - f32::from(dest.0);
    if drop < MIN_CHIRP_SEMITONES {
        return None;
    }
    let end_frame = min_frame + 1;
    let time = (f64::from(end_frame - start_frame) * timing.seconds_per_call() * 1000.0) as f32;
    Some((
        end_frame,
        u32::from(dest.0),
        Glide {
            from: GlideFrom::Semitones(drop),
            time,
            interp: GlideInterp::Continuous,
        },
    ))
}

/// Build `event`'s note as a downward fall: repitched to where the sweep bottoms
/// out with a glide from the onset pitch, its duration extended through the
/// post-gate tail ([`fall_extent`]). `None` when the trace shows no audible
/// downward sweep. Shared by the portamento slide-through path (a fall *through*
/// a melodic note) and the percussion onset pitch-drop (the fast "pew" a tuned
/// tom/snare sweeps within the hit).
fn fall_note(
    id: u32,
    event: &NoteEvent,
    states: &[ProgramFrame],
    timing: PlaybackTiming,
    voice_index: usize,
    frame_count: u32,
    tpr: u32,
) -> Option<Note> {
    let mut note = build_note(id, event, tpr, frame_count, states);
    let start_frame = note.start / tpr;
    let gated_end = start_frame + note.duration / tpr;
    let (end_frame, dest, glide) = fall_extent(
        states,
        timing,
        voice_index,
        event.midi.0,
        start_frame,
        gated_end,
        frame_count,
    )?;
    note.duration = end_frame
        .saturating_sub(start_frame)
        .max(1)
        .saturating_mul(tpr);
    note.pitch = dest;
    note.glide = Some(glide);
    Some(note)
}

/// Build the note(s) for one held `NoteEvent` of a non-arpeggio group,
/// applying the per-note pitch effects this exporter reproduces. Effects
/// **compose** rather than excluding one another:
///
/// - **Legato split** is the base decomposition: the gate-held region is split
///   at its *sustained* pitch plateaus ([`pitch_plateaus`]) into one note per
///   pitch, tied with `legato` so the engine re-pitches without re-attacking. A
///   single sustained pitch (the common case) yields one plain note.
/// - **Vibrato** keeps the note whole: a ≤1-semitone wobble is one pitch, not a
///   legato run, so a vibrato'd note is emitted as a single note carrying a
///   [`NoteExpression`] (with any onset glide still composed in).
/// - **Portamento → glide** rides the note the slide lands on. Only a slide at
///   the note's *onset* (`span.start == note.start`) becomes a [`Glide`], since
///   the engine anchors glides at note start; and only when its destination is
///   the first sustained plateau's pitch, so a pure up-down gesture (which never
///   settles on the slide's endpoint) is not mis-pitched to its peak.
#[allow(clippy::too_many_arguments)]
fn push_expressive_notes(
    notes: &mut Vec<Note>,
    event: &NoteEvent,
    states: &[ProgramFrame],
    effects: &[EffectSpan],
    timing: PlaybackTiming,
    voice: VoiceId,
    voice_index: usize,
    frame_count: u32,
    tpr: u32,
    authored: Option<&AuthoredEffects>,
    percussion: bool,
) {
    let clock = timing.clock;
    let start = event.start_frame.0;
    let end = authored_note_end_frame(event, frame_count);

    // For melodic glides the destination lies within an octave of the gated
    // pitch; a far destination is handled separately (a far *downward* drop is a
    // drum/zap relocated to its body pitch below; anything else this far off is a
    // transient we do not chase onto the note).
    let near_gate =
        |dest: u32| (dest as i32 - i32::from(event.midi.0)).abs() <= MAX_ONSET_GLIDE_SEMITONES;

    // A glide only from a slide at the note's onset (the engine anchors glides
    // at note start; mid-note slides are left to the legato decomposition).
    let onset_glide = overlapping_span(effects, Effect::Portamento, voice, start, end)
        .filter(|span| span.start_frame.0 == start)
        .and_then(|span| glide_from_span(states, span, timing, voice_index));
    let portamento_present = onset_glide.is_some();

    // A vibrato'd note stays a single sustained note; compose any onset glide,
    // falling back to a §A8 attack chirp settling onto the played pitch when no
    // musical portamento opened the note.
    //
    // The patch's *authored* vibrato (decoded from the driver's instrument
    // table — E1) backfills notes where the per-note heuristic measured
    // nothing: the driver applies vibrato to every note of the instrument,
    // but short notes never develop a measurable span. When both candidates
    // exist, the forward model decides (slice 5): whichever predicts the
    // trace with the lower mean residual wins, ties to the measured value —
    // so once the authored `+5` high-nibble RE lands (see
    // `authored_patch_effects` in `hubbard.rs`), authored parameters win
    // automatically wherever they fit, with no precedence flag to flip.
    // Depths below audibility are dropped rather than emitted: a `+5` byte
    // whose encoding is not yet fully REd can decode to ~0 — better no
    // backfill than a wrong one.
    let authored_vibrato = authored
        .and_then(|a| a.vibrato)
        .filter(|v| v.depth_semitones >= MIN_PITCH_EFFECT_SEMITONES)
        .map(|v| Vibrato {
            depth: v.depth_semitones,
            rate: v.rate_hz,
            delay: 0.0,
            shape: VibratoShape::Triangle,
        });
    let measured_vibrato = measured_vibrato_for_event(
        event,
        states,
        effects,
        timing,
        voice,
        voice_index,
        frame_count,
    );
    let chosen_vibrato = match (measured_vibrato, authored_vibrato) {
        (Some(measured), _) if measured.delay > 0.0 => Some(measured),
        (Some(m), Some(a)) => {
            let score = |v: &Vibrato| {
                vibrato_mean_residual(event, v, states, timing, voice_index, frame_count)
            };
            match (score(&m), score(&a)) {
                (Some(rm), Some(ra)) if ra < rm => Some(a),
                _ => Some(m),
            }
        }
        (m, a) => m.or(a),
    };
    if let Some(vibrato) = chosen_vibrato {
        if vibrato.delay > 0.0 {
            push_single(notes, event, tpr, frame_count, states, None, None);
            return;
        }
        let glide = onset_glide
            .or_else(|| {
                (!portamento_present)
                    .then(|| onset_chirp(states, event, event.midi.0, timing, voice_index))
                    .flatten()
                    .map(|g| (u32::from(event.midi.0), g))
            })
            .filter(|(dest, _)| near_gate(*dest));
        push_single(notes, event, tpr, frame_count, states, glide, Some(vibrato));
        return;
    }

    let plateaus = pitch_plateaus(states, clock, voice_index, start, end);

    // Accept the onset glide only when it lands on the first sustained pitch —
    // rejecting up-down gestures that never settle on the slide's endpoint. When
    // no portamento opened the note, fall back to a §A8 attack chirp: a short
    // pitch settling onto the first sustained pitch, emitted as a fast glide.
    let first_pitch = plateaus.first().map(|p| u32::from(p.pitch));

    // A Hubbard drum/zap gates the written note then sweeps the pitch down to a
    // low body that it holds: the trace shows the gate pitch lasting a single
    // frame and the dropped body sustained for the rest of the note, so the body
    // is the faithful sustained pitch, not the gate. When a note's first sustained
    // plateau sits more than an octave below its gate pitch, emit it at the body
    // pitch carrying the fast onset pitch-drop (the "drop" transient) at its
    // natural length. Within-octave glides are melodic and settle via the path
    // below (guarded by [`near_gate`]).
    if let Some(dest) = plateau_drop_dest(first_pitch, event.midi.0) {
        let id = notes.len() as u32;
        let mut note = build_note(id, event, tpr, frame_count, states);
        note.pitch = dest;
        note.glide = onset_glide
            .filter(|(d, _)| *d == dest)
            .or_else(|| {
                onset_chirp(states, event, dest as u8, timing, voice_index).map(|g| (dest, g))
            })
            .map(|(_, g)| g);
        notes.push(note);
        return;
    }

    let glide = onset_glide
        .filter(|(dest, _)| first_pitch == Some(*dest))
        .or_else(|| {
            if portamento_present {
                return None;
            }
            let settled = plateaus.first()?.pitch;
            onset_chirp(states, event, settled, timing, voice_index)
                .map(|g| (u32::from(settled), g))
        })
        .filter(|(dest, _)| near_gate(*dest));

    // A `Portamento` span covering most of the gate-held note is a continuous
    // slide *through* it (a Hubbard frequency fall), not a legato run of stable
    // pitches. The sliding frames would otherwise fabricate spurious plateaus and
    // split the note into discrete sampled pitches — non-parallel across voices,
    // so two falling leads clash (Monty on the Run, blocks #20/#21, #26/#27).
    // Render it as one note gliding continuously from the onset (gate) pitch down
    // to where the sweep is cut at gate-off: a smooth fall, no discrete mid-sweep
    // samples to clash. A short onset slide *into* a note covers far less and
    // keeps the glide/legato handling above.
    let slide_through = overlapping_span(effects, Effect::Portamento, voice, start, end)
        .is_some_and(|span| {
            let span_len = span.end_frame.0.saturating_sub(span.start_frame.0);
            span_len.saturating_mul(2) >= end.saturating_sub(start)
        });
    if slide_through {
        let id = notes.len() as u32;
        let note = fall_note(id, event, states, timing, voice_index, frame_count, tpr)
            .unwrap_or_else(|| build_note(id, event, tpr, frame_count, states));
        notes.push(note);
        return;
    }

    // Percussion hits (snare/tom) carry a fast onset pitch-drop the chip sweeps
    // within the gated note — the tom "pew". The plateau logic misses it (the
    // onset holds a few frames before dropping, so the held pitch reads as the
    // note), so render it as the same fall glide when the trace shows the drop.
    if percussion
        && let Some(note) = fall_note(
            notes.len() as u32,
            event,
            states,
            timing,
            voice_index,
            frame_count,
            tpr,
        )
    {
        notes.push(note);
        return;
    }

    if plateaus.len() <= 1 {
        push_single(notes, event, tpr, frame_count, states, glide, None);
        return;
    }

    // Legato: one tied note per sustained plateau; the onset glide rides the
    // first note (whose pitch is the slide destination it settles on).
    let velocity = export_velocity(event, states);
    let mut glide = glide;
    for (i, plateau) in plateaus.iter().enumerate() {
        let id = notes.len() as u32;
        let mut pitch = u32::from(plateau.pitch);
        let glide_field = if i == 0 {
            glide.take().map(|(dest, g)| {
                pitch = dest;
                g
            })
        } else {
            None
        };
        notes.push(Note {
            id,
            start: plateau.start.saturating_mul(tpr),
            duration: plateau.len.max(1).saturating_mul(tpr),
            pitch,
            velocity,
            track: None,
            expression: None,
            glide: glide_field,
            legato: i > 0,
        });
    }
}

/// Emit one plain [`Note`] for `event`, composing an optional onset `glide`
/// (which repitches the note to the slide destination) and optional `vibrato`.
fn push_single(
    notes: &mut Vec<Note>,
    event: &NoteEvent,
    tpr: u32,
    frame_count: u32,
    states: &[ProgramFrame],
    glide: Option<(u32, Glide)>,
    vibrato: Option<Vibrato>,
) {
    let id = notes.len() as u32;
    let mut note = build_note(id, event, tpr, frame_count, states);
    if let Some((dest, g)) = glide {
        note.pitch = dest;
        note.glide = Some(g);
    }
    if let Some(vibrato) = vibrato {
        note.expression = Some(NoteExpression {
            vibrato: Some(vibrato),
        });
    }
    notes.push(note);
}

/// The forward-model degradation ladder's bottom rung (slice 2,
/// `docs/forward-model-gate.md`): re-emit one event as per-frame pitch runs
/// read straight off the trace — one legato-tied [`Note`] per run of equal
/// nearest-MIDI pitch over the event's gated span. Always trace-faithful by
/// construction (each run's pitch *is* the chip's, within rounding and the
/// instrument's `detune_cents`, which is subtracted so the rendered
/// pitch+detune lands back on the trace). Noise/silent frames extend the
/// current run — they carry no pitch to contradict, and the chip's 1-frame
/// noise interleaves (Hubbard stab attacks) must not chop the melody. The
/// span extends through a *moving release tail* (slice 7,
/// [`forward::release_tail_end`]): a driver retuning the register while the
/// envelope rings (a descending stab figure, a fall running on) is playing
/// content the bake must render, so the tail's pitches become runs too.
/// Falls back to a plain [`build_note`] when the span never carries a pitch.
#[allow(clippy::too_many_arguments)]
fn bake_pitch_runs(
    notes: &mut Vec<Note>,
    event: &NoteEvent,
    detune_cents: f32,
    tpr: u32,
    frame_count: u32,
    states: &[ProgramFrame],
    clock: SystemClock,
    voice_index: usize,
) {
    let authored_end = authored_note_end_frame(event, frame_count);
    let start = gate_on_start(states, voice_index, event.start_frame.0, authored_end);
    let end = gate_on_end(states, voice_index, start, authored_end);
    let end = end.max(forward::release_tail_end(states, voice_index, end));
    let velocity = export_velocity(event, states);

    // (pitch, run_start_frame, run_len)
    let mut runs: Vec<(u32, u32, u32)> = Vec::new();
    for f in start..end.max(start + 1) {
        let pitch = states.get(f as usize).and_then(|s| {
            let v = &s.voices[voice_index];
            if v.control.waveform.is_noise_only() || v.freq.0 == 0 {
                return None;
            }
            let (midi, cents) = hertz_to_midi(v.freq.to_hertz(clock))?;
            let fine = f32::from(midi.0) * 100.0 + cents.0 - detune_cents;
            Some(((fine / 100.0).round().clamp(0.0, 127.0)) as u32)
        });
        match (pitch, runs.last_mut()) {
            (Some(p), Some(run)) if run.0 == p => run.2 += 1,
            (Some(p), _) => runs.push((p, f, 1)),
            // Unpitched frame: hold the running note rather than splitting.
            (None, Some(run)) => run.2 += 1,
            (None, None) => {}
        }
    }
    if runs.is_empty() {
        let id = notes.len() as u32;
        notes.push(build_note(id, event, tpr, frame_count, states));
        return;
    }
    for (i, (pitch, run_start, len)) in runs.into_iter().enumerate() {
        notes.push(Note {
            id: notes.len() as u32,
            start: run_start.saturating_mul(tpr),
            duration: len.max(1).saturating_mul(tpr),
            pitch,
            velocity,
            track: None,
            expression: None,
            glide: None,
            legato: i > 0,
        });
    }
}

/// Enforcement wrapper around [`push_expressive_notes`] (slices 2 & 4): emit
/// the heuristic proposal, verify it against the trace with the forward model
/// ([`forward::batch_passes`] over every emitted event note), and on failure replace
/// it with the [`bake_pitch_runs`] bottom rung. Every event is enforced —
/// melodic, `percussion` (the tuned-tom fall path), and drum-drop bodies —
/// because verification is pitch-only: a faithful tom fall or drop body
/// passes (its glide/body comes from the same trace) while a mis-decomposed
/// one degrades to the chip's real pitches. [`SynthOptions::forward_gate`] is
/// the explicit A/B switch. An unverifiable event keeps its proposal — there is no
/// evidence against it.
#[allow(clippy::too_many_arguments)]
fn push_melodic_event(
    notes: &mut Vec<Note>,
    event: &NoteEvent,
    states: &[ProgramFrame],
    effects: &[EffectSpan],
    timing: PlaybackTiming,
    voice: VoiceId,
    voice_index: usize,
    frame_count: u32,
    tpr: u32,
    authored: Option<&AuthoredEffects>,
    percussion: bool,
    detune_cents: f32,
    forward_gate: bool,
    fidelity: &mut forward::ResidualCensus,
    ownership_end: Option<FrameIndex>,
) {
    let clock = timing.clock;
    let mark = notes.len();
    push_expressive_notes(
        notes,
        event,
        states,
        effects,
        timing,
        voice,
        voice_index,
        frame_count,
        tpr,
        authored,
        percussion,
    );
    if !percussion {
        extend_release_tail(notes, mark, event, states, tpr, ownership_end);
    }
    if !forward_gate {
        return;
    }
    let specs: Vec<forward::NoteSpec> = notes[mark..]
        .iter()
        .map(|n| note_spec(n, detune_cents, None, tpr))
        .collect();
    if forward::batch_passes(&specs, voice_index, timing, states) == Some(false) {
        notes.truncate(mark);
        bake_pitch_runs(
            notes,
            event,
            detune_cents,
            tpr,
            frame_count,
            states,
            clock,
            voice_index,
        );
        fidelity.events_degraded += 1;
    }
    drop_ungated_notes(notes, mark, tpr, states, voice_index, fidelity);
}

fn extend_release_tail(
    notes: &mut [Note],
    mark: usize,
    event: &NoteEvent,
    states: &[ProgramFrame],
    tpr: u32,
    ownership_end: Option<FrameIndex>,
) {
    let Some(mut sound_end) = event.sound_end_frame(states) else {
        return;
    };
    if let Some(end) = ownership_end {
        sound_end = FrameIndex(sound_end.0.min(end.0));
    }
    if !articulation_is_automatable(event, states) || !release_waveform_is_stable(event, states) {
        return;
    }
    let Some(note) = notes.get_mut(mark..).and_then(|slice| slice.last_mut()) else {
        return;
    };
    let end_tick = sound_end.0.saturating_mul(tpr);
    note.duration = end_tick.saturating_sub(note.start).max(note.duration);
}

fn release_waveform_is_stable(event: &NoteEvent, states: &[ProgramFrame]) -> bool {
    let Some(release) = event.release_frame() else {
        return true;
    };
    let Some(sound_end) = event.sound_end_frame(states) else {
        return false;
    };
    let voice = event.voice.to_index();
    let mut waveforms = states
        .get(release.0 as usize..sound_end.0 as usize)
        .unwrap_or(&[])
        .iter()
        .map(|state| state.voices[voice].control.waveform);
    let Some(first) = waveforms.next() else {
        return true;
    };
    waveforms.all(|waveform| waveform == first)
}

/// Mean pitch residual (cents) of one vibrato candidate over the event's
/// gate-snapped span — the slice-5 scoring function for authored-vs-measured
/// vibrato precedence. Raw mean of |predicted − trace| (no slack: slack is
/// for pass/fail forgiveness, not for ranking two candidates against the
/// same trace). Instrument detune shifts both candidates equally, so it is
/// omitted. `None` when nothing is verifiable.
fn vibrato_mean_residual(
    event: &NoteEvent,
    vibrato: &Vibrato,
    states: &[ProgramFrame],
    timing: PlaybackTiming,
    voice_index: usize,
    frame_count: u32,
) -> Option<f32> {
    let authored_end = authored_note_end_frame(event, frame_count);
    let start = gate_on_start(states, voice_index, event.start_frame.0, authored_end);
    let end = gate_on_end(states, voice_index, start, authored_end);
    let spec = forward::NoteSpec {
        start_frame: start,
        frames: (end.saturating_sub(start)).max(1),
        pitch_cents: f32::from(event.midi.0) * 100.0,
        glide: None,
        vibrato: Some(forward::VibratoSpec {
            depth_cents: vibrato.depth * 100.0,
            rate_hz: vibrato.rate,
            delay_ms: vibrato.delay,
            triangle: matches!(vibrato.shape, VibratoShape::Triangle),
        }),
        arp: None,
        track_pitch_cents: Vec::new(),
        slack_cents: 0.0,
    };
    forward::note_residual(&spec, voice_index, timing, states).map(|r| r.mean_cents)
}

/// Remove notes whose whole span has neither a gate nor audible envelope
/// activity. Survivors are re-numbered and a dangling legato tie onto a
/// removed successor is cleared.
fn drop_ungated_notes(
    notes: &mut Vec<Note>,
    mark: usize,
    tpr: u32,
    states: &[ProgramFrame],
    voice_index: usize,
    fidelity: &mut forward::ResidualCensus,
) {
    let tpr = tpr.max(1);
    let tail = notes.split_off(mark);
    let before = tail.len();
    for mut n in tail {
        let f = n.start / tpr;
        let d = (n.duration / tpr).max(1);
        if span_has_signal(states, voice_index, f, f + d) {
            n.id = notes.len() as u32;
            notes.push(n);
        }
    }
    if notes.len() - mark < before {
        fidelity.events_silent += 1;
        if notes.len() > mark
            && let Some(last) = notes.last_mut()
        {
            last.legato = false;
        }
    }
}

/// Whether `[start, end)` contains a gated or envelope-active interval.
fn span_has_signal(states: &[ProgramFrame], voice_index: usize, start: u32, end: u32) -> bool {
    let end = end.min(states.len() as u32);
    (start..end.max(start)).any(|f| {
        states.get(f as usize).is_some_and(|s| {
            s.voices[voice_index].control.gate
                || s.digital_voices[voice_index].envelope.level.0 > 0
                || s.digital_voices[voice_index]
                    .envelope_activity
                    .active_cycles
                    .0
                    > 0
        })
    })
}

/// First `EffectSpan` of `effect` on `voice` whose inclusive frame range
/// overlaps the half-open note range `[start, end)`.
fn overlapping_span(
    effects: &[EffectSpan],
    effect: Effect,
    voice: VoiceId,
    start: u32,
    end: u32,
) -> Option<&EffectSpan> {
    effects.iter().find(|s| {
        s.effect == effect
            && s.voice == Some(voice)
            && s.start_frame.0 < end
            && s.end_frame.0 >= start
    })
}

/// Raw per-voice frequency at `frame`, or `None` when out of range or silent.
fn freq_at(states: &[ProgramFrame], voice_index: usize, frame: u32) -> Option<SidFreq> {
    let f = states.get(frame as usize)?.voices[voice_index].freq;
    (f.0 != 0).then_some(f)
}

#[allow(clippy::too_many_arguments)]
fn measured_vibrato_for_event(
    event: &NoteEvent,
    states: &[ProgramFrame],
    effects: &[EffectSpan],
    timing: PlaybackTiming,
    voice: VoiceId,
    voice_index: usize,
    frame_count: u32,
) -> Option<Vibrato> {
    let start = event.start_frame.0;
    let end = authored_note_end_frame(event, frame_count);
    overlapping_span(effects, Effect::Vibrato, voice, start, end)
        .and_then(|span| vibrato_from_span(states, span, timing, voice_index, event.start_frame))
}

/// Derive a [`Vibrato`] from a `Vibrato` span. The span is measured once in
/// `analysis::effects` ([`measure_vibrato`]); here that measurement is mapped
/// to Pertylizer units: `depth` is half the peak-to-peak excursion in semitones
/// (the peak deviation from centre), and `rate` is the LFO frequency in Hz from
/// the reversal count (two per cycle) over the span's duration. Returns `None`
/// for a degenerate span (no audible depth or no resolvable rate).
fn vibrato_from_span(
    states: &[ProgramFrame],
    span: &EffectSpan,
    timing: PlaybackTiming,
    voice_index: usize,
    note_start: FrameIndex,
) -> Option<Vibrato> {
    let m = measure_vibrato(states, span, voice_index, note_start)?;

    let depth = m.excursion.to_semitones() / 2.0;
    if depth < MIN_PITCH_EFFECT_SEMITONES {
        return None;
    }

    let duration_secs = f64::from(m.frames) * timing.seconds_per_call();
    let cycles = f64::from(m.reversals) / 2.0;
    let rate = if duration_secs > 0.0 {
        (cycles / duration_secs) as f32
    } else {
        0.0
    };
    if rate <= 0.0 {
        return None;
    }

    Some(Vibrato {
        depth,
        rate,
        delay: (f64::from(m.delay_frames) * timing.seconds_per_call() * 1000.0) as f32,
        shape: match m.shape {
            crate::analysis::effects::VibratoContour::Sine => VibratoShape::Sine,
            crate::analysis::effects::VibratoContour::Triangle => VibratoShape::Triangle,
            crate::analysis::effects::VibratoContour::Square => VibratoShape::Square,
            crate::analysis::effects::VibratoContour::Saw => VibratoShape::Saw,
        },
    })
}

/// Derive a destination pitch + [`Glide`] from a `Portamento` span. The span is
/// measured once in `analysis::effects` ([`measure_slide`]); here the settled
/// destination frequency is quantised to a MIDI note (the note is repitched
/// there), the signed origin interval becomes `glide.from`, and the frame count
/// becomes `time` in ms. Returns `None` when an endpoint is silent, the
/// destination is unpitchable, or the slide is inaudibly small.
fn glide_from_span(
    states: &[ProgramFrame],
    span: &EffectSpan,
    timing: PlaybackTiming,
    voice_index: usize,
) -> Option<(u32, Glide)> {
    let m = measure_slide(states, span, voice_index)?;
    let (dest, _) = hertz_to_midi(m.dest.to_hertz(timing.clock))?;

    let from_offset = m.from.to_semitones();
    if from_offset.abs() < MIN_PITCH_EFFECT_SEMITONES {
        return None;
    }
    let time = (f64::from(m.frames) * timing.seconds_per_call() * 1000.0) as f32;

    Some((
        u32::from(dest.0),
        Glide {
            from: GlideFrom::Semitones(from_offset),
            time,
            interp: GlideInterp::Continuous,
        },
    ))
}

/// Fractional MIDI pitch of a raw SID frequency (continuous, not rounded to a
/// semitone). `None` for a silent / non-positive frequency.
fn fine_pitch(freq: SidFreq, clock: SystemClock) -> Option<f32> {
    let hz = freq.to_hertz(clock).0;
    (hz > 0.0).then(|| (69.0 + 12.0 * (hz / 440.0).log2()) as f32)
}

/// Detect a §A8 onset pitch chirp — a fast settling transient at the note's
/// start (a few frames sweeping onto the played pitch, e.g. a Hubbard lead's
/// percussive "pluck"). Distinct from a musical portamento *between* notes:
/// the chirp settles onto the note's *own* pitch within a handful of frames.
///
/// Returns a fast [`Glide`] from the initial pitch offset (signed semitones
/// relative to `settled_midi`) over the chirp's duration in ms. Returns `None`
/// when the note opens already at pitch (offset below [`MIN_CHIRP_SEMITONES`])
/// or when the pitch does not settle to within [`CHIRP_SETTLE_SEMITONES`] of
/// `settled_midi` inside [`MAX_CHIRP_FRAMES`] (a longer movement is a slide,
/// left to the portamento / legato paths).
fn onset_chirp(
    states: &[ProgramFrame],
    event: &NoteEvent,
    settled_midi: u8,
    timing: PlaybackTiming,
    voice_index: usize,
) -> Option<Glide> {
    let clock = timing.clock;
    let start = event.start_frame.0;
    let settled = f32::from(settled_midi);
    let initial = fine_pitch(freq_at(states, voice_index, start)?, clock)?;
    let from_offset = initial - settled;
    if from_offset.abs() < MIN_CHIRP_SEMITONES {
        return None;
    }

    // Walk forward to the first frame whose pitch has arrived at the settled
    // note; that frame ends the chirp window. Keep scanning within this note past
    // it: a real chirp *holds* the settled pitch, so if the pitch swings back to
    // the onset pitch it is a transient dip-and-return (a hard-restart attack
    // blip), not a settle — reject it so the note keeps its own onset pitch
    // (Nemesis st1 V1 bar 25: an F5 dipping to D#5 for 2 frames then back to F5).
    let note_end = event
        .end_frame
        .map_or(start + MAX_CHIRP_FRAMES + 1, |e| e.0);
    let mut chirp_frames = None;
    for k in 1..=MAX_CHIRP_FRAMES {
        if start + k >= note_end {
            break;
        }
        let Some(p) = freq_at(states, voice_index, start + k).and_then(|f| fine_pitch(f, clock))
        else {
            continue;
        };
        if chirp_frames.is_none() {
            if (p - settled).abs() <= CHIRP_SETTLE_SEMITONES {
                chirp_frames = Some(k);
            }
        } else if (p - initial).abs() <= CHIRP_SETTLE_SEMITONES {
            return None;
        }
    }
    let frames = chirp_frames?;
    let time = (f64::from(frames) * timing.seconds_per_call() * 1000.0) as f32;

    Some(Glide {
        from: GlideFrom::Semitones(from_offset),
        time,
        interp: GlideInterp::Continuous,
    })
}

/// A sustained-pitch run within one gate-held note: the editor-frame `start`,
/// its length in frames, and the MIDI pitch held.
struct Plateau {
    start: u32,
    len: u32,
    pitch: u8,
}

/// Split the gate-held frame range `[start, end)` into the sustained MIDI-pitch
/// plateaus a SID legato run steps through. Each frame's frequency maps to a
/// MIDI note (silent/out-of-range frames carry the previous pitch forward) and
/// consecutive equal-pitch frames coalesce into runs.
///
/// Only a run that *itself* holds for at least [`MIN_LEGATO_FRAMES`] is an
/// **anchor** — a genuinely sustained pitch. Transient runs (the changing-pitch
/// frames of a slide) are never anchors and never accumulate into one: the
/// surviving notes tile `[start, end)` with one note per anchor, each running to
/// the next anchor's start, so transient frames fold into the adjacent sustained
/// note rather than fabricating a plateau of their own.
///
/// Fewer than two anchors → a single plateau spanning the whole range (the
/// caller emits one plain note), so a steady note or a pure slide is never
/// split. Two or more anchors → one plateau per anchor (a true legato run).
/// Returns empty only for an empty range.
fn pitch_plateaus(
    states: &[ProgramFrame],
    clock: SystemClock,
    voice_index: usize,
    start: u32,
    end: u32,
) -> Vec<Plateau> {
    let mut runs: Vec<Plateau> = Vec::new();
    let mut carried: Option<u8> = None;
    for frame in start..end {
        let pitch = freq_at(states, voice_index, frame)
            .and_then(|f| hertz_to_midi(f.to_hertz(clock)))
            .map(|(m, _)| m.0)
            .or(carried);
        let Some(pitch) = pitch else {
            continue;
        };
        carried = Some(pitch);
        match runs.last_mut() {
            Some(run) if run.pitch == pitch => run.len += 1,
            _ => runs.push(Plateau {
                start: frame,
                len: 1,
                pitch,
            }),
        }
    }
    if runs.is_empty() {
        return Vec::new();
    }

    // Anchors = runs that sustain on their own; transient runs are not anchors.
    let anchors: Vec<usize> = runs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.len >= MIN_LEGATO_FRAMES)
        .map(|(i, _)| i)
        .collect();

    // Not a legato run: one note over the whole range at the longest-held pitch.
    // (The caller treats a `len <= 1` result as the plain single-note case.)
    if anchors.len() < 2 {
        let pitch = runs
            .iter()
            .max_by_key(|r| r.len)
            .map_or(runs[0].pitch, |r| r.pitch);
        return vec![Plateau {
            start,
            len: end - start,
            pitch,
        }];
    }

    // Tile [start, end): each anchor's note runs to the next anchor's start, so
    // leading/intervening transient (slide) frames fold into the sustained note
    // that precedes them; the last note extends to `end`.
    anchors
        .iter()
        .enumerate()
        .map(|(k, &ai)| {
            let note_start = if k == 0 { start } else { runs[ai].start };
            let note_end = anchors.get(k + 1).map_or(end, |&next| runs[next].start);
            Plateau {
                start: note_start,
                len: note_end - note_start,
                pitch: runs[ai].pitch,
            }
        })
        .collect()
}

/// Distinct per-voice editor colour (voice 1 warm, 2 cool, 3 green).
fn track_color(voice_index: usize) -> TrackColor {
    match voice_index {
        0 => TrackColor {
            r: 255,
            g: 180,
            b: 90,
        },
        1 => TrackColor {
            r: 90,
            g: 180,
            b: 255,
        },
        _ => TrackColor {
            r: 150,
            g: 220,
            b: 120,
        },
    }
}

/// PSID header strings are fixed-width and may be empty or whitespace; fall
/// back to a sensible default when blank.
fn clean(s: &str, default: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    }
}

// ---------------------------------------------------------------------------
// Serde shapes — mirror `SID SynthSource v0 Test.json` exactly. Every parameter
// key is emitted (defaults included), matching the gold file's policy.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ProjectFile {
    file_type: &'static str,
    version: &'static str,
    instruments: Vec<Instrument>,
    active_instrument_id: u32,
    song: Song,
    global: GlobalProjectState,
}

#[derive(Serialize)]
struct GlobalProjectState {
    master_volume: f32,
    octave_offset: i32,
    glide_time: f32,
    /// The §A9 master-bus coloring chain ([`master_chain`]), applied to the full
    /// mix in order.
    master_effects: Vec<Module>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    return_bus_effects: Vec<ReturnBusEffectsState>,
}

impl Default for GlobalProjectState {
    fn default() -> Self {
        Self {
            master_volume: MASTER_VOLUME,
            octave_offset: 0,
            glide_time: 0.0,
            master_effects: master_chain(),
            return_bus_effects: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct Instrument {
    id: u32,
    name: String,
    channel: u32,
    volume: f32,
    pan: f32,
    muted: bool,
    solo: bool,
    key_range: [u32; 2],
    transpose: f32,
    oversampling: u32,
    category: u32,
    description: String,
    allocation_mode: &'static str,
    stealing_strategy: &'static str,
    max_voices: u32,
    velocity_amp_sensitivity: f32,
    velocity_filter_sensitivity: f32,
    patch: PatchBlock,
}

#[derive(Serialize)]
struct PatchBlock {
    name: String,
    version: &'static str,
    description: &'static str,
    modules: Vec<Module>,
    connections: Vec<Connection>,
    settings: PatchSettings,
}

#[derive(Serialize)]
struct PatchSettings {
    master_volume: f32,
    bpm: f32,
    octave_offset: i32,
    glide_time: f32,
    canvas_size: CanvasSize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    effect_chain_order: Vec<&'static str>,
}

impl Default for PatchSettings {
    fn default() -> Self {
        Self {
            master_volume: 0.8,
            bpm: 125.0,
            octave_offset: 0,
            glide_time: 0.0,
            canvas_size: CanvasSize {
                width: 1024.0,
                height: 512.0,
            },
            effect_chain_order: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct CanvasSize {
    width: f32,
    height: f32,
}

#[derive(Serialize)]
struct Module {
    id: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    position: Position,
    parameters: Parameters,
    /// Per-slot YAMS control scripts, keyed by 1-based slot number — the
    /// project schema's `ModuleState.scripts` field (source text compiled on
    /// load). `None` for the ordinary parameter-only modules, `Some` on a
    /// `script` module carrying a driver program (rollout E3: program replay
    /// instead of baked per-frame automation).
    #[serde(skip_serializing_if = "Option::is_none")]
    scripts: Option<BTreeMap<&'static str, String>>,
}

#[derive(Serialize)]
struct Position {
    x: f32,
    y: f32,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Parameters {
    SidOscillator(Box<SidOscillatorParams>),
    Envelope(EnvelopeParams),
    Amplifier(AmplifierParams),
    StereoOutput(StereoOutputParams),
    Filter(FilterParams),
    Distortion(DistortionParams),
    Eq(EqParams),
    Limiter(LimiterParams),
    Mixer(MixerParams),
    Script(ScriptParams),
    Oscillator(modern::OscillatorParams),
    WavetableOsc(modern::WavetableOscParams),
    SubOscillator(modern::SubOscillatorParams),
    Noise(modern::NoiseParams),
    Chorus(modern::ChorusParams),
    EnsembleChorus(modern::EnsembleChorusParams),
    Delay(modern::DelayParams),
    Reverb(modern::ReverbParams),
    Compressor(modern::CompressorParams),
}

/// The `script` module has no descriptor parameters — its behaviour is the
/// YAMS source carried on [`Module::scripts`] — so its `parameters` object is
/// empty.
#[derive(Serialize)]
struct ScriptParams {}

/// Parameters for Pertylizer's native `sid_oscillator` module (schema ids from
/// `docs/pertylizer/descriptors.json`). Waveform bits and other booleans are
/// serialized as `0.0`/`1.0` (the schema accepts number-or-boolean); `model`,
/// `clock` and `quality` are the descriptor choice ids. `pw_reg`/`freq_reg`
/// carry raw SID register values — no normalization.
#[derive(Serialize)]
struct SidOscillatorParams {
    triangle: f32,
    sawtooth: f32,
    pulse: f32,
    noise: f32,
    noise_seed: f32,
    freq_reg: f32,
    track_pitch: f32,
    pw_reg: f32,
    test: f32,
    ring_mod: f32,
    hard_sync: f32,
    model: &'static str,
    clock: &'static str,
    quality: &'static str,
    level: f32,
    seq_len: f32,
    seq_rate: f32,
    seq_loop: f32,
    seq_freq_mask: f32,
    seq_step_0: f32,
    seq_step_1: f32,
    seq_step_2: f32,
    seq_step_3: f32,
    seq_step_4: f32,
    seq_step_5: f32,
    seq_step_6: f32,
    seq_step_7: f32,
    seq_step_8: f32,
    seq_step_9: f32,
    seq_step_10: f32,
    seq_step_11: f32,
    seq_step_12: f32,
    seq_step_13: f32,
    seq_step_14: f32,
    seq_step_15: f32,
    seq_step_freq_0: f32,
    seq_step_freq_1: f32,
    seq_step_freq_2: f32,
    seq_step_freq_3: f32,
    seq_step_freq_4: f32,
    seq_step_freq_5: f32,
    seq_step_freq_6: f32,
    seq_step_freq_7: f32,
    seq_step_freq_8: f32,
    seq_step_freq_9: f32,
    seq_step_freq_10: f32,
    seq_step_freq_11: f32,
    seq_step_freq_12: f32,
    seq_step_freq_13: f32,
    seq_step_freq_14: f32,
    seq_step_freq_15: f32,
}

impl SidOscillatorParams {
    /// A note-tracking `sid_oscillator` with the given waveform mask (SID
    /// control-register bits 4..=7 shifted down: bit 0 = triangle, 1 = saw,
    /// 2 = pulse, 3 = noise), pulse-width register, chip model and clock.
    /// Quality is always `fast` — measured closer to reSID than the 4×
    /// oversampled path on clean waveforms, and combined/noise masks
    /// auto-oversample regardless (PoC matrix, sound-engine plan).
    fn tonal(mask: u8, pw_reg: PulseWidth, model: SidModel, clock: SystemClock) -> Self {
        Self {
            triangle: f32::from(mask & 0x1 != 0),
            sawtooth: f32::from(mask & 0x2 != 0),
            pulse: f32::from(mask & 0x4 != 0),
            noise: f32::from(mask & 0x8 != 0),
            noise_seed: SID_NOISE_SEED as f32,
            freq_reg: 0.0,
            track_pitch: 1.0,
            pw_reg: f32::from(pw_reg.0 & 0x0FFF),
            test: 0.0,
            ring_mod: 0.0,
            hard_sync: 0.0,
            // Unknown/Both → 6581: sidplayfp's default for an unflagged header,
            // pinned empirically against the PoC fixtures (mk_sid.py note).
            model: match model {
                SidModel::Mos8580 => "8580",
                _ => "6581",
            },
            clock: match clock {
                SystemClock::Pal => "pal",
                SystemClock::Ntsc => "ntsc",
            },
            quality: "fast",
            level: 1.0,
            seq_len: 0.0,
            seq_rate: 1.0,
            seq_loop: 0.0,
            seq_freq_mask: 0.0,
            seq_step_0: 0.0,
            seq_step_1: 0.0,
            seq_step_2: 0.0,
            seq_step_3: 0.0,
            seq_step_4: 0.0,
            seq_step_5: 0.0,
            seq_step_6: 0.0,
            seq_step_7: 0.0,
            seq_step_8: 0.0,
            seq_step_9: 0.0,
            seq_step_10: 0.0,
            seq_step_11: 0.0,
            seq_step_12: 0.0,
            seq_step_13: 0.0,
            seq_step_14: 0.0,
            seq_step_15: 0.0,
            seq_step_freq_0: 0.0,
            seq_step_freq_1: 0.0,
            seq_step_freq_2: 0.0,
            seq_step_freq_3: 0.0,
            seq_step_freq_4: 0.0,
            seq_step_freq_5: 0.0,
            seq_step_freq_6: 0.0,
            seq_step_freq_7: 0.0,
            seq_step_freq_8: 0.0,
            seq_step_freq_9: 0.0,
            seq_step_freq_10: 0.0,
            seq_step_freq_11: 0.0,
            seq_step_freq_12: 0.0,
            seq_step_freq_13: 0.0,
            seq_step_freq_14: 0.0,
            seq_step_freq_15: 0.0,
        }
    }

    /// Program the per-frame waveform-mask sequence: one mask per driver frame
    /// (`seq_rate` 1), truncated at the module's 16 steps, and **looped for the
    /// whole note** (`seq_loop` on). Every current caller is a repeating
    /// waveform alternation (the Hubbard tri↔noise lead); a one-shot
    /// drum-attack program would clear `seq_loop` so the module holds the last
    /// step instead.
    fn set_seq(&mut self, steps: &[u8]) {
        let mut masks = [0.0f32; 16];
        for (slot, &step) in masks.iter_mut().zip(steps.iter()) {
            *slot = f32::from(step & 0x0F);
        }
        self.seq_len = steps.len().min(16) as f32;
        self.seq_rate = 1.0;
        self.seq_loop = 1.0;
        [
            self.seq_step_0,
            self.seq_step_1,
            self.seq_step_2,
            self.seq_step_3,
            self.seq_step_4,
            self.seq_step_5,
            self.seq_step_6,
            self.seq_step_7,
            self.seq_step_8,
            self.seq_step_9,
            self.seq_step_10,
            self.seq_step_11,
            self.seq_step_12,
            self.seq_step_13,
            self.seq_step_14,
            self.seq_step_15,
        ] = masks;
    }

    fn set_seq_frequencies(&mut self, frequencies: &[u16; 16]) {
        let mut mask = 0u16;
        for (step, &frequency) in frequencies.iter().enumerate() {
            if frequency > 0 {
                mask |= 1u16 << step;
            }
        }
        self.seq_freq_mask = f32::from(mask);
        [
            self.seq_step_freq_0,
            self.seq_step_freq_1,
            self.seq_step_freq_2,
            self.seq_step_freq_3,
            self.seq_step_freq_4,
            self.seq_step_freq_5,
            self.seq_step_freq_6,
            self.seq_step_freq_7,
            self.seq_step_freq_8,
            self.seq_step_freq_9,
            self.seq_step_freq_10,
            self.seq_step_freq_11,
            self.seq_step_freq_12,
            self.seq_step_freq_13,
            self.seq_step_freq_14,
            self.seq_step_freq_15,
        ] = frequencies.map(f32::from);
    }
}

#[derive(Serialize)]
struct FilterParams {
    cutoff: f32,
    cv_amt: f32,
    drive: f32,
    env_amt: f32,
    key_track: f32,
    model: &'static str,
    morph: f32,
    resonance: f32,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct DistortionParams {
    bit_depth: f32,
    drive: f32,
    mix: f32,
    tone: f32,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct EqParams {
    high_freq: f32,
    high_gain: f32,
    low_freq: f32,
    low_gain: f32,
    mid_freq: f32,
    mid_gain: f32,
    mid_q: f32,
    mix: f32,
}

#[derive(Serialize)]
struct LimiterParams {
    ceiling: f32,
    look_ahead: f32,
    release: f32,
    mix: f32,
}

#[derive(Serialize)]
struct EnvelopeParams {
    atk_curve: f32,
    attack: f32,
    dec_curve: f32,
    decay: f32,
    rel_curve: f32,
    release: f32,
    sustain: f32,
    vel_sens: f32,
}

#[derive(Serialize)]
struct AmplifierParams {
    cv_bipolar: f32,
    level: f32,
    pan: f32,
}

#[derive(Serialize)]
struct MixerParams {
    input_1: f32,
    input_2: f32,
    input_3: f32,
    input_4: f32,
    input_5: f32,
    input_6: f32,
    input_7: f32,
    input_8: f32,
    master: f32,
}

#[derive(Serialize)]
struct StereoOutputParams {
    dither: f32,
    limit: f32,
    master: f32,
    mute: f32,
    pan: f32,
}

#[derive(Serialize)]
struct Connection {
    from: [&'static str; 2],
    to: [&'static str; 2],
}

#[derive(Serialize)]
struct Song {
    name: String,
    author: String,
    patterns: Vec<Pattern>,
    next_pattern_id: u32,
    tracks: Vec<Track>,
    next_track_id: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    return_busses: Vec<ReturnBus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_return_bus_id: Option<u16>,
    arrangement: Vec<PatternPlacement>,
    note_graphs: Vec<NoteGraph>,
    next_note_graph_id: u32,
    tempo_changes: Vec<()>,
    time_signature_changes: Vec<()>,
    default_tempo: f32,
    default_time_signature: TimeSignature,
    row_resolution: RowResolution,
}

#[derive(Serialize)]
struct TimeSignature {
    numerator: u32,
    denominator: u32,
}

#[derive(Serialize)]
struct RowResolution {
    rows: u32,
    ticks_per_row: u32,
}

#[derive(Serialize)]
struct Pattern {
    id: u32,
    name: String,
    length: u32,
    notes: Vec<Note>,
    automation: Vec<AutomationLane>,
    next_note_id: u32,
    /// Pattern note-processors (schema chain stage 2). The exporter emits at
    /// most one — a SID-native `Arpeggiator` (mode `Custom`) on a clean arp plan,
    /// replacing the per-frame [`expand_arpeggio`] bake. Skipped when empty so
    /// every non-arp pattern stays byte-identical to the pre-processor output.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    processors: Vec<NoteProcessor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note_graph: Option<u32>,
}

#[derive(Serialize)]
struct NoteGraph {
    id: u32,
    name: String,
    nodes: BTreeMap<u32, NoteModuleConfig>,
    connections: Vec<NoteConnection>,
}

#[derive(Serialize)]
enum NoteModuleConfig {
    Processor(NoteProcessor),
}

#[derive(Serialize)]
struct NoteConnection {
    from: u32,
    to: u32,
    port: &'static str,
}

/// A pattern note-processor. Externally tagged to match the schema's
/// `NoteProcessor` `oneOf` (`{ "Arpeggiator": { … } }`); the exporter emits only
/// the [`Arpeggiator`] variant.
#[derive(Serialize, Clone)]
enum NoteProcessor {
    Arpeggiator(Arpeggiator),
}

/// A SID-native chiptune arpeggiator (chain stage 2, mode `Custom`): one held
/// source note whose pitch cycles through [`custom`](Self::custom) offsets at the
/// resolved play-call rate under a single gate — the exact model the chip plays,
/// in place of baking per-frame sub-notes. Field set matches the validated recipe
/// (swing/latch left at their schema defaults).
#[derive(Serialize, Clone)]
struct Arpeggiator {
    /// Cyclic semitone offset table (the traced SID arp table), one step per
    /// `rate` tick, applied to each source note's own pitch (0 = the note).
    custom: Vec<i8>,
    mode: &'static str,
    rate: ArpRate,
    octaves: u8,
    legato: bool,
    gate: f32,
    velocity: &'static str,
}

/// The `ArpRate` schema `oneOf` variant the exporter uses: a frame-/tempo-
/// independent step rate in millihertz (`{ "MilliHz": 50000 }` = 50 Hz PAL).
#[derive(Serialize, Clone)]
enum ArpRate {
    MilliHz(u32),
}

impl Arpeggiator {
    /// The canonical SID arp: cycle `offsets` one step per play call, under one
    /// gate (`legato`, full `gate`), tracking the source melody (`mode: Custom`).
    fn sid_native(offsets: &[i8], timing: PlaybackTiming) -> Self {
        Self {
            custom: offsets.to_vec(),
            mode: "Custom",
            rate: ArpRate::MilliHz((timing.calls_per_second() * 1000.0).round() as u32),
            octaves: 1,
            legato: true,
            gate: 1.0,
            velocity: "AsPlayed",
        }
    }
}

/// One automation lane: a target parameter and its interpolation points.
#[derive(Serialize)]
struct AutomationLane {
    target: AutomationTarget,
    points: Vec<AutomationPoint>,
}

/// The `AutomationTarget` schema `oneOf`. The exporter emits two of its four
/// variants: a per-module parameter ([`ModuleTarget`], the PWM / cutoff / ADSR
/// lanes), a per-track parameter ([`TrackTarget`], exact pitch fallback), and a
/// chip-global parameter ([`GlobalTarget`], the §A5 master-volume contour).
/// `untagged` because each inner shape already carries its own externally-tagged
/// wrapper (`{ "Module": … }` / `{ "Track": … }` / `{ "Global": … }`).
#[derive(Serialize)]
#[serde(untagged)]
enum AutomationTarget {
    Module(ModuleTarget),
    Track(TrackTarget),
    Global(GlobalTarget),
}

/// An `AutomationTarget::Track` that follows the track hosting its pattern.
/// The SID pitch fallback uses `Pitch`; omitting an explicit track ID keeps the
/// lane valid when native structure reuses a pattern on its owning track.
#[derive(Serialize)]
struct TrackTarget {
    #[serde(rename = "Track")]
    track: TrackTargetInner,
}

#[derive(Serialize)]
struct TrackTargetInner {
    param: &'static str,
}

impl TrackTarget {
    fn pitch() -> Self {
        Self {
            track: TrackTargetInner { param: "Pitch" },
        }
    }
}

/// An `AutomationTarget::Global` — a chip-global parameter. Serialises as
/// `{ "Global": "MasterVolume" }` to match the schema's `oneOf`; the exporter
/// only uses `MasterVolume` (the §A5 `$D418` volume contour).
#[derive(Serialize)]
struct GlobalTarget {
    #[serde(rename = "Global")]
    param: &'static str,
}

impl GlobalTarget {
    fn master_volume() -> Self {
        Self {
            param: "MasterVolume",
        }
    }
}

/// An `AutomationTarget::Module` — a parameter on a specific module within an
/// instrument's graph. Serialises externally-tagged as `{ "Module": { ... } }`
/// to match the schema's `oneOf`.
#[derive(Serialize)]
struct ModuleTarget {
    #[serde(rename = "Module")]
    module: ModuleTargetInner,
}

#[derive(Serialize)]
struct ModuleTargetInner {
    instrument: u32,
    module_type: &'static str,
    instance: u16,
    param_id: &'static str,
}

impl ModuleTarget {
    /// A target for the single instance (`instance = 1`) of `module_type`
    /// within `instrument`'s graph. Our instruments carry exactly one
    /// oscillator and one filter, so the positional instance is always 1.
    fn new(instrument: u32, module_type: &'static str, param_id: &'static str) -> Self {
        Self::with_instance(instrument, module_type, 1, param_id)
    }

    fn with_instance(
        instrument: u32,
        module_type: &'static str,
        instance: u16,
        param_id: &'static str,
    ) -> Self {
        Self {
            module: ModuleTargetInner {
                instrument,
                module_type,
                instance,
                param_id,
            },
        }
    }
}

/// A single automation point: position (ticks from pattern start), normalized
/// `0.0..=1.0` value, and the interpolation curve to the next point.
#[derive(Serialize)]
struct AutomationPoint {
    tick: u32,
    value: f32,
    curve: CurveType,
}

/// Interpolation from one [`AutomationPoint`] to the next. Serialises to match
/// the schema's `CurveType` `oneOf`: the unit variants become the bare strings
/// `"Linear"`/`"Step"`/`"SCurve"`, and `Exponential(k)` becomes
/// `{ "Exponential": k }` (serde's default externally-tagged encoding). The
/// engine's interpolation math for each variant lives in `synth_sequencer`'s
/// `CurveType::interpolate`.
#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
enum CurveType {
    Linear,
    Step,
    Exponential(i8),
    SCurve,
}

impl CurveType {
    /// Reconstruct the value at normalised position `t` (`0.0..=1.0`) between
    /// two points valued `from` and `to`. Mirrors `synth_sequencer`'s
    /// `CurveType::interpolate` exactly so the curve chosen by [`fit_curve`]
    /// reproduces what the engine will render.
    fn interpolate(self, from: f32, to: f32, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => from + (to - from) * t,
            Self::Step => from,
            Self::Exponential(strength) => {
                let exp_t = if strength >= 0 {
                    t.powf(1.0 + f32::from(strength) * 0.02)
                } else {
                    1.0 - (1.0 - t).powf(1.0 - f32::from(strength) * 0.02)
                };
                from + (to - from) * exp_t
            }
            Self::SCurve => {
                let s = t * t * (3.0 - 2.0 * t);
                from + (to - from) * s
            }
        }
    }
}

#[derive(Clone, Serialize)]
struct Note {
    id: u32,
    start: u32,
    duration: u32,
    pitch: u32,
    velocity: f32,
    track: Option<u32>,
    /// Per-note expression block (currently only vibrato). Omitted when the
    /// note carries no expression so plain notes stay compact.
    #[serde(skip_serializing_if = "Option::is_none")]
    expression: Option<NoteExpression>,
    /// Per-note glide (portamento). Omitted when the note does not slide.
    #[serde(skip_serializing_if = "Option::is_none")]
    glide: Option<Glide>,
    /// Tie to the successor note without re-gating (SID gate-held legato).
    /// Omitted when `false` (the schema default).
    #[serde(skip_serializing_if = "is_false")]
    legato: bool,
}

/// `serde` `skip_serializing_if` predicate: drop a `bool` field when it is the
/// `false` default.
fn is_false(b: &bool) -> bool {
    !*b
}

/// Per-note expression (`NoteExpression` in the schema). Only `vibrato` is
/// populated by the exporter; the other note-shape scalars (accent, gate,
/// ghost, probability) keep their schema defaults and are omitted.
#[derive(Clone, Serialize)]
struct NoteExpression {
    #[serde(skip_serializing_if = "Option::is_none")]
    vibrato: Option<Vibrato>,
}

/// Per-note vibrato. Pertylizer interprets `delay` as a depth fade-in, so this
/// structured form is emitted only for zero-delay SID contours. A measured
/// stable prefix lowers through track-pitch automation instead.
#[derive(Clone, Serialize)]
struct Vibrato {
    depth: f32,
    rate: f32,
    delay: f32,
    shape: VibratoShape,
}

/// `VibratoShape` schema enum. Measured vibrato selects the closest supported
/// contour; authored driver vibrato uses the driver's known LFO shape.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Hash)]
enum VibratoShape {
    Sine,
    Triangle,
    Square,
    Saw,
}

/// Per-note glide (`Glide` in the schema): slide into this note's pitch from
/// `from` over `time` ms using `interp`.
#[derive(Serialize, Clone, Copy)]
struct Glide {
    from: GlideFrom,
    time: f32,
    interp: GlideInterp,
}

/// `GlideFrom` schema enum. The exporter always uses a signed semitone offset
/// from the note's own pitch (serialised as `{ "Semitones": <f32> }`).
#[derive(Serialize, Clone, Copy)]
enum GlideFrom {
    Semitones(f32),
}

/// `GlideInterp` schema enum. SID frequency slides are continuous, so the
/// exporter always uses `Continuous`.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Hash)]
enum GlideInterp {
    Continuous,
}

#[derive(Serialize)]
struct Track {
    id: u32,
    name: String,
    instrument: u32,
    volume: f32,
    pan: f32,
    mute: bool,
    solo: bool,
    color: TrackColor,
    mode: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sends: Vec<TrackSend>,
}

#[derive(Serialize)]
struct TrackSend {
    target: u16,
    level: f32,
    pre_fader: bool,
    enabled: bool,
}

#[derive(Serialize)]
struct ReturnBus {
    id: u16,
    name: &'static str,
    volume: f32,
    pan: f32,
    mute: bool,
    solo: bool,
    color: TrackColor,
    description: &'static str,
    sends: Vec<ReturnSend>,
}

#[derive(Serialize)]
struct ReturnSend {
    target: u16,
    level: f32,
    enabled: bool,
}

#[derive(Serialize)]
struct ReturnBusEffectsState {
    id: u16,
    effects: Vec<Module>,
}

#[derive(Serialize)]
struct TrackColor {
    r: u32,
    g: u32,
    b: u32,
}

#[derive(Serialize)]
struct PatternPlacement {
    pattern_id: u32,
    track_id: u32,
    start: u32,
    transpose: f32,
    gain: f32,
    length_override: Option<u32>,
    loop_mode: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pal_timing() -> PlaybackTiming {
        PlaybackTiming::vblank(SystemClock::Pal)
    }

    fn fast_cia_timing() -> PlaybackTiming {
        PlaybackTiming {
            cia_timed: true,
            ..pal_timing()
        }
        .with_cia_period(crate::emu::CiaTimerPeriod::new(9_828))
    }

    #[test]
    fn census_serializes_report_only_envelope_metrics() {
        let value = serde_json::to_value(Census::default()).unwrap();
        assert_eq!(value["envelope"]["attack_entries"], 0);
        assert_eq!(value["envelope"]["inexact_voice_calls_excluded"], 0);
    }

    #[test]
    fn canonical_note_keeps_musical_identity_separate_from_expression() {
        let note = |expression, glide| Note {
            id: 0,
            start: 40,
            duration: 80,
            pitch: 64,
            velocity: 0.75,
            track: None,
            expression,
            glide,
            legato: false,
        };
        let plain_note = note(None, None);
        let expressed_note = note(
            Some(NoteExpression {
                vibrato: Some(Vibrato {
                    depth: 0.25,
                    rate: 6.0,
                    delay: 0.0,
                    shape: VibratoShape::Triangle,
                }),
            }),
            Some(Glide {
                from: GlideFrom::Semitones(-2.0),
                time: 120.0,
                interp: GlideInterp::Continuous,
            }),
        );

        let plain = CanonicalNote::from_note(&plain_note);
        let expressed = CanonicalNote::from_note(&expressed_note);
        assert!(plain.musical == expressed.musical);
        assert!(plain.expression != expressed.expression);
    }

    #[test]
    fn identical_arpeggiators_share_one_note_graph() {
        fn pattern(id: u32) -> Pattern {
            Pattern {
                id,
                name: format!("p{id}"),
                length: 40,
                notes: Vec::new(),
                automation: Vec::new(),
                next_note_id: 0,
                processors: vec![NoteProcessor::Arpeggiator(Arpeggiator::sid_native(
                    &[0, 4, 7],
                    pal_timing(),
                ))],
                note_graph: None,
            }
        }
        let mut patterns = vec![pattern(0), pattern(1)];
        let graphs = pool_note_graphs(&mut patterns);
        assert_eq!(graphs.len(), 1);
        assert_eq!(patterns[0].note_graph, Some(0));
        assert_eq!(patterns[1].note_graph, Some(0));
        assert!(patterns.iter().all(|pattern| pattern.processors.is_empty()));
    }

    #[test]
    fn automation_compaction_removes_only_redundant_step_holds() {
        let mut patterns = vec![Pattern {
            id: 0,
            name: "automation".to_string(),
            length: 80,
            notes: Vec::new(),
            automation: vec![AutomationLane {
                target: AutomationTarget::Global(GlobalTarget::master_volume()),
                points: vec![
                    AutomationPoint {
                        tick: 0,
                        value: 0.5,
                        curve: CurveType::Step,
                    },
                    AutomationPoint {
                        tick: 20,
                        value: 0.5,
                        curve: CurveType::Step,
                    },
                    AutomationPoint {
                        tick: 40,
                        value: 0.75,
                        curve: CurveType::Linear,
                    },
                ],
            }],
            next_note_id: 0,
            processors: Vec::new(),
            note_graph: None,
        }];
        compact_song_automation(&mut patterns);
        let points = &patterns[0].automation[0].points;
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].tick, 0);
        assert_eq!(points[1].tick, 40);
    }

    #[test]
    fn set_seq_programs_per_frame_masks() {
        let mut p =
            SidOscillatorParams::tonal(0x1, PulseWidth(2048), SidModel::Mos6581, SystemClock::Pal);
        p.set_seq(&[0x1, 0x8, 0x0]);
        assert_eq!(p.seq_len, 3.0);
        assert_eq!(p.seq_rate, 1.0);
        assert_eq!(
            (p.seq_step_0, p.seq_step_1, p.seq_step_2, p.seq_step_3),
            (1.0, 8.0, 0.0, 0.0)
        );
        assert_eq!(p.noise_seed, SID_NOISE_SEED as f32);
    }

    #[test]
    fn set_seq_frequency_overrides_only_selected_steps() {
        let mut p =
            SidOscillatorParams::tonal(0x1, PulseWidth(2048), SidModel::Mos6581, SystemClock::Pal);
        p.set_seq_frequencies(&[0, 0x684C, 0, 0x1234, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(p.seq_freq_mask, 0b1010 as f32);
        assert_eq!(p.seq_step_freq_0, 0.0);
        assert_eq!(p.seq_step_freq_1, 0x684C as f32);
        assert_eq!(p.seq_step_freq_3, 0x1234 as f32);
    }

    #[test]
    fn noise_step_frequency_uses_plan_median_and_tonal_steps_inherit() {
        let first = ev_at(0);
        let second = ev_at(10);
        let third = ev_at(20);
        let mut plan = probe_plan(vec![(0, &first), (0, &second), (0, &third)]);
        plan.waveform_program = Some(vec![0x1, 0x8]);
        let mut states = gated_states_with_ctrl(0, &[(0x1000, 0x41); 32]);
        for (frame, frequency) in [(1usize, 0x1000), (11, 0x3000), (21, 0x2000)] {
            let regs = [
                (frequency & 0xFF) as u8,
                (frequency >> 8) as u8,
                0,
                0,
                0x81,
                0,
                0,
            ];
            states[frame].voices[0] = crate::analysis::voice::VoiceState::from_regs(&regs);
        }
        let frequencies = program_noise_frequencies(&plan, &states);
        assert_eq!(frequencies[0], 0, "tonal step inherits note pitch");
        assert_eq!(frequencies[1], 0x2000);
    }

    #[test]
    fn measured_release_extends_to_envelope_zero_but_not_trace_end() {
        use crate::emu::sid::EnvLevel;
        use crate::trace::CpuCycles;

        let event = ev_at(0);
        let mut states = gated_states_with_ctrl(
            0,
            &(0..14)
                .map(|frame| (0x2000, if frame < 10 { 0x41 } else { 0x40 }))
                .collect::<Vec<_>>(),
        );
        for (frame, state) in states.iter_mut().enumerate() {
            state.digital_state_exact = true;
            state.digital_voices[0].envelope_activity.peak_level = EnvLevel(200);
            state.digital_voices[0].envelope_activity.end_level =
                EnvLevel(if frame < 12 { 200 } else { 0 });
            state.digital_voices[0].envelope.level = EnvLevel(if frame < 12 { 200 } else { 0 });
            state.digital_voices[0].envelope_activity.active_cycles =
                CpuCycles(u64::from(frame < 12));
        }
        let mut notes = vec![build_note(0, &event, 40, states.len() as u32, &states)];
        extend_release_tail(&mut notes, 0, &event, &states, 40, None);
        assert_eq!(notes[0].duration, 12 * 40);

        states[12].digital_voices[0].envelope_activity.active_cycles = CpuCycles(1);
        states[12].digital_voices[0].envelope.level = EnvLevel(1);
        states[13].digital_voices[0].envelope_activity.active_cycles = CpuCycles(1);
        states[13].digital_voices[0].envelope.level = EnvLevel(1);
        let mut censored = vec![build_note(0, &event, 40, states.len() as u32, &states)];
        extend_release_tail(&mut censored, 0, &event, &states, 40, None);
        assert_eq!(censored[0].duration, 10 * 40);
    }

    #[test]
    fn role_drives_instrument_identity() {
        // Bass: mono, bass-named, Bass category (InstrumentCategory::Bass = 2).
        let bass = InstrumentRole::from_role_tags(&RoleTags {
            bass: true,
            ..RoleTags::default()
        });
        assert_eq!(bass.name(), Some("Bass"));
        assert_eq!(bass.category(), 2);
        assert_eq!(bass.allocation_mode(), "Mono");
        assert_eq!(bass.max_voices(), 1);

        // Drum subclass names the instrument; Drums category = 1.
        let kick = InstrumentRole::from_role_tags(&RoleTags {
            percussive: true,
            drum_subclass: Some(DrumSubclass::Kick),
            ..RoleTags::default()
        });
        assert_eq!(kick.name(), Some("Kick"));
        assert_eq!(kick.category(), 1);

        // Pad keeps polyphony for overlapping sustained voices; Pad category = 3.
        let pad = InstrumentRole::from_role_tags(&RoleTags {
            pad: true,
            ..RoleTags::default()
        });
        assert_eq!(pad.name(), Some("Pad"));
        assert_eq!(pad.category(), 3);
        assert_eq!(pad.allocation_mode(), "Polyphonic");
        assert_eq!(pad.max_voices(), 4);

        // A resolved drum subclass wins over a co-occurring melodic tag.
        let both = InstrumentRole::from_role_tags(&RoleTags {
            percussive: true,
            drum_subclass: Some(DrumSubclass::Snare),
            bass: true,
            ..RoleTags::default()
        });
        assert!(matches!(both, InstrumentRole::Drum(DrumSubclass::Snare)));

        // Untagged falls back to no role name + Uncategorized (0).
        let untagged = InstrumentRole::from_role_tags(&RoleTags::default());
        assert_eq!(untagged.name(), None);
        assert_eq!(untagged.category(), 0);
        assert_eq!(untagged.allocation_mode(), "Polyphonic");
        assert_eq!(untagged.max_voices(), 4);
    }

    #[test]
    fn default_ticks_per_frame_follows_clock() {
        assert_eq!(default_ticks_per_frame(SystemClock::Pal), 40);
        assert_eq!(default_ticks_per_frame(SystemClock::Ntsc), 33);
    }

    /// A patch on voice 1 carrying only the fields the adoption gate reads.
    fn adoption_patch(id: u16, waveform: u8, adsr: Adsr, filter_routed: bool) -> Patch {
        use crate::analysis::filter::{FilterMode, Resonance};
        use crate::analysis::timbre::{FilterContour, PatchId, PwEnvelope};
        Patch {
            id: PatchId(id),
            adsr,
            waveform,
            role_tags: RoleTags::default(),
            drum_drop: false,
            authored_effects: None,
            authored_definition: None,
            member_count: 2,
            voices: vec![PatchVoiceProfile {
                voice: VoiceId(1),
                member_count: 2,
                pw_envelope: PwEnvelope::default(),
                filter_routed,
                filter_contour: FilterContour::default(),
                filter_mode: FilterMode::default(),
                filter_resonance: Resonance(0),
                hardware_tricks: Vec::new(),
                ring_source_hz: None,
                sync_source_hz: None,
                waveform_loop: None,
                arpeggio_loop: None,
                noise_freq_hz: None,
                noise_run_frames: 0.0,
            }],
        }
    }

    fn note_chars(dominant_waveform: u8, adsr: Adsr, filter_routed: bool) -> NoteCharacteristics {
        NoteCharacteristics {
            dominant_waveform,
            starting_adsr: adsr,
            filter_routed,
            ..NoteCharacteristics::default()
        }
    }

    const PAD_ADSR: Adsr = Adsr {
        attack: 2,
        decay: 8,
        sustain: 10,
        release: 6,
    };

    #[test]
    fn timbre_gate_matches_close_pulse_leads() {
        let patch = adoption_patch(0, 0x40, PAD_ADSR, false);
        let profile = patch.profile(VoiceId(1)).unwrap();
        // Same pulse waveform, envelope drifting within tolerance → compatible.
        let near = note_chars(
            0x40,
            Adsr {
                attack: 3,
                decay: 10,
                sustain: 9,
                release: 8,
            },
            false,
        );
        assert!(timbre_compatible(&near, &patch, profile));
        // Pulse+triangle combined byte shares the pulse bit → still compatible.
        let combined = note_chars(0x50, PAD_ADSR, false);
        assert!(timbre_compatible(&combined, &patch, profile));
    }

    #[test]
    fn timbre_gate_rejects_mismatched_timbre() {
        let patch = adoption_patch(0, 0x40, PAD_ADSR, false);
        let profile = patch.profile(VoiceId(1)).unwrap();
        // A noise one-shot (SFX) must never adopt a pulse lead.
        let noise = note_chars(0x80, Adsr::default(), false);
        assert!(!timbre_compatible(&noise, &patch, profile));
        // Pure triangle shares no waveform-select bit with pulse.
        let tri = note_chars(0x10, PAD_ADSR, false);
        assert!(!timbre_compatible(&tri, &patch, profile));
        // A filtered note against a dry patch is an audibly different timbre.
        let filtered = note_chars(0x40, PAD_ADSR, true);
        assert!(!timbre_compatible(&filtered, &patch, profile));
        // A percussive envelope (no sustain, no release tail) is too far off.
        let plucky = note_chars(
            0x40,
            Adsr {
                attack: 2,
                decay: 8,
                sustain: 0,
                release: 0,
            },
            false,
        );
        assert!(!timbre_compatible(&plucky, &patch, profile));
    }

    #[test]
    fn adopt_patch_picks_nearest_compatible_and_guards() {
        let far = adoption_patch(1, 0x40, PAD_ADSR, false);
        let near = adoption_patch(2, 0x40, PAD_ADSR, false);
        let near_profile = near.profile(VoiceId(1)).unwrap();
        let far_profile = far.profile(VoiceId(1)).unwrap();
        let note = note_chars(0x40, PAD_ADSR, false);

        // Among compatible candidates the temporally nearest wins (patch 2).
        let ev = ev_at(100);
        let assigned = vec![
            (10u32, 1u16, &far, far_profile),
            (105u32, 2u16, &near, near_profile),
        ];
        assert_eq!(adopt_patch(&ev, Some(&note), &assigned), Some(2));

        // A note with no characteristics can't be gated → stays raw (`None`).
        assert_eq!(adopt_patch(&ev, None, &assigned), None);

        // Every candidate beyond the distance backstop → no adoption.
        let stranded = vec![(u32::MAX, 1u16, &far, far_profile)];
        assert_eq!(adopt_patch(&ev, Some(&note), &stranded), None);

        // A lone incompatible candidate (noise patch) is rejected, not forced.
        let noise_patch = adoption_patch(3, 0x80, Adsr::default(), false);
        let noise_profile = noise_patch.profile(VoiceId(1)).unwrap();
        let only_noise = vec![(100u32, 3u16, &noise_patch, noise_profile)];
        assert_eq!(adopt_patch(&ev, Some(&note), &only_noise), None);
    }

    /// A minimal voice profile carrying only a waveform loop — the one field
    /// [`alternation_seq`] reads.
    fn alt_profile(waveform_loop: Vec<u8>) -> PatchVoiceProfile {
        use crate::analysis::filter::{FilterMode, Resonance};
        use crate::analysis::timbre::{FilterContour, PwEnvelope};
        PatchVoiceProfile {
            voice: VoiceId(2),
            member_count: 2,
            pw_envelope: PwEnvelope::default(),
            filter_routed: false,
            filter_contour: FilterContour::default(),
            filter_mode: FilterMode::default(),
            filter_resonance: Resonance(0),
            hardware_tricks: Vec::new(),
            ring_source_hz: None,
            sync_source_hz: None,
            waveform_loop: Some(waveform_loop),
            arpeggio_loop: None,
            noise_freq_hz: None,
            noise_run_frames: 0.0,
        }
    }

    #[test]
    fn alternation_seq_preserves_the_detected_period() {
        // tri (0x10) ↔ noise (0x80) → a two-step tri/noise sequence.
        assert_eq!(
            alternation_seq(alt_profile(vec![0x10, 0x80]).waveform_loop.as_deref()),
            Some(vec![0x1, 0x8])
        );
        // Held frames are part of the period, not duplicate values to discard.
        assert_eq!(
            alternation_seq(
                alt_profile(vec![0x10, 0x10, 0x80, 0x80])
                    .waveform_loop
                    .as_deref()
            ),
            Some(vec![0x1, 0x1, 0x8, 0x8])
        );
        // Silent frames do not count as an audible class but keep their timing
        // when two audible masks establish a real alternation.
        assert_eq!(
            alternation_seq(alt_profile(vec![0x80, 0x00, 0x20]).waveform_loop.as_deref()),
            Some(vec![0x8, 0x0, 0x2])
        );
        // Combined bytes keep their full mask (pulse ↔ pulse+tri = {0x4, 0x5}).
        assert_eq!(
            alternation_seq(alt_profile(vec![0x40, 0x50]).waveform_loop.as_deref()),
            Some(vec![0x4, 0x5])
        );
        // Three distinct classes are now expressible (native seq, unlike the old
        // two-source gate) — a 3-step sequence.
        assert_eq!(
            alternation_seq(alt_profile(vec![0x40, 0x10, 0x20]).waveform_loop.as_deref()),
            Some(vec![0x4, 0x1, 0x2])
        );
        // An overlong representative window falls back to its distinct audible
        // masks, retaining the established Nemesis representation.
        let mut nemesis = vec![0x10, 0x10];
        for _ in 0..10 {
            nemesis.extend([0x80, 0x10]);
        }
        assert_eq!(alternation_seq(Some(&nemesis)), Some(vec![0x1, 0x8]));
    }

    #[test]
    fn alternation_seq_rejects_non_alternating() {
        // A constant mask is not an alternation.
        assert_eq!(
            alternation_seq(alt_profile(vec![0x40, 0x40]).waveform_loop.as_deref()),
            None
        );
        // A single waveform gated on/off (one non-silent class) is not either.
        assert_eq!(
            alternation_seq(alt_profile(vec![0x40, 0x00]).waveform_loop.as_deref()),
            None
        );
        // No loop at all.
        assert_eq!(alternation_seq(None), None);
    }

    #[test]
    fn repeating_waveform_program_is_part_of_the_merge_shape() {
        let patch = adoption_patch(0, 0x10, PAD_ADSR, false);
        let static_profile = alt_profile(vec![0x10, 0x10]);
        let daglish_profile = alt_profile(vec![0x10, 0x10, 0x80, 0x80]);

        let static_shape = shape_of(Some(&patch), Some(&static_profile), None);
        let daglish_shape = shape_of(Some(&patch), Some(&daglish_profile), None);

        assert!(static_shape != daglish_shape);
        assert_eq!(
            daglish_shape.waveform_loop,
            shape_program(Some(&[0x1, 0x1, 0x8, 0x8]))
        );
    }

    #[test]
    fn observed_program_wins_over_generic_percussion() {
        let mut patch = adoption_patch(0, 0x40, PAD_ADSR, false);
        patch.role_tags = RoleTags {
            percussive: true,
            drum_subclass: Some(DrumSubclass::Snare),
            ..RoleTags::default()
        };
        let static_profile = alt_profile(vec![0x40, 0x40]);
        assert!(use_generic_percussion(&patch, &static_profile, None));

        let observed = alt_profile(vec![0x10, 0x80]);
        assert_eq!(observed_waveform_program(&observed), Some(vec![0x1, 0x8]));
        assert!(!use_generic_percussion(&patch, &observed, None));
        assert!(!use_generic_percussion(
            &patch,
            &static_profile,
            Some(&[0x1, 0x8]),
        ));
    }

    #[test]
    fn set_seq_loops_the_waveform_sequence() {
        let mut p =
            SidOscillatorParams::tonal(0x1, PulseWidth(2048), SidModel::Mos6581, SystemClock::Pal);
        p.set_seq(&[0x1, 0x8]);
        assert_eq!(p.seq_len, 2.0);
        assert_eq!(p.seq_rate, 1.0);
        assert_eq!(
            p.seq_loop, 1.0,
            "a repeating alternation loops for the whole note"
        );
    }

    /// §A2 drift guard: the exporter sources its normalization ranges + curves
    /// from `descriptors.json`, so a Pertylizer re-tune (range or curve change)
    /// that we re-sync must be noticed here rather than silently mis-scaling
    /// every export. Asserts the params the exporter depends on are present
    /// (not the identity fallback) with the curves/ranges it assumes. If this
    /// fails after a descriptor re-sync, Pertylizer changed a mapping — review
    /// the affected normalize/clamp path before updating the expectation.
    #[test]
    fn descriptors_cover_exporter_params() {
        use descriptors::ResponseCurve::{Exponential, Linear, Logarithmic};

        let pw = pulse_width_param();
        assert_eq!(pw.curve, Linear, "sid_oscillator.pw_reg should be linear");
        assert!(pw.min.abs() < 1e-6, "pw_reg min {}", pw.min);
        assert!((pw.max - 4095.0).abs() < 1e-3, "pw_reg max {}", pw.max);

        let cutoff = cutoff_param();
        assert_eq!(cutoff.curve, Logarithmic, "filter.cutoff should be log");
        assert!(
            (cutoff.min - 20.0).abs() < 1e-3,
            "cutoff min {}",
            cutoff.min
        );
        assert!(
            (cutoff.max - 20000.0).abs() < 1e-1,
            "cutoff max {}",
            cutoff.max
        );

        // attack/decay/release share one range + curve (the exporter uses the
        // attack descriptor for all three).
        for stage in ["attack", "decay", "release"] {
            let env = descriptors::param("envelope", stage);
            assert_eq!(env.curve, Exponential, "envelope.{stage} should be exp");
            assert!(
                (env.min - 0.0).abs() < 1e-6,
                "envelope.{stage} min {}",
                env.min
            );
            assert!(
                (env.max - 10.0).abs() < 1e-3,
                "envelope.{stage} max {}",
                env.max
            );
        }

        // sustain is a plain linear 0..1 fraction (mapped directly, no curve).
        let sustain = descriptors::param("envelope", "sustain");
        assert_eq!(sustain.curve, Linear, "envelope.sustain should be linear");
        assert!((sustain.min - 0.0).abs() < 1e-6);
        assert!((sustain.max - 1.0).abs() < 1e-6);
    }

    /// §A9: the coloring lives on the master bus, not per instrument. An
    /// instrument carries no `distortion` module, and its source is the native
    /// `sid_oscillator` in `fast` quality (measured closer to reSID than the 4×
    /// path — PoC matrix).
    #[test]
    fn instrument_has_no_per_voice_coloring() {
        let inst = generic_pulse_instrument(1, VoiceId::V1, SidModel::Mos6581, SystemClock::Pal);
        assert!(
            inst.patch.modules.iter().all(|m| m.kind != "distortion"),
            "coloring is on the master bus, not per instrument"
        );
        for m in &inst.patch.modules {
            if let Parameters::SidOscillator(p) = &m.parameters {
                assert_eq!(p.quality, "fast");
                assert_eq!(p.model, "6581");
                assert_eq!(p.clock, "pal");
            }
        }
    }

    /// §A9: the master-bus chain colours the summed voices, then contains
    /// reconstructed peaks with a look-ahead limiter.
    #[test]
    fn master_chain_is_tube_then_eq_then_limiter() {
        let chain = master_chain();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].kind, "distortion");
        assert_eq!(chain[0].id, "dst-1");
        assert_eq!(chain[1].kind, "eq");
        assert_eq!(chain[1].id, "equ-1");
        assert_eq!(chain[2].kind, "limiter");
        assert_eq!(chain[2].id, "lmt-1");
        if let Parameters::Limiter(params) = &chain[2].parameters {
            assert_eq!(params.ceiling, MASTER_LIMITER_CEILING_DB);
            assert_eq!(params.mix, 1.0);
        } else {
            panic!("third master effect must be the limiter");
        }
        // The default project state ships the chain on the master bus.
        let global = GlobalProjectState::default();
        assert_eq!(global.master_effects.len(), 3);
        assert_eq!(global.master_volume, MASTER_VOLUME);
    }

    #[test]
    fn adsr_table_maps_nibbles_to_seconds() {
        // attack nibble 0 = 2 ms, decay/release = 3× their column.
        let a = adsr_to_seconds(Adsr {
            attack: 0,
            decay: 0,
            sustain: 15,
            release: 0,
        });
        assert!((a.attack - 0.002).abs() < 1e-6);
        assert!((a.decay - 0.006).abs() < 1e-6);
        assert!((a.release - 0.006).abs() < 1e-6);
        assert!((a.sustain - 1.0).abs() < 1e-6);

        // Top nibble: 8000 ms attack; decay/release would be 24 s but clamp
        // to the schema's 10 s cap. Sustain 0.
        let b = adsr_to_seconds(Adsr {
            attack: 15,
            decay: 15,
            sustain: 0,
            release: 15,
        });
        let env_max = env_time_param().max;
        assert!((b.attack - 8.0).abs() < 1e-6);
        assert!((b.decay - env_max).abs() < 1e-6);
        assert!((b.release - env_max).abs() < 1e-6);
        assert!((b.sustain - 0.0).abs() < 1e-6);
    }

    #[test]
    fn pulse_width_normalizes_linearly_over_band() {
        // SID PW=0 collapses to the 0.01 floor → lane value 0.0.
        assert!(normalize_pulse_width(0).abs() < 1e-6);
        // SID PW=4095 → 1.0 raw → clamped to the 0.99 ceiling → lane value 1.0.
        assert!((normalize_pulse_width(4095) - 1.0).abs() < 1e-6);
        // SID 50% (≈2048/4095 = 0.5003) sits near the band midpoint.
        let mid = normalize_pulse_width(2048);
        assert!((mid - 0.5).abs() < 0.02, "midpoint {mid} not near 0.5");
        // Monotonic.
        assert!(normalize_pulse_width(1000) < normalize_pulse_width(3000));
    }

    #[test]
    fn cutoff_normalizes_logarithmically() {
        // 20 Hz (the param floor) → 0.0; 20 kHz (ceiling) → 1.0.
        assert!(normalize_cutoff_hz(20.0).abs() < 1e-6);
        assert!((normalize_cutoff_hz(20000.0) - 1.0).abs() < 1e-6);
        // Logarithmic: the geometric mean (≈632 Hz) maps to the lane midpoint,
        // unlike a linear curve (which would put 0.5 at ~10 kHz).
        let geo_mean = (20.0f32 * 20000.0).sqrt();
        assert!((normalize_cutoff_hz(geo_mean) - 0.5).abs() < 1e-4);
        // Out-of-range inputs clamp into [0, 1].
        assert!(normalize_cutoff_hz(5.0).abs() < 1e-6);
        assert!((normalize_cutoff_hz(40000.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn env_seconds_normalize_matches_exponential_curve() {
        // Endpoints: 0 s → 0; the upper cap → 1.
        let env_max = env_time_param().max;
        assert!(normalize_env_seconds(0.0).abs() < 1e-6);
        assert!((normalize_env_seconds(env_max) - 1.0).abs() < 1e-6);
        // Monotonic increasing.
        assert!(normalize_env_seconds(0.5) < normalize_env_seconds(2.0));
        // Slow-start in seconds space (denormalize is concave), so a given
        // seconds value maps *above* the linear diagonal: 5 s (linear 0.5)
        // normalizes above 0.5.
        assert!(normalize_env_seconds(5.0) > 0.5);
        // Round-trips through the engine's `ResponseCurve::Exponential::
        // denormalize` (curved = (eⁿ − 1)/(e − 1); secs = curved · 10), so the
        // lane value decodes back to the intended seconds in Pertylizer.
        let e = std::f32::consts::E;
        let env_max = env_time_param().max;
        for secs in [0.05_f32, 0.5, 1.0, 3.0, 7.5] {
            let n = normalize_env_seconds(secs);
            let denorm = ((e.powf(n) - 1.0) / (e - 1.0)) * env_max;
            assert!((denorm - secs).abs() < 1e-3, "round-trip {secs} → {denorm}");
        }
    }

    fn ev_at(start: u32) -> NoteEvent {
        use crate::analysis::note::{Cents, GmProgram, MidiNote, Velocity};
        use crate::trace::FrameIndex;
        NoteEvent {
            voice: VoiceId::from_index(0),
            start_frame: FrameIndex(start),
            end_frame: Some(FrameIndex(start + 10)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity(100),
        }
    }

    #[test]
    fn legato_started_note_keeps_its_mid_note_retrigger() {
        use crate::emu::sid::{EnvelopeEvent, EnvelopeEventKind};
        use crate::trace::SubFrameOffset;
        let mut states = vec![ProgramFrame::default(); 12];
        for (index, state) in states.iter_mut().enumerate() {
            state.frame = crate::trace::FrameIndex(index as u32);
            state.digital_state_exact = true;
        }
        // The only attack is mid-note (frame 5): the note started legato.
        states[5].digital_voices[0]
            .envelope_activity
            .events
            .push(EnvelopeEvent {
                offset: SubFrameOffset(0),
                kind: EnvelopeEventKind::EnteredAttack,
            });
        let event = ev_at(0);
        assert_eq!(chip_articulation(&event, &states).retriggers, 1);
        // A gated note (attack in its start frame) still subtracts its onset.
        states[0].digital_voices[0]
            .envelope_activity
            .events
            .push(EnvelopeEvent {
                offset: SubFrameOffset(0),
                kind: EnvelopeEventKind::EnteredAttack,
            });
        assert_eq!(chip_articulation(&event, &states).retriggers, 1);
    }

    #[test]
    fn row_leading_gate_onset_is_not_a_retrigger() {
        use crate::emu::sid::{EnvelopeEvent, EnvelopeEventKind};
        use crate::trace::SubFrameOffset;
        let mut states = vec![ProgramFrame::default(); 12];
        for (index, state) in states.iter_mut().enumerate() {
            state.frame = crate::trace::FrameIndex(index as u32);
            state.digital_state_exact = true;
        }
        // Row frame precedes the gate: the onset attack lands at offset 2,
        // inside the gate-snap lead window — not a retrigger.
        states[2].digital_voices[0]
            .envelope_activity
            .events
            .push(EnvelopeEvent {
                offset: SubFrameOffset(0),
                kind: EnvelopeEventKind::EnteredAttack,
            });
        assert_eq!(chip_articulation(&ev_at(0), &states).retriggers, 0);
        // The same attack on a voice already gate-high at the start (true
        // legato) IS a retrigger.
        states[0].voices[0].control.gate = true;
        assert_eq!(chip_articulation(&ev_at(0), &states).retriggers, 1);
    }

    #[test]
    fn clean_normal_adsr_uses_static_recipe_while_delay_bug_stays_measured() {
        use crate::emu::sid::{EnvLevel, EnvelopeEvent, EnvelopeEventKind};
        use crate::trace::SubFrameOffset;
        let event = ev_at(0);
        let plan = probe_plan(vec![(0, &event)]);
        let mut states = vec![ProgramFrame::default(); 12];
        for state in &mut states {
            state.digital_state_exact = true;
            state.digital_voices[0].envelope_activity.peak_level = EnvLevel(255);
        }
        states[0].digital_voices[0]
            .envelope_activity
            .events
            .push(EnvelopeEvent {
                offset: SubFrameOffset(1),
                kind: EnvelopeEventKind::EnteredAttack,
            });
        states[0].digital_voices[0].envelope_activity.first_nonzero = Some(SubFrameOffset(2));
        states[10].digital_voices[0]
            .envelope_activity
            .events
            .push(EnvelopeEvent {
                offset: SubFrameOffset(1),
                kind: EnvelopeEventKind::ReachedZero,
            });
        assert!(!plan_uses_measured_envelope(&plan, &states));

        states[0].digital_voices[0].envelope_activity.first_nonzero = None;
        states[0].digital_voices[0].envelope_activity.end_level = EnvLevel(0);
        assert!(plan_uses_measured_envelope(&plan, &states));
    }

    fn adsr_secs(attack: f32) -> AdsrSeconds {
        AdsrSeconds {
            attack,
            decay: 0.0,
            sustain: 1.0,
            release: 0.1,
        }
    }

    fn probe_plan(timeline: Vec<(usize, &NoteEvent)>) -> TrackPlan<'_> {
        TrackPlan {
            instrument_id: 3,
            voice: VoiceId::from_index(0),
            voice_index: 0,
            shape: MergeShape {
                is_noise: false,
                waveform: "pulse",
                authored_pwm: None,
                secondary_waveform: None,
                has_ring: false,
                has_sync: false,
                filter_kind: None,
                waveform_program: None,
                waveform_loop: None,
                tonal_arp: None,
            },
            drum_drop: false,
            drum_noise: None,
            waveform_program: None,
            segments: Vec::new(),
            timeline,
        }
    }

    fn ev_cents(start: u32, cents: f32) -> NoteEvent {
        use crate::analysis::note::Cents;
        NoteEvent {
            cents: Cents(cents),
            ..ev_at(start)
        }
    }

    #[test]
    fn plan_detune_cents_takes_the_median_and_clamps() {
        // Consistently detuned voice: median of the per-note cents, not the mean
        // (the +30 outlier does not drag it).
        let (n0, n1, n2) = (ev_cents(0, 18.0), ev_cents(100, 20.0), ev_cents(200, 30.0));
        let plan = probe_plan(vec![(0, &n0), (1, &n1), (0, &n2)]);
        assert!((plan_detune_cents(&plan) - 20.0).abs() < 1e-6);

        // Empty plan → no detune.
        assert_eq!(plan_detune_cents(&probe_plan(Vec::new())), 0.0);

        // Out-of-range cents clamp to the module's ±100 ct limit.
        let big = ev_cents(0, 250.0);
        assert_eq!(
            plan_detune_cents(&probe_plan(vec![(0, &big)])),
            DETUNE_LIMIT_CENTS
        );
    }

    #[test]
    fn test_reset_policy_uses_the_measured_deterministic_noise_seed() {
        use crate::trace::{RegisterWrite, SidRegister, SubFrameOffset};
        let event = ev_at(0);
        let plan = probe_plan(vec![(0, &event)]);
        let mut states = vec![ProgramFrame::default(); 10];
        states[0].digital_state_exact = true;
        states[0].digital_voices[0].oscillator.noise_shift_register = 0x123456;
        states[0].register_writes = vec![
            RegisterWrite {
                reg: SidRegister(4),
                value: 0x48,
                offset: SubFrameOffset(1),
            },
            RegisterWrite {
                reg: SidRegister(4),
                value: 0x41,
                offset: SubFrameOffset(2),
            },
        ];
        let mut instrument =
            generic_pulse_instrument(3, VoiceId::V1, SidModel::Mos6581, SystemClock::Pal);
        enforce_oscillator_restart_policy(&plan, &states, &mut instrument);
        let seed = instrument
            .patch
            .modules
            .iter()
            .find_map(|module| match &module.parameters {
                Parameters::SidOscillator(params) => Some(params.noise_seed),
                _ => None,
            })
            .expect("instrument has SID oscillator");
        assert_eq!(seed, 0x123456 as f32);
    }

    #[test]
    fn silent_sync_modulator_uses_the_measured_physical_source_frequency() {
        let event = ev_at(0);
        let plan = probe_plan(vec![(0, &event)]);
        let mut states = vec![ProgramFrame::default(); 10];
        for state in &mut states {
            state.digital_state_exact = true;
            state.voices[0].control.sync = true;
            state.digital_voices[0].oscillator.sync_resets = 1;
            state.voices[2].freq = crate::analysis::voice::SidFreq(0x2345);
            state.digital_voices[2].oscillator.source_msb_edges = 1;
        }
        let mut instrument =
            generic_pulse_instrument(3, VoiceId::V1, SidModel::Mos6581, SystemClock::Pal);
        instrument.patch.modules.push(neighbour_source_module(
            "sid-2",
            1,
            SidModel::Mos6581,
            SystemClock::Pal,
        ));
        enforce_measured_hardware_effects(&plan, &states, &mut instrument);
        let source = instrument
            .patch
            .modules
            .iter()
            .find(|module| module.id == "sid-2")
            .expect("source module");
        let Parameters::SidOscillator(params) = &source.parameters else {
            panic!("source is SID oscillator");
        };
        assert_eq!(params.freq_reg, f32::from(0x2345u16));
        assert_eq!(params.track_pitch, 0.0);
    }

    #[test]
    fn moving_ring_source_becomes_second_oscillator_frequency_automation() {
        let event = ev_at(0);
        let mut plan = probe_plan(vec![(0, &event)]);
        plan.shape.has_ring = true;
        let ownership = PhysicalVoiceOwnership::from_plans(std::slice::from_ref(&plan));
        let mut states = vec![ProgramFrame::default(); 12];
        for (frame, state) in states.iter_mut().enumerate() {
            state.voices[2].freq = SidFreq(0x2000 + frame as u16 * 0x40);
        }

        let lanes = build_plan_automation(
            &plan,
            &ownership,
            &states,
            &[],
            pal_timing(),
            SidModel::Mos6581,
            12,
            40,
        );
        let lane = lanes
            .iter()
            .find(|lane| {
                matches!(
                    &lane.target,
                    AutomationTarget::Module(target)
                        if target.module.module_type == "sid_oscillator"
                            && target.module.instance == 2
                            && target.module.param_id == "freq_reg"
                )
            })
            .expect("moving physical source lane");

        assert!(lane.points.len() >= 2);
        assert!((lane.points[0].value - f32::from(0x2000u16) / 65535.0).abs() < 1e-6);
    }

    #[test]
    fn measured_vibrato_onset_delay_lowers_to_track_pitch_automation() {
        let event = ev_at(0);
        let plan = probe_plan(vec![(0, &event)]);
        let ownership = PhysicalVoiceOwnership::from_plans(std::slice::from_ref(&plan));
        let freqs = [
            4000u16, 4000, 4000, 4000, 3950, 3950, 4050, 4050, 3950, 3950,
        ];
        let states = states_with_freqs(0, &freqs);
        let effects = [span(Effect::Vibrato, 0, 0, freqs.len() as u32 - 1)];

        let lane = build_delayed_vibrato_pitch_lane(
            &plan,
            &ownership,
            &states,
            &effects,
            pal_timing(),
            freqs.len() as u32,
            40,
        )
        .expect("a stable prefix needs the exact pitch fallback");
        assert!(matches!(lane.target, AutomationTarget::Track(_)));
        assert!(lane.points.len() >= 3);
        assert_eq!(lane.points.first().unwrap().tick, 0);
        assert_eq!(lane.points.last().unwrap().tick, 10 * 40);
        assert!((lane.points.last().unwrap().value - 0.5).abs() < 1e-6);

        let mut notes = Vec::new();
        push_expressive_notes(
            &mut notes,
            &event,
            &states,
            &effects,
            pal_timing(),
            VoiceId::V1,
            0,
            freqs.len() as u32,
            40,
            None,
            false,
        );
        assert_eq!(notes.len(), 1);
        assert!(notes[0].expression.is_none());
    }

    #[test]
    fn zero_delay_vibrato_keeps_the_structured_expression() {
        let event = ev_at(0);
        let freqs = [
            4000u16, 4050, 4000, 3950, 4000, 4050, 4000, 3950, 4000, 4050,
        ];
        let states = states_with_freqs(0, &freqs);
        let effects = [span(Effect::Vibrato, 0, 0, freqs.len() as u32 - 1)];
        let mut notes = Vec::new();

        push_expressive_notes(
            &mut notes,
            &event,
            &states,
            &effects,
            pal_timing(),
            VoiceId::V1,
            0,
            freqs.len() as u32,
            40,
            None,
            false,
        );
        let vibrato = notes[0]
            .expression
            .as_ref()
            .and_then(|expression| expression.vibrato.as_ref())
            .expect("zero-delay vibrato is target-native");
        assert_eq!(vibrato.delay, 0.0);
    }

    #[test]
    fn shared_filter_accepts_note_boundary_routing_and_rejects_dynamic_cutoff() {
        let (filtered_note, dry_note) = (ev_at(0), ev_at(10));
        let mut filtered = probe_plan(vec![(0, &filtered_note)]);
        filtered.shape.filter_kind = Some("lowpass");
        let dry = probe_plan(vec![(0, &dry_note)]);
        let mut states = vec![ProgramFrame::default(); 20];
        let routed = crate::analysis::filter::FilterState::from_regs(&[0, 128, 0x71, 0x1F]);
        let unrouted = crate::analysis::filter::FilterState::from_regs(&[0, 128, 0x70, 0x1F]);
        for state in &mut states[..10] {
            state.filter = routed;
        }
        for state in &mut states[10..] {
            state.filter = unrouted;
        }

        let plans = [filtered, dry];
        assert!(shared_filter_spec(&plans, &states, SidModel::Mos6581).is_some());

        states[5].filter = crate::analysis::filter::FilterState::from_regs(&[0, 129, 0x71, 0x1F]);
        assert!(shared_filter_spec(&plans, &states, SidModel::Mos6581).is_none());
    }

    #[test]
    fn build_adsr_lane_steps_at_changes_and_coalesces_back() {
        // Voice alternates segment 0 (short attack), segment 1 (long), segment 0
        // again — the lane follows the timeline, stepping A→B→A.
        let (n0, n1, n2) = (ev_at(0), ev_at(100), ev_at(200));
        let plan = probe_plan(vec![(0, &n0), (1, &n1), (0, &n2)]);
        let seg_adsr = vec![Some(adsr_secs(0.01)), Some(adsr_secs(5.0))];
        let lane = build_adsr_lane(
            ModuleTarget::new(3, "envelope", "attack"),
            &plan,
            &seg_adsr,
            40,
            |a| normalize_env_seconds(a.attack),
        )
        .expect("attack varies → a lane");
        assert_eq!(lane.points.len(), 3, "A→B→A = three steps");
        assert_eq!(lane.points[0].tick, 0);
        assert_eq!(lane.points[1].tick, 100 * 40);
        assert_eq!(lane.points[2].tick, 200 * 40);
        assert!(
            lane.points
                .iter()
                .all(|p| matches!(p.curve, CurveType::Step))
        );
        assert!(
            lane.points[1].value > lane.points[0].value,
            "5 s attack normalizes above 0.01 s"
        );
        assert!(
            (lane.points[0].value - lane.points[2].value).abs() < 1e-6,
            "stepping back to segment 0 restores its value"
        );
    }

    #[test]
    fn build_adsr_lane_constant_value_yields_none() {
        // Two segments with identical attack — nothing to automate.
        let (n0, n1) = (ev_at(0), ev_at(100));
        let plan = probe_plan(vec![(0, &n0), (1, &n1)]);
        let seg_adsr = vec![Some(adsr_secs(0.2)), Some(adsr_secs(0.2))];
        assert!(
            build_adsr_lane(
                ModuleTarget::new(3, "envelope", "attack"),
                &plan,
                &seg_adsr,
                40,
                |a| normalize_env_seconds(a.attack),
            )
            .is_none()
        );
    }

    #[test]
    fn mix_volume_divides_headroom_across_active_voices() {
        fn plan(voice_index: usize) -> TrackPlan<'static> {
            TrackPlan {
                instrument_id: 1,
                voice: VoiceId::from_index(voice_index),
                voice_index,
                shape: MergeShape {
                    is_noise: false,
                    waveform: "pulse",
                    authored_pwm: None,
                    secondary_waveform: None,
                    has_ring: false,
                    has_sync: false,
                    filter_kind: None,
                    waveform_program: None,
                    waveform_loop: None,
                    tonal_arp: None,
                },
                drum_drop: false,
                drum_noise: None,
                waveform_program: None,
                segments: Vec::new(),
                timeline: Vec::new(),
            }
        }
        // Budget is split by active-voice count (the §A9 coloring is on the
        // master bus now, so no per-track makeup trim).
        // Three distinct voices → the headroom budget split three ways.
        let three = [plan(0), plan(1), plan(2)];
        assert!((mix_volume(&three) - MIX_HEADROOM / 3.0).abs() < 1e-6);
        // Many tracks but only two distinct voices (patch-split) → divided by 2,
        // not by the track count — same-voice tracks never overlap.
        let two = [plan(0), plan(0), plan(0), plan(2)];
        assert!((mix_volume(&two) - MIX_HEADROOM / 2.0).abs() < 1e-6);
        // A single active voice keeps the full budget.
        let one = [plan(1)];
        assert!((mix_volume(&one) - MIX_HEADROOM).abs() < 1e-6);

        let mut drum_drop = plan(0);
        drum_drop.drum_drop = true;
        assert!((plan_mix_trim(&drum_drop) - DRUM_DROP_MIX_TRIM).abs() < 1e-6);
        assert_eq!(
            plan_output_cutoff(&drum_drop),
            Some(DRUM_DROP_OUTPUT_CUTOFF)
        );

        let mut corrected =
            generic_pulse_instrument(1, VoiceId::V1, SidModel::Mos6581, SystemClock::Pal);
        add_output_lowpass(&mut corrected, DRUM_DROP_OUTPUT_CUTOFF);
        assert!(
            corrected
                .patch
                .modules
                .iter()
                .any(|module| module.id == "flt-2")
        );
        assert!(corrected.patch.connections.iter().any(|connection| {
            connection.from == ["flt-2", "out"] && connection.to == ["out-1", "in"]
        }));

        let mut triangle_pulse = plan(0);
        triangle_pulse.shape.secondary_waveform = Some("triangle");
        assert!((plan_mix_trim(&triangle_pulse) - TRIANGLE_PULSE_MIX_TRIM).abs() < 1e-6);

        let mut lead_patch = adoption_patch(0, 0x40, PAD_ADSR, false);
        lead_patch.role_tags.lead = true;
        let lead_profile = lead_patch.profile(VoiceId::V1).expect("voice profile");
        let mut lead = plan(0);
        lead.segments.push(Segment {
            patch: Some(&lead_patch),
            profile: Some(lead_profile),
            events: Vec::new(),
        });
        assert!((plan_mix_trim(&lead) - LEAD_MIX_TRIM).abs() < 1e-6);

        let mut arp_patch = adoption_patch(1, 0x40, PAD_ADSR, false);
        arp_patch.role_tags.lead = true;
        arp_patch.voices[0].arpeggio_loop = Some(vec![0, 4, 7]);
        let arp_profile = arp_patch.profile(VoiceId::V1).expect("voice profile");
        let mut arp = plan(0);
        arp.segments.push(Segment {
            patch: Some(&arp_patch),
            profile: Some(arp_profile),
            events: Vec::new(),
        });
        assert!((plan_mix_trim(&arp) - ARPEGGIATED_LEAD_MIX_TRIM).abs() < 1e-6);
        assert_eq!(
            plan_output_cutoff(&arp),
            Some(ARPEGGIATED_LEAD_OUTPUT_CUTOFF)
        );

        let mut drum_patch = adoption_patch(2, 0x40, PAD_ADSR, false);
        drum_patch.role_tags.drum_subclass = Some(DrumSubclass::PercMetallic);
        let drum_profile = drum_patch.profile(VoiceId::V1).expect("voice profile");
        let mut drum = plan(0);
        let drum_event = ev_at(0);
        drum.timeline = vec![(0, &drum_event); DRUM_OUTPUT_FILTER_MIN_NOTES];
        drum.segments.push(Segment {
            patch: Some(&drum_patch),
            profile: Some(drum_profile),
            events: Vec::new(),
        });
        assert!((plan_mix_trim(&drum) - DRUM_MIX_TRIM).abs() < 1e-6);
        assert_eq!(plan_output_cutoff(&drum), Some(DRUM_OUTPUT_CUTOFF));
    }

    #[test]
    fn waveform_priority_is_pulse_then_saw_then_triangle() {
        assert_eq!(waveform_string(0x40), "pulse");
        assert_eq!(waveform_string(0x20), "sawtooth");
        assert_eq!(waveform_string(0x10), "triangle");
        // tri|saw|pulse all set → pulse wins.
        assert_eq!(waveform_string(0x70), "pulse");
        // tri|saw set → sawtooth wins.
        assert_eq!(waveform_string(0x30), "sawtooth");
        // none set → pulse fallback.
        assert_eq!(waveform_string(0x00), "pulse");
    }

    #[test]
    fn combined_secondary_waveform_pairs_dominant_with_next_bit() {
        // Single tonal waveforms → no second oscillator.
        assert_eq!(combined_secondary_waveform(0x40), None);
        assert_eq!(combined_secondary_waveform(0x20), None);
        assert_eq!(combined_secondary_waveform(0x10), None);
        // pulse+triangle (0x50) and pulse+saw (0x60): pulse is the primary bit.
        assert_eq!(combined_secondary_waveform(0x50), Some("triangle"));
        assert_eq!(combined_secondary_waveform(0x60), Some("sawtooth"));
        // saw+triangle (0x30): sawtooth is the primary bit.
        assert_eq!(combined_secondary_waveform(0x30), Some("triangle"));
        // pulse+saw+triangle (0x70): the two highest-priority bits (pulse, saw).
        assert_eq!(combined_secondary_waveform(0x70), Some("sawtooth"));
        // Noise present → handled by the noise source, never a summed oscillator.
        assert_eq!(combined_secondary_waveform(0xD0), None);
        assert_eq!(combined_secondary_waveform(0x80), None);
    }

    #[test]
    fn every_combined_waveform_class_has_a_model_policy() {
        let masks: Vec<u8> = (1u8..=15).filter(|mask| mask.count_ones() >= 2).collect();
        assert_eq!(masks.len(), 11);
        for model in [SidModel::Mos6581, SidModel::Mos8580] {
            for &mask in &masks {
                assert_ne!(
                    combined_waveform_support(model, mask, false),
                    "not_combined"
                );
            }
        }
    }

    #[test]
    fn rdp_collapses_linear_ramp_to_endpoints() {
        let ramp: Vec<f32> = (0..=10).map(|i| i as f32 / 10.0).collect();
        assert_eq!(rdp_indices(&ramp, 0.01), vec![0, 10]);
    }

    #[test]
    fn rdp_keeps_triangle_turning_point() {
        // Up 0→1 over [0,5], back down 1→0 over [5,10]: the peak at 5 is the
        // only interior anchor a linear reconstruction needs.
        let mut tri: Vec<f32> = (0..=5).map(|i| i as f32 / 5.0).collect();
        tri.extend((1..=5).map(|i| 1.0 - i as f32 / 5.0));
        assert_eq!(rdp_indices(&tri, 0.01), vec![0, 5, 10]);
    }

    #[test]
    fn rdp_coalesces_jitter_within_epsilon() {
        // A flat line dithered by ±0.005 (< epsilon) keeps only the endpoints.
        let jitter = [0.5, 0.503, 0.497, 0.502, 0.498, 0.5];
        assert_eq!(rdp_indices(&jitter, 0.01), vec![0, 5]);
    }

    #[test]
    fn rdp_short_series_kept_verbatim() {
        assert_eq!(rdp_indices(&[], 0.01), Vec::<usize>::new());
        assert_eq!(rdp_indices(&[0.3], 0.01), vec![0]);
        assert_eq!(rdp_indices(&[0.3, 0.7], 0.01), vec![0, 1]);
    }

    #[test]
    fn collapse_spans_per_voice_keeps_ranges_and_drops_empties() {
        let ranges = [(10, 20), (30, 30), (40, 55)];
        // Per-voice (pulse width): one range per note, empty (30,30) dropped.
        assert_eq!(collapse_spans(&ranges, false), vec![(10, 20), (40, 55)]);
    }

    #[test]
    fn collapse_spans_continuous_merges_across_gaps() {
        let ranges = [(10, 20), (40, 55), (70, 90)];
        // Global (cutoff): one contiguous range spanning the rests, so the
        // sweep is sampled through the gaps instead of frozen per note.
        assert_eq!(collapse_spans(&ranges, true), vec![(10, 90)]);
    }

    #[test]
    fn collapse_spans_empty_when_nothing_valid() {
        assert_eq!(collapse_spans(&[(5, 5)], true), Vec::<(u32, u32)>::new());
        assert_eq!(collapse_spans(&[], false), Vec::<(u32, u32)>::new());
    }

    #[test]
    fn interpolate_matches_engine_formulas() {
        // Spot-check each variant at t = 0.5 against synth_sequencer's math.
        assert!((CurveType::Linear.interpolate(0.0, 1.0, 0.5) - 0.5).abs() < 1e-6);
        assert!((CurveType::Step.interpolate(0.2, 0.8, 0.5) - 0.2).abs() < 1e-6);
        // SCurve smoothstep(0.5) = 0.5; quarter point eases below the diagonal.
        assert!((CurveType::SCurve.interpolate(0.0, 1.0, 0.5) - 0.5).abs() < 1e-6);
        assert!(CurveType::SCurve.interpolate(0.0, 1.0, 0.25) < 0.25);
        // Positive strength = slow start (below diagonal); negative = fast start.
        assert!(CurveType::Exponential(50).interpolate(0.0, 1.0, 0.5) < 0.5);
        assert!(CurveType::Exponential(-50).interpolate(0.0, 1.0, 0.5) > 0.5);
    }

    #[test]
    fn fit_curve_prefers_scurve_for_smoothstep() {
        // A pure smoothstep from 0→1: SCurve fits ~exactly, Linear does not.
        let n = 20usize;
        let values: Vec<f32> = (0..=n)
            .map(|i| {
                let t = i as f32 / n as f32;
                t * t * (3.0 - 2.0 * t)
            })
            .collect();
        let (curve, residual) = fit_curve(&values, 0, n);
        assert_eq!(curve, CurveType::SCurve);
        assert!(residual < 1e-3, "SCurve residual {residual} too high");
    }

    #[test]
    fn fit_curve_prefers_exponential_for_power_ease() {
        // A convex power ease t^2.5 (slow start): Exponential should beat Linear.
        let n = 20usize;
        let values: Vec<f32> = (0..=n).map(|i| (i as f32 / n as f32).powf(2.5)).collect();
        let (curve, residual) = fit_curve(&values, 0, n);
        assert!(
            matches!(curve, CurveType::Exponential(s) if s > 0),
            "expected positive Exponential, got {curve:?}"
        );
        assert!(residual < 0.01, "Exponential residual {residual} too high");
    }

    #[test]
    fn fit_curve_keeps_linear_for_straight_ramp() {
        let values: Vec<f32> = (0..=10).map(|i| i as f32 / 10.0).collect();
        assert_eq!(fit_curve(&values, 0, 10).0, CurveType::Linear);
    }

    #[test]
    fn fit_curve_treats_near_linear_ease_as_linear() {
        // t^1.06 fits Exponential strength 3 (exponent 1.06), at the threshold:
        // it must stay Linear rather than earn a near-straight Exponential label.
        let n = 20usize;
        let values: Vec<f32> = (0..=n).map(|i| (i as f32 / n as f32).powf(1.06)).collect();
        assert_eq!(fit_curve(&values, 0, n).0, CurveType::Linear);
    }

    #[test]
    fn fit_segment_curves_merges_smooth_ease_into_one_segment() {
        // RDP on a convex ease yields several linear anchors; the curve fitter
        // should merge them back into a single Exponential segment + terminal.
        let n = 30usize;
        let values: Vec<f32> = (0..=n).map(|i| (i as f32 / n as f32).powf(2.5)).collect();
        let anchors = rdp_indices(&values, AUTOMATION_EPSILON);
        assert!(anchors.len() > 2, "ramp should need ≥ 3 linear anchors");
        let fitted = fit_segment_curves(&values, &anchors, AUTOMATION_EPSILON);
        assert_eq!(fitted.len(), 2, "should merge to one curve + terminal Step");
        assert!(matches!(fitted[0].1, CurveType::Exponential(_)));
        assert_eq!(fitted[1].1, CurveType::Step);
    }

    fn fspec(min: u16, max: u16, resonance: u8, mode: FilterMode) -> FilterSpec {
        FilterSpec::from_state(min, max, Resonance(resonance), mode, SidModel::Mos6581)
    }

    #[test]
    fn filter_spec_maps_cutoff_midpoint_resonance_and_mode() {
        // Midpoint of [0, 2047] = 1023.5, which on the measured 6581 curve sits
        // right in the FC-1024 dip — between (1023, 6000 Hz) and (1024, 4600 Hz),
        // so ~5300 Hz.
        let spec = fspec(
            0,
            2047,
            15,
            FilterMode {
                low_pass: true,
                ..Default::default()
            },
        );
        assert_eq!(spec.kind, "lowpass");
        // Resonance 15 maps to the capped ceiling, not full scale.
        assert!((spec.resonance - RESONANCE_MAX_NORM).abs() < 1e-6);
        assert!(
            (spec.cutoff_hz - 5300.0).abs() < 10.0,
            "cutoff {} not near 5300 Hz (FC-1024 dip midpoint)",
            spec.cutoff_hz
        );

        // Low-pass + high-pass together → notch (checked before the single bits).
        let notch = fspec(
            0,
            0,
            0,
            FilterMode {
                low_pass: true,
                high_pass: true,
                ..Default::default()
            },
        );
        assert_eq!(notch.kind, "notch");

        // Mode priority: band_pass beats high_pass when low_pass is unset.
        let bp = fspec(
            0,
            0,
            0,
            FilterMode {
                band_pass: true,
                high_pass: true,
                ..Default::default()
            },
        );
        assert_eq!(bp.kind, "bandpass");

        // high_pass only.
        let hp = fspec(
            0,
            0,
            0,
            FilterMode {
                high_pass: true,
                ..Default::default()
            },
        );
        assert_eq!(hp.kind, "highpass");

        // No mode bit → lowpass fallback; minimum cutoff clamps to the param floor.
        let none = fspec(0, 0, 0, FilterMode::default());
        assert_eq!(none.kind, "lowpass");
        assert!(none.cutoff_hz >= cutoff_param().min);
    }

    #[test]
    fn filter_leak_is_bounded_to_low_cutoff_6581_lowpass() {
        assert!((filter_leak(420.0, "lowpass", SidModel::Mos6581) - FILTER_LEAK_MAX).abs() < 1e-6);
        assert_eq!(
            filter_leak(FILTER_LEAK_END_HZ, "lowpass", SidModel::Mos6581),
            0.0
        );
        assert_eq!(filter_leak(420.0, "bandpass", SidModel::Mos6581), 0.0);
        assert_eq!(filter_leak(420.0, "lowpass", SidModel::Mos8580), 0.0);
    }

    #[test]
    fn cutoff_curve_matches_resid_measured_points() {
        use SidModel::{Both, Mos6581, Mos8580, Unknown};
        // Measured 6581 knots are returned (near-)exactly at their FC register.
        assert!((cutoff_value_to_hz(256.0, Mos6581) - 250.0).abs() < 0.5);
        assert!((cutoff_value_to_hz(768.0, Mos6581) - 1600.0).abs() < 0.5);
        assert!((cutoff_value_to_hz(1280.0, Mos6581) - 9500.0).abs() < 0.5);
        // The whole point of the change: the old linear 30 Hz…12 kHz model put
        // FC 256 at ~1526 Hz; the measured curve is far darker there.
        assert!(cutoff_value_to_hz(256.0, Mos6581) < 400.0);
        // The FC-1024 dip — 1023 is brighter than 1024.
        assert!(cutoff_value_to_hz(1023.0, Mos6581) > cutoff_value_to_hz(1024.0, Mos6581));
        // Linear interpolation between knots: FC 320 sits between (256, 250 Hz)
        // and (384, 300 Hz) → ~275 Hz.
        let mid = cutoff_value_to_hz(320.0, Mos6581);
        assert!((mid - 275.0).abs() < 1.0, "interp {mid} not ~275 Hz");
        // 8580 is near-linear and brighter than the 6581 at the same FC.
        assert!((cutoff_value_to_hz(256.0, Mos8580) - 1600.0).abs() < 0.5);
        assert!(cutoff_value_to_hz(256.0, Mos8580) > cutoff_value_to_hz(256.0, Mos6581));
        // Both / Unknown fall back to the 6581 curve.
        assert_eq!(
            cutoff_value_to_hz(768.0, Both),
            cutoff_value_to_hz(768.0, Mos6581)
        );
        assert_eq!(
            cutoff_value_to_hz(768.0, Unknown),
            cutoff_value_to_hz(768.0, Mos6581)
        );
        // Out-of-range clamps to the end knots (and Pertylizer's cutoff floor).
        assert!(cutoff_value_to_hz(0.0, Mos6581) >= cutoff_param().min);
        assert!((cutoff_value_to_hz(3000.0, Mos6581) - 18000.0).abs() < 0.5);
    }

    // Per-voice filter/pulse-width derivation now lives in `extract_patches`
    // (`PatchVoiceProfile`); see `shared_patch_carries_per_voice_profiles` in
    // `analysis::timbre::patch`. The exporter consumes the profile, so these
    // values are no longer derived from raw `ProgramFrame` here.

    /// One `ProgramFrame` per entry, setting `voice_index`'s frequency to the
    /// given raw value; everything else default. Frame index = position.
    fn states_with_freqs(voice_index: usize, freqs: &[u16]) -> Vec<ProgramFrame> {
        use crate::trace::FrameIndex;
        freqs
            .iter()
            .enumerate()
            .map(|(i, &f)| {
                let mut s = ProgramFrame {
                    frame: FrameIndex(i as u32),
                    ..Default::default()
                };
                s.voices[voice_index].freq = SidFreq(f);
                s
            })
            .collect()
    }

    fn span(effect: Effect, voice_index: usize, start: u32, end: u32) -> EffectSpan {
        use crate::trace::FrameIndex;
        EffectSpan {
            effect,
            voice: Some(VoiceId::from_index(voice_index)),
            start_frame: FrameIndex(start),
            end_frame: FrameIndex(end),
        }
    }

    /// [`states_with_freqs`] with each frame carrying a full control byte
    /// (gate + waveform), so the forward-model mask sees the frames as
    /// verifiable. `0x41` = gated pulse, `0x81` = gated noise.
    fn gated_states_with_ctrl(voice_index: usize, frames: &[(u16, u8)]) -> Vec<ProgramFrame> {
        use crate::analysis::voice::VoiceState;
        use crate::trace::FrameIndex;
        frames
            .iter()
            .enumerate()
            .map(|(i, &(freq, ctrl))| {
                let regs = [(freq & 0xFF) as u8, (freq >> 8) as u8, 0, 0, ctrl, 0, 0];
                let mut s = ProgramFrame {
                    frame: FrameIndex(i as u32),
                    ..Default::default()
                };
                s.voices[voice_index] = VoiceState::from_regs(&regs);
                s
            })
            .collect()
    }

    fn melodic_event(end: u32, midi: u8) -> NoteEvent {
        use crate::analysis::note::{Cents, GmProgram, MidiNote, Velocity};
        use crate::trace::FrameIndex;
        NoteEvent {
            voice: VoiceId::from_index(0),
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(end)),
            midi: MidiNote(midi),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity(100),
        }
    }

    #[test]
    fn bake_pitch_runs_coalesces_and_holds_through_noise() {
        // 4 frames at A, one 1-frame noise interleave (a Hubbard stab attack),
        // 4 frames at B: two legato-tied runs — the noise frame extends A's
        // run instead of chopping the melody.
        let a = 2000u16;
        let b = 4000u16;
        let mut frames = vec![(a, 0x41u8); 4];
        frames.push((a, 0x81));
        frames.extend([(b, 0x41); 4]);
        let states = gated_states_with_ctrl(0, &frames);
        let event = melodic_event(9, midi_of(a) as u8);
        let mut notes = Vec::new();
        bake_pitch_runs(&mut notes, &event, 0.0, 40, 9, &states, SystemClock::Pal, 0);
        assert_eq!(notes.len(), 2, "two pitch runs");
        assert_eq!(notes[0].pitch, midi_of(a));
        assert_eq!(notes[0].duration, 5 * 40, "noise frame extends the A run");
        assert_eq!(notes[1].pitch, midi_of(b));
        assert!(
            !notes[0].legato && notes[1].legato,
            "the second run is a legato continuation"
        );
    }

    #[test]
    fn bake_pitch_runs_compensates_instrument_detune() {
        // The chip plays ~+40 ct above MIDI 60; a +40 ct-detuned instrument
        // renders pitch+detune, so the bake must pick 60 (not round to 61 and
        // land 40 ct sharp after the instrument transpose).
        let hz = 440.0 * ((60.40 - 69.0) / 12.0f64).exp2();
        let raw = (hz * 16_777_216.0 / 985_248.0).round() as u16;
        let states = gated_states_with_ctrl(0, &[(raw, 0x41); 6]);
        let event = melodic_event(6, 60);
        let mut notes = Vec::new();
        bake_pitch_runs(
            &mut notes,
            &event,
            40.0,
            40,
            6,
            &states,
            SystemClock::Pal,
            0,
        );
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pitch, 60);
    }

    #[test]
    fn bake_pitch_runs_renders_the_moving_release_tail() {
        // 4 gated frames at A, then the driver steps the released register
        // down (B, then C): the bake extends through the moving tail — the
        // descent the ear hears under the ringing envelope becomes pitch runs.
        let a = 4000u16;
        let b = 3500u16;
        let c = 3000u16;
        let mut frames = vec![(a, 0x41u8); 4];
        frames.extend([(b, 0x40); 3]);
        frames.extend([(c, 0x40); 3]);
        let mut states = gated_states_with_ctrl(0, &frames);
        for state in &mut states[4..] {
            state.digital_voices[0].envelope_activity.active_cycles = crate::trace::CpuCycles(1);
        }
        let event = melodic_event(6, midi_of(a) as u8);
        let mut notes = Vec::new();
        bake_pitch_runs(
            &mut notes,
            &event,
            0.0,
            40,
            10,
            &states,
            SystemClock::Pal,
            0,
        );
        assert_eq!(notes.len(), 3, "gated run + two tail runs");
        assert_eq!(notes[1].pitch, midi_of(b));
        assert_eq!(notes[2].pitch, midi_of(c));
        assert!(!notes[0].legato && notes[1].legato && notes[2].legato);

        // Parking after the gate: no tail, the bake still ends at gate-off.
        let mut frames = vec![(a, 0x41u8); 4];
        frames.extend([(b, 0x40); 6]);
        let states = gated_states_with_ctrl(0, &frames);
        let mut notes = Vec::new();
        bake_pitch_runs(
            &mut notes,
            &event,
            0.0,
            40,
            10,
            &states,
            SystemClock::Pal,
            0,
        );
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pitch, midi_of(a));
        assert_eq!(notes[0].duration, 4 * 40);
    }

    #[test]
    fn drop_ungated_keeps_moving_tail_notes_drops_parking_rows() {
        let a = 4000u16;
        let mk = |start_frame: u32| Note {
            id: 0,
            start: start_frame * 40,
            duration: 3 * 40,
            pitch: 60,
            velocity: 1.0,
            track: None,
            expression: None,
            glide: None,
            legato: false,
        };
        let mut fidelity = forward::ResidualCensus::default();

        // A run note living entirely in the released descent survives.
        let mut frames = vec![(a, 0x41u8); 4];
        frames.extend([(3500, 0x40); 3]);
        frames.extend([(3000, 0x40); 3]);
        let mut states = gated_states_with_ctrl(0, &frames);
        for state in &mut states[4..] {
            state.digital_voices[0].envelope_activity.active_cycles = crate::trace::CpuCycles(1);
        }
        let mut notes = vec![mk(4)];
        drop_ungated_notes(&mut notes, 0, 40, &states, 0, &mut fidelity);
        assert_eq!(notes.len(), 1, "a moving-tail note is content");
        assert_eq!(fidelity.events_silent, 0);

        // The same note over a parked released register is dropped.
        let mut frames = vec![(a, 0x41u8); 4];
        frames.extend([(3500, 0x40); 6]);
        let states = gated_states_with_ctrl(0, &frames);
        let mut notes = vec![mk(4)];
        drop_ungated_notes(&mut notes, 0, 40, &states, 0, &mut fidelity);
        assert!(notes.is_empty(), "a parking row is not a note");
        assert_eq!(fidelity.events_silent, 1);
    }

    #[test]
    fn push_melodic_event_degrades_a_wrong_proposal_to_the_bake() {
        // The event claims MIDI 65 but the chip holds ~60 the whole note (the
        // Monty V2 short-figure class): the proposal fails verification and
        // the bake re-emits the chip's pitch.
        let held = 2000u16;
        let states = gated_states_with_ctrl(0, &[(held, 0x41); 10]);
        let wrong = midi_of(held) as u8 + 5;
        let event = melodic_event(10, wrong);
        let mut notes = Vec::new();
        let mut fidelity = forward::ResidualCensus::default();
        push_melodic_event(
            &mut notes,
            &event,
            &states,
            &[],
            pal_timing(),
            VoiceId::from_index(0),
            0,
            10,
            40,
            None,
            false,
            0.0,
            true,
            &mut fidelity,
            None,
        );
        assert_eq!(fidelity.events_degraded, 1);
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].pitch,
            midi_of(held),
            "bake replaces the wrong pitch with the chip's"
        );
    }

    #[test]
    fn push_melodic_event_keeps_a_faithful_proposal() {
        let held = 2000u16;
        let states = gated_states_with_ctrl(0, &[(held, 0x41); 10]);
        let event = melodic_event(10, midi_of(held) as u8);
        let mut notes = Vec::new();
        let mut fidelity = forward::ResidualCensus::default();
        push_melodic_event(
            &mut notes,
            &event,
            &states,
            &[],
            pal_timing(),
            VoiceId::from_index(0),
            0,
            10,
            40,
            None,
            false,
            0.0,
            true,
            &mut fidelity,
            None,
        );
        assert_eq!(fidelity.events_degraded, 0);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pitch, midi_of(held));
    }

    #[test]
    fn overlapping_span_matches_effect_voice_and_range() {
        let v0 = VoiceId::from_index(0);
        let v1 = VoiceId::from_index(1);
        let spans = [
            span(Effect::Vibrato, 0, 10, 20),
            span(Effect::Portamento, 0, 30, 40),
            span(Effect::Vibrato, 1, 10, 20),
        ];
        // Matches effect + voice + overlapping range.
        let hit = overlapping_span(&spans, Effect::Vibrato, v0, 15, 25).unwrap();
        assert_eq!(hit.start_frame.0, 10);
        // Wrong voice is rejected even with overlap.
        assert_eq!(
            overlapping_span(&spans, Effect::Portamento, v1, 30, 40),
            None
        );
        // A note ending before the span starts does not overlap (half-open end).
        assert_eq!(overlapping_span(&spans, Effect::Vibrato, v0, 0, 10), None);
        // Touching the inclusive span end still overlaps.
        assert!(overlapping_span(&spans, Effect::Vibrato, v0, 20, 30).is_some());
    }

    #[test]
    fn vibrato_from_span_recovers_depth_rate_and_shape() {
        // A ±50-unit wobble around 4000 over 16 frames at PAL (50 fps): eight
        // direction changes → four cycles in 0.32 s → ~12.5 Hz.
        let freqs = [
            4000, 4050, 4000, 3950, 4000, 4050, 4000, 3950, 4000, 4050, 4000, 3950, 4000, 4050,
            4000, 3950,
        ];
        let states = states_with_freqs(0, &freqs);
        let s = span(Effect::Vibrato, 0, 0, 15);
        let vib = vibrato_from_span(&states, &s, pal_timing(), 0, FrameIndex(0)).expect("vibrato");
        // Depth = half the 3950..4050 excursion in semitones (~0.022 st each way).
        let expected_depth = (12.0 * (4050.0_f32 / 3950.0).log2()) / 2.0;
        assert!((vib.depth - expected_depth).abs() < 1e-4);
        assert!(vib.rate > 0.0, "rate should be positive");
        assert_eq!(vib.delay, 0.0);
        assert!(matches!(vib.shape, VibratoShape::Triangle));
    }

    #[test]
    fn vibrato_from_span_uses_cia_call_rate() {
        let freqs = [
            4000, 4050, 4000, 3950, 4000, 4050, 4000, 3950, 4000, 4050, 4000, 3950, 4000, 4050,
            4000, 3950,
        ];
        let states = states_with_freqs(0, &freqs);
        let span = span(Effect::Vibrato, 0, 0, 15);
        let vblank = vibrato_from_span(&states, &span, pal_timing(), 0, FrameIndex(0))
            .expect("vblank vibrato");
        let cia = vibrato_from_span(&states, &span, fast_cia_timing(), 0, FrameIndex(0))
            .expect("CIA vibrato");

        assert!(
            (cia.rate - 2.0 * vblank.rate).abs() < 1e-4,
            "twice the call rate must produce twice the measured LFO rate"
        );
    }

    #[test]
    fn vibrato_from_span_rejects_steady_pitch() {
        let states = states_with_freqs(0, &[4000; 12]);
        let s = span(Effect::Vibrato, 0, 0, 11);
        assert!(vibrato_from_span(&states, &s, pal_timing(), 0, FrameIndex(0)).is_none());
    }

    #[test]
    fn vibrato_from_span_rejects_span_touching_silence() {
        // A silent frame (freq 0) inside the span would make the excursion
        // (`cents_between(0, max)`) non-finite; the span must be rejected, never
        // yield a `Vibrato` with an infinite depth.
        let freqs = [4000u16, 4050, 0, 3950, 4000, 4050, 4000, 3950, 4000, 4050];
        let states = states_with_freqs(0, &freqs);
        let s = span(Effect::Vibrato, 0, 0, freqs.len() as u32 - 1);
        assert!(vibrato_from_span(&states, &s, pal_timing(), 0, FrameIndex(0)).is_none());
    }

    #[test]
    fn vibrato_from_span_preserves_delay_and_square_shape() {
        let freqs = [
            4000, 4000, 4000, 4000, 3950, 3950, 4050, 4050, 3950, 3950, 4050, 4050,
        ];
        let states = states_with_freqs(0, &freqs);
        let s = span(Effect::Vibrato, 0, 0, freqs.len() as u32 - 1);
        let vib = vibrato_from_span(&states, &s, pal_timing(), 0, FrameIndex(0)).expect("vibrato");

        let expected_delay = 3.0 * pal_timing().seconds_per_call() as f32 * 1000.0;
        assert!((vib.delay - expected_delay).abs() < 1e-4);
        assert!(matches!(vib.shape, VibratoShape::Square));
    }

    #[test]
    fn vibrato_from_span_preserves_saw_shape() {
        let freqs = [
            4000, 4010, 4020, 4030, 4040, 4000, 4010, 4020, 4030, 4040, 4000, 4010,
        ];
        let states = states_with_freqs(0, &freqs);
        let s = span(Effect::Vibrato, 0, 0, freqs.len() as u32 - 1);
        let vib = vibrato_from_span(&states, &s, pal_timing(), 0, FrameIndex(0)).expect("vibrato");

        assert_eq!(vib.delay, 0.0);
        assert!(matches!(vib.shape, VibratoShape::Saw));
    }

    #[test]
    fn glide_from_span_repitches_to_destination_with_signed_origin() {
        // Slide up an octave: 2000 → 4000 over four PAL raster calls.
        let freqs = [2000, 2500, 3200, 4000];
        let states = states_with_freqs(0, &freqs);
        let s = span(Effect::Portamento, 0, 0, 3);
        let (dest, glide) = glide_from_span(&states, &s, pal_timing(), 0).expect("glide");
        // Destination pitch is the MIDI note of the final frequency.
        let (dest_midi, _) = hertz_to_midi(SidFreq(4000).to_hertz(SystemClock::Pal)).unwrap();
        assert_eq!(dest, u32::from(dest_midi.0));
        // Origin is an octave below the destination → -12 semitones.
        let GlideFrom::Semitones(from) = glide.from;
        assert!((from - (-12.0)).abs() < 1e-3, "from {from} not ~-12");
        let expected_ms = 3.0 / SystemClock::Pal.frame_rate() * 1000.0;
        assert!(
            (f64::from(glide.time) - expected_ms).abs() < 1e-3,
            "time {} not {expected_ms}",
            glide.time
        );
        assert!(matches!(glide.interp, GlideInterp::Continuous));
    }

    #[test]
    fn pitch_plateaus_single_pitch_yields_one_plateau() {
        let states = states_with_freqs(0, &[3000; 10]);
        let plateaus = pitch_plateaus(&states, SystemClock::Pal, 0, 0, 10);
        assert_eq!(plateaus.len(), 1);
        assert_eq!(plateaus[0].len, 10);
    }

    #[test]
    fn pitch_plateaus_splits_two_sustained_pitches() {
        // Six frames an octave apart each → two distinct MIDI plateaus.
        let mut freqs = vec![2000u16; 6];
        freqs.extend([4000u16; 6]);
        let states = states_with_freqs(0, &freqs);
        let plateaus = pitch_plateaus(&states, SystemClock::Pal, 0, 0, 12);
        assert_eq!(plateaus.len(), 2);
        assert_ne!(plateaus[0].pitch, plateaus[1].pitch);
        assert_eq!(plateaus[0].len, 6);
        assert_eq!(plateaus[1].len, 6);
    }

    #[test]
    fn pitch_plateaus_absorbs_short_transient_between_pitches() {
        // A,A,A,A,A,A, C (1-frame transient), B×6 → the lone C frame merges into
        // the preceding A plateau; the result is two plateaus, not three.
        let mut freqs = vec![2000u16; 6];
        freqs.push(8000);
        freqs.extend([4000u16; 6]);
        let states = states_with_freqs(0, &freqs);
        let plateaus = pitch_plateaus(&states, SystemClock::Pal, 0, 0, 13);
        assert_eq!(plateaus.len(), 2);
        assert_eq!(
            plateaus[0].len, 7,
            "transient frame folded into first plateau"
        );
        assert_eq!(plateaus[1].len, 6);
    }

    #[test]
    fn pitch_plateaus_folds_short_leading_run_forward() {
        // Two transient leading frames before a long plateau fold forward into
        // it rather than surviving as their own (impossible-to-tie) plateau.
        let mut freqs = vec![8000u16; 2];
        freqs.extend([4000u16; 8]);
        let states = states_with_freqs(0, &freqs);
        let plateaus = pitch_plateaus(&states, SystemClock::Pal, 0, 0, 10);
        assert_eq!(plateaus.len(), 1);
        assert_eq!(plateaus[0].start, 0);
        assert_eq!(plateaus[0].len, 10);
    }

    #[test]
    fn pitch_plateaus_does_not_fabricate_plateau_from_leading_transients() {
        // Four distinct 1-frame pitches (a slide), then a 6-frame sustain. Their
        // lengths sum past MIN_LEGATO_FRAMES, but none sustains on its own, so
        // only the hold is an anchor → one plateau, never a phantom leading note.
        let mut freqs = vec![1000u16, 1200, 1400, 1600];
        freqs.extend([4000u16; 6]);
        let states = states_with_freqs(0, &freqs);
        let plateaus = pitch_plateaus(&states, SystemClock::Pal, 0, 0, freqs.len() as u32);
        assert_eq!(
            plateaus.len(),
            1,
            "transient frames must not accumulate into their own plateau"
        );
        assert_eq!(plateaus[0].len, freqs.len() as u32);
    }

    /// MIDI note (as the exporter stores it) of a raw SID frequency at PAL.
    fn midi_of(freq: u16) -> u32 {
        let (m, _) = hertz_to_midi(SidFreq(freq).to_hertz(SystemClock::Pal)).unwrap();
        u32::from(m.0)
    }

    /// Run [`push_expressive_notes`] for a single voice-0 note covering all of
    /// `freqs`, with the given effect spans, at PAL / 40 ticks-per-row.
    fn run_expressive(freqs: &[u16], spans: &[EffectSpan]) -> Vec<Note> {
        run_expressive_p(freqs, spans, false)
    }

    /// [`run_expressive`] with an explicit percussion flag (the tuned-tom onset
    /// pitch-drop path).
    fn run_expressive_p(freqs: &[u16], spans: &[EffectSpan], percussion: bool) -> Vec<Note> {
        use crate::analysis::note::{Cents, GmProgram, MidiNote, Velocity};
        use crate::trace::FrameIndex;
        let states = states_with_freqs(0, freqs);
        let end = freqs.len() as u32;
        let event = NoteEvent {
            voice: VoiceId::from_index(0),
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(end)),
            // The note's own gated pitch — its onset frequency — so the
            // `near_gate` guard sees a realistic note (production derives it from
            // the driver / detected note, not a value detached from the trace).
            midi: MidiNote(midi_of(freqs[0]) as u8),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity(100),
        };
        let mut notes = Vec::new();
        push_expressive_notes(
            &mut notes,
            &event,
            &states,
            spans,
            pal_timing(),
            VoiceId::from_index(0),
            0,
            end,
            40,
            None,
            percussion,
        );
        notes
    }

    #[test]
    fn portamento_into_legato_run_composes_glide_and_keeps_later_pitch() {
        // Onset slide 1500→2000, hold A=2000 six frames, step to B=4000 six
        // frames. The slide rides the first note; B survives as a tied note.
        let freqs = [
            1500u16, 1800, 2000, 2000, 2000, 2000, 2000, 2000, 4000, 4000, 4000, 4000, 4000, 4000,
        ];
        let spans = [span(Effect::Portamento, 0, 0, 2)];
        let notes = run_expressive(&freqs, &spans);
        assert_eq!(notes.len(), 2, "A and B both survive as tied notes");
        assert_eq!(notes[0].pitch, midi_of(2000));
        assert_eq!(notes[1].pitch, midi_of(4000));
        assert!(notes[0].glide.is_some(), "onset slide rides the first note");
        assert!(!notes[0].legato, "first note carries the attack");
        assert!(notes[1].legato, "second note is a legato continuation");
        assert_eq!(notes[0].start, 0);
    }

    #[test]
    fn portamento_through_note_renders_as_one_falling_glide() {
        // Monty on the Run blocks #20/#21, #26/#27: a lead gate-on at the top
        // pitch that then slides continuously down for the note's whole length (a
        // Hubbard frequency fall). The top and bottom dwell long enough to be
        // plateau anchors, so without the slide-through guard the note splits into
        // two discrete pitches — and two such falling leads sample the sweep at
        // different points and clash. A `Portamento` span covering the slide must
        // collapse it to one note gliding from the onset down to the tail pitch.
        let top = 6675u16; // G4
        let bottom = 4700u16; // ~D4, ~6 semitones down (melodic, not a drum drop)
        let freqs = [
            top, top, top, top, top, // sustained onset anchor
            6300, 5900, 5500, 5100, // transient sliding frames
            bottom, bottom, bottom, bottom, bottom, // sustained tail anchor
        ];
        // Control: with no portamento span the two anchors split into two notes.
        assert_eq!(
            run_expressive(&freqs, &[]).len(),
            2,
            "unguarded: the two anchors split"
        );
        // Guarded: a portamento span over the slide → one note gliding onset→tail.
        let spans = [span(Effect::Portamento, 0, 0, 13)];
        let notes = run_expressive(&freqs, &spans);
        assert_eq!(notes.len(), 1, "slide-through note is not split");
        assert_eq!(
            notes[0].pitch,
            midi_of(bottom),
            "repitched to the fall destination"
        );
        let (dest, glide) = (
            notes[0].pitch,
            notes[0].glide.expect("carries a fall glide"),
        );
        let GlideFrom::Semitones(s) = glide.from;
        assert!(
            (s - (midi_of(top) as f32 - dest as f32)).abs() < 0.01,
            "glide starts at the onset pitch"
        );
        assert!(!notes[0].legato, "single note does not tie");
    }

    #[test]
    fn percussion_hit_renders_the_onset_pitch_drop() {
        // A Hubbard tom/snare hit: gate on, then a fast downward pitch sweep (the
        // "pew"). No `Portamento` span is detected on so short a hit, so only the
        // percussion path can render the drop — as a glide from the onset down to
        // the tail pitch. A non-percussion note with the same trace stays put.
        let onset = 5000u16;
        let tail = 4000u16; // ~4 semitones down
        // Onset held longest (the plateau/anchor), then one dropped frame — so the
        // onset chirp does not fire (the note opens already on the settled pitch).
        let freqs = [onset, onset, onset, onset, onset, tail];
        // Non-percussion: no chirp, no legato run → plain note at the onset.
        let plain = run_expressive_p(&freqs, &[], false);
        assert_eq!(plain.len(), 1);
        assert_eq!(
            plain[0].pitch,
            midi_of(onset),
            "melodic note keeps its onset"
        );
        assert!(plain[0].glide.is_none());
        // Percussion: the same hit pews down to the tail with a glide.
        let perc = run_expressive_p(&freqs, &[], true);
        assert_eq!(perc.len(), 1, "one hit, not split");
        assert_eq!(
            perc[0].pitch,
            midi_of(tail),
            "repitched to the pew's bottom"
        );
        assert!(perc[0].glide.is_some(), "carries the onset pitch-drop");
    }

    #[test]
    fn short_hard_restart_dip_keeps_onset_pitch() {
        // Nemesis st1 V1 bar 25: a 4-frame F5 note opened by a 2-frame hard-restart
        // dip to D#5. The dip (0x295E) is the longest frame-run, but the note's
        // pitch is its F5 onset (0x2E72 = the event's midi). The onset chirp must
        // settle onto that onset pitch, not the dip — otherwise it repitched the
        // whole note down to D#5 and buried the F5 ("the missing rise").
        let f5 = 0x2E72u16; // MIDI 77
        let ds5 = 0x295Eu16; // MIDI 75
        let notes = run_expressive(&[f5, ds5, ds5, f5], &[]);
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].pitch,
            midi_of(f5),
            "kept the F5 onset, not the dip"
        );
        assert!(
            notes[0].glide.is_none(),
            "no phantom onset chirp onto the dip"
        );
    }

    #[test]
    fn noise_source_is_the_sid_oscillator_noise_bit() {
        // SID noise is the chip's own 23-bit LFSR inside the `sid_oscillator`
        // (measured 0.76 dB from the reSID reference — PoC matrix).
        let m = SidSource::simple(0x8, 2048, SidModel::Mos6581, SystemClock::Pal).module();
        assert_eq!(m.id, "sid-1");
        assert_eq!(m.kind, "sid_oscillator");
        match m.parameters {
            Parameters::SidOscillator(p) => {
                assert_eq!(p.noise, 1.0);
                assert_eq!((p.triangle, p.sawtooth, p.pulse), (0.0, 0.0, 0.0));
            }
            _ => panic!("noise source must emit a sid_oscillator module"),
        }
    }

    #[test]
    fn arpeggio_subnotes_tie_under_one_attack() {
        use crate::analysis::note::{Cents, GmProgram, MidiNote, Velocity};
        use crate::trace::FrameIndex;
        // A 6-frame held note arpeggiating a major triad [0,3,7]; each frame is
        // a distinct pitch, so six sub-notes. They share one SID gate → all but
        // the last tie (legato), the last terminates the run.
        let event = NoteEvent {
            voice: VoiceId::from_index(0),
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(6)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity(100),
        };
        let mut notes = Vec::new();
        expand_arpeggio(&mut notes, &event, &[0, 3, 7], 40, 100, &[]);
        assert_eq!(notes.len(), 6, "one sub-note per frame");
        assert_eq!(
            notes.iter().map(|n| n.pitch).collect::<Vec<_>>(),
            vec![60, 63, 67, 60, 63, 67]
        );
        assert!(!notes[0].legato, "the first sub-note carries the attack");
        assert!(
            notes[1..].iter().all(|n| n.legato),
            "every later sub-note is a legato continuation under one attack"
        );
    }

    /// A note clustered into an arpeggio patch but which the chip does *not*
    /// actually arpeggiate (a percussion body or flat tone folded in by timbre
    /// clustering) must not gain a phantom chord step. This is the
    /// Auf_Wiedersehen_Monty stray-F#3 class — an authored `+20` offset over a
    /// base the chip holds flat would invent a note the tune never plays. The
    /// old per-frame clamp is retired: the forward gate measures the expansion
    /// against the trace and degrades it to the pitch bake, which plays the
    /// chip's held pitch.
    #[test]
    fn phantom_arp_step_degrades_to_the_held_pitch() {
        // The chip holds one steady gated frequency for the whole note.
        let freq = 1900u16;
        let held = midi_of(freq);
        let states = gated_states_with_ctrl(0, &[(freq, 0x41); 12]);
        let event = melodic_event(12, held as u8);
        let mut notes = Vec::new();
        let mut fidelity = forward::ResidualCensus::default();
        // An authored arp with a far `+20` step the chip never plays.
        push_arp_event(
            &mut notes,
            &event,
            &[0, 20],
            40,
            12,
            &states,
            pal_timing(),
            0,
            0.0,
            true,
            &mut fidelity,
        );
        assert_eq!(fidelity.events_degraded, 1, "phantom expansion degrades");
        assert!(
            notes.iter().all(|n| n.pitch == held),
            "the bake plays the chip's held pitch, not phantom chord tones: {:?}",
            notes.iter().map(|n| n.pitch).collect::<Vec<_>>()
        );
    }

    /// An authored row the chip never gates (release-phase parking writes
    /// decoded as a note — Monty's drum-drop `$0CA8` rows) emits nothing:
    /// there is no attack to reproduce, and the retuned release tail is the
    /// previous note's business. This is the stray-F#3 class, judged by the
    /// gate instead of the retired modal floor snap.
    #[test]
    fn ungated_authored_row_is_dropped_silently() {
        // Gate off the whole span; the freq register parks at ~MIDI 54.
        let states = gated_states_with_ctrl(0, &[(3240, 0x40); 4]);
        let event = melodic_event(4, 54);
        let mut notes = Vec::new();
        let mut fidelity = forward::ResidualCensus::default();
        push_melodic_event(
            &mut notes,
            &event,
            &states,
            &[],
            pal_timing(),
            VoiceId::from_index(0),
            0,
            4,
            40,
            None,
            false,
            0.0,
            true,
            &mut fidelity,
            None,
        );
        assert!(notes.is_empty(), "an ungated row must not become a note");
        assert_eq!(fidelity.events_silent, 1);
        assert_eq!(fidelity.events_degraded, 0);
    }

    /// A real arpeggio whose expansion matches the trace keeps the expansion —
    /// the gate only degrades measured divergence.
    #[test]
    fn faithful_arp_expansion_survives_the_gate() {
        // The chip cycles base / base+octave per frame, exactly the offsets.
        let base_freq = 2000u16;
        let octave = base_freq * 2;
        let frames: Vec<(u16, u8)> = (0..12)
            .map(|f| ([base_freq, octave][f % 2], 0x41))
            .collect();
        let states = gated_states_with_ctrl(0, &frames);
        let event = melodic_event(12, midi_of(base_freq) as u8);
        let mut notes = Vec::new();
        let mut fidelity = forward::ResidualCensus::default();
        push_arp_event(
            &mut notes,
            &event,
            &[0, 12],
            40,
            12,
            &states,
            pal_timing(),
            0,
            0.0,
            true,
            &mut fidelity,
        );
        assert_eq!(fidelity.events_degraded, 0);
        assert_eq!(notes.len(), 12, "per-frame expansion kept");
        assert_eq!(notes[0].pitch, midi_of(base_freq));
        assert_eq!(notes[1].pitch, midi_of(octave));
    }

    #[test]
    fn gate_on_end_clamps_staccato_keeps_legato() {
        use crate::trace::FrameIndex;
        let mk = |gates: &[bool]| -> Vec<ProgramFrame> {
            gates
                .iter()
                .enumerate()
                .map(|(i, &g)| {
                    let mut s = ProgramFrame {
                        frame: FrameIndex(i as u32),
                        ..Default::default()
                    };
                    s.voices[0].control.gate = g;
                    s
                })
                .collect()
        };
        // Staccato: gated on for 3 frames, then off — clamps to the gate-off.
        let stac = mk(&[
            true, true, true, false, false, false, false, false, false, false, false,
        ]);
        assert_eq!(
            gate_on_end(&stac, 0, 0, 11),
            3,
            "staccato clamps to gate-off"
        );
        // Legato: gate held the whole row — full length kept.
        let leg = mk(&[true; 11]);
        assert_eq!(
            gate_on_end(&leg, 0, 0, 11),
            11,
            "held note keeps full length"
        );
        // Hard-restart: a one-frame gate dip is a re-attack, not a note-off.
        let hr = mk(&[
            true, true, true, false, true, true, true, true, true, true, true,
        ]);
        assert_eq!(
            gate_on_end(&hr, 0, 0, 11),
            11,
            "one-frame dip is not a note-off"
        );
        // The note's start can precede the real gate-on; pre-gate frames are skipped.
        let pre = mk(&[
            false, false, true, true, true, false, false, false, false, false, false,
        ]);
        assert_eq!(
            gate_on_end(&pre, 0, 0, 11),
            5,
            "skips pre-gate, clamps at gate-off"
        );
    }

    #[test]
    fn gate_on_start_snaps_to_gate_within_window() {
        use crate::trace::FrameIndex;
        let mk = |gates: &[bool]| -> Vec<ProgramFrame> {
            gates
                .iter()
                .enumerate()
                .map(|(i, &g)| {
                    let mut s = ProgramFrame {
                        frame: FrameIndex(i as u32),
                        ..Default::default()
                    };
                    s.voices[0].control.gate = g;
                    s
                })
                .collect()
        };
        // Authored start leads the gate-on by 2 frames — snap forward to it.
        let lead = mk(&[false, false, true, true, true, true, true, true]);
        assert_eq!(
            gate_on_start(&lead, 0, 0, 8),
            2,
            "snaps to the first gate-high frame"
        );
        // Already gate-high at start (legato/held) — unchanged.
        let held = mk(&[true; 8]);
        assert_eq!(gate_on_start(&held, 0, 0, 8), 0, "gate-high start is kept");
        // Gate-on lands past the lead window — the start is trusted as authored.
        let far = mk(&[false, false, false, false, true, true, true, true]);
        assert_eq!(
            gate_on_start(&far, 0, 0, 8),
            0,
            "a lead beyond GATE_ON_LEAD_MAX keeps the authored start"
        );
    }

    /// Slice 4: the drum-drop modal snap is retired — a drum body is judged
    /// by the trace, not by the plan's floor. A body the chip really holds
    /// (even far above the other kicks) is KEPT, while a body the chip does
    /// not hold degrades to the chip's pitches. The stray-F#3 class stays
    /// dead because a wrong body no longer verifies; a real high accent no
    /// longer gets falsified down to the floor.
    #[test]
    fn drum_body_is_judged_by_the_trace_not_the_floor() {
        // The chip holds MIDI ~54 for this hit: an emitted 54 body passes…
        let high = 3240u16; // ≈ MIDI 54
        let states = gated_states_with_ctrl(0, &[(high, 0x41); 8]);
        let event = melodic_event(8, midi_of(high) as u8);
        let mut notes = Vec::new();
        let mut fidelity = forward::ResidualCensus::default();
        push_melodic_event(
            &mut notes,
            &event,
            &states,
            &[],
            pal_timing(),
            VoiceId::from_index(0),
            0,
            8,
            40,
            None,
            true,
            0.0,
            true,
            &mut fidelity,
            None,
        );
        assert_eq!(fidelity.events_degraded, 0);
        assert_eq!(notes[0].pitch, midi_of(high), "trace-backed body kept");

        // …while an emitted 54 body over a chip that holds the kick floor
        // (~34) degrades to the floor pitch — measured, not modal-snapped.
        let low = 992u16; // ≈ MIDI 34
        let states = gated_states_with_ctrl(0, &[(low, 0x41); 8]);
        let event = melodic_event(8, midi_of(high) as u8);
        let mut notes = Vec::new();
        let mut fidelity = forward::ResidualCensus::default();
        push_melodic_event(
            &mut notes,
            &event,
            &states,
            &[],
            pal_timing(),
            VoiceId::from_index(0),
            0,
            8,
            40,
            None,
            true,
            0.0,
            true,
            &mut fidelity,
            None,
        );
        // The plateau-drop proposal already repitches to the traced body (34)
        // and therefore verifies — no degradation needed; had it guessed
        // wrong, the gate would have baked the same pitches. Either way the
        // emitted body is the chip's, with no modal floor involved.
        assert!(
            notes.iter().all(|n| n.pitch == midi_of(low)),
            "the emitted body must be the chip's pitch: {:?}",
            notes.iter().map(|n| n.pitch).collect::<Vec<_>>()
        );
    }

    #[test]
    fn portamento_up_down_gesture_is_not_repitched_to_its_peak() {
        // A pitch gesture that rises to 4000 then falls back, never sustaining.
        let freqs = [
            2000u16, 2400, 2800, 3200, 3600, 4000, 3600, 3200, 2800, 2400, 2000,
        ];
        let spans = [span(Effect::Portamento, 0, 0, 5)]; // the up-slide only
        let notes = run_expressive(&freqs, &spans);
        assert_eq!(notes.len(), 1, "an unsettled gesture stays a single note");
        assert!(notes[0].glide.is_none(), "no glide onto the unsettled peak");
        assert_ne!(
            notes[0].pitch,
            midi_of(4000),
            "note must not be pitched to the gesture's peak"
        );
    }

    /// One `ProgramFrame` per entry, setting the master-volume nibble; everything
    /// else default. Frame index = position.
    fn states_with_volumes(vols: &[u8]) -> Vec<ProgramFrame> {
        use crate::trace::FrameIndex;
        vols.iter()
            .enumerate()
            .map(|(i, &v)| ProgramFrame {
                frame: FrameIndex(i as u32),
                volume: crate::analysis::Volume(v),
                ..Default::default()
            })
            .collect()
    }

    // ---- §A6 tempo / time base ------------------------------------------

    #[test]
    fn fold_bpm_normalizes_octaves_into_musical_window() {
        // A half-time or double-time pulse folds back to the same tempo.
        assert!((fold_bpm(125.0) - 125.0).abs() < 1e-6);
        assert!((fold_bpm(62.5) - 125.0).abs() < 1e-6, "half-time doubles");
        assert!((fold_bpm(250.0) - 125.0).abs() < 1e-6, "double-time halves");
        assert!((fold_bpm(31.25) - 125.0).abs() < 1e-6, "quarter-time ×4");
        // Everything lands inside [BPM_FOLD_MIN, BPM_FOLD_MAX).
        for raw in [30.0, 95.0, 140.0, 175.0, 400.0] {
            let b = fold_bpm(raw);
            assert!((BPM_FOLD_MIN..BPM_FOLD_MAX).contains(&b), "{raw} → {b}");
        }
        // Degenerate input falls back to 125.
        assert!((fold_bpm(0.0) - 125.0).abs() < 1e-6);
        assert!((fold_bpm(f64::NAN) - 125.0).abs() < 1e-6);
    }

    #[test]
    fn detect_frames_per_beat_finds_periodic_onset_pulse() {
        // Onsets every 24 frames (a 125 BPM beat at PAL) over 50 beats.
        let onsets: Vec<u32> = (0..50).map(|k| k * 24).collect();
        let frames_per_beat =
            detect_frames_per_beat(&onsets, 24 * 50).expect("a periodic pulse is detectable");
        // The detected period folds to a 125 BPM beat regardless of which
        // metrical level the autocorrelation latched onto.
        let bpm = fold_bpm(60.0 * SystemClock::Pal.frame_rate() / frames_per_beat);
        assert!((bpm - 125.0).abs() < 1.0, "folded BPM {bpm} not ~125");
    }

    #[test]
    fn detect_frames_per_beat_needs_enough_onsets() {
        // Below MIN_TEMPO_ONSETS → no estimate (caller uses the clock default).
        let onsets: Vec<u32> = (0..5).map(|k| k * 24).collect();
        assert!(detect_frames_per_beat(&onsets, 200).is_none());
    }

    #[test]
    fn derived_time_base_preserves_real_time_across_schedules() {
        let cases = [
            (PlaybackTiming::vblank(SystemClock::Pal), 24, 40),
            (PlaybackTiming::vblank(SystemClock::Ntsc), 30, 32),
            (fast_cia_timing(), 30, MIN_TICKS_PER_FRAME),
        ];
        for (timing, onset_period, expected_ticks_per_frame) in cases {
            let onsets: Vec<u32> = (0..50).map(|index| index * onset_period).collect();
            let frame_count = onset_period * 50;
            let time_base = derive_time_base_from_timing(timing, &onsets, frame_count);
            assert_eq!(time_base.ticks_per_frame, expected_ticks_per_frame);

            let tick = f64::from(frame_count * time_base.ticks_per_frame);
            let seconds = tick * 60.0 / (f64::from(time_base.bpm) * f64::from(TICKS_PER_QUARTER));
            let expected_seconds = f64::from(frame_count) / timing.calls_per_second();
            assert!(
                (seconds - expected_seconds).abs() < 1e-5,
                "schedule {timing:?}: {seconds} != {expected_seconds}"
            );
        }
    }

    // ---- §A5 master-volume contour --------------------------------------

    #[test]
    fn master_volume_lane_tracks_a_swell() {
        // Volume ramps 0 → 15 over 16 frames → an interpolating Global lane.
        let vols: Vec<u8> = (0..16).map(|i| i as u8).collect();
        let states = states_with_volumes(&vols);
        let lane = build_master_volume_lane(&states, 16, 40).expect("a swell yields a lane");
        assert!(matches!(lane.target, AutomationTarget::Global(_)));
        assert!(lane.points.len() >= 2);
        for p in &lane.points {
            assert!((0.0..=1.0).contains(&p.value));
        }
        // A linear ramp decimates to its endpoints.
        assert!((lane.points.first().unwrap().value - 0.0).abs() < 1e-6);
        assert!((lane.points.last().unwrap().value - 1.0).abs() < 1e-6);
    }

    #[test]
    fn master_volume_lane_none_for_constant_volume() {
        let states = states_with_volumes(&[15; 64]);
        assert!(build_master_volume_lane(&states, 64, 40).is_none());
    }

    #[test]
    fn master_volume_lane_none_for_d418_digi_hammer() {
        // A tune hammering $D418 for PCM digi swings the nibble every frame, so
        // decimation can't reduce it; past the cap the lane is dropped rather
        // than modulating the whole mix with the digi stream.
        let vols: Vec<u8> = (0..2000).map(|i| if i % 2 == 0 { 0 } else { 15 }).collect();
        let states = states_with_volumes(&vols);
        assert!(build_master_volume_lane(&states, vols.len() as u32, 40).is_none());
    }

    #[test]
    fn master_volume_lane_serializes_as_global_target() {
        let lane = AutomationLane {
            target: AutomationTarget::Global(GlobalTarget::master_volume()),
            points: vec![
                AutomationPoint {
                    tick: 0,
                    value: 1.0,
                    curve: CurveType::Step,
                },
                AutomationPoint {
                    tick: 400,
                    value: 0.0,
                    curve: CurveType::Step,
                },
            ],
        };
        let v = serde_json::to_value(&lane).unwrap();
        assert_eq!(v["target"]["Global"], "MasterVolume");
        assert!(v["target"].get("Module").is_none());
    }

    #[test]
    fn pitch_lane_serializes_as_placement_relative_track_target() {
        let lane = AutomationLane {
            target: AutomationTarget::Track(TrackTarget::pitch()),
            points: vec![AutomationPoint {
                tick: 0,
                value: 0.5,
                curve: CurveType::Step,
            }],
        };
        let value = serde_json::to_value(&lane).unwrap();
        assert_eq!(value["target"]["Track"]["param"], "Pitch");
        assert!(value["target"]["Track"].get("track").is_none());
    }

    // ---- E3 authored-PWM program replay ---------------------------------

    #[test]
    fn authored_pwm_script_regenerates_driver_staircase() {
        // Monty-style PWM: step every two PAL raster calls, +64 per step,
        // base pw at the bounce floor ($800) → phase origin 0.
        let pwm = AuthoredPwm {
            period_frames: 2,
            step: 0x40,
        };
        let script = authored_pwm_script(pwm, 2048, pal_timing());
        assert_eq!(
            script,
            "let steps_per_sec = 25.062271\n\
             let step_units = 64\n\
             let band_floor = 2048\n\
             let band_span = 1536\n\
             let origin_units = 0\n\
             let base_pw = 2048\n\
             let t = floor(age * steps_per_sec)\n\
             let x = origin_units + step_units * t\n\
             let tri = band_span - abs((x % (2 * band_span)) - band_span)\n\
             out = band_floor + tri - base_pw"
        );

        // Evaluate the script's formula in Rust across a full bounce cycle:
        // it must reflect at $E00 and $800, exactly like the driver.
        let pw_at = |age: f32| {
            let t = (age * 25.0).floor();
            let x = 64.0 * t;
            2048.0 + 1536.0 - ((x % 3072.0) - 1536.0).abs()
        };
        assert_eq!(pw_at(0.0), 2048.0, "starts at the base pw");
        assert_eq!(pw_at(0.04), 2048.0 + 64.0, "one step after 1 period");
        // 24 steps → +1536 = the $E00 ceiling; the next step reflects down.
        assert_eq!(pw_at(24.0 * 0.04), 3584.0, "peaks at the bounce ceiling");
        assert_eq!(pw_at(25.0 * 0.04), 3584.0 - 64.0, "reflects downward");
        assert_eq!(pw_at(48.0 * 0.04), 2048.0, "returns to the floor");
    }

    #[test]
    fn authored_pwm_script_offsets_from_mid_band_base() {
        // A base pw inside the band starts the triangle mid-phase: the offset
        // is still 0 at note-on (script output = pw - base_pw).
        let pwm = AuthoredPwm {
            period_frames: 1,
            step: 0x20,
        };
        let script = authored_pwm_script(pwm, 2816, pal_timing());
        assert!(script.contains("let origin_units = 768"), "{script}");
        assert!(script.contains("let step_units = 32"), "{script}");
        assert!(script.contains("let base_pw = 2816"), "{script}");
        assert!(
            script.ends_with("out = band_floor + tri - base_pw"),
            "{script}"
        );
    }

    #[test]
    fn authored_pwm_script_uses_cia_call_rate() {
        let timing = fast_cia_timing();
        let pwm = AuthoredPwm {
            period_frames: 2,
            step: 0x40,
        };
        let script = authored_pwm_script(pwm, 2048, timing);
        let expected = format!(
            "let steps_per_sec = {}",
            timing.calls_per_second() as f32 / 2.0
        );
        assert!(script.starts_with(&expected), "{script}");
    }

    #[test]
    fn arpeggiator_recipe_uses_cia_call_rate() {
        let timing = fast_cia_timing();
        let arp = Arpeggiator::sid_native(&[0, 4, 7], timing);
        let ArpRate::MilliHz(rate) = arp.rate;
        assert_eq!(rate, (timing.calls_per_second() * 1000.0).round() as u32);
    }

    // ---- §A8 onset chirp ------------------------------------------------

    #[test]
    fn onset_chirp_detects_fast_settle() {
        // Open an octave high (8000), settle to 4000 by frame 2.
        let freqs = [8000u16, 8000, 4000, 4000, 4000, 4000, 4000, 4000];
        let states = states_with_freqs(0, &freqs);
        let settled = midi_of(4000) as u8;
        let event = ev_at(0);
        let glide = onset_chirp(&states, &event, settled, pal_timing(), 0).expect("a chirp");
        let GlideFrom::Semitones(from) = glide.from;
        assert!(from > 1.0, "chirp opens above pitch (from {from})");
        let expected_ms = 2.0 / SystemClock::Pal.frame_rate() * 1000.0;
        assert!(
            (f64::from(glide.time) - expected_ms).abs() < 1e-3,
            "two PAL raster calls = {expected_ms} ms"
        );
        assert!(matches!(glide.interp, GlideInterp::Continuous));
    }

    #[test]
    fn onset_chirp_none_when_note_opens_at_pitch() {
        let freqs = [4000u16; 8];
        let states = states_with_freqs(0, &freqs);
        let settled = midi_of(4000) as u8;
        assert!(onset_chirp(&states, &ev_at(0), settled, pal_timing(), 0).is_none());
    }

    #[test]
    fn onset_chirp_none_when_pitch_never_settles() {
        // A long monotonic slide (never within the chirp window) is not a chirp.
        let freqs = [8000u16; 12];
        let states = states_with_freqs(0, &freqs);
        let settled = midi_of(4000) as u8;
        assert!(onset_chirp(&states, &ev_at(0), settled, pal_timing(), 0).is_none());
    }

    #[test]
    fn chirp_rides_first_note_when_no_portamento() {
        // A note opening with a fast chirp and no portamento span gets a glide.
        let freqs = [8000u16, 8000, 4000, 4000, 4000, 4000, 4000, 4000];
        let notes = run_expressive(&freqs, &[]);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].glide.is_some(), "chirp emitted as an onset glide");
        let GlideFrom::Semitones(from) = notes[0].glide.as_ref().unwrap().from;
        assert!(from > 1.0, "chirp from-offset positive (octave up)");
    }

    /// Corpus-wide note-fidelity census (forward-model gate slice 6,
    /// `docs/forward-model-gate.md`): run the heuristic `--format synth`
    /// pipeline over a `.sid` tree, collect every tune's `note_fidelity`
    /// block, and print the distribution plus the ranked worst lists — the
    /// queue that replaces ear-discovery as the source of the next fidelity
    /// bug. PSID only (the CLI's RSID caveat applies doubly to a bulk sweep).
    ///
    /// `SID_HVSC_ROOT=<C64Music>` selects the tree; `SID_LIMIT` caps the tune
    /// count (default 400); `SID_FRAMES` sets the per-tune window (default
    /// 1500). Run:
    /// `SID_HVSC_ROOT=… cargo test -p sid-analyzer --release --lib measure_note_fidelity_corpus -- --ignored --nocapture`
    #[test]
    #[ignore = "corpus census tool, needs HVSC; run manually"]
    fn measure_note_fidelity_corpus() {
        use crate::emu;
        use crate::header::{self, Format};
        use rayon::prelude::*;

        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping corpus census");
            return;
        };
        let limit: usize = std::env::var("SID_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(400);
        let frames: u32 = std::env::var("SID_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1500);

        let mut paths = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|x| x.to_str()) == Some("sid") {
                    paths.push(p);
                }
            }
        }
        paths.sort();
        paths.truncate(limit);

        #[derive(Clone)]
        struct Row {
            name: String,
            ok: u32,
            fail: u32,
            degraded: u32,
            silent: u32,
            mean_cents: f32,
            uncovered: u32,
        }
        enum Out {
            Row(Box<Row>),
            Skip,
            Timeout,
        }

        fn census_one(bytes: Vec<u8>, name: String, frames: u32) -> Out {
            let Ok(hdr) = header::parse(&bytes) else {
                return Out::Skip;
            };
            if hdr.format != Format::Psid {
                return Out::Skip;
            }
            let subtune = hdr.start_song;
            let Ok(trace) = emu::run(&hdr, &bytes, subtune, frames) else {
                return Out::Skip;
            };
            let clock = SystemClock::from(hdr.flags.clock);
            let timing = crate::emu::PlaybackTiming::vblank(clock);
            let program = AnalyzedSidProgram::from_trace(&hdr, subtune, timing, &trace);
            if program.frames().is_empty() {
                return Out::Skip;
            }
            let Ok(census) = write_synth(&program, &mut std::io::sink()) else {
                return Out::Skip;
            };
            let nf = &census.note_fidelity;
            Out::Row(Box::new(Row {
                name,
                ok: nf.notes_ok,
                fail: nf.notes_fail,
                degraded: nf.events_degraded,
                silent: nf.events_silent,
                mean_cents: nf.mean_cents,
                uncovered: nf.uncovered_gated_frames.iter().sum(),
            }))
        }

        // Per-tune wall-clock deadline: a spinning tune is abandoned (its
        // detached worker finishes on its own, the result is dropped).
        let deadline = std::time::Duration::from_secs(20);
        let outcomes: Vec<Out> = paths
            .par_iter()
            .map(|path| {
                let Ok(bytes) = std::fs::read(path) else {
                    return Out::Skip;
                };
                let name = path
                    .strip_prefix(std::env::var("SID_HVSC_ROOT").unwrap_or_default())
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(census_one(bytes, name, frames));
                });
                rx.recv_timeout(deadline).unwrap_or(Out::Timeout)
            })
            .collect();

        let mut rows: Vec<Row> = Vec::new();
        let (mut skip, mut timeout) = (0u32, 0u32);
        for o in outcomes {
            match o {
                Out::Row(r) => rows.push(*r),
                Out::Skip => skip += 1,
                Out::Timeout => timeout += 1,
            }
        }

        let tunes = rows.len() as u32;
        let (mut ok, mut fail, mut degraded, mut silent, mut uncovered) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut clean = 0u32; // zero fails AND zero uncovered
        for r in &rows {
            ok += u64::from(r.ok);
            fail += u64::from(r.fail);
            degraded += u64::from(r.degraded);
            silent += u64::from(r.silent);
            uncovered += u64::from(r.uncovered);
            if r.fail == 0 && r.uncovered == 0 {
                clean += 1;
            }
        }
        println!("tunes {tunes} (skip {skip} · timeout {timeout}) · frames/tune {frames}");
        println!(
            "notes: {ok} ok · {fail} fail ({pct:.2}%) · events: {degraded} degraded · {silent} silent-dropped",
            pct = 100.0 * fail as f64 / (ok + fail).max(1) as f64
        );
        println!(
            "coverage: {uncovered} uncovered gated frames · clean tunes (0 fail, 0 uncovered): {clean}/{tunes} ({cp:.0}%)",
            cp = 100.0 * f64::from(clean) / f64::from(tunes.max(1))
        );

        rows.sort_by(|a, b| {
            b.fail.cmp(&a.fail).then(
                b.mean_cents
                    .partial_cmp(&a.mean_cents)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        println!("\nworst by melodic fails:");
        for r in rows.iter().take(15) {
            println!(
                "  fail {:>4} · mean {:>6.0} ct · degraded {:>3} · {}",
                r.fail, r.mean_cents, r.degraded, r.name
            );
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.uncovered));
        println!("\nworst by uncovered gated frames (dropped content):");
        for r in rows.iter().take(15) {
            println!(
                "  uncovered {:>5} · fail {:>4} · {}",
                r.uncovered, r.fail, r.name
            );
        }
    }
}
