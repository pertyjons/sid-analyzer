//! Frame-domain forward model of the synth export's note plans — slice 1 of
//! the forward-model gate (`docs/forward-model-gate.md`).
//!
//! For every note the exporter emits, predict the pitch Pertylizer will play
//! per frame (base pitch + glide + vibrato LFO + pattern `Arpeggiator`) and
//! measure the residual against the trace. **Report-only**: nothing here
//! changes the emitted project; the residuals feed the fidelity census so
//! decomposition errors ("wrong notes") become measurable before slice 2
//! starts enforcing a degradation ladder on them.
//!
//! The module is deliberately self-contained: the exporter lowers each emitted
//! note into a [`NoteSpec`] (plain data, no synth types), so the model is unit
//! testable against synthetic traces and stays decoupled from the serializer
//! structs.
//!
//! Engine-semantics assumptions (to pin against a live Pertylizer render —
//! see the plan's risk list): glide interpolates linearly in semitones over
//! its `time`; vibrato starts at LFO phase zero after `delay`; the
//! `Arpeggiator` restarts its step phase on each note onset. A wrong
//! assumption here shows up as residual on the affected class of notes, not
//! as a wrong export.

use serde::Serialize;

use crate::analysis::note::hertz_to_midi;
use crate::analysis::sid_program::query::ProgramFrame;
use crate::analysis::{SystemClock, VoiceId};
use crate::emu::PlaybackTiming;
use crate::trace::FrameIndex;

/// Residual tolerance per frame, in cents. Half a semitone: comfortably above
/// pitch-analysis scatter and instrument detune (≤ ±100 ct, usually ≪ 50),
/// comfortably below the smallest musical interval error (100 ct). Slice 1
/// measures the distribution; the value is re-derived from it before slice 2
/// enforces anything.
pub(crate) const RESIDUAL_TOL_CENTS: f32 = 50.0;

/// Fraction of a note's verifiable frames that must sit within
/// [`RESIDUAL_TOL_CENTS`] for the note to pass. The complement absorbs
/// transition frames (glide endpoints straddling a frame boundary, LFO
/// phase error) without letting a systematically wrong note through.
pub(crate) const RESIDUAL_OK_FRAME_FRACTION: f32 = 0.9;

/// Onset frames skipped by the shared verification mask: attack transients
/// (hard-restart parking, one-frame drum clicks) are driver mechanics, not
/// pitch intent. Mirrors the exporter's `ARP_ATTACK_SKIP`.
pub(crate) const ATTACK_SKIP_FRAMES: u32 = 2;

/// How many worst-offender notes the census carries.
const WORST_NOTES_KEPT: usize = 10;
/// How many longest uncovered gated spans the census carries.
const WORST_UNCOVERED_SPANS_KEPT: usize = 10;

/// First frame of the gate-off run containing `frame` (walks back to the
/// frame after the last gated one).
pub(crate) fn release_run_start(states: &[ProgramFrame], voice_index: usize, frame: u32) -> u32 {
    let mut f = frame;
    while f > 0
        && states
            .get((f - 1) as usize)
            .is_some_and(|s| !s.voices[voice_index].control.gate)
    {
        f -= 1;
    }
    f
}

/// Exclusive end of the audible release beginning at `off_start`.
///
/// Envelope activity decides whether the tail exists. Frequency motion remains
/// relevant to its pitch representation, but a stationary ringing tail is
/// still content and a retuned envelope at zero is not.
pub(crate) fn release_tail_end(states: &[ProgramFrame], voice_index: usize, off_start: u32) -> u32 {
    let mut f = off_start;
    while let Some(state) = states.get(f as usize) {
        if state.voices[voice_index].control.gate
            || state.digital_voices[voice_index]
                .envelope_activity
                .active_cycles
                .0
                == 0
        {
            break;
        }
        f += 1;
    }
    f
}

/// One emitted note, lowered to the plain data the forward model needs. All
/// pitch fields are in cents (MIDI × 100) so instrument detune composes by
/// addition.
pub(crate) struct NoteSpec {
    /// First active frame (absolute).
    pub start_frame: u32,
    /// Active frame count (exclusive end at `start_frame + frames`).
    pub frames: u32,
    /// The note's sounding pitch in cents: `pitch × 100` plus the
    /// instrument's transpose/detune contribution.
    pub pitch_cents: f32,
    pub glide: Option<GlideSpec>,
    pub vibrato: Option<VibratoSpec>,
    /// The pattern-level `Arpeggiator`, when the note's pattern carries one
    /// (it applies to every note in the pattern).
    pub arp: Option<ArpSpec>,
    /// Per-frame pitch offset supplied by placement-relative track automation.
    /// Empty for notes whose pitch is fully described by the note fields above.
    pub track_pitch_cents: Vec<f32>,
    /// Extra per-frame tolerance (cents) on top of [`RESIDUAL_TOL_CENTS`],
    /// for modulation whose *phase* the model cannot know exactly: a vibrato
    /// prediction at a slightly-off rate drifts against the chip's LFO, so a
    /// vibrato-carrying note grants its depth as slack. Verifies the carrier
    /// (center pitch + glide) instead of failing on LFO phase.
    pub slack_cents: f32,
}

/// A per-note glide: the note opens at `pitch + from_cents` and reaches its
/// own pitch after `time_ms`. The engine (Pertylizer `GlideState`) renders
/// `f(t) = from · (to/from)^t` — **linear in cents** over the glide time.
pub(crate) struct GlideSpec {
    pub from_cents: f32,
    pub time_ms: f32,
}

impl GlideSpec {
    /// The engine's pitch offset (cents above the note) at glide fraction
    /// `frac ∈ [0, 1]`: linear in cents.
    fn engine_offset(&self, frac: f32) -> f32 {
        self.from_cents * (1.0 - frac)
    }

