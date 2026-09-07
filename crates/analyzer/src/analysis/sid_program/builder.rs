use super::evidence::{
    ConfidencePermille, Evidence, EvidenceValidity, Evidenced, Provenance, SourceReference,
};
use super::ids::{SignalInterpretationId, SignalPointId, SoundRegionId};
use super::interpretation::{InterpretedSignalSource, SignalInterpretation};
use super::observable::RenderObservableSet;
use super::region::{SoundRegion, SoundRegionKind};
use super::semantic::SemanticProgramView;
use super::semantic::{
    CausalConsumer, CausalDependency, CausalSource, CausalSupport, CausalTransform,
    ContinuousInitialState, ContinuousProgramComplexity, ContinuousProgramOccurrence,
    ContinuousProgramView, EffectIndex, LocalAutomationRef, MusicalEffectView, MusicalNoteView,
    NoteIndex, OccurrenceInitialState, ProgramContentDigest, ProgramDefinition,
    ProgramDefinitionCount, ProgramEventCount, ProgramOccurrence, ProgramOccurrenceCount,
    ProgramReuseGroup, ProgramSerializedSize, ReusePolicy, SemitoneOffset,
};
use super::signal::{
    ControlRegister, EventSignal, FilterModeRegister, FilterRoutingRegister, MasterVolume,
    SidChipProgram, SidRegisterFile, SidVoiceProgram, SignalPoint, StepSignal, SwitchState,
    WaveformRegister,
};
use super::time::SourceSpan;
use super::topology::{SidTopology, TopologyEdge, TopologyEdgeKind, TopologyNodeId};
use super::{
    AnalysisInputs, AnalyzedSidProgram, AnalyzerRevision, ProgramRevisions, ProgramSource,
    voice_array,
};
use crate::analysis::VoiceId;
use crate::analysis::filter::{Cutoff, Resonance};
use crate::analysis::voice::{Adsr, PulseWidth, SidFreq};
use crate::emu::PlaybackTiming;
use crate::emu::capture::{
    CAPTURE_SCHEMA_VERSION, CapturedSidExecution, SidAddressClass, SidBusAccess, SidCallId,
    SidChipId,
};
use crate::header::{Header, SubtuneIndex};
use crate::trace::{ChipCycle, SidRegister};
use md5::{Digest, Md5};
use std::collections::BTreeMap;

pub(super) fn build(
    header: &Header,
    subtune: SubtuneIndex,
    timing: PlaybackTiming,
    capture: CapturedSidExecution,
    inputs: AnalysisInputs,
) -> AnalyzedSidProgram {
    let mut voices = voice_array(SidVoiceProgram::empty);
    let mut chip = SidChipProgram {
        sid_model: capture.sid_model,
        initial_registers: Evidenced::one(SidRegisterFile([0; 0x1d]), Evidence::power_on()),
        cutoff: EventSignal::default(),
        resonance: EventSignal::default(),
        routing: EventSignal::default(),
        filter_mode: EventSignal::default(),
        volume: EventSignal::default(),
        digi: EventSignal::default(),
        topology: SidTopology::default(),
    };
    let mut registers = [0_u8; 0x1d];
    let mut next_signal = SignalPointId(0);

    for event in &capture.events {
        if event.chip != SidChipId::PRIMARY {
            continue;
        }
        if !matches!(
            event.address_class,
            SidAddressClass::Base | SidAddressClass::Mirror
        ) || event.access != SidBusAccess::Write
        {
            continue;
        }
        let Some(register) = event.register else {
            continue;
        };
        registers[usize::from(register.0)] = event.value;
        let evidence = Evidence::exact_event(event.id, event.timestamp_quality);
        if register.0 < 0x15 {
            let voice_index = usize::from(register.0 / 7);
            let offset = register.0 % 7;
            let base = voice_index * 7;
            match offset {
                0 | 1 => {
                    let frequency =
                        SidFreq(u16::from_le_bytes([registers[base], registers[base + 1]]));
                    push_event(
                        &mut voices[voice_index].frequency,
                        &mut next_signal,
                        event.cycle,
                        frequency,
                        evidence,
                    );
                }
                2 | 3 => {
                    let pulse_width = PulseWidth(
                        u16::from(registers[base + 2])
                            | (u16::from(registers[base + 3] & 0x0f) << 8),
                    );
                    push_event(
                        &mut voices[voice_index].pulse_width,
                        &mut next_signal,
                        event.cycle,
                        pulse_width,
                        evidence,
                    );
                }
                4 => push_control(
                    &mut voices[voice_index],
                    &mut next_signal,
                    event.cycle,
                    event.value,
                    evidence,
                ),
                5 | 6 => {
                    let adsr = Adsr::from_bytes(registers[base + 5], registers[base + 6]);
                    push_event(
                        &mut voices[voice_index].adsr,
                        &mut next_signal,
                        event.cycle,
                        adsr,
                        evidence,
                    );
                }
                _ => {}
            }
        } else {
            push_chip_event(
                &mut chip,
                &mut next_signal,
                &registers,
                register,
                event.cycle,
                event.value,
                evidence,
            );
        }
    }

    for checkpoint in &capture.checkpoints {
        for (voice, voice_program) in voices.iter_mut().enumerate() {
            let evidence = Evidence::exact_checkpoint(checkpoint.id);
            push_step(
                &mut voice_program.envelope,
                &mut next_signal,
                checkpoint.cycle,
                checkpoint.digital_sid.envelopes[voice].clone(),
                evidence,
            );
            push_step(
                &mut voice_program.oscillator,
                &mut next_signal,
                checkpoint.cycle,
                checkpoint.digital_sid.oscillators[voice].clone(),
                evidence,
            );
        }
    }

    chip.topology = build_topology(&capture, &voices, &chip);
    let regions = build_regions(&capture, &inputs.states, &voices);
    let (note_views, effect_views, program_definitions, occurrences) =
        build_semantic_links(&capture, &inputs, &regions);
    let continuous = build_continuous_program(&capture, &regions);
    let causal_dependencies = build_causal_dependencies(&capture);
    let interpretations = build_interpretations(&capture, &voices, &chip);
    let indexes = super::query::ProgramIndexes::build(&capture);
    let AnalysisInputs {
        states: _,
        notes,
        effects,
        voice_relations,
        characteristics,
        patches,
        patch_assignments,
    } = inputs;
    let mut program = AnalyzedSidProgram {
        source: ProgramSource {
            header: header.clone(),
            subtune,
            timing,
        },
        capture,
        chip,
        voices,
        semantic: SemanticProgramView {
            notes,
            effects,
            voice_relations,
            characteristics,
            patches,
            patch_assignments,
            regions,
            note_views,
            effect_views,
            program_definitions,
            occurrences,
            continuous,
            causal_dependencies,
            interpretations,
            structure: None,
            native: None,
        },
        observables: RenderObservableSet::default(),
        revisions: ProgramRevisions {
            analyzer: AnalyzerRevision(env!("CARGO_PKG_VERSION").to_owned()),
            capture_schema: CAPTURE_SCHEMA_VERSION,
        },
        indexes,
    };
    let frames = super::query::reconstruct_frames(&program);
    program.indexes.cache_frames(frames);
    program
}

