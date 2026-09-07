pub(crate) mod descriptors;
pub(crate) mod forward;
pub mod json;
pub mod midi;
pub mod native;
pub mod synth;
pub mod text;

use crate::analysis::VoiceId;
use crate::trace::FrameIndex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[must_use]
pub struct PatternNumber(pub u8);

impl fmt::Display for PatternNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct PatternTranspose(pub i16);

impl From<PatternTranspose> for i32 {
    fn from(value: PatternTranspose) -> Self {
        i32::from(value.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct OrderOffset(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct RepeatOrdinal(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct PatternByteOffset(pub u16);

impl fmt::UpperHex for PatternByteOffset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct PatternDuration(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct FrequencyTableIndex(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct InstrumentNumber(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct NativeEffectByte(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct NativeOperand(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct NativeDriverOpcode(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct PatternRepeatCount(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct NativeRowTick(pub u32);

/// One exact event from a recovered driver pattern. This is source structure,
/// not a render block: rests, holds, instrument changes, and effect bytes are
/// retained even when project lowering later omits or combines them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveredPatternEvent {
    pub offset: PatternByteOffset,
    pub duration: PatternDuration,
    pub frequency_index: Option<FrequencyTableIndex>,
    pub instrument: Option<InstrumentNumber>,
    pub hold: bool,
    pub slide: Option<NativeEffectByte>,
    /// Raw driver command when this event is a command row.
    pub command: Option<NativeDriverOpcode>,
    /// Optional trailing one-byte command parameter.
    pub command_data: Option<NativeEffectByte>,
    /// Raw index into a driver-owned duration table, when distinct from duration.
    pub duration_index: Option<NativeEffectByte>,
    /// Raw absolute command operand for procedural stream formats.
    pub operand: Option<NativeOperand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveredPatternInstance {
    pub pattern: PatternNumber,
    pub transpose: PatternTranspose,
    pub repeat_ordinal: RepeatOrdinal,
    pub order_offset: OrderOffset,
    pub start_tick: NativeRowTick,
    pub start_frame: FrameIndex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum RecoveredOrderCommand {
    Pattern {
        order_offset: OrderOffset,
        pattern: PatternNumber,
        repeat: PatternRepeatCount,
    },
    SetTranspose {
        order_offset: OrderOffset,
        transpose: PatternTranspose,
    },
    DriverCommand {
        order_offset: OrderOffset,
        opcode: NativeDriverOpcode,
    },
    Loop {
        order_offset: OrderOffset,
        target: OrderOffset,
    },
    Call {
        order_offset: OrderOffset,
        target: PatternNumber,
        transpose: Option<PatternTranspose>,
    },
    Jump {
        order_offset: OrderOffset,
        target: PatternNumber,
        transpose: Option<PatternTranspose>,
    },
    Return {
        order_offset: OrderOffset,
    },
    Stop {
        order_offset: OrderOffset,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveredVoiceStructure {
    pub voice: VoiceId,
    pub order_loop_offset: Option<OrderOffset>,
    pub order_commands: Vec<RecoveredOrderCommand>,
    pub instances: Vec<RecoveredPatternInstance>,
}

/// Exact driver grammar retained independently from render-oriented placement
/// and pattern deduplication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveredStructure {
    pub patterns: BTreeMap<PatternNumber, Vec<RecoveredPatternEvent>>,
    pub voices: Vec<RecoveredVoiceStructure>,
}

/// One placement of a driver-native pattern on a voice's timeline: the pattern's
/// identity (`pattern_number`), the absolute play frame it starts at, and the
/// transpose (semitones) the driver's orderlist applies to it.
///
/// Recovered verbatim from a driver's own orderlist — reuse is simply a pattern
/// number recurring across the timeline, and the transpose is the driver's own
/// orderlist transpose command (added to a chromatic frequency-table index, so
/// one index step is one semitone). No similarity heuristics: the export never
/// guesses that two blocks are "alike", it reads which block the driver placed.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct NativePlacement {
    pub pattern_number: PatternNumber,
    pub start_frame: FrameIndex,
    pub transpose: PatternTranspose,
    pub order_offset: Option<OrderOffset>,
    pub repeat_ordinal: Option<RepeatOrdinal>,
}

/// A voice's full placement timeline over the analysed window — the driver's
/// orderlist walked (and looped) exactly as the note decoder walks it, in play
/// order. The shared frame domain (matching [`crate::analysis::note::NoteEvent`]
/// start frames) lets the synth exporter slice the flat note timeline back into
/// these placements without re-deriving any timing.
#[derive(Debug, Clone, Serialize)]
pub struct VoicePlacements {
    pub voice: VoiceId,
    pub placements: Vec<NativePlacement>,
}