    /// Per-frame tolerance allowance for the glide's *trajectory shape*: the
    /// distance at fraction `frac` between the engine's cents-linear curve
    /// and the chip's Hz-linear slide (SID drivers add a constant frequency
    /// delta per frame). Both are plausible renderings of the same musical
    /// gesture; a trace following either passes, while a trace that does not
    /// glide at all (a mis-decomposed held note) exceeds the envelope and
    /// fails. Zero outside the glide window, so the settled tail is verified
    /// strictly.
    fn shape_tolerance(&self, frac: f32) -> f32 {
        // Chip curve: freq(t) linear from `r·f_target` to `f_target`, where
        // r = 2^(from_cents/1200); offset = 1200·log2(r − frac·(r − 1)).
        let r = (self.from_cents / 1200.0).exp2();
        let hz_linear = 1200.0 * (r - frac * (r - 1.0)).max(f32::EPSILON).log2();
        (hz_linear - self.engine_offset(frac)).abs()
    }
}

/// Per-note vibrato expression: `depth_cents` peak deviation around the
/// note's pitch at `rate_hz`, starting (phase 0) after `delay_ms`.
pub(crate) struct VibratoSpec {
    pub depth_cents: f32,
    pub rate_hz: f32,
    pub delay_ms: f32,
    /// Triangle LFO when set, sine otherwise — the only two shapes the
    /// exporter emits.
    pub triangle: bool,
}

/// The pattern `Arpeggiator`: cyclic semitone offsets stepped at `rate_millihz`,
/// phase restarting on each note onset.
pub(crate) struct ArpSpec {
    pub offsets: Vec<i8>,
    pub rate_millihz: u32,
}

impl NoteSpec {
    /// The per-frame tolerance at `k` frames after onset: the base rule plus
    /// the note's whole-note slack (vibrato phase) plus the glide's
    /// trajectory-shape allowance while inside the glide window.
    fn tolerance_cents(&self, k: u32, timing: PlaybackTiming) -> f32 {
        let mut tol = RESIDUAL_TOL_CENTS + self.slack_cents;
        if let Some(g) = &self.glide
            && g.time_ms > 0.0
        {
            let t_ms = (f64::from(k) * timing.seconds_per_call() * 1000.0) as f32;
            if t_ms < g.time_ms {
                tol += g.shape_tolerance(t_ms / g.time_ms);
            }
        }
        tol
    }

    /// Predicted pitch in cents at `k` frames after the note's onset.
    fn predicted_cents(&self, k: u32, timing: PlaybackTiming) -> f32 {
        let t_ms = (f64::from(k) * timing.seconds_per_call() * 1000.0) as f32;
        let mut cents = self.pitch_cents;
        if let Some(g) = &self.glide
            && g.time_ms > 0.0
            && t_ms < g.time_ms
        {
            cents += g.engine_offset(t_ms / g.time_ms);
        }
        if let Some(v) = &self.vibrato
            && v.rate_hz > 0.0
            && t_ms >= v.delay_ms
        {
            let phase = (t_ms - v.delay_ms) / 1000.0 * v.rate_hz;
            let cycle = phase.fract();
            let lfo = if v.triangle {
                // Zero-crossing start, +1 at ¼ cycle, −1 at ¾.
                if cycle < 0.25 {
                    4.0 * cycle
                } else if cycle < 0.75 {
                    2.0 - 4.0 * cycle
                } else {
                    4.0 * cycle - 4.0
                }
            } else {
                (2.0 * std::f32::consts::PI * cycle).sin()
            };
            cents += v.depth_cents * lfo;
        }
        if let Some(a) = &self.arp
            && !a.offsets.is_empty()
        {
            // Integer step math so a call-rate arp advances exactly one step
            // per captured frame (float flooring at the boundary would be off-by-one).
            let call_millihz = (timing.calls_per_second() * 1000.0).round() as u64;
            let steps = u64::from(k) * u64::from(a.rate_millihz) / call_millihz.max(1);
            let idx = (steps % a.offsets.len() as u64) as usize;
            cents += f32::from(a.offsets[idx]) * 100.0;
        }
        if let Some(offset) = self.track_pitch_cents.get(k as usize) {
            cents += offset;
        }
        cents
    }
}

/// Per-note residual between the forward model and the trace.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct NoteResidual {
    pub start_frame: u32,
    /// Frames that passed the verification mask (gated or inside a moving
    /// release tail, tonal, past the attack skip). Zero-checked at
    /// construction: an unverifiable note yields `None`, not a vacuous pass.
    pub frames_checked: u32,
    pub frames_over_tol: u32,
    pub max_cents: f32,
    pub mean_cents: f32,
}

impl NoteResidual {
    /// The slice-2 acceptance rule (report-only in slice 1): at least
    /// [`RESIDUAL_OK_FRAME_FRACTION`] of verifiable frames within tolerance.
    pub(crate) fn passes(&self) -> bool {
        let ok = self.frames_checked - self.frames_over_tol;
        ok as f32 >= RESIDUAL_OK_FRAME_FRACTION * self.frames_checked as f32
    }
}

/// The authored continuous-PWM program's *carrier*: its bounce band. The PW
/// twin of the pitch model's vibrato treatment (slice 5) — the emitted YAMS
/// script restarts its staircase phase at note-on while some drivers free-run
/// the sweep across notes, so the exact phase is unknowable and perceptually
/// benign. What is audible — and what this verifies — is the band the duty
/// cycle lives in; the sweep *rate* is checked separately
/// ([`pw_step_rate`]).
pub(crate) struct PwBandSpec {
    /// Bottom of the bounce band (raw 12-bit units, `$800` for the Hubbard
    /// sweep the script hardcodes).
    pub floor: f32,
    /// Band height (raw units, `$E00 − $800`).
    pub span: f32,
    /// Allowed excursion outside the band before a frame counts as over.
    pub tol: f32,
}

