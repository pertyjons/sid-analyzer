use crate::analysis::note::Cents;
use crate::analysis::voice::SidFreq;
use crate::analysis::{FrameState, MODE_VOL_REG, VoiceId};
use crate::trace::{FrameIndex, Trace};
use serde::Serialize;
use std::fmt;

/// Effects detectable from SID register write patterns. Designed to be
/// composer-agnostic — each variant has a definition rooted in the chip's
/// register semantics, not in a particular composer's idiom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Effect {
    /// Sync bit (bit 1 of voice control register) is set. The voice is
    /// hard-synced to the previous voice's oscillator.
    HardSync,
    /// Ring-modulation bit (bit 2 of voice control register) is set.
    RingMod,
    /// 4-bit PCM digi playback via dense, varying writes to `$D418`.
    Sample,
    /// Pulse-width modulation: the voice's PW register changes on
    /// consecutive frames while the pulse waveform is enabled.
    #[serde(rename = "PWM")]
    Pwm,
    /// Filter cutoff sweep: the cutoff register changes on consecutive
    /// frames.
    FilterSweep,
    /// Filter resonance changes repeatedly while at least one source is routed.
    FilterResonanceSweep,
    /// The active low/band/high-pass combination changes while the filter is audible.
    FilterModeModulation,
    /// Monotonic frequency slide over several frames while the gate is held.
    Portamento,
    /// Frequency oscillates around a center pitch within a bounded range
    /// while the gate is held.
    Vibrato,
    /// Audible level oscillates repeatedly while a voice is held, or through master volume.
    Tremolo,
    /// Frequency cycles between 2-4 fixed pitches at rapid intervals while
    /// the gate is held (typical "fake chord" trick).
    Arpeggio,
    /// Voice 3 is deliberately muted at the mixer while its oscillator is consumed as a source.
    Voice3Modulator,
}

impl Effect {
    pub const ALL: [Self; 12] = [
        Self::HardSync,
        Self::RingMod,
        Self::Sample,
        Self::Pwm,
        Self::FilterSweep,
        Self::FilterResonanceSweep,
        Self::FilterModeModulation,
        Self::Portamento,
        Self::Vibrato,
        Self::Tremolo,
        Self::Arpeggio,
        Self::Voice3Modulator,
    ];
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::HardSync => "HardSync",
            Self::RingMod => "RingMod",
            Self::Sample => "Sample",
            Self::Pwm => "PWM",
            Self::FilterSweep => "FilterSweep",
            Self::FilterResonanceSweep => "FilterResonanceSweep",
            Self::FilterModeModulation => "FilterModeModulation",
            Self::Portamento => "Portamento",
            Self::Vibrato => "Vibrato",
            Self::Tremolo => "Tremolo",
            Self::Arpeggio => "Arpeggio",
            Self::Voice3Modulator => "Voice3Modulator",
        })
    }
}