fn build_interpretations(
    capture: &CapturedSidExecution,
    voices: &[SidVoiceProgram; 3],
    chip: &SidChipProgram,
) -> Vec<SignalInterpretation> {
    let end = capture
        .checkpoints
        .last()
        .map_or(ChipCycle(0), |checkpoint| checkpoint.cycle);
    let mut interpretations = Vec::new();
    for voice in voices {
        for interpretation in [
            super::interpretation::from_event_signal(
                SignalInterpretationId(interpretations.len() as u64),
                InterpretedSignalSource::VoiceFrequency { voice: voice.voice },
                &voice.frequency,
                end,
                |value| i64::from(value.0),
            ),
            super::interpretation::from_event_signal(
                SignalInterpretationId((interpretations.len() + 1) as u64),
                InterpretedSignalSource::VoicePulseWidth { voice: voice.voice },
                &voice.pulse_width,
                end,
                |value| i64::from(value.0),
            ),
        ]
        .into_iter()
        .flatten()
        {
            let mut interpretation = interpretation;
            interpretation.id = SignalInterpretationId(interpretations.len() as u64);
            interpretations.push(interpretation);
        }
        let envelope_points: Vec<_> = voice
            .envelope
            .0
            .iter()
            .map(|point| {
                (
                    point.at,
                    point.value.value.level.0,
                    point.value.evidence.clone(),
                )
            })
            .collect();
        if let Some(interpretation) = super::interpretation::envelope(
            SignalInterpretationId(interpretations.len() as u64),
            voice.voice,
            &envelope_points,
            end,
        ) {
            interpretations.push(interpretation);
        }
    }
    for interpretation in [
        super::interpretation::from_event_signal(
            SignalInterpretationId(interpretations.len() as u64),
            InterpretedSignalSource::ChipCutoff,
            &chip.cutoff,
            end,
            |value| i64::from(value.0),
        ),
        super::interpretation::from_event_signal(
            SignalInterpretationId((interpretations.len() + 1) as u64),
            InterpretedSignalSource::MasterVolume,
            &chip.volume,
            end,
            |value| i64::from(value.0),
        ),
    ]
    .into_iter()
    .flatten()
    {
        let mut interpretation = interpretation;
        interpretation.id = SignalInterpretationId(interpretations.len() as u64);
        interpretations.push(interpretation);
    }
    interpretations
}