/// Measure one note span against the program's band — the pulse-width twin
/// of [`note_residual`] (the shared `NoteResidual` fields read as raw pw
/// units here, not cents; the per-frame diff is the distance *outside* the
/// band, zero anywhere inside). Verifiable frames are gated frames whose
/// waveform carries the pulse bit (pulse width is inaudible otherwise).
/// `None` when nothing is verifiable.
pub(crate) fn pw_band_residual(
    spec: &PwBandSpec,
    start_frame: u32,
    frames: u32,
    voice_index: usize,
    states: &[ProgramFrame],
) -> Option<NoteResidual> {
    let mut checked = 0u32;
    let mut over = 0u32;
    let mut max = 0.0f32;
    let mut sum = 0.0f64;
    for k in 0..frames {
        if k < ATTACK_SKIP_FRAMES && frames > ATTACK_SKIP_FRAMES {
            continue;
        }
        let Some(voice) = states
            .get((start_frame + k) as usize)
            .map(|s| &s.voices[voice_index])
        else {
            break;
        };
        if !voice.control.gate || !voice.control.waveform.pulse {
            continue;
        }
        let pw = f32::from(voice.pulse_width.0);
        let diff = (spec.floor - pw)
            .max(pw - (spec.floor + spec.span))
            .max(0.0);
        checked += 1;
        sum += f64::from(diff);
        max = max.max(diff);
        if diff > spec.tol {
            over += 1;
        }
    }
    (checked > 0).then(|| NoteResidual {
        start_frame,
        frames_checked: checked,
        frames_over_tol: over,
        max_cents: max,
        mean_cents: (sum / f64::from(checked)) as f32,
    })
}

/// The trace's mean per-frame pulse-width movement over one note span:
/// `(mean |Δpw|, consecutive-pair count)` across gated pulse frames. The
/// rate half of the phase-blind program check — a program sweeping
/// `step/period` units per frame must see the register actually move at
/// that order of magnitude (a flat register means the driver gated the
/// effect off; a much faster sweep means the decoded parameters do not
/// describe this driver). `None` without at least one consecutive pair.
pub(crate) fn pw_step_rate(
    start_frame: u32,
    frames: u32,
    voice_index: usize,
    states: &[ProgramFrame],
) -> Option<(f32, u32)> {
    let mut pairs = 0u32;
    let mut sum = 0.0f64;
    let mut prev: Option<f32> = None;
    for k in 0..frames {
        let Some(voice) = states
            .get((start_frame + k) as usize)
            .map(|s| &s.voices[voice_index])
        else {
            break;
        };
        if !voice.control.gate || !voice.control.waveform.pulse {
            prev = None;
            continue;
        }
        let pw = f32::from(voice.pulse_width.0);
        if let Some(p) = prev {
            pairs += 1;
            sum += f64::from((pw - p).abs());
        }
        prev = Some(pw);
    }
    (pairs > 0).then(|| ((sum / f64::from(pairs)) as f32, pairs))
}

/// Measure one note against the trace. Returns `None` when no frame passes
/// the verification mask (nothing to verify — counted separately by the
/// census, never as a pass).
pub(crate) fn note_residual(
    spec: &NoteSpec,
    voice_index: usize,
    timing: PlaybackTiming,
    states: &[ProgramFrame],
) -> Option<NoteResidual> {
    let mut checked = 0u32;
    let mut over = 0u32;
    let mut max = 0.0f32;
    let mut sum = 0.0f64;
    // The moving release tail of the gate-off run the scan is currently
    // inside (slice 7): computed once per run, cleared at the next gated
    // frame. Post-gate frames inside the tail verify like gated ones;
    // post-gate frames beyond it stay masked.
    let mut tail: Option<u32> = None;
    for k in 0..spec.frames {
        if k < ATTACK_SKIP_FRAMES && spec.frames > ATTACK_SKIP_FRAMES {
            continue;
        }
        let frame = spec.start_frame + k;
        let Some(voice) = states.get(frame as usize).map(|s| &s.voices[voice_index]) else {
            break;
        };
        if voice.control.gate {
            tail = None;
        } else {
            let end = *tail.get_or_insert_with(|| {
                let run = release_run_start(states, voice_index, frame);
                release_tail_end(states, voice_index, run)
            });
            if frame >= end {
                continue;
            }
        }
        if voice.control.waveform.is_noise_only() || voice.freq.0 == 0 {
            continue;
        }
        let Some((midi, cents)) = hertz_to_midi(voice.freq.to_hertz(timing.clock)) else {
            continue;
        };
        let trace = f32::from(midi.0) * 100.0 + cents.0;
        let diff = (spec.predicted_cents(k, timing) - trace).abs();
        checked += 1;
        sum += f64::from(diff);
        max = max.max(diff);
        if diff > spec.tolerance_cents(k, timing) {
            over += 1;
        }
    }
    (checked > 0).then(|| NoteResidual {
        start_frame: spec.start_frame,
        frames_checked: checked,
        frames_over_tol: over,
        max_cents: max,
        mean_cents: (sum / f64::from(checked)) as f32,
    })
}

/// The slice-2 enforcement check for one emitted event (all the notes one
/// proposal produced for it). Every verifiable emitted note must satisfy its
/// own acceptance rule; otherwise a long correct run could hide a completely
/// wrong one-frame arpeggio step. `None` when nothing is verifiable — the caller
/// keeps the proposal because there is no evidence against it and the bake
/// would have nothing to bake from either.
pub(crate) fn batch_passes(
    specs: &[NoteSpec],
    voice_index: usize,
    timing: PlaybackTiming,
    states: &[ProgramFrame],
) -> Option<bool> {
    let mut verified = false;
    for spec in specs {
        if let Some(r) = note_residual(spec, voice_index, timing, states) {
            verified = true;
            if !r.passes() {
                return Some(false);
            }
        }
    }
    verified.then_some(true)
}

