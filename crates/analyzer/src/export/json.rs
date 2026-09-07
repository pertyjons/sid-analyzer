use crate::analysis::effects::{EffectSpan, VoiceRelationSpan};
use crate::analysis::note::NoteEvent;
use crate::analysis::timbre::{NoteCharacteristics, Patch, PatchId};
use crate::emu::PlaybackTiming;
use crate::emu::capture::CapturedSidChip;
use crate::header::{Header, SubtuneIndex};
use crate::stil::StilEntry;
use serde::Serialize;
use std::io::{self, Write};

/// Top-level JSON shape: a self-describing record of the analysis of one
/// subtune. Includes the SID file header, the chosen subtune, frame count,
/// detected notes, and detected effects. Raw per-frame state is not
/// included by default — use the `--format text` tracker export for that.
///
/// When the caller has run Slice 1/2 (per-note characterization + patch
/// clustering), the `patches[]` array is populated and each note in
/// `notes[]` carries a `patch_id` + `characteristics` inline.
#[derive(Serialize)]
pub struct Export<'a> {
    pub header: &'a Header,
    pub subtune: SubtuneIndex,
    pub timing: PlaybackTiming,
    pub frame_count: usize,
    /// Per-subtune durations in seconds, looked up by SID-MD5 in an HVSC
    /// `Songlengths.md5` database. Indexed by subtune number minus 1.
    /// `None` when no songlengths database was supplied or no entry matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtune_lengths_secs: Option<Vec<f64>>,
    /// HVSC STIL metadata matched by canonical path or unique filename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stil: Option<&'a StilEntry>,
    /// Slice 2 patch-book — `None` when timbre extraction wasn't run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patches: Option<&'a [Patch]>,
    pub notes: Vec<EnrichedNote<'a>>,
    pub effects: &'a [EffectSpan],
    pub voice_relations: &'a [VoiceRelationSpan],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub digi_streams: Vec<crate::analysis::sid_program::observable::DigiStreamSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_sid_chips: Vec<SidChipAnalysis>,
    /// Driver-native song structure (`--format synth-native` only): per-voice
    /// pattern placements that let the synth exporter rebuild the real reused
    /// blocks. Not part of the JSON record — only the synth backend reads it.
    #[serde(skip)]
    pub structure: Option<&'a [super::VoicePlacements]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeExportMetadata<'a>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SidChipAnalysis {
    pub chip: CapturedSidChip,
    pub frame_count: usize,
    pub patches: Vec<Patch>,
    pub notes: Vec<NoteEvent>,
    pub effects: Vec<EffectSpan>,
    pub voice_relations: Vec<VoiceRelationSpan>,
}

#[derive(Serialize, Clone, Copy)]
pub struct NativeExportMetadata<'a> {
    pub driver: &'a str,
    pub extractor: &'a str,
    pub validation: &'a super::native::NativeValidationReport,
    pub provenance: &'a [super::native::ProvenanceEvidence],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovered_structure: Option<&'a super::RecoveredStructure>,
}

/// A note event flattened with its optional patch assignment and
/// characteristics. `patch_id` and `characteristics` are omitted from
/// JSON when `None` so consumers that don't care about timbre see the
/// pre-Slice-1 shape unchanged.
///
/// **Reserved field names**: `patch_id` and `characteristics` are
/// added at this level via `#[serde(flatten)]`. Don't introduce
/// fields with those names on [`NoteEvent`] — serde's flatten path
/// silently drops the duplicate.
#[derive(Serialize)]
pub struct EnrichedNote<'a> {
    #[serde(flatten)]
    pub event: &'a NoteEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_id: Option<PatchId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub characteristics: Option<&'a NoteCharacteristics>,
}

impl<'a> EnrichedNote<'a> {
    /// Build a vector of enriched notes from parallel slices. Pass
    /// `None` for `patch_ids` and/or `characteristics` to omit those
    /// fields from the JSON output.
    #[must_use]
    pub fn enrich(
        notes: &'a [NoteEvent],
        patch_ids: Option<&'a [Option<PatchId>]>,
        characteristics: Option<&'a [NoteCharacteristics]>,
    ) -> Vec<Self> {
        notes
            .iter()
            .enumerate()
            .map(|(i, event)| Self {
                event,
                patch_id: patch_ids.and_then(|ids| ids.get(i).copied()).flatten(),
                characteristics: characteristics.and_then(|c| c.get(i)),
            })
            .collect()
    }
}

pub fn write_json(export: &Export<'_>, out: &mut dyn Write, pretty: bool) -> io::Result<()> {
    if pretty {
        serde_json::to_writer_pretty(out, export).map_err(io::Error::other)
    } else {
        serde_json::to_writer(out, export).map_err(io::Error::other)
    }
}
