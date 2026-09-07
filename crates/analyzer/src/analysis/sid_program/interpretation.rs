use super::evidence::Evidence;
use super::ids::SignalInterpretationId;
use super::signal::{EventSignal, SignalPoint};
use super::time::SourceSpan;
use crate::analysis::VoiceId;
use crate::trace::ChipCycle;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
#[must_use]
pub enum InterpretedSignalSource {
    VoiceFrequency { voice: VoiceId },
    VoicePulseWidth { voice: VoiceId },
    ChipCutoff,
    MasterVolume,
    EnvelopeLevel { voice: VoiceId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum SignalInterpretationKind {
    Constant,
    Ramp,
    Periodic,
    Table,
    Envelope,
    BoundedScript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct InterpretedValue(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SignalStepIndex(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct InterpretedSignalStep {
    pub at: ChipCycle,
    pub duration: ChipCycle,
    pub value: InterpretedValue,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SignalInterpretation {
    pub id: SignalInterpretationId,
    pub source: InterpretedSignalSource,
    pub span: SourceSpan,
    pub kind: SignalInterpretationKind,
    pub steps: Vec<InterpretedSignalStep>,
    pub loop_point: Option<SignalStepIndex>,
    pub initial_position: SignalStepIndex,
    pub evidence: Vec<Evidence>,
}

pub(super) fn from_event_signal<T: Copy>(
    id: SignalInterpretationId,
    source: InterpretedSignalSource,
    signal: &EventSignal<T>,
    end: ChipCycle,
    value: impl Fn(T) -> i64,
) -> Option<SignalInterpretation> {
    let first = signal.0.first()?;
    let span = SourceSpan {
        start: first.at,
        end: end.max(first.at),
    };
    let values: Vec<_> = signal
        .0
        .iter()
        .map(|point| InterpretedValue(value(point.value.value)))
        .collect();
    let base_kind = classify(&values);
    let loop_start = if matches!(
        base_kind,
        SignalInterpretationKind::Constant | SignalInterpretationKind::Ramp
    ) {
        None
    } else {
        find_repeating_suffix(&values)
    };
    let kind = if loop_start.is_some() {
        SignalInterpretationKind::Periodic
    } else {
        base_kind
    };
    let loop_point = loop_start.map(SignalStepIndex);
    Some(SignalInterpretation {
        id,
        source,
        span,
        kind,
        steps: make_steps(&signal.0, end, value),
        loop_point,
        initial_position: SignalStepIndex(0),
        evidence: first.value.evidence.clone(),
    })
}

pub(super) fn envelope(
    id: SignalInterpretationId,
    voice: VoiceId,
    points: &[(ChipCycle, u8, Vec<Evidence>)],
    end: ChipCycle,
) -> Option<SignalInterpretation> {
    let first = points.first()?;
    Some(SignalInterpretation {
        id,
        source: InterpretedSignalSource::EnvelopeLevel { voice },
        span: SourceSpan {
            start: first.0,
            end: end.max(first.0),
        },
        kind: SignalInterpretationKind::Envelope,
        steps: points
            .iter()
            .enumerate()
            .map(|(index, (at, level, evidence))| InterpretedSignalStep {
                at: *at,
                duration: ChipCycle(
                    points
                        .get(index + 1)
                        .map_or(end, |next| next.0)
                        .0
                        .saturating_sub(at.0),
                ),
                value: InterpretedValue(i64::from(*level)),
                evidence: evidence.clone(),
            })
            .collect(),
        loop_point: None,
        initial_position: SignalStepIndex(0),
        evidence: first.2.clone(),
    })
}

fn make_steps<T: Copy>(
    points: &[SignalPoint<T>],
    end: ChipCycle,
    value: impl Fn(T) -> i64,
) -> Vec<InterpretedSignalStep> {
    points
        .iter()
        .enumerate()
        .map(|(index, point)| InterpretedSignalStep {
            at: point.at,
            duration: ChipCycle(
                points
                    .get(index + 1)
                    .map_or(end, |next| next.at)
                    .0
                    .saturating_sub(point.at.0),
            ),
            value: InterpretedValue(value(point.value.value)),
            evidence: point.value.evidence.clone(),
        })
        .collect()
}

fn classify(values: &[InterpretedValue]) -> SignalInterpretationKind {
    if values.windows(2).all(|pair| pair[0] == pair[1]) {
        SignalInterpretationKind::Constant
    } else if values.len() >= 3
        && values
            .windows(2)
            .map(|pair| pair[1].0 - pair[0].0)
            .collect::<Vec<_>>()
            .windows(2)
            .all(|pair| pair[0] == pair[1])
    {
        SignalInterpretationKind::Ramp
    } else if values.len() <= 256 {
        SignalInterpretationKind::Table
    } else {
        SignalInterpretationKind::BoundedScript
    }
}

fn find_repeating_suffix(values: &[InterpretedValue]) -> Option<u32> {
    for period in 1..=values.len().saturating_div(2).min(256) {
        let tail = values.len() - period;
        let previous = tail - period;
        if values[previous..tail] != values[tail..] {
            continue;
        }
        let mut start = previous;
        while start >= period && values[start - period..start] == values[tail..] {
            start -= period;
        }
        return u32::try_from(start).ok();
    }
    None
}