/// A census worst-offender entry: enough to find the note in the export and
/// the trace without re-running anything.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorstNote {
    pub plan: String,
    pub voice: u8,
    pub start_frame: u32,
    pub frames_checked: u32,
    pub frames_over_tol: u32,
    pub max_cents: f32,
    pub mean_cents: f32,
}

/// A contiguous trace span whose audible gated content no emitted note covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct UncoveredGatedSpan {
    pub voice: VoiceId,
    pub start_frame: FrameIndex,
    pub end_frame: FrameIndex,
}

impl UncoveredGatedSpan {
    fn len(self) -> u32 {
        self.end_frame.0.saturating_sub(self.start_frame.0)
    }
}

/// Report-only residual aggregate carried by the fidelity census.
///
/// Two independent signals:
/// - **Note residuals** — do the notes we emit play the pitches the chip
///   played? Bucketed by the acceptance rule, with percussion (whose body
///   model diverges deliberately) counted apart from melodic notes.
/// - **Coverage** — did we emit notes *at all* where the chip plays gated
///   tonal content? `uncovered_gated_frames` is the "trailing content /
///   duration mismatch" measure (the V1 Stab #5 class: decode drops held
///   content the trace shows).
#[derive(Debug, Default, Serialize)]
pub(crate) struct ResidualCensus {
    /// Melodic notes whose residual passes the acceptance rule.
    pub notes_ok: u32,
    /// Melodic notes failing the rule — the slice-2 degradation candidates.
    pub notes_fail: u32,
    /// Notes with no verifiable frame under the mask (all-noise, silent, or
    /// out of trace range).
    pub notes_unverifiable: u32,
    /// Percussion-plan notes, bucketed apart (pass, fail): their pitch-body
    /// model diverges from the chip's zap/alternation by design, so they
    /// must not drown the melodic signal.
    pub percussion_ok: u32,
    pub percussion_fail: u32,
    /// Melodic events whose proposal failed [`batch_passes`] and were
    /// re-emitted as the per-frame pitch bake (the ladder bottom) — the
    /// slice-2 enforcement counter. Counted at emission; the recorded
    /// residuals above then measure the *bake*, not the rejected proposal.
    pub events_degraded: u32,
    /// Events whose span the chip never gates (silent authored rows — a
    /// driver's release-phase parking writes decoded as notes). Emitted as
    /// nothing: the chip has no attack there, and its retuned release tail
    /// is covered by the previous note's own release (slice 4).
    pub events_silent: u32,
    /// Mean of melodic per-note mean residuals (cents), verifiable notes only.
    pub mean_cents: f32,
    /// Gated, tonal, pitched trace frames no emitted note covers, per voice —
    /// content the export silently drops.
    pub uncovered_gated_frames: [u32; 3],
    /// Longest contiguous uncovered spans, so duration-integrity failures can
    /// be located without instrumenting or rerunning the export.
    pub worst_uncovered_gated_spans: Vec<UncoveredGatedSpan>,
    /// Worst melodic offenders by mean residual.
    pub worst: Vec<WorstNote>,
    #[serde(skip)]
    covered: [Vec<bool>; 3],
    #[serde(skip)]
    mean_sum: f64,
}