fn build_causal_dependencies(capture: &CapturedSidExecution) -> Vec<CausalDependency> {
    let mut dependencies = Vec::new();
    for (index, event) in capture.events.iter().enumerate() {
        if event.chip != SidChipId::PRIMARY || event.access != SidBusAccess::Read {
            continue;
        }
        let source = match event.register.map(|register| register.0) {
            Some(0x1b) => CausalSource::Oscillator3Read,
            Some(0x1c) => CausalSource::Envelope3Read,
            _ => continue,
        };
        let consumers: Vec<_> = capture.events[index + 1..]
            .iter()
            .take_while(|candidate| {
                candidate.call == event.call
                    && !(candidate.access == SidBusAccess::Read
                        && matches!(
                            candidate.register.map(|register| register.0),
                            Some(0x1b | 0x1c)
                        ))
            })
            .filter(|candidate| candidate.access == SidBusAccess::Write)
            .filter(|candidate| candidate.chip == SidChipId::PRIMARY)
            .filter_map(|candidate| {
                candidate.register.map(|register| CausalConsumer {
                    event: candidate.id,
                    register,
                    transform: if candidate.value == event.value {
                        CausalTransform::Identity
                    } else if candidate.value & 0x0f == event.value & 0x0f {
                        CausalTransform::LowNibble
                    } else if candidate.value >> 4 == event.value >> 4 {
                        CausalTransform::HighNibble
                    } else {
                        CausalTransform::Ordered6502Path
                    },
                })
            })
            .collect();
        let has_value_transform = consumers
            .iter()
            .any(|consumer| consumer.transform != CausalTransform::Ordered6502Path);
        dependencies.push(CausalDependency {
            id: super::ids::CausalDependencyId(dependencies.len() as u64),
            source,
            producer: event.id,
            producer_value: event.value,
            support: if consumers.is_empty() {
                CausalSupport::UnknownConsumer
            } else if has_value_transform {
                CausalSupport::ProducerTransformConsumer
            } else {
                CausalSupport::OrderedSameCallHypothesis
            },
            consumers,
            evidence: vec![Evidence {
                provenance: Provenance::Inferred,
                confidence: ConfidencePermille(if has_value_transform { 750 } else { 250 }),
                source: SourceReference::Event(event.id),
                validity: if has_value_transform {
                    EvidenceValidity::TimingBounded
                } else {
                    EvidenceValidity::Unknown
                },
            }],
        });
    }
    dependencies
}