/// A contiguous run of frames during which an effect was active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct EffectSpan {
    pub effect: Effect,
    /// `Some(_)` for per-voice effects; `None` for chip-global effects
    /// (`Sample`, `FilterSweep`).
    pub voice: Option<VoiceId>,
    pub start_frame: FrameIndex,
    /// Inclusive end frame: the last frame the effect was active.
    pub end_frame: FrameIndex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum VoiceRelation {
    Detune,
    Octave,
    Echo,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct VoiceRelationSpan {
    pub relation: VoiceRelation,
    pub source: VoiceId,
    pub destination: VoiceId,
    pub start_frame: FrameIndex,
    pub end_frame: FrameIndex,
    pub interval: Cents,
}

/// Tunable thresholds for the heuristic detectors. Defaults are chosen to
/// be permissive across composers; tighten per use case.
#[derive(Debug, Clone, Copy)]
#[must_use]
pub struct EffectThresholds {
    /// Min writes to `$D418` within one play frame to count as `Sample`.
    pub sample_writes_per_frame: usize,
    /// Min consecutive frames of pulse-width change to count as `Pwm`.
    pub pwm_min_frames: u32,
    /// Min consecutive frames of cutoff change to count as `FilterSweep`.
    pub filter_sweep_min_frames: u32,
    /// Min consecutive routed resonance changes.
    pub filter_resonance_min_frames: u32,
    /// Min consecutive routed filter-mode changes.
    pub filter_mode_min_frames: u32,
    /// Min length of a monotonic-frequency run to count as `Portamento`.
    pub portamento_min_frames: u32,
    /// Max consecutive held (unchanged-frequency) frames tolerated mid-slide
    /// before a `Portamento` run is cut. Drivers often step the pitch every
    /// few frames or pause briefly on rounding; without slack the run would
    /// fragment below `portamento_min_frames`.
    pub portamento_max_hold_frames: u32,
    /// Min total pitch change across a `Portamento` run.
    pub portamento_min_cents: Cents,
    /// Min length of a gate-held region to evaluate for `Vibrato`.
    pub vibrato_min_frames: u32,
    /// Min direction reversals within a region to count as `Vibrato`.
    pub vibrato_min_reversals: u32,
    /// Max peak-to-peak frequency excursion for `Vibrato` — wider
    /// excursions look more like portamento or instrument changes.
    pub vibrato_max_cents: Cents,
    /// Min held frames and reversals for envelope/master-volume tremolo.
    pub tremolo_min_frames: u32,
    pub tremolo_min_reversals: u32,
    /// Largest held plateau inside a master-volume modulation run.
    pub tremolo_max_hold_frames: u32,
    /// Minimum peak-to-peak envelope excursion on the 8-bit digital envelope.
    pub tremolo_min_level_excursion: u8,
    /// Min length of a gate-held region to evaluate for `Arpeggio`.
    pub arpeggio_min_frames: u32,
    /// Min distinct frequencies in a region to count as `Arpeggio`.
    pub arpeggio_min_distinct: usize,
    /// Max distinct frequencies in a region to count as `Arpeggio`.
    pub arpeggio_max_distinct: usize,
    /// Min total frequency switches within a region to count as `Arpeggio`.
    pub arpeggio_min_changes: u32,
    /// Min simultaneous frames for detune/octave voice relationships.
    pub voice_relation_min_frames: u32,
    /// Largest near-unison interval classified as detune.
    pub detune_max_cents: Cents,
    /// Tolerance around an integer octave relationship.
    pub octave_tolerance_cents: Cents,
    /// Largest onset delay classified as an echo voice.
    pub echo_max_delay_frames: u32,
}

impl Default for EffectThresholds {
    fn default() -> Self {
        Self {
            sample_writes_per_frame: 4,
            pwm_min_frames: 4,
            filter_sweep_min_frames: 4,
            filter_resonance_min_frames: 2,
            filter_mode_min_frames: 2,
            portamento_min_frames: 4,
            portamento_max_hold_frames: 1,
            portamento_min_cents: Cents(50.0),
            vibrato_min_frames: 8,
            vibrato_min_reversals: 3,
            vibrato_max_cents: Cents(100.0),
            tremolo_min_frames: 8,
            tremolo_min_reversals: 3,
            tremolo_max_hold_frames: 3,
            tremolo_min_level_excursion: 8,
            arpeggio_min_frames: 4,
            arpeggio_min_distinct: 2,
            arpeggio_max_distinct: 4,
            arpeggio_min_changes: 3,
            voice_relation_min_frames: 4,
            detune_max_cents: Cents(50.0),
            octave_tolerance_cents: Cents(35.0),
            echo_max_delay_frames: 8,
        }
    }
}

#[must_use]
pub fn detect_voice_relations(
    states: &[FrameState],
    thresholds: EffectThresholds,
) -> Vec<VoiceRelationSpan> {
    let mut relations = Vec::new();
    for source in 0..3 {
        for destination in source + 1..3 {
            detect_parallel_relation(states, source, destination, &thresholds, &mut relations);
        }
    }
    detect_echo_relations(states, &thresholds, &mut relations);
    relations.sort_by_key(|relation| {
        (
            relation.start_frame,
            relation.source,
            relation.destination,
            relation.relation,
        )
    });
    relations
}

fn classify_parallel_relation(
    state: &FrameState,
    source: usize,
    destination: usize,
    thresholds: &EffectThresholds,
) -> Option<(VoiceRelation, Cents)> {
    if !state.voices[source].control.gate || !state.voices[destination].control.gate {
        return None;
    }
    let source_freq = tonal_frequency(state, source)?;
    let destination_freq = tonal_frequency(state, destination)?;
    let interval = cents_between(source_freq, destination_freq);
    let absolute = interval.abs();
    if absolute >= Cents(1.0) && absolute <= thresholds.detune_max_cents {
        return Some((VoiceRelation::Detune, interval));
    }
    let octaves = (absolute.0 / 1_200.0).round();
    let distance = (absolute.0 - octaves * 1_200.0).abs();
    (octaves >= 1.0 && distance <= thresholds.octave_tolerance_cents.0)
        .then_some((VoiceRelation::Octave, interval))
}

fn detect_parallel_relation(
    states: &[FrameState],
    source: usize,
    destination: usize,
    thresholds: &EffectThresholds,
    out: &mut Vec<VoiceRelationSpan>,
) {
    let mut open: Option<(VoiceRelation, usize, Cents)> = None;
    for (index, state) in states.iter().enumerate() {
        let classified = classify_parallel_relation(state, source, destination, thresholds);
        match (open, classified) {
            (Some((kind, start, interval)), Some((next, _))) if kind == next => {
                open = Some((kind, start, interval));
            }
            (Some((kind, start, interval)), next) => {
                push_voice_relation(
                    states,
                    source,
                    destination,
                    kind,
                    start,
                    index.saturating_sub(1),
                    interval,
                    thresholds.voice_relation_min_frames,
                    out,
                );
                open = next.map(|(kind, interval)| (kind, index, interval));
            }
            (None, Some((kind, interval))) => open = Some((kind, index, interval)),
            (None, None) => {}
        }
    }
    if let Some((kind, start, interval)) = open {
        push_voice_relation(
            states,
            source,
            destination,
            kind,
            start,
            states.len().saturating_sub(1),
            interval,
            thresholds.voice_relation_min_frames,
            out,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn push_voice_relation(
    states: &[FrameState],
    source: usize,
    destination: usize,
    relation: VoiceRelation,
    start: usize,
    end: usize,
    interval: Cents,
    minimum_frames: u32,
    out: &mut Vec<VoiceRelationSpan>,
) {
    if end < start || ((end - start + 1) as u32) < minimum_frames {
        return;
    }
    out.push(VoiceRelationSpan {
        relation,
        source: VoiceId::from_index(source),
        destination: VoiceId::from_index(destination),
        start_frame: states[start].frame,
        end_frame: states[end].frame,
        interval,
    });
}

fn detect_echo_relations(
    states: &[FrameState],
    thresholds: &EffectThresholds,
    out: &mut Vec<VoiceRelationSpan>,
) {
    let mut previous_gate = [false; 3];
    let mut onsets: Vec<(usize, usize, SidFreq)> = Vec::new();
    for (index, state) in states.iter().enumerate() {
        for (voice, previous) in previous_gate.iter_mut().enumerate() {
            let gate = state.voices[voice].control.gate;
            if gate
                && !*previous
                && let Some(frequency) = tonal_frequency(state, voice)
            {
                if let Some(&(source_index, source, source_frequency)) =
                    onsets
                        .iter()
                        .rev()
                        .find(|(source_index, source, source_frequency)| {
                            *source != voice
                                && index.saturating_sub(*source_index) > 0
                                && index.saturating_sub(*source_index)
                                    <= thresholds.echo_max_delay_frames as usize
                                && cents_between(*source_frequency, frequency).abs()
                                    <= thresholds.octave_tolerance_cents
                        })
                {
                    let end = states[index..]
                        .iter()
                        .position(|candidate| !candidate.voices[voice].control.gate)
                        .map_or(states.len().saturating_sub(1), |offset| {
                            index + offset.saturating_sub(1)
                        });
                    out.push(VoiceRelationSpan {
                        relation: VoiceRelation::Echo,
                        source: VoiceId::from_index(source),
                        destination: VoiceId::from_index(voice),
                        start_frame: states[source_index].frame,
                        end_frame: states[end].frame,
                        interval: cents_between(source_frequency, frequency),
                    });
                }
                onsets.push((index, voice, frequency));
            }
            *previous = gate;
        }
        onsets.retain(|(onset, _, _)| {
            index.saturating_sub(*onset) <= thresholds.echo_max_delay_frames as usize
        });
    }
}

/// Run all detectors over the trace and analyzed states. Returned spans are
/// sorted by `start_frame` (stable across detectors).
#[must_use]
pub fn detect_effects(
    trace: &Trace,
    states: &[FrameState],
    thresholds: EffectThresholds,
) -> Vec<EffectSpan> {
    let mut out = Vec::new();
    for voice_idx in 0..3 {
        detect_voice_bit(states, voice_idx, Effect::HardSync, &mut out, |s| {
            s.voices[voice_idx].control.sync
        });
        detect_voice_bit(states, voice_idx, Effect::RingMod, &mut out, |s| {
            s.voices[voice_idx].control.ring_mod
        });
        detect_pwm(states, voice_idx, thresholds.pwm_min_frames, &mut out);
        detect_portamento(states, voice_idx, &thresholds, &mut out);
        detect_vibrato(states, voice_idx, &thresholds, &mut out);
        detect_tremolo(states, voice_idx, &thresholds, &mut out);
        detect_arpeggio(states, voice_idx, &thresholds, &mut out);
    }
    detect_sample_playback(trace, thresholds.sample_writes_per_frame, &mut out);
    detect_filter_sweep(states, thresholds.filter_sweep_min_frames, &mut out);
    detect_filter_resonance(states, thresholds.filter_resonance_min_frames, &mut out);
    detect_filter_mode(states, thresholds.filter_mode_min_frames, &mut out);
    detect_master_volume_tremolo(trace, states, &thresholds, &mut out);
    detect_voice3_modulator(trace, states, &mut out);
    out.sort_by_key(|s| s.start_frame);
    out
}

/// Generic helper: emit one `EffectSpan` per contiguous run where `predicate`
/// holds, with a minimum-length filter.
fn emit_runs<T, P, F>(
    items: &[T],
    frame_of: F,
    voice: Option<VoiceId>,
    effect: Effect,
    min_frames: u32,
    mut predicate: P,
    out: &mut Vec<EffectSpan>,
) where
    F: Fn(&T) -> FrameIndex,
    P: FnMut(&T) -> bool,
{
    let mut run_start: Option<FrameIndex> = None;
    let mut run_len: u32 = 0;
    let mut last_frame = FrameIndex(0);
    for item in items {
        if predicate(item) {
            if run_start.is_none() {
                run_start = Some(frame_of(item));
            }
            run_len += 1;
            last_frame = frame_of(item);
        } else if let Some(start) = run_start.take() {
            if run_len >= min_frames {
                out.push(EffectSpan {
                    effect,
                    voice,
                    start_frame: start,
                    end_frame: last_frame,
                });
            }
            run_len = 0;
        }
    }
    if let Some(start) = run_start
        && run_len >= min_frames
    {
        out.push(EffectSpan {
            effect,
            voice,
            start_frame: start,
            end_frame: last_frame,
        });
    }
}

fn detect_voice_bit(
    states: &[FrameState],
    voice_idx: usize,
    effect: Effect,
    out: &mut Vec<EffectSpan>,
    predicate: impl Fn(&FrameState) -> bool,
) {
    emit_runs(
        states,
        |s| s.frame,
        Some(VoiceId::from_index(voice_idx)),
        effect,
        1,
        predicate,
        out,
    );
}

fn detect_pwm(states: &[FrameState], voice_idx: usize, min_frames: u32, out: &mut Vec<EffectSpan>) {
    let mut prev_pw = None;
    emit_runs(
        states,
        |s| s.frame,
        Some(VoiceId::from_index(voice_idx)),
        Effect::Pwm,
        min_frames,
        |s| {
            let v = &s.voices[voice_idx];
            let pulse_on = v.control.waveform.pulse;
            let changed = matches!(prev_pw, Some(p) if p != v.pulse_width);
            prev_pw = Some(v.pulse_width);
            pulse_on && changed
        },
        out,
    );
}

fn detect_filter_sweep(states: &[FrameState], min_frames: u32, out: &mut Vec<EffectSpan>) {
    let mut prev_cutoff = None;
    emit_runs(
        states,
        |s| s.frame,
        None,
        Effect::FilterSweep,
        min_frames,
        // Only count cutoff changes as a sweep when ≥1 source is actually
        // routed through the filter; otherwise the cutoff register writes
        // are inaudible scratch.
        |s| {
            let cutoff = s.filter.cutoff;
            let changed = matches!(prev_cutoff, Some(c) if c != cutoff);
            prev_cutoff = Some(cutoff);
            s.filter.routing.any() && changed
        },
        out,
    );
}

fn detect_filter_resonance(states: &[FrameState], min_frames: u32, out: &mut Vec<EffectSpan>) {
    let mut previous = None;
    emit_runs(
        states,
        |state| state.frame,
        None,
        Effect::FilterResonanceSweep,
        min_frames,
        |state| {
            let changed = previous.is_some_and(|value| value != state.filter.resonance);
            previous = Some(state.filter.resonance);
            state.filter.routing.any() && changed
        },
        out,
    );
}

fn detect_filter_mode(states: &[FrameState], min_frames: u32, out: &mut Vec<EffectSpan>) {
    let mut previous = None;
    emit_runs(
        states,
        |state| state.frame,
        None,
        Effect::FilterModeModulation,
        min_frames,
        |state| {
            let mode = (
                state.filter.mode.low_pass,
                state.filter.mode.band_pass,
                state.filter.mode.high_pass,
            );
            let changed = previous.is_some_and(|value| value != mode);
            previous = Some(mode);
            state.filter.routing.any() && changed
        },
        out,
    );
}

fn detect_sample_playback(trace: &Trace, min_writes: usize, out: &mut Vec<EffectSpan>) {
    const MIN_CROSS_CALL_RATE_HZ: f64 = 200.0;
    let high_call_rate = trace.call_rate.calls_per_second() >= MIN_CROSS_CALL_RATE_HZ;
    let mut start = None;
    let mut end = FrameIndex(0);
    let mut frames = 0_usize;
    let mut has_dense_call = false;
    let mut volume_values = 0_u16;
    let finish = |start: &mut Option<FrameIndex>,
                  end: FrameIndex,
                  frames: &mut usize,
                  has_dense_call: &mut bool,
                  volume_values: &mut u16,
                  out: &mut Vec<EffectSpan>| {
        if let Some(start_frame) = start.take()
            && (*has_dense_call || *frames >= min_writes)
            && volume_values.count_ones() >= 2
        {
            out.push(EffectSpan {
                effect: Effect::Sample,
                voice: None,
                start_frame,
                end_frame: end,
            });
        }
        *frames = 0;
        *has_dense_call = false;
        *volume_values = 0;
    };
    for frame in &trace.frames {
        let mut writes = 0;
        let mut frame_volume_values = 0_u16;
        for write in &frame.writes {
            if write.reg == MODE_VOL_REG {
                writes += 1;
                frame_volume_values |= 1 << (write.value & 0x0f);
            }
        }
        let dense = writes >= min_writes;
        if dense || high_call_rate && writes > 0 {
            start.get_or_insert(frame.frame);
            end = frame.frame;
            frames += 1;
            has_dense_call |= dense;
            volume_values |= frame_volume_values;
        } else {
            finish(
                &mut start,
                end,
                &mut frames,
                &mut has_dense_call,
                &mut volume_values,
                out,
            );
        }
    }
    finish(
        &mut start,
        end,
        &mut frames,
        &mut has_dense_call,
        &mut volume_values,
        out,
    );
}

fn detect_voice3_modulator(trace: &Trace, states: &[FrameState], out: &mut Vec<EffectSpan>) {
    emit_runs(
        states,
        |state| state.frame,
        Some(VoiceId::V3),
        Effect::Voice3Modulator,
        1,
        |state| {
            let voice3 = state.voices[VoiceId::V3.to_index()];
            if !state.filter.mode.voice3_off
                || state.filter.routing.voice3
                || voice3.freq == SidFreq(0)
                || voice3.control.waveform.is_silent()
            {
                return false;
            }
            let read_as_source = trace
                .frames
                .get(state.frame.0 as usize)
                .is_some_and(|frame| {
                    frame
                        .reads
                        .iter()
                        .any(|read| matches!(read.reg.0, 0x1b | 0x1c))
                });
            let hardware_source = state.voices[VoiceId::V1.to_index()].control.sync
                || state.voices[VoiceId::V1.to_index()].control.ring_mod;
            read_as_source || hardware_source
        },
        out,
    );
}

fn reversals_and_range(values: impl IntoIterator<Item = u8>) -> Option<(u32, u8)> {
    let mut previous = None;
    let mut previous_sign = None;
    let mut reversals = 0;
    let mut min = u8::MAX;
    let mut max = 0;
    let mut seen = false;
    for value in values {
        seen = true;
        min = min.min(value);
        max = max.max(value);
        if let Some(previous) = previous {
            let sign = (i16::from(value) - i16::from(previous)).signum();
            if sign != 0 {
                if previous_sign.is_some_and(|old| old != sign) {
                    reversals += 1;
                }
                previous_sign = Some(sign);
            }
        }
        previous = Some(value);
    }
    seen.then_some((reversals, max.saturating_sub(min)))
}

fn detect_tremolo(
    states: &[FrameState],
    voice_idx: usize,
    thresholds: &EffectThresholds,
    out: &mut Vec<EffectSpan>,
) {
    let voice = Some(VoiceId::from_index(voice_idx));
    for_each_gate_region(states, voice_idx, |region| {
        if (region.len() as u32) < thresholds.tremolo_min_frames {
            return;
        }
        let Some((reversals, range)) = reversals_and_range(
            region
                .iter()
                .map(|state| state.digital_voices[voice_idx].envelope.level.0),
        ) else {
            return;
        };
        if reversals >= thresholds.tremolo_min_reversals
            && range >= thresholds.tremolo_min_level_excursion
        {
            out.push(span_over_region(Effect::Tremolo, voice, region));
        }
    });
}

fn detect_master_volume_tremolo(
    trace: &Trace,
    states: &[FrameState],
    thresholds: &EffectThresholds,
    out: &mut Vec<EffectSpan>,
) {
    if out.iter().any(|span| span.effect == Effect::Sample) {
        return;
    }
    if trace.frames.iter().any(|frame| {
        frame
            .writes
            .iter()
            .filter(|write| write.reg == MODE_VOL_REG)
            .count()
            >= thresholds.sample_writes_per_frame
    }) {
        return;
    }
    let maximum_change_interval = thresholds.tremolo_max_hold_frames as usize + 1;
    let mut run_start = None;
    let mut last_change = None;
    for index in 1..states.len() {
        if states[index].volume == states[index - 1].volume {
            continue;
        }
        if last_change.is_some_and(|last| index - last > maximum_change_interval) {
            emit_master_volume_tremolo_run(states, run_start, last_change, thresholds, out);
            run_start = None;
        }
        run_start.get_or_insert(index - 1);
        last_change = Some(index);
    }
    emit_master_volume_tremolo_run(states, run_start, last_change, thresholds, out);
}

fn emit_master_volume_tremolo_run(
    states: &[FrameState],
    start: Option<usize>,
    end: Option<usize>,
    thresholds: &EffectThresholds,
    out: &mut Vec<EffectSpan>,
) {
    let (Some(start), Some(end)) = (start, end) else {
        return;
    };
    let region = &states[start..=end];
    if (region.len() as u32) < thresholds.tremolo_min_frames {
        return;
    }
    if let Some((reversals, range)) = reversals_and_range(region.iter().map(|state| state.volume.0))
        && reversals >= thresholds.tremolo_min_reversals
        && range >= 2
    {
        out.push(span_over_region(Effect::Tremolo, None, region));
    }
}

/// Invoke `f` once per maximal contiguous run of frames where voice
/// `voice_idx` has its gate bit high.
fn for_each_gate_region<F: FnMut(&[FrameState])>(
    states: &[FrameState],
    voice_idx: usize,
    mut f: F,
) {
    let mut start: Option<usize> = None;
    for (i, state) in states.iter().enumerate() {
        let gate = state.voices[voice_idx].control.gate;
        if gate {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            f(&states[s..i]);
        }
    }
    if let Some(s) = start {
        f(&states[s..]);
    }
}

/// Cents from `a` up to `b` (signed), for two non-zero `SidFreq` values.
/// Independent of the system clock because `Hz = SidFreq · Φ2 / 2^24` is linear
/// in `SidFreq`, so the frequency ratio is the same in both units.
pub(crate) fn cents_between(a: SidFreq, b: SidFreq) -> Cents {
    if a == SidFreq(0) || b == SidFreq(0) {
        return Cents(0.0);
    }
    Cents((1200.0 * (f64::from(b.0) / f64::from(a.0)).log2()) as f32)
}

fn tonal_frequency(state: &FrameState, voice_idx: usize) -> Option<SidFreq> {
    let voice = state.voices[voice_idx];
    (voice.freq != SidFreq(0) && !voice.control.waveform.is_noise_only()).then_some(voice.freq)
}

/// Per-voice frequency excursion over a frame slice: the running min/max and
/// the number of direction reversals (a non-zero pitch step whose sign differs
/// from the previous non-zero step). Shared by [`detect_vibrato`] and the
/// exporter's [`measure_vibrato`] so a wobble is measured identically wherever
/// it is read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FreqExcursion {
    pub min: SidFreq,
    pub max: SidFreq,
    pub reversals: u32,
}

pub(crate) fn freq_excursion(frames: &[FrameState], voice_idx: usize) -> FreqExcursion {
    let mut min = SidFreq(u16::MAX);
    let mut max = SidFreq(0);
    let mut reversals: u32 = 0;
    let mut prev_sign: Option<i32> = None;
    let mut prev_freq: Option<SidFreq> = None;
    for state in frames {
        let Some(f) = tonal_frequency(state, voice_idx) else {
            continue;
        };
        min = min.min(f);
        max = max.max(f);
        if let Some(p) = prev_freq {
            let sign = (i32::from(f.0) - i32::from(p.0)).signum();
            if sign != 0 {
                if let Some(prev_s) = prev_sign
                    && sign != prev_s
                {
                    reversals += 1;
                }
                prev_sign = Some(sign);
            }
        }
        prev_freq = Some(f);
    }
    FreqExcursion {
        min,
        max,
        reversals,
    }
}

/// Format-neutral measurement of a `Vibrato` span, derived once here so any
/// backend consumes the same numbers instead of re-walking the frames. `frames`
/// is the inter-frame step count (`end − start`) used as the rate denominator.
/// Returns `None` for a degenerate span (silent, or no reversals).
#[derive(Debug, Clone, Copy)]
pub(crate) struct VibratoMeasure {
    /// Peak-to-peak frequency excursion across the span.
    pub excursion: Cents,
    /// Direction reversals (two per LFO cycle).
    pub reversals: u32,
    /// Inter-frame step count (`end − start`); the rate denominator.
    pub frames: u32,
    /// Stable time between the note onset and the first periodic pitch step.
    pub delay_frames: u32,
    /// Closest supported periodic contour.
    pub shape: VibratoContour,
}

/// Trace-derived vibrato contour, kept format-neutral for all exporters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VibratoContour {
    Sine,
    Triangle,
    Square,
    Saw,
}

fn vibrato_contour(frames: &[FrameState], voice_idx: usize) -> VibratoContour {
    let values: Vec<i32> = frames
        .iter()
        .filter_map(|state| tonal_frequency(state, voice_idx))
        .map(|freq| i32::from(freq.0))
        .collect();
    let mut distinct = values.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let steps: Vec<i32> = values
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .filter(|step| *step != 0)
        .collect();
    let held_steps = values.windows(2).filter(|pair| pair[0] == pair[1]).count();
    if distinct.len() <= 2
        || (distinct.len() <= 3 && held_steps.saturating_mul(5) >= values.len().max(2) * 2)
    {
        return VibratoContour::Square;
    }
    let rises = steps.iter().filter(|step| **step > 0).count();
    let falls = steps.iter().filter(|step| **step < 0).count();
    let short_side = rises.min(falls);
    let long_side = rises.max(falls);
    if short_side > 0 && long_side >= short_side.saturating_mul(3) {
        return VibratoContour::Saw;
    }

    let mean =
        steps.iter().map(|step| f64::from(step.abs())).sum::<f64>() / steps.len().max(1) as f64;
    let variance = steps
        .iter()
        .map(|step| {
            let delta = f64::from(step.abs()) - mean;
            delta * delta
        })
        .sum::<f64>()
        / steps.len().max(1) as f64;
    if mean > 0.0 && variance.sqrt() / mean <= 0.35 {
        VibratoContour::Triangle
    } else {
        VibratoContour::Sine
    }
}

#[must_use]
pub(crate) fn measure_vibrato(
    states: &[FrameState],
    span: &EffectSpan,
    voice_idx: usize,
    note_start: FrameIndex,
) -> Option<VibratoMeasure> {
    let measured_start = span.start_frame.max(note_start);
    let start = measured_start.0 as usize;
    let end = (span.end_frame.0 as usize).min(states.len().checked_sub(1)?);
    if end < start {
        return None;
    }
    let frames = &states[start..=end];
    if frames
        .iter()
        .any(|state| state.voices[voice_idx].freq == SidFreq(0))
    {
        return None;
    }
    let ex = freq_excursion(frames, voice_idx);
    // A silent frame (`min == 0`) would make `cents_between(0, max)` non-finite,
    // so reject any span touching silence — the same guard `detect_vibrato`
    // applies before emitting a span. `min == 0` also covers the all-silent and
    // (with `reversals == 0`) the empty-slice cases.
    if ex.min == SidFreq(0) || ex.min == SidFreq(u16::MAX) || ex.reversals == 0 {
        return None;
    }
    let stable_prefix = frames
        .windows(2)
        .position(|pair| pair[0].voices[voice_idx].freq != pair[1].voices[voice_idx].freq)
        .unwrap_or(0) as u32;
    let delay_frames = measured_start
        .0
        .saturating_sub(note_start.0)
        .saturating_add(stable_prefix);
    let contour_start = stable_prefix.min(frames.len().saturating_sub(1) as u32) as usize;
    Some(VibratoMeasure {
        excursion: cents_between(ex.min, ex.max),
        reversals: ex.reversals,
        frames: (end - start) as u32 - stable_prefix.min((end - start) as u32),
        delay_frames,
        shape: vibrato_contour(&frames[contour_start..], voice_idx),
    })
}

/// Format-neutral measurement of a `Portamento` span: the settled destination
/// frequency, the signed interval back to the origin (negative = slid up into
/// the destination), and the slide's frame count. Derived once here so a glide
/// renderer (synth) and a pitch-bend renderer (MIDI) agree on the slide.
/// Returns `None` when either endpoint is silent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SlideMeasure {
    /// Settled (destination) frequency at the slide's end.
    pub dest: SidFreq,
    /// Signed interval from the destination to the origin.
    pub from: Cents,
    /// Number of frames the slide took.
    pub frames: u32,
}

#[must_use]
pub(crate) fn measure_slide(
    states: &[FrameState],
    span: &EffectSpan,
    voice_idx: usize,
) -> Option<SlideMeasure> {
    let start = states.get(span.start_frame.0 as usize)?.voices[voice_idx].freq;
    let end = states.get(span.end_frame.0 as usize)?.voices[voice_idx].freq;
    if start == SidFreq(0) || end == SidFreq(0) {
        return None;
    }
    Some(SlideMeasure {
        dest: end,
        from: cents_between(end, start),
        frames: span.end_frame.0.saturating_sub(span.start_frame.0),
    })
}

fn span_over_region(effect: Effect, voice: Option<VoiceId>, region: &[FrameState]) -> EffectSpan {
    EffectSpan {
        effect,
        voice,
        start_frame: region.first().map(|s| s.frame).unwrap_or_default(),
        end_frame: region.last().map(|s| s.frame).unwrap_or_default(),
    }
}

fn detect_portamento(
    states: &[FrameState],
    voice_idx: usize,
    th: &EffectThresholds,
    out: &mut Vec<EffectSpan>,
) {
    let voice = Some(VoiceId::from_index(voice_idx));
    for_each_gate_region(states, voice_idx, |region| {
        let tonal: Vec<_> = region
            .iter()
            .enumerate()
            .filter_map(|(index, state)| {
                tonal_frequency(state, voice_idx).map(|freq| (index, freq))
            })
            .collect();
        if tonal.len() < 2 {
            return;
        }
        // Walk the region splitting it into maximal monotonic runs.
        let mut i = 0;
        while i + 1 < tonal.len() {
            let start_freq = tonal[i].1;
            let mut run_dir: i32 = 0;
            let mut j = i;
            let mut holds: u32 = 0;
            while j + 1 < tonal.len() {
                let a = i32::from(tonal[j].1.0);
                let b = i32::from(tonal[j + 1].1.0);
                let d = b - a;
                if d == 0 {
                    holds += 1;
                    if holds > th.portamento_max_hold_frames {
                        break;
                    }
                    j += 1;
                    continue;
                }
                let sign = d.signum();
                if run_dir == 0 {
                    run_dir = sign;
                } else if sign != run_dir {
                    break;
                }
                holds = 0;
                j += 1;
            }
            let run_len = tonal[j].0.saturating_sub(tonal[i].0) as u32;
            let end_freq = tonal[j].1;
            if run_len >= th.portamento_min_frames
                && end_freq != SidFreq(0)
                && cents_between(start_freq, end_freq).abs() >= th.portamento_min_cents
            {
                out.push(EffectSpan {
                    effect: Effect::Portamento,
                    voice,
                    start_frame: region[tonal[i].0].frame,
                    end_frame: region[tonal[j].0].frame,
                });
            }
            // Continue from where this run ended (j is the last frame of the
            // run; advance past it).
            i = if j > i { j } else { j + 1 };
        }
    });
}

fn detect_vibrato(
    states: &[FrameState],
    voice_idx: usize,
    th: &EffectThresholds,
    out: &mut Vec<EffectSpan>,
) {
    let voice = Some(VoiceId::from_index(voice_idx));
    for_each_gate_region(states, voice_idx, |region| {
        if (region.len() as u32) < th.vibrato_min_frames {
            return;
        }
        let ex = freq_excursion(region, voice_idx);
        if ex.min == SidFreq(0) {
            return;
        }
        if cents_between(ex.min, ex.max) > th.vibrato_max_cents {
            return;
        }
        if ex.reversals >= th.vibrato_min_reversals {
            out.push(span_over_region(Effect::Vibrato, voice, region));
        }
    });
}

fn detect_arpeggio(
    states: &[FrameState],
    voice_idx: usize,
    th: &EffectThresholds,
    out: &mut Vec<EffectSpan>,
) {
    let voice = Some(VoiceId::from_index(voice_idx));
    for_each_gate_region(states, voice_idx, |region| {
        if (region.len() as u32) < th.arpeggio_min_frames {
            return;
        }
        let tonal: Vec<_> = region
            .iter()
            .filter_map(|state| tonal_frequency(state, voice_idx))
            .collect();
        let mut sorted = tonal.clone();
        sorted.sort_unstable();
        sorted.dedup();
        let distinct = sorted.len();
        if distinct < th.arpeggio_min_distinct || distinct > th.arpeggio_max_distinct {
            return;
        }
        let changes = tonal
            .windows(2)
            .filter(|window| window[0] != window[1])
            .count() as u32;
        if changes < th.arpeggio_min_changes {
            return;
        }
        out.push(span_over_region(Effect::Arpeggio, voice, region));
    });
}