impl ResidualCensus {
    /// Record one emitted note's residual and mark its span covered.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        plan: &str,
        voice_index: usize,
        percussion: bool,
        spec: &NoteSpec,
        timing: PlaybackTiming,
        states: &[ProgramFrame],
    ) {
        self.mark_covered(voice_index, spec.start_frame, spec.frames, states.len());
        let Some(r) = note_residual(spec, voice_index, timing, states) else {
            if !percussion {
                self.notes_unverifiable += 1;
            }
            return;
        };
        if percussion {
            if r.passes() {
                self.percussion_ok += 1;
            } else {
                self.percussion_fail += 1;
            }
            return;
        }
        if r.passes() {
            self.notes_ok += 1;
        } else {
            self.notes_fail += 1;
        }
        self.mean_sum += f64::from(r.mean_cents);
        self.worst.push(WorstNote {
            plan: plan.to_owned(),
            voice: voice_index as u8 + 1,
            start_frame: r.start_frame,
            frames_checked: r.frames_checked,
            frames_over_tol: r.frames_over_tol,
            max_cents: r.max_cents,
            mean_cents: r.mean_cents,
        });
        self.worst.sort_by(|a, b| {
            b.mean_cents
                .partial_cmp(&a.mean_cents)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.worst.truncate(WORST_NOTES_KEPT);
    }

    fn mark_covered(&mut self, voice_index: usize, start: u32, frames: u32, total: usize) {
        let lane = &mut self.covered[voice_index.min(2)];
        if lane.len() < total {
            lane.resize(total, false);
        }
        let end = (start + frames).min(total as u32);
        for f in start..end {
            lane[f as usize] = true;
        }
    }

    /// Close the census: compute the melodic mean and count the gated tonal
    /// trace frames no note covered.
    pub(crate) fn finalize(&mut self, states: &[ProgramFrame], clock: SystemClock) {
        let verified = self.notes_ok + self.notes_fail;
        self.mean_cents = if verified > 0 {
            (self.mean_sum / f64::from(verified)) as f32
        } else {
            0.0
        };
        self.worst_uncovered_gated_spans.clear();
        for (vi, lane) in self.covered.iter().enumerate() {
            let mut uncovered = 0u32;
            let mut span_start = None;
            for (f, s) in states.iter().enumerate() {
                let v = &s.voices[vi];
                // A gated frame whose measured envelope stayed at zero
                // (ADSR delay bug, slow attack) is not audible content —
                // the export is faithfully silent there too. Register-level
                // gating remains the test for inexact digital frames.
                let audible = !s.digital_state_exact
                    || s.digital_voices[vi].envelope_activity.active_cycles.0 > 0;
                let is_uncovered = v.control.gate
                    && audible
                    && !v.control.waveform.is_noise_only()
                    && v.freq.0 != 0
                    && hertz_to_midi(v.freq.to_hertz(clock)).is_some()
                    && !lane.get(f).copied().unwrap_or(false);
                if is_uncovered {
                    uncovered += 1;
                    span_start.get_or_insert(f as u32);
                } else if let Some(start) = span_start.take() {
                    self.worst_uncovered_gated_spans.push(UncoveredGatedSpan {
                        voice: VoiceId::from_index(vi),
                        start_frame: FrameIndex(start),
                        end_frame: FrameIndex(f as u32),
                    });
                }
            }
            if let Some(start) = span_start {
                self.worst_uncovered_gated_spans.push(UncoveredGatedSpan {
                    voice: VoiceId::from_index(vi),
                    start_frame: FrameIndex(start),
                    end_frame: FrameIndex(states.len() as u32),
                });
            }
            self.uncovered_gated_frames[vi] = uncovered;
        }
        self.worst_uncovered_gated_spans.sort_by(|a, b| {
            b.len()
                .cmp(&a.len())
                .then_with(|| a.start_frame.cmp(&b.start_frame))
                .then_with(|| a.voice.cmp(&b.voice))
        });
        self.worst_uncovered_gated_spans
            .truncate(WORST_UNCOVERED_SPANS_KEPT);
    }

    /// One-line stderr summary appended to the census print.
    pub(crate) fn summary(&self) -> String {
        format!(
            "note fidelity: {ok} ok · {fail} fail · {un} unverifiable · {deg} degraded · \
             {sil} silent-dropped · mean {mc:.1} ct · percussion {pok}/{ptot} · \
             uncovered gated frames {u1}/{u2}/{u3}",
            ok = self.notes_ok,
            fail = self.notes_fail,
            un = self.notes_unverifiable,
            deg = self.events_degraded,
            sil = self.events_silent,
            mc = self.mean_cents,
            pok = self.percussion_ok,
            ptot = self.percussion_ok + self.percussion_fail,
            u1 = self.uncovered_gated_frames[0],
            u2 = self.uncovered_gated_frames[1],
            u3 = self.uncovered_gated_frames[2],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::voice::VoiceState;

    const CLOCK: SystemClock = SystemClock::Pal;

    fn timing() -> PlaybackTiming {
        PlaybackTiming::vblank(CLOCK)
    }

    /// SID freq register for a fractional MIDI pitch (in cents).
    fn freq_for_cents(cents: f32) -> u16 {
        let midi = f64::from(cents) / 100.0;
        let hz = 440.0 * ((midi - 69.0) / 12.0).exp2();
        (hz * 16_777_216.0 / f64::from(CLOCK.phi2_hz())).round() as u16
    }

    /// A gated frame playing `cents` with control byte `ctrl` on voice 1.
    fn frame_at(cents: f32, ctrl: u8) -> ProgramFrame {
        let raw = freq_for_cents(cents);
        let regs = [(raw & 0xFF) as u8, (raw >> 8) as u8, 0, 0, ctrl, 0, 0];
        ProgramFrame {
            voices: [
                VoiceState::from_regs(&regs),
                VoiceState::default(),
                VoiceState::default(),
            ],
            ..ProgramFrame::default()
        }
    }

    fn sounding_release(cents: f32) -> ProgramFrame {
        let mut state = frame_at(cents, PULSE_OFF);
        state.digital_voices[0].envelope_activity.active_cycles = crate::trace::CpuCycles(1);
        state
    }

    const PULSE_GATED: u8 = 0x41;
    const NOISE_GATED: u8 = 0x81;

    fn plain_spec(start: u32, frames: u32, pitch_cents: f32) -> NoteSpec {
        NoteSpec {
            start_frame: start,
            frames,
            pitch_cents,
            glide: None,
            vibrato: None,
            arp: None,
            track_pitch_cents: Vec::new(),
            slack_cents: 0.0,
        }
    }

    #[test]
    fn steady_note_at_the_traced_pitch_passes() {
        let states: Vec<ProgramFrame> = (0..10).map(|_| frame_at(6000.0, PULSE_GATED)).collect();
        let r =
            note_residual(&plain_spec(0, 10, 6000.0), 0, timing(), &states).expect("verifiable");
        assert!(r.passes());
        assert!(r.max_cents < 5.0, "max {} ct", r.max_cents);
        assert_eq!(r.frames_checked, 8, "attack-skip masks the first 2 frames");
    }

    #[test]
    fn wrong_pitch_fails() {
        let states: Vec<ProgramFrame> = (0..10).map(|_| frame_at(6000.0, PULSE_GATED)).collect();
        let r =
            note_residual(&plain_spec(0, 10, 6700.0), 0, timing(), &states).expect("verifiable");
        assert!(!r.passes());
        assert!(r.mean_cents > 600.0);
    }

    #[test]
    fn track_pitch_automation_is_part_of_the_forward_model() {
        let offsets = [0.0, 0.0, 25.0, 50.0, 25.0, 0.0, -25.0, -50.0, -25.0, 0.0];
        let states: Vec<ProgramFrame> = offsets
            .iter()
            .map(|offset| frame_at(6000.0 + offset, PULSE_GATED))
            .collect();
        let mut spec = plain_spec(0, offsets.len() as u32, 6000.0);
        let flat = note_residual(&spec, 0, timing(), &states).expect("verifiable");
        assert!(!flat.passes(), "a static note must not explain the wobble");

        spec.track_pitch_cents = offsets.to_vec();
        let automated = note_residual(&spec, 0, timing(), &states).expect("verifiable");
        assert!(
            automated.passes(),
            "track automation must explain the wobble"
        );
        assert!(
            automated.mean_cents < 1.0,
            "mean {} ct",
            automated.mean_cents
        );
    }

    #[test]
    fn glide_tracks_a_linear_fall_into_the_note() {
        // Trace slides from +1200 ct above the note down to the note over 10
        // frames (200 ms PAL), then holds.
        let time_ms = 200.0f32;
        let states: Vec<ProgramFrame> = (0..20)
            .map(|f| {
                let t = f as f32 * 20.0;
                let off = if t < time_ms {
                    1200.0 * (1.0 - t / time_ms)
                } else {
                    0.0
                };
                frame_at(6000.0 + off, PULSE_GATED)
            })
            .collect();
        let spec = NoteSpec {
            glide: Some(GlideSpec {
                from_cents: 1200.0,
                time_ms,
            }),
            ..plain_spec(0, 20, 6000.0)
        };
        let r = note_residual(&spec, 0, timing(), &states).expect("verifiable");
        assert!(r.passes(), "max {} mean {}", r.max_cents, r.mean_cents);
        // Without the glide the early frames are hundreds of cents off.
        let flat = note_residual(&plain_spec(0, 20, 6000.0), 0, timing(), &states).unwrap();
        assert!(flat.max_cents > 500.0);
    }

    #[test]
    fn glide_shape_envelope_accepts_hz_linear_but_rejects_a_held_note() {
        // A SID driver's fall is linear in FREQUENCY; the engine renders
        // linear-in-cents. Both are the same musical gesture, so a chip-style
        // Hz-linear fall must pass via the shape envelope…
        let from_cents = 1900.0f32; // ~19 semitones above the note
        let time_ms = 160.0; // 8 PAL frames
        let target = 6000.0f32;
        let r = (from_cents / 1200.0).exp2();
        let hz_linear = |frac: f32| target + 1200.0 * (r - frac * (r - 1.0)).log2();
        let mut frames: Vec<ProgramFrame> = (0..8)
            .map(|k| frame_at(hz_linear(k as f32 * 20.0 / time_ms), PULSE_GATED))
            .collect();
        frames.extend((0..8).map(|_| frame_at(target, PULSE_GATED)));
        let spec = |start, n| NoteSpec {
            glide: Some(GlideSpec {
                from_cents,
                time_ms,
            }),
            ..plain_spec(start, n, target)
        };
        let r1 = note_residual(&spec(0, 16), 0, timing(), &frames).expect("verifiable");
        assert!(
            r1.passes(),
            "chip-style Hz-linear fall must pass the shape envelope"
        );

        // …while a trace that does not glide at all (the mis-decomposed held
        // stab: chip holds the origin pitch) exceeds the envelope and fails.
        let held: Vec<ProgramFrame> = (0..8)
            .map(|_| frame_at(target + from_cents, PULSE_GATED))
            .collect();
        let r2 = note_residual(&spec(0, 8), 0, timing(), &held).expect("verifiable");
        assert!(!r2.passes(), "a held note under a glide spec must fail");
    }

    #[test]
    fn triangle_vibrato_tracks_the_traced_wobble() {
        // 5 Hz triangle, ±40 ct: one cycle per 10 PAL frames.
        let depth = 40.0f32;
        let states: Vec<ProgramFrame> = (0..40)
            .map(|f| {
                let cycle = (f as f32 * 5.0 / 50.0).fract();
                let lfo = if cycle < 0.25 {
                    4.0 * cycle
                } else if cycle < 0.75 {
                    2.0 - 4.0 * cycle
                } else {
                    4.0 * cycle - 4.0
                };
                frame_at(6000.0 + depth * lfo, PULSE_GATED)
            })
            .collect();
        let spec = NoteSpec {
            vibrato: Some(VibratoSpec {
                depth_cents: depth,
                rate_hz: 5.0,
                delay_ms: 0.0,
                triangle: true,
            }),
            ..plain_spec(0, 40, 6000.0)
        };
        let r = note_residual(&spec, 0, timing(), &states).expect("verifiable");
        assert!(r.passes(), "max {} mean {}", r.max_cents, r.mean_cents);
        assert!(r.mean_cents < 10.0, "mean {}", r.mean_cents);
    }

    #[test]
    fn arpeggiator_steps_once_per_frame_and_restarts_per_note() {
        // Chip cycles base/base+12 per frame; the processor spec must follow,
        // and a second note starting mid-cycle must restart at offset[0].
        let offsets = vec![0i8, 12];
        let arp = || {
            Some(ArpSpec {
                offsets: offsets.clone(),
                rate_millihz: (timing().calls_per_second() * 1000.0).round() as u32,
            })
        };
        let states: Vec<ProgramFrame> = (0..16)
            .map(|f| frame_at(6000.0 + f32::from(offsets[f % 2]) * 100.0, PULSE_GATED))
            .collect();
        let spec = NoteSpec {
            arp: arp(),
            ..plain_spec(0, 16, 6000.0)
        };
        let r = note_residual(&spec, 0, timing(), &states).expect("verifiable");
        assert!(r.passes(), "max {} ct", r.max_cents);

        // A note whose onset lands on the chip's +12 phase: the processor
        // restarts at offset[0]=0, so every frame is 1200 ct off — the exact
        // divergence `arp_plan_clean`'s cycle rule guards against.
        let spec = NoteSpec {
            arp: arp(),
            ..plain_spec(1, 8, 6000.0)
        };
        let r = note_residual(&spec, 0, timing(), &states).expect("verifiable");
        assert!(!r.passes(), "phase restart must show as residual");
    }

    #[test]
    fn arpeggiator_forward_model_uses_cia_call_rate() {
        let timing = PlaybackTiming {
            cia_timed: true,
            ..PlaybackTiming::vblank(CLOCK)
        }
        .with_cia_period(crate::emu::CiaTimerPeriod::new(9_828));
        let offsets = [0i8, 12];
        let states: Vec<ProgramFrame> = (0..16)
            .map(|f| frame_at(6000.0 + f32::from(offsets[f % 2]) * 100.0, PULSE_GATED))
            .collect();
        let spec = NoteSpec {
            arp: Some(ArpSpec {
                offsets: offsets.to_vec(),
                rate_millihz: (timing.calls_per_second() * 1000.0).round() as u32,
            }),
            ..plain_spec(0, 16, 6000.0)
        };

        let exact = note_residual(&spec, 0, timing, &states).expect("CIA-timed residual");
        assert!(exact.passes(), "one processor step per CIA call must pass");

        let wrong = note_residual(&spec, 0, PlaybackTiming::vblank(CLOCK), &states)
            .expect("vblank residual");
        assert!(
            !wrong.passes(),
            "evaluating the same absolute rate on a vblank timeline must fail"
        );
    }

    #[test]
    fn vibrato_slack_forgives_lfo_phase_drift() {
        // The chip wobbles ±60 ct but the predicted LFO is anti-phase (rate
        // slightly off, worst case): raw diff peaks at ~120 ct. The vibrato
        // slack (its depth) widens the per-frame tolerance so the carrier
        // verifies instead of the phase failing the note.
        let depth = 60.0f32;
        let states: Vec<ProgramFrame> = (0..40)
            .map(|f| {
                let cycle = (f as f32 * 5.0 / 50.0).fract();
                let lfo = (2.0 * std::f32::consts::PI * cycle).sin();
                frame_at(6000.0 + depth * lfo, PULSE_GATED)
            })
            .collect();
        let anti_phase = |slack| NoteSpec {
            vibrato: Some(VibratoSpec {
                depth_cents: depth,
                rate_hz: 5.0,
                delay_ms: 100.0, // half a cycle late: anti-phase
                triangle: false,
            }),
            slack_cents: slack,
            ..plain_spec(0, 40, 6000.0)
        };
        let strict = note_residual(&anti_phase(0.0), 0, timing(), &states).expect("verifiable");
        assert!(!strict.passes(), "anti-phase without slack must fail");
        // Anti-phase diverges by up to 2×depth — the slack the lowering grants.
        let slacked =
            note_residual(&anti_phase(2.0 * depth), 0, timing(), &states).expect("verifiable");
        assert!(slacked.passes(), "slack must forgive the phase drift");
    }

    #[test]
    fn batch_passes_requires_every_verifiable_event_note_to_pass() {
        let states: Vec<ProgramFrame> = (0..20).map(|_| frame_at(6000.0, PULSE_GATED)).collect();
        // Two right notes and nothing else: passes.
        let ok = [plain_spec(0, 10, 6000.0), plain_spec(10, 10, 6000.0)];
        assert_eq!(batch_passes(&ok, 0, timing(), &states), Some(true));
        // One of the two badly wrong: the event fails.
        let bad = [plain_spec(0, 10, 6000.0), plain_spec(10, 10, 7100.0)];
        assert_eq!(batch_passes(&bad, 0, timing(), &states), Some(false));
        // A single wrong arpeggio step may be less than 10 % of the event, but
        // it remains an audible wrong note and cannot hide inside the long run.
        let hidden_step = [plain_spec(0, 19, 6000.0), plain_spec(19, 1, 7100.0)];
        assert_eq!(
            batch_passes(&hidden_step, 0, timing(), &states),
            Some(false)
        );
        // Nothing verifiable: no verdict.
        let silent: Vec<ProgramFrame> = (0..4).map(|_| frame_at(6000.0, 0x40)).collect();
        assert_eq!(batch_passes(&ok[..1], 0, timing(), &silent), None);
    }

    #[test]
    fn noise_frames_are_masked_and_all_noise_is_unverifiable() {
        let mut states: Vec<ProgramFrame> =
            (0..10).map(|_| frame_at(6000.0, PULSE_GATED)).collect();
        states[5] = frame_at(9000.0, NOISE_GATED); // drum parking frame
        let r =
            note_residual(&plain_spec(0, 10, 6000.0), 0, timing(), &states).expect("verifiable");
        assert!(r.passes(), "noise frame must not count against the note");
        assert_eq!(r.frames_checked, 7);

        let noise: Vec<ProgramFrame> = (0..6).map(|_| frame_at(9000.0, NOISE_GATED)).collect();
        assert!(note_residual(&plain_spec(0, 6, 3600.0), 0, timing(), &noise).is_none());
    }

    const PULSE_OFF: u8 = 0x40;

    #[test]
    fn release_tail_ends_at_envelope_zero_and_ignores_parking() {
        let mut states: Vec<ProgramFrame> = (0..5).map(|_| frame_at(6000.0, PULSE_GATED)).collect();
        states.extend((0..3).map(|_| sounding_release(5800.0)));
        states.push(sounding_release(5600.0));
        states.extend((0..10).map(|_| frame_at(5600.0, PULSE_OFF)));
        assert_eq!(release_run_start(&states, 0, 10), 5);
        assert_eq!(
            release_tail_end(&states, 0, 5),
            9,
            "ends when the envelope reaches zero"
        );

        // Register motion with an envelope at zero is parking, not content.
        let mut parked: Vec<ProgramFrame> = (0..5).map(|_| frame_at(6000.0, PULSE_GATED)).collect();
        parked.extend((0..10).map(|_| frame_at(2000.0, PULSE_OFF)));
        assert_eq!(release_tail_end(&parked, 0, 5), 5);
    }

    #[test]
    fn sounding_release_tail_is_verified_and_parking_is_not() {
        // The driver steps the register down through the release (a stab
        // figure): those frames are verifiable, so a held-note spec that
        // ignores the audible descent fails.
        let mut states: Vec<ProgramFrame> = (0..5).map(|_| frame_at(6000.0, PULSE_GATED)).collect();
        states.extend((0..3).map(|_| sounding_release(5800.0)));
        states.push(sounding_release(5600.0));
        states.extend((0..2).map(|_| frame_at(5600.0, PULSE_OFF)));
        let r =
            note_residual(&plain_spec(0, 11, 6000.0), 0, timing(), &states).expect("verifiable");
        assert_eq!(r.frames_checked, 7, "3 gated (post skip) + 4 tail frames");
        assert!(!r.passes(), "the ignored descent must register as residual");

        // Parking stays masked: it cannot fail the note.
        let mut parked: Vec<ProgramFrame> = (0..5).map(|_| frame_at(6000.0, PULSE_GATED)).collect();
        parked.extend((0..6).map(|_| frame_at(2000.0, PULSE_OFF)));
        let r =
            note_residual(&plain_spec(0, 11, 6000.0), 0, timing(), &parked).expect("verifiable");
        assert_eq!(r.frames_checked, 3, "only the gated frames verify");
        assert!(r.passes());
    }

    /// A gated pulse frame with an explicit pulse-width register.
    fn frame_pw(pw: u16, ctrl: u8) -> ProgramFrame {
        let regs = [0, 0x10, (pw & 0xFF) as u8, (pw >> 8) as u8, ctrl, 0, 0];
        ProgramFrame {
            voices: [
                VoiceState::from_regs(&regs),
                VoiceState::default(),
                VoiceState::default(),
            ],
            ..ProgramFrame::default()
        }
    }

    fn band_spec(tol: f32) -> PwBandSpec {
        PwBandSpec {
            floor: 2048.0,
            span: 1536.0,
            tol,
        }
    }

    #[test]
    fn pw_band_accepts_in_band_rejects_out_of_band() {
        let spec = band_spec(2.0 * 224.0 + 64.0);
        // A sweep living inside $800..$E00 passes regardless of phase.
        let in_band: Vec<ProgramFrame> = (0..12)
            .map(|k| frame_pw(2048 + (k % 7) * 224, 0x41))
            .collect();
        let r = pw_band_residual(&spec, 0, 12, 0, &in_band).expect("verifiable");
        assert!(r.passes(), "in-band sweep must pass (max {})", r.max_cents);

        // Monty's variant sweeps $400..$E80 — far below the band floor for
        // much of the cycle: the program would fold its duty cycle wrong.
        let low: Vec<ProgramFrame> = (0..12).map(|_| frame_pw(0x400, 0x41)).collect();
        let r = pw_band_residual(&spec, 0, 12, 0, &low).expect("verifiable");
        assert!(!r.passes(), "an out-of-band register must fail");

        // Non-pulse frames are unverifiable: a triangle-only note gives no
        // verdict on a pulse-width program.
        let tri: Vec<ProgramFrame> = (0..6).map(|_| frame_pw(2048, 0x11)).collect();
        assert!(pw_band_residual(&spec, 0, 6, 0, &tri).is_none());
    }

    #[test]
    fn pw_step_rate_measures_movement_and_flags_flat() {
        // +224/frame sweep: rate ≈ 224 (kept inside the 12-bit register).
        let sweeping: Vec<ProgramFrame> = (0..12).map(|k| frame_pw(1024 + k * 224, 0x41)).collect();
        let (rate, pairs) = pw_step_rate(0, 12, 0, &sweeping).expect("pairs");
        assert_eq!(pairs, 11);
        assert!((rate - 224.0).abs() < 1.0);

        // A flat register measures zero movement — the conditional-PWM
        // "driver gated the effect off" signal.
        let flat: Vec<ProgramFrame> = (0..12).map(|_| frame_pw(2048, 0x41)).collect();
        let (rate, _) = pw_step_rate(0, 12, 0, &flat).expect("pairs");
        assert_eq!(rate, 0.0);
    }

    #[test]
    fn census_counts_and_uncovered_frames() {
        // 20 gated tonal frames; one passing note covers [0, 10) — the tail
        // [10, 20) is uncovered gated content (the stab-tail class).
        let states: Vec<ProgramFrame> = (0..20).map(|_| frame_at(6000.0, PULSE_GATED)).collect();
        let mut census = ResidualCensus::default();
        census.record(
            "V1 test",
            0,
            false,
            &plain_spec(0, 10, 6000.0),
            timing(),
            &states,
        );
        census.finalize(&states, CLOCK);
        assert_eq!(census.notes_ok, 1);
        assert_eq!(census.notes_fail, 0);
        assert_eq!(census.uncovered_gated_frames, [10, 0, 0]);
        assert_eq!(
            census.worst_uncovered_gated_spans,
            [UncoveredGatedSpan {
                voice: VoiceId::V1,
                start_frame: FrameIndex(10),
                end_frame: FrameIndex(20),
            }]
        );

        // A failing note lands in the worst list.
        let mut census = ResidualCensus::default();
        census.record(
            "V1 test",
            0,
            false,
            &plain_spec(0, 20, 7100.0),
            timing(),
            &states,
        );
        census.finalize(&states, CLOCK);
        assert_eq!(census.notes_fail, 1);
        assert_eq!(census.worst.len(), 1);
        assert!(census.worst[0].mean_cents > 1000.0);
        assert_eq!(census.uncovered_gated_frames, [0, 0, 0]);
    }

    #[test]
    fn percussion_buckets_apart_from_melodic() {
        let states: Vec<ProgramFrame> = (0..10).map(|_| frame_at(6000.0, PULSE_GATED)).collect();
        let mut census = ResidualCensus::default();
        census.record(
            "V1 drum",
            0,
            true,
            &plain_spec(0, 10, 3000.0),
            timing(),
            &states,
        );
        census.finalize(&states, CLOCK);
        assert_eq!(census.percussion_fail, 1);
        assert_eq!(census.notes_fail, 0, "percussion must not pollute melodic");
        assert!(census.worst.is_empty());
    }
}