fn build_semantic_links(
    capture: &CapturedSidExecution,
    inputs: &AnalysisInputs,
    regions: &[SoundRegion],
) -> (
    Vec<MusicalNoteView>,
    Vec<MusicalEffectView>,
    Vec<ProgramDefinition>,
    Vec<ProgramOccurrence>,
) {
    let calls: BTreeMap<crate::trace::FrameIndex, &crate::emu::capture::CapturedCallSpan> = capture
        .calls
        .iter()
        .filter_map(|call| match call.call {
            crate::emu::capture::SidCallId::Play(frame) => Some((frame, call)),
            crate::emu::capture::SidCallId::Init => None,
        })
        .collect();
    let trace_end = capture
        .checkpoints
        .last()
        .map_or(ChipCycle(0), |checkpoint| checkpoint.cycle);
    let frame_cycle = |frame: crate::trace::FrameIndex| {
        calls.get(&frame).map_or(trace_end, |call| call.start_cycle)
    };
    let mut definitions = Vec::new();
    let mut definition_by_digest = BTreeMap::new();
    let mut note_views = Vec::with_capacity(inputs.notes.len());
    let mut occurrences: Vec<ProgramOccurrence> = Vec::with_capacity(inputs.notes.len());
    let mut previous_by_voice: [Option<(crate::trace::FrameIndex, super::ids::ProgramOccurrenceId)>;
        3] = [None; 3];
    for (index, note) in inputs.notes.iter().enumerate() {
        let start = frame_cycle(note.start_frame);
        let end_frame = note.sound_end_frame(&inputs.states).or(note.end_frame);
        let end = end_frame.map_or(trace_end, frame_cycle);
        let span = SourceSpan { start, end };
        let linked_regions = overlapping_regions(regions, span, Some(note.voice));
        note_views.push(MusicalNoteView {
            id: super::ids::NoteViewId(index as u64),
            note_index: NoteIndex(index),
            span,
            regions: linked_regions.clone(),
        });
        let occurrence_id = super::ids::ProgramOccurrenceId(index as u64);
        let patch = inputs.patch_assignments.get(index).copied().flatten();
        let first_event = capture
            .events
            .partition_point(|event| event.cycle < span.start);
        let next_event = capture
            .events
            .partition_point(|event| event.cycle < span.end);
        let occurrence_events: Vec<_> = capture.events[first_event..next_event]
            .iter()
            .filter(|event| {
                span.start <= event.cycle
                    && event.cycle < span.end
                    && event.chip == SidChipId::PRIMARY
                    && event.access == SidBusAccess::Write
                    && event.register.is_some_and(|register| {
                        register.0 / 7 == note.voice.0.saturating_sub(1) || register.0 >= 0x15
                    })
            })
            .map(|event| {
                (
                    event.cycle.0.saturating_sub(span.start.0),
                    event.register,
                    event.value,
                )
            })
            .collect();
        let occurrence_duration = ChipCycle(span.end.0.saturating_sub(span.start.0));
        let encoded = serde_json::to_vec(&(patch, occurrence_duration, &occurrence_events))
            .unwrap_or_default();
        let mut hasher = Md5::new();
        hasher.update(encoded);
        let digest = ProgramContentDigest(hasher.finalize().into());
        let definition = definition_by_digest.get(&digest).copied().or_else(|| {
            let id = super::ids::ProgramDefinitionId(definitions.len() as u64);
            definitions.push(ProgramDefinition {
                id,
                patch,
                content_digest: digest,
                event_count: ProgramEventCount(occurrence_events.len() as u64),
                duration: occurrence_duration,
            });
            definition_by_digest.insert(digest, id);
            Some(id)
        });
        let checkpoint = capture
            .checkpoints
            .partition_point(|checkpoint| checkpoint.cycle <= start)
            .checked_sub(1)
            .map(|index| &capture.checkpoints[index]);
        let previous = previous_by_voice[note.voice.to_index()]
            .filter(|(previous_end, _)| *previous_end == note.start_frame)
            .map(|(_, occurrence)| occurrence);
        if let Some(previous) = previous
            && let Some(occurrence) = occurrences.get_mut(previous.0 as usize)
        {
            occurrence.continuation = Some(occurrence_id);
        }
        let local_automation = [
            InterpretedSignalSource::VoiceFrequency { voice: note.voice },
            InterpretedSignalSource::VoicePulseWidth { voice: note.voice },
            InterpretedSignalSource::ChipCutoff,
            InterpretedSignalSource::MasterVolume,
        ]
        .into_iter()
        .map(|source| LocalAutomationRef { source, span })
        .collect();
        occurrences.push(ProgramOccurrence {
            id: occurrence_id,
            definition,
            voice: note.voice,
            span,
            regions: linked_regions,
            initial: OccurrenceInitialState {
                oscillator: checkpoint.map(|checkpoint| {
                    checkpoint.digital_sid.oscillators[note.voice.to_index()].clone()
                }),
                envelope: checkpoint.map(|checkpoint| {
                    checkpoint.digital_sid.envelopes[note.voice.to_index()].clone()
                }),
                table_position: None,
                transpose: SemitoneOffset(0),
                velocity: note.velocity,
            },
            local_automation,
            previous,
            continuation: None,
        });
        if let Some(end_frame) = note.end_frame {
            previous_by_voice[note.voice.to_index()] = Some((end_frame, occurrence_id));
        }
    }

    let effect_views = inputs
        .effects
        .iter()
        .enumerate()
        .map(|(index, effect)| {
            let start = frame_cycle(effect.start_frame);
            let next = crate::trace::FrameIndex(effect.end_frame.0.saturating_add(1));
            let end = calls.get(&next).map_or(trace_end, |call| call.start_cycle);
            let span = SourceSpan { start, end };
            MusicalEffectView {
                id: super::ids::EffectViewId(index as u64),
                effect_index: EffectIndex(index),
                span,
                regions: overlapping_regions(regions, span, effect.voice),
            }
        })
        .collect();
    (note_views, effect_views, definitions, occurrences)
}

