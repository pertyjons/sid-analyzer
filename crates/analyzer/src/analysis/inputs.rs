use super::effects::{
    EffectSpan, EffectThresholds, VoiceRelationSpan, detect_effects, detect_voice_relations,
};
use super::note::{NoteEvent, detect_notes};
use super::timbre::{NoteCharacteristics, Patch, PatchId, extract_timbre};
use super::{FrameState, SystemClock, analyze};
use crate::trace::Trace;

#[derive(Debug, Clone)]
#[must_use]
pub struct AnalysisInputs {
    pub states: Vec<FrameState>,
    pub notes: Vec<NoteEvent>,
    pub effects: Vec<EffectSpan>,
    pub voice_relations: Vec<VoiceRelationSpan>,
    pub characteristics: Vec<NoteCharacteristics>,
    pub patches: Vec<Patch>,
    pub patch_assignments: Vec<Option<PatchId>>,
}

impl AnalysisInputs {
    pub fn build(trace: &Trace, clock: SystemClock) -> Self {
        let states = analyze(trace);
        Self::from_states(trace, states, clock)
    }

    pub fn from_states(trace: &Trace, states: Vec<FrameState>, clock: SystemClock) -> Self {
        let notes = detect_notes(&states, clock);
        let effects = detect_effects(trace, &states, EffectThresholds::default());
        let voice_relations = detect_voice_relations(&states, EffectThresholds::default());
        let voice3_reads = trace.voice3_reads_per_frame();
        let (characteristics, patches, patch_assignments) =
            extract_timbre(&notes, &states, &effects, &voice3_reads, clock);
        Self {
            states,
            notes,
            effects,
            voice_relations,
            characteristics,
            patches,
            patch_assignments,
        }
    }
}
