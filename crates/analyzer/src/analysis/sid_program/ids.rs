use serde::Serialize;

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize)]
        #[serde(transparent)]
        #[must_use]
        pub struct $name(pub u64);
    };
}

id_newtype!(SignalPointId);
id_newtype!(SoundRegionId);
id_newtype!(ProgramDefinitionId);
id_newtype!(ProgramOccurrenceId);
id_newtype!(RenderArtifactId);
id_newtype!(NoteViewId);
id_newtype!(EffectViewId);
id_newtype!(CausalDependencyId);
id_newtype!(SignalInterpretationId);
id_newtype!(ContinuousOccurrenceId);
id_newtype!(ProgramReuseGroupId);