fn build_continuous_program(
    capture: &CapturedSidExecution,
    regions: &[SoundRegion],
) -> ContinuousProgramView {
    let mut occurrences: Vec<ContinuousProgramOccurrence> = Vec::new();
    let mut previous_by_voice: [Option<super::ids::ContinuousOccurrenceId>; 3] = [None; 3];
    for region in regions
        .iter()
        .filter(|region| region.kind != SoundRegionKind::SilentParked)
    {
        let id = super::ids::ContinuousOccurrenceId(occurrences.len() as u64);
        let events: Vec<_> = capture
            .events
            .iter()
            .filter(|event| {
                region.span.start < event.cycle
                    && event.cycle < region.span.end
                    && event.access == SidBusAccess::Write
                    && event.register.is_some_and(|register| match region.voice {
                        Some(voice) => {
                            (register.0 < 0x15 && register.0 / 7 == voice.0.saturating_sub(1))
                                || register.0 >= 0x15
                        }
                        None => register.0 >= 0x15,
                    })
            })
            .map(|event| {
                (
                    event.cycle.0.saturating_sub(region.span.start.0),
                    event.register,
                    event.value,
                )
            })
            .collect();
        let duration = ChipCycle(region.span.end.0.saturating_sub(region.span.start.0));
        let encoded =
            serde_json::to_vec(&(region.voice, region.kind, duration, &events)).unwrap_or_default();
        let mut hasher = Md5::new();
        hasher.update(encoded);
        let content_digest = ProgramContentDigest(hasher.finalize().into());
        let initial = ContinuousInitialState {
            digital_sid: super::query::capture_state_at(capture, region.span.start).digital_sid,
        };
        let physical_voice = region.voice.filter(|_| region.kind.is_physical_timeline());
        let previous = physical_voice.and_then(|voice| previous_by_voice[voice.to_index()]);
        if let Some(previous) = previous
            && let Some(prior) = occurrences.get_mut(previous.0 as usize)
        {
            prior.continuation = Some(id);
        }
        occurrences.push(ContinuousProgramOccurrence {
            id,
            region: region.id,
            voice: region.voice,
            kind: region.kind,
            span: region.span,
            content_digest,
            event_count: ProgramEventCount(events.len() as u64),
            initial,
            previous,
            continuation: None,
        });
        if let Some(voice) = physical_voice {
            previous_by_voice[voice.to_index()] = Some(id);
        }
    }

    let mut grouped = BTreeMap::<
        (Option<VoiceId>, ProgramContentDigest),
        Vec<super::ids::ContinuousOccurrenceId>,
    >::new();
    for occurrence in &occurrences {
        grouped
            .entry((occurrence.voice, occurrence.content_digest))
            .or_default()
            .push(occurrence.id);
    }
    let mut reuse_groups = Vec::with_capacity(grouped.len());
    for ((voice, content_digest), members) in grouped {
        let instances: Vec<_> = members
            .iter()
            .filter_map(|id| occurrences.get(id.0 as usize))
            .collect();
        let policy = if instances.len() < 2 {
            ReusePolicy::Unique
        } else if instances
            .iter()
            .any(|occurrence| occurrence.previous.is_some() || occurrence.continuation.is_some())
        {
            ReusePolicy::ContinuationRequired
        } else if instances
            .windows(2)
            .all(|pair| pair[0].initial == pair[1].initial)
        {
            ReusePolicy::ExactInitialState
        } else {
            ReusePolicy::ExplicitInitialState
        };
        reuse_groups.push(ProgramReuseGroup {
            id: super::ids::ProgramReuseGroupId(reuse_groups.len() as u64),
            content_digest,
            voice,
            occurrences: members,
            policy,
        });
    }
    let events = occurrences
        .iter()
        .map(|occurrence| occurrence.event_count.0)
        .sum();
    let serialized_size = serde_json::to_vec(&(&occurrences, &reuse_groups))
        .map_or(0, |encoded| encoded.len() as u64);
    ContinuousProgramView {
        complexity: ContinuousProgramComplexity {
            occurrences: ProgramOccurrenceCount(occurrences.len() as u64),
            reusable_groups: ProgramDefinitionCount(
                reuse_groups
                    .iter()
                    .filter(|group| group.policy != ReusePolicy::Unique)
                    .count() as u64,
            ),
            events: ProgramEventCount(events),
            serialized_size: ProgramSerializedSize(serialized_size),
        },
        occurrences,
        reuse_groups,
    }
}

fn overlapping_regions(
    regions: &[SoundRegion],
    span: SourceSpan,
    voice: Option<VoiceId>,
) -> Vec<SoundRegionId> {
    regions
        .iter()
        .filter(|region| {
            voice.is_none_or(|voice| region.voice == Some(voice))
                && region.span.start < span.end
                && span.start < region.span.end
        })
        .map(|region| region.id)
        .collect()
}

fn push_chip_event(
    chip: &mut SidChipProgram,
    next_signal: &mut SignalPointId,
    registers: &[u8; 0x1d],
    register: SidRegister,
    cycle: ChipCycle,
    value: u8,
    evidence: Evidence,
) {
    match register.0 {
        0x15 | 0x16 => {
            let cutoff =
                Cutoff((u16::from(registers[0x16]) << 3) | u16::from(registers[0x15] & 0x07));
            push_event(&mut chip.cutoff, next_signal, cycle, cutoff, evidence);
        }
        0x17 => {
            push_event(
                &mut chip.resonance,
                next_signal,
                cycle,
                Resonance(value >> 4),
                evidence,
            );
            push_event(
                &mut chip.routing,
                next_signal,
                cycle,
                FilterRoutingRegister(value & 0x0f),
                evidence,
            );
        }
        0x18 => {
            push_event(
                &mut chip.filter_mode,
                next_signal,
                cycle,
                FilterModeRegister(value >> 4),
                evidence,
            );
            push_event(
                &mut chip.volume,
                next_signal,
                cycle,
                MasterVolume(value & 0x0f),
                evidence,
            );
            push_event(
                &mut chip.digi,
                next_signal,
                cycle,
                MasterVolume(value & 0x0f),
                evidence,
            );
        }
        _ => {}
    }
}

fn push_control(
    voice: &mut SidVoiceProgram,
    next_signal: &mut SignalPointId,
    cycle: ChipCycle,
    value: u8,
    evidence: Evidence,
) {
    push_event(
        &mut voice.control,
        next_signal,
        cycle,
        ControlRegister(value),
        evidence,
    );
    push_step(
        &mut voice.waveform,
        next_signal,
        cycle,
        WaveformRegister(value >> 4),
        evidence,
    );
    for (signal, selected) in [
        (&mut voice.gate, value & 0x01 != 0),
        (&mut voice.sync, value & 0x02 != 0),
        (&mut voice.ring, value & 0x04 != 0),
        (&mut voice.test, value & 0x08 != 0),
    ] {
        push_step(signal, next_signal, cycle, SwitchState(selected), evidence);
    }
}

fn push_event<T>(
    signal: &mut EventSignal<T>,
    next_signal: &mut SignalPointId,
    at: ChipCycle,
    value: T,
    evidence: Evidence,
) {
    signal.push(point(next_signal, at, value, evidence));
}

fn push_step<T: PartialEq>(
    signal: &mut StepSignal<T>,
    next_signal: &mut SignalPointId,
    at: ChipCycle,
    value: T,
    evidence: Evidence,
) {
    signal.push_compact(point(next_signal, at, value, evidence));
}

fn point<T>(
    next_signal: &mut SignalPointId,
    at: ChipCycle,
    value: T,
    evidence: Evidence,
) -> SignalPoint<T> {
    let id = *next_signal;
    next_signal.0 += 1;
    SignalPoint {
        id,
        at,
        value: Evidenced::one(value, evidence),
    }
}

fn build_topology(
    capture: &CapturedSidExecution,
    voices: &[SidVoiceProgram; 3],
    chip: &SidChipProgram,
) -> SidTopology {
    let end = capture
        .checkpoints
        .last()
        .map_or(ChipCycle(0), |checkpoint| checkpoint.cycle);
    let whole = SourceSpan {
        start: ChipCycle(0),
        end,
    };
    let whole_evidence = span_evidence(whole, Provenance::ExactEmulation);
    let mut topology = SidTopology {
        nodes: vec![
            TopologyNodeId::Filter,
            TopologyNodeId::FilteredBus,
            TopologyNodeId::BypassBus,
            TopologyNodeId::FinalMixer,
            TopologyNodeId::ExternalInput,
        ],
        edges: vec![
            TopologyEdge {
                source: TopologyNodeId::Filter,
                destination: TopologyNodeId::FilteredBus,
                kind: TopologyEdgeKind::Audio,
                span: whole,
                evidence: whole_evidence,
            },
            TopologyEdge {
                source: TopologyNodeId::FilteredBus,
                destination: TopologyNodeId::FinalMixer,
                kind: TopologyEdgeKind::Mix,
                span: whole,
                evidence: whole_evidence,
            },
            TopologyEdge {
                source: TopologyNodeId::BypassBus,
                destination: TopologyNodeId::FinalMixer,
                kind: TopologyEdgeKind::Mix,
                span: whole,
                evidence: whole_evidence,
            },
        ],
    };
    for voice in VoiceId::V1.0..=VoiceId::V3.0 {
        let voice = VoiceId(voice);
        topology.nodes.extend([
            TopologyNodeId::Oscillator(voice),
            TopologyNodeId::Envelope(voice),
            TopologyNodeId::PreFilterTap(voice),
        ]);
        topology.edges.extend([
            TopologyEdge {
                source: TopologyNodeId::Oscillator(voice),
                destination: TopologyNodeId::PreFilterTap(voice),
                kind: TopologyEdgeKind::Audio,
                span: whole,
                evidence: whole_evidence,
            },
            TopologyEdge {
                source: TopologyNodeId::Envelope(voice),
                destination: TopologyNodeId::PreFilterTap(voice),
                kind: TopologyEdgeKind::AmplitudeControl,
                span: whole,
                evidence: whole_evidence,
            },
        ]);
    }
    push_routing_edges(&mut topology, chip, whole);
    push_cross_voice_edges(&mut topology, voices, whole);
    topology
}

fn push_routing_edges(topology: &mut SidTopology, chip: &SidChipProgram, whole: SourceSpan) {
    let mut starts = Vec::with_capacity(chip.routing.0.len() + 1);
    starts.push((
        whole.start,
        FilterRoutingRegister(0),
        span_evidence(whole, Provenance::ExactEmulation),
    ));
    starts.extend(
        chip.routing
            .0
            .iter()
            .map(|point| (point.at, point.value.value, point.value.evidence[0])),
    );
    for index in 0..starts.len() {
        let (start, routing, evidence) = starts[index];
        let end = starts.get(index + 1).map_or(whole.end, |next| next.0);
        if start >= end {
            continue;
        }
        let span = SourceSpan { start, end };
        for voice_index in 0..3 {
            let voice = VoiceId::from_index(voice_index);
            let filtered = routing.0 & (1 << voice_index) != 0;
            topology.edges.push(TopologyEdge {
                source: TopologyNodeId::PreFilterTap(voice),
                destination: if filtered {
                    TopologyNodeId::Filter
                } else {
                    TopologyNodeId::BypassBus
                },
                kind: if filtered {
                    TopologyEdgeKind::FilterRoute
                } else {
                    TopologyEdgeKind::BypassRoute
                },
                span,
                evidence,
            });
        }
        if routing.0 & 0x08 != 0 {
            topology.edges.push(TopologyEdge {
                source: TopologyNodeId::ExternalInput,
                destination: TopologyNodeId::Filter,
                kind: TopologyEdgeKind::FilterRoute,
                span,
                evidence,
            });
        }
    }
}

fn push_cross_voice_edges(
    topology: &mut SidTopology,
    voices: &[SidVoiceProgram; 3],
    whole: SourceSpan,
) {
    for (destination_index, voice) in voices.iter().enumerate() {
        for index in 0..voice.control.0.len() {
            let point = &voice.control.0[index];
            let end = voice
                .control
                .0
                .get(index + 1)
                .map_or(whole.end, |next| next.at);
            if point.at >= end {
                continue;
            }
            let source = VoiceId::from_index((destination_index + 2) % 3);
            let destination = VoiceId::from_index(destination_index);
            for (mask, kind) in [
                (0x02, TopologyEdgeKind::Sync),
                (0x04, TopologyEdgeKind::Ring),
            ] {
                if point.value.value.0 & mask != 0 {
                    topology.edges.push(TopologyEdge {
                        source: TopologyNodeId::Oscillator(source),
                        destination: TopologyNodeId::Oscillator(destination),
                        kind,
                        span: SourceSpan {
                            start: point.at,
                            end,
                        },
                        evidence: point.value.evidence[0],
                    });
                }
            }
        }
    }
}

fn build_regions(
    capture: &CapturedSidExecution,
    states: &[crate::analysis::FrameState],
    voices: &[SidVoiceProgram; 3],
) -> Vec<SoundRegion> {
    let calls: Vec<_> = capture
        .calls
        .iter()
        .filter(|call| matches!(call.call, crate::emu::capture::SidCallId::Play(_)))
        .collect();
    let mut regions = Vec::new();
    let mut next_id = SoundRegionId(0);
    for (voice_index, voice_program) in voices.iter().enumerate() {
        let mut open: Option<(SoundRegionKind, ChipCycle, ChipCycle)> = None;
        for (state, call) in states.iter().zip(&calls) {
            let voice = state.voices[voice_index];
            let digital = &state.digital_voices[voice_index];
            let kind = if voice.control.gate && voice.control.waveform.noise {
                SoundRegionKind::NoiseTransient
            } else if voice.control.gate
                && digital.envelope.phase == crate::emu::sid::EnvPhase::Attack
            {
                SoundRegionKind::TonalAttack
            } else if voice.control.gate {
                SoundRegionKind::Sustain
            } else if digital.envelope.level.0 > 0 {
                SoundRegionKind::ReleaseTail
            } else {
                let destination = (voice_index + 1) % 3;
                if states.get(state.frame.0 as usize).is_some_and(|frame| {
                    frame.voices[destination].control.sync
                        || frame.voices[destination].control.ring_mod
                }) {
                    SoundRegionKind::SilentModulator
                } else {
                    SoundRegionKind::SilentParked
                }
            };
            match open {
                Some((open_kind, start, _)) if open_kind == kind => {
                    open = Some((open_kind, start, call.sampling_boundary));
                }
                Some((open_kind, start, end)) => {
                    push_region(
                        &mut regions,
                        &mut next_id,
                        Some(VoiceId::from_index(voice_index)),
                        open_kind,
                        start,
                        end,
                    );
                    open = Some((kind, call.start_cycle, call.sampling_boundary));
                }
                None => open = Some((kind, call.start_cycle, call.sampling_boundary)),
            }
        }
        if let Some((kind, start, end)) = open {
            push_region(
                &mut regions,
                &mut next_id,
                Some(VoiceId::from_index(voice_index)),
                kind,
                start,
                end,
            );
        }
        let voice = voice_program;
        for point in &voice.waveform.0 {
            let Some(call) = calls
                .iter()
                .find(|call| call.start_cycle <= point.at && point.at < call.sampling_boundary)
            else {
                continue;
            };
            push_region(
                &mut regions,
                &mut next_id,
                Some(VoiceId::from_index(voice_index)),
                SoundRegionKind::WaveformTransient,
                point.at,
                call.sampling_boundary,
            );
        }
        let evolving = voice.frequency.0.windows(2).any(|points| {
            points[0].value.value != points[1].value.value && points[0].at < points[1].at
        }) || voice.pulse_width.0.windows(2).any(|points| {
            points[0].value.value != points[1].value.value && points[0].at < points[1].at
        });
        if evolving {
            let first_gate = voice
                .gate
                .0
                .iter()
                .find(|point| point.value.value.0)
                .map(|point| point.at);
            let last = voice
                .frequency
                .0
                .last()
                .map(|point| point.at)
                .or_else(|| voice.pulse_width.0.last().map(|point| point.at));
            if let (Some(start), Some(last)) = (first_gate, last)
                && start < last
            {
                push_region(
                    &mut regions,
                    &mut next_id,
                    Some(VoiceId::from_index(voice_index)),
                    SoundRegionKind::ContinuousTexture,
                    start,
                    last,
                );
            }
        }
    }
    for span in d418_stream_spans(capture) {
        push_region(
            &mut regions,
            &mut next_id,
            None,
            SoundRegionKind::DigiStream,
            span.start,
            span.end,
        );
    }
    regions.sort_by_key(|region| (region.span.start, region.voice, region.id));
    for (index, region) in regions.iter_mut().enumerate() {
        region.id = SoundRegionId(index as u64);
    }
    regions
}

fn d418_stream_spans(capture: &CapturedSidExecution) -> Vec<SourceSpan> {
    const MIN_WRITES_PER_CALL: usize = 4;
    const MIN_STREAM_EVENTS: usize = 4;
    const MIN_CROSS_CALL_RATE_HZ: f64 = 200.0;
    struct OpenStream {
        span: SourceSpan,
        events: usize,
        last_call_dense: bool,
    }

    fn finish(open: &mut Option<OpenStream>, spans: &mut Vec<SourceSpan>) {
        if let Some(stream) = open.take()
            && stream.events >= MIN_STREAM_EVENTS
        {
            spans.push(stream.span);
        }
    }

    let cross_call_dense = capture.call_rate.calls_per_second() >= MIN_CROSS_CALL_RATE_HZ;
    let maximum_sparse_interval =
        (f64::from(capture.system_clock.phi2_hz()) / MIN_CROSS_CALL_RATE_HZ).ceil() as u64;
    let mut spans = Vec::new();
    let mut open: Option<OpenStream> = None;
    for call in &capture.calls {
        if call.call == SidCallId::Init {
            continue;
        }
        let events = &capture.events[call.first_event.0 as usize..call.next_event.0 as usize];
        let writes: Vec<_> = events
            .iter()
            .filter(|event| {
                event.chip == SidChipId::PRIMARY
                    && event.access == SidBusAccess::Write
                    && event.register == Some(SidRegister(0x18))
            })
            .collect();
        let Some(first) = writes.first() else {
            finish(&mut open, &mut spans);
            continue;
        };
        let last = writes.last().copied().unwrap_or(first);
        let call_dense = writes.len() >= MIN_WRITES_PER_CALL;
        if !call_dense && !cross_call_dense {
            finish(&mut open, &mut spans);
            continue;
        }
        let end = ChipCycle(last.cycle.0.saturating_add(1));
        let connects = open.as_ref().is_some_and(|stream| {
            call_dense && stream.last_call_dense
                || first.cycle.0.saturating_sub(stream.span.end.0) <= maximum_sparse_interval
        });
        if !connects {
            finish(&mut open, &mut spans);
        }
        match &mut open {
            Some(stream) => {
                stream.span.end = end;
                stream.events += writes.len();
                stream.last_call_dense = call_dense;
            }
            None => {
                open = Some(OpenStream {
                    span: SourceSpan {
                        start: first.cycle,
                        end,
                    },
                    events: writes.len(),
                    last_call_dense: call_dense,
                });
            }
        }
    }
    finish(&mut open, &mut spans);
    spans
}

fn push_region(
    regions: &mut Vec<SoundRegion>,
    next_id: &mut SoundRegionId,
    voice: Option<VoiceId>,
    kind: SoundRegionKind,
    start: ChipCycle,
    end: ChipCycle,
) {
    let span = SourceSpan { start, end };
    regions.push(SoundRegion {
        id: *next_id,
        voice,
        kind,
        span,
        evidence: vec![span_evidence(span, Provenance::TraceMeasured)],
    });
    next_id.0 += 1;
}

fn span_evidence(span: SourceSpan, provenance: Provenance) -> Evidence {
    Evidence {
        provenance,
        confidence: ConfidencePermille(1000),
        source: SourceReference::Span(span),
        validity: EvidenceValidity::Valid,
    }
}
