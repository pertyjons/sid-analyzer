//! Native decoder for the relocated GoatTracker V2 player.
//!
//! Discovery anchors on four operands: the `SEC / SBC #$60 / STA
//! note_indices,X` that turns a pattern byte into the authored note index the
//! player keeps per voice, the `LDA playing_lo,X / STA $D400,X` mirror that
//! names the playing-frequency cell, and the song and pattern pointer-table
//! readers. Anchoring the frequency on the chip mirror rather than on the
//! note-to-frequency table lookup is what carries the locator across the many
//! relocator layouts in the wild.
//!
//! The full player grammar also recovers orderlists and packed patterns;
//! compact players retain pitch extraction but do not claim native structure.
//! Authored instruments remain a separate increment.

mod decode;

use super::super::{DriverExtractor, EmulationStage, NativeContext, NativeError, NativeSong};
use super::{
    CursorLayout, absolute_operand, assemble, decode_pitches, module_end, pointer_table_readers,
    resolve_phase, sample_native_state, trace_context, unique,
};
use crate::emu::Emulator;
use crate::header::PlayAddress;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) struct GoatTrackerV2Layout {
    pub note_indices: u16,
    pub resolved_frequencies: u16,
    pub song_pointer_lo: u16,
    pub song_pointer_hi: u16,
    pub pattern_pointer_lo: u16,
    pub pattern_pointer_hi: u16,
    pub order_positions: u16,
    pub pattern_numbers: u16,
    pub pattern_positions: u16,
    pub full_pattern_grammar: bool,
}

impl GoatTrackerV2Layout {
    fn cursors(&self) -> CursorLayout {
        CursorLayout {
            frequency_state_lo: self.resolved_frequencies,
            frequency_state_hi: self.resolved_frequencies.wrapping_add(1),
            order_positions: self.order_positions,
            pattern_numbers: self.pattern_numbers,
            pattern_positions: self.pattern_positions,
        }
    }
}

/// `SEC / SBC #$60 / STA note_indices,X` — the player turning a pattern note
/// byte into the authored note index it keeps per voice.
fn note_index_states(ram: &[u8]) -> Vec<u16> {
    let mut states = Vec::new();
    for start in 0..ram.len().saturating_sub(6) {
        if ram[start..start + 4] == [0x38, 0xE9, 0x60, 0x9D] {
            states.push(absolute_operand(&ram[start + 4..start + 6]));
        }
    }
    states.sort_unstable();
    states.dedup();
    states
}

/// `LDA playing_lo,X / STA $D400,X` — the cell the player mirrors to the chip,
/// and so the one holding the frequency it actually resolved for the voice.
/// Reading the cell rather than the note-to-frequency table keeps the locator
/// working across the many relocator layouts in the wild.
fn playing_frequency_states(ram: &[u8]) -> Vec<u16> {
    let mut states = Vec::new();
    for start in 0..ram.len().saturating_sub(7) {
        if ram[start] == 0xBD && ram[start + 3..start + 6] == [0x9D, 0x00, 0xD4] {
            states.push(absolute_operand(&ram[start + 1..start + 3]));
        }
    }
    states.sort_unstable();
    states.dedup();
    states
}

fn in_module(address: u16, module_start: u16, module_end: u16) -> bool {
    address >= module_start && address < module_end
}

fn closest_unique<T>(
    mut candidates: Vec<T>,
    anchor: u16,
    address: impl Fn(&T) -> u16,
) -> Option<T> {
    candidates.sort_by_key(|candidate| address(candidate).abs_diff(anchor));
    if candidates.len() > 1
        && address(&candidates[0]).abs_diff(anchor) == address(&candidates[1]).abs_diff(anchor)
    {
        return None;
    }
    candidates.into_iter().next()
}

fn resolved_entry(ram: &[u8], mut address: PlayAddress) -> u16 {
    for _ in 0..4 {
        let cursor = usize::from(address.0);
        let Some(instruction) = ram.get(cursor..cursor.saturating_add(3)) else {
            break;
        };
        if instruction[0] != 0x4C {
            break;
        }
        address = PlayAddress(absolute_operand(&instruction[1..3]));
    }
    address.0
}

fn locate_in_module(
    ram: &[u8],
    module_start: u16,
    module_end: u16,
    play_address: Option<PlayAddress>,
) -> Result<GoatTrackerV2Layout, String> {
    let note_candidates: Vec<_> = note_index_states(ram)
        .into_iter()
        .filter(|address| in_module(*address, module_start, module_end))
        .collect();
    let frequency_candidates: Vec<_> = playing_frequency_states(ram)
        .into_iter()
        .filter(|address| in_module(*address, module_start, module_end))
        .collect();
    let reader_in_module = |reader: &super::PointerTableReader| {
        [
            reader.table_lo,
            reader.table_hi,
            reader.index_state,
            reader.position_state,
        ]
        .into_iter()
        .all(|address| in_module(address, module_start, module_end))
    };
    let song_candidates: Vec<_> = pointer_table_readers(ram, 0xFF)
        .into_iter()
        .filter(&reader_in_module)
        .collect();
    let pattern_candidates: Vec<_> = pointer_table_readers(ram, 0x40)
        .into_iter()
        .filter(reader_in_module)
        .collect();
    let counts = (
        note_candidates.len(),
        frequency_candidates.len(),
        song_candidates.len(),
        pattern_candidates.len(),
    );
    let pattern = match play_address {
        Some(play_address) => closest_unique(
            pattern_candidates,
            resolved_entry(ram, play_address),
            |candidate| u16::try_from(candidate.code_address).unwrap_or(u16::MAX),
        ),
        None => unique(pattern_candidates),
    };
    let Some(pattern) = pattern else {
        return Err(format!(
            "expected one active V2 layout, found note={}, frequency={}, song={}, pattern={}",
            counts.0, counts.1, counts.2, counts.3
        ));
    };
    let anchor = u16::try_from(pattern.code_address).unwrap_or(u16::MAX);
    let note_indices = closest_unique(note_candidates, anchor, |address| *address);
    let resolved_frequencies = closest_unique(frequency_candidates, anchor, |address| *address);
    let song = closest_unique(song_candidates, anchor, |candidate| {
        u16::try_from(candidate.code_address).unwrap_or(u16::MAX)
    });
    let (Some(note_indices), Some(resolved_frequencies), Some(song)) =
        (note_indices, resolved_frequencies, song)
    else {
        return Err(format!(
            "could not correlate the active V2 layout from note={}, frequency={}, song={}, pattern={}",
            counts.0, counts.1, counts.2, counts.3
        ));
    };
    let grammar_end = ram.len().min(pattern.code_address + 40);
    Ok(GoatTrackerV2Layout {
        note_indices,
        resolved_frequencies,
        song_pointer_lo: song.table_lo,
        song_pointer_hi: song.table_hi,
        pattern_pointer_lo: pattern.table_lo,
        pattern_pointer_hi: pattern.table_hi,
        order_positions: song.position_state,
        pattern_numbers: pattern.index_state,
        pattern_positions: pattern.position_state,
        full_pattern_grammar: ram[pattern.code_address..grammar_end]
            .windows(2)
            .any(|bytes| bytes == [0xC9, 0x60]),
    })
}

pub(crate) fn locate(ram: &[u8]) -> Option<GoatTrackerV2Layout> {
    locate_in_module(ram, 0, u16::MAX, None).ok()
}

/// Extractor for the standard relocated GoatTracker V2 player.
pub struct GoatTracker;

impl DriverExtractor for GoatTracker {
    fn name(&self) -> &'static str {
        "goattracker-v2"
    }

    fn handles(&self, driver: &str) -> bool {
        // Some multi-player files are identified by the other generation even
        // when the selected subtune runs V2. Validation chooses the active
        // extractor; unsupported compact players still fail the locator.
        matches!(driver, "GoatTracker_V1.x" | "GoatTracker_V2.x")
    }

    fn extract(&self, ctx: &NativeContext<'_>) -> Result<NativeSong, NativeError> {
        if ctx.header.second_sid_address.is_some() {
            return Err(NativeError::UnsupportedConfiguration {
                driver: ctx.driver.to_owned(),
                extractor: self.name(),
                reason: "multi-SID native extraction is not implemented".to_owned(),
            });
        }
        let mut image = Emulator::with_timing(ctx.timing);
        image
            .load(ctx.header, ctx.bytes)
            .map_err(|error| NativeError::Emulation {
                driver: ctx.driver.to_owned(),
                stage: EmulationStage::ExtractorLoad,
                reason: error.to_string(),
            })?;
        image
            .call_init(ctx.header.init_address, ctx.subtune, ctx.header.songs)
            .map_err(|error| NativeError::Emulation {
                driver: ctx.driver.to_owned(),
                stage: EmulationStage::ExtractorInit,
                reason: error.to_string(),
            })?;
        let module_ram = image.ram_image();
        let module_start = ctx
            .header
            .effective_load_address(ctx.bytes)
            .map_err(|error| NativeError::DecodeFailed {
                driver: ctx.driver.to_owned(),
                extractor: self.name(),
                reason: error.to_string(),
            })?
            .0;
        let module_end = module_end(ctx, self.name())?;
        let layout = locate_in_module(
            &module_ram,
            module_start,
            module_end,
            Some(ctx.header.play_address),
        )
        .map_err(|reason| NativeError::LocateFailed {
            driver: ctx.driver.to_owned(),
            extractor: self.name(),
            reason,
        })?;
        let samples = sample_native_state(
            &mut image,
            &layout.cursors(),
            ctx.header.play_address,
            ctx.frames,
        )
        .map_err(|error| NativeError::Emulation {
            driver: ctx.driver.to_owned(),
            stage: EmulationStage::NativeSampling,
            reason: error.to_string(),
        })?;
        let trace = trace_context(ctx, self.name())?;
        let decode = |delay| decode_pitches(ctx.timing.clock, &trace.truth, &samples, delay);
        let (notes, validation) = resolve_phase(decode, &trace.truth, trace.timing);
        let (structure, recovered_structure) = if layout.full_pattern_grammar {
            match decode::recover(&module_ram, &layout, module_end, ctx.subtune, &samples) {
                Ok((placements, recovered)) => (Some(placements), Some(recovered)),
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };
        assemble(
            ctx,
            self.name(),
            trace,
            notes,
            validation,
            structure,
            recovered_structure,
        )
    }
}

#[cfg(all(test, feature = "asset-tests"))]
mod tests {
    use super::*;
    use crate::export::native::extract_native_song;
    use crate::header::SubtuneIndex;
    use crate::playerid::PlayerDb;

    fn post_init() -> Vec<u8> {
        let bytes =
            std::fs::read("../../assets/music/GoatTracker_V2_Tomb_of_the_Pharao.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut emulator = Emulator::new();
        emulator.load(&header, &bytes).unwrap();
        emulator
            .call_init(header.init_address, SubtuneIndex(1), header.songs)
            .unwrap();
        emulator.ram_image()
    }

    #[test]
    fn locates_relocated_v2_tables_and_state() {
        let layout = locate(&post_init()).unwrap();
        assert_eq!(layout.note_indices, 0xF854);
        assert_eq!(layout.resolved_frequencies, 0xF840);
        assert_eq!(
            (layout.song_pointer_lo, layout.song_pointer_hi),
            (0xF889, 0xF88C)
        );
        assert_eq!(
            (layout.pattern_pointer_lo, layout.pattern_pointer_hi),
            (0xF88F, 0xF892)
        );
    }

    #[test]
    fn extracts_relocated_v2_fixture() {
        let bytes =
            std::fs::read("../../assets/music/GoatTracker_V2_Tomb_of_the_Pharao.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let subtune = SubtuneIndex(1);
        let timing = crate::emu::PlaybackTiming::for_subtune(&header, subtune);
        let (_, extractor, song) =
            extract_native_song(&PlayerDb::embedded(), &header, &bytes, subtune, timing, 600)
                .unwrap();
        assert_eq!(extractor, "goattracker-v2");
        assert!(song.validation.accepted);
        assert!(!song.notes.is_empty());
    }

    #[test]
    fn recovers_full_v2_orderlists_and_packed_patterns() {
        let bytes = std::fs::read("../../assets/music/GoatTracker_V2_1982.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let subtune = SubtuneIndex(1);
        let timing = crate::emu::PlaybackTiming::for_subtune(&header, subtune);
        let (_, extractor, song) = extract_native_song(
            &PlayerDb::embedded(),
            &header,
            &bytes,
            subtune,
            timing,
            3_000,
        )
        .unwrap();
        assert_eq!(extractor, "goattracker-v2");
        let recovered = song.recovered_structure.unwrap();
        assert_eq!(recovered.patterns.len(), 23);
        assert_eq!(recovered.voices.len(), 3);
        assert!(recovered.patterns.values().all(|events| !events.is_empty()));
        assert!(
            recovered
                .patterns
                .values()
                .flatten()
                .any(|event| event.duration.0 > 1)
        );
        assert!(
            recovered
                .patterns
                .values()
                .flatten()
                .any(|event| event.command.is_some())
        );
        assert!(recovered.voices.iter().all(|voice| {
            voice
                .order_commands
                .iter()
                .any(|command| matches!(command, crate::export::RecoveredOrderCommand::Loop { .. }))
        }));
        assert!(
            recovered
                .voices
                .iter()
                .all(|voice| !voice.instances.is_empty())
        );
        assert!(song.structure.is_some());
    }

    /// Two *different* relocations of the player in one image name two
    /// different sets of tables, and there is no evidence for choosing one —
    /// so discovery must refuse rather than pick. (A single relocation reached
    /// from several code sites is not this: its operands agree, and
    /// `dedup_by_operands` folds it back into one candidate.)
    #[test]
    fn locator_rejects_two_relocations_of_the_player() {
        let mut ram = post_init();
        assert!(locate(&ram).is_some());
        let other = {
            let bytes = std::fs::read("../../assets/music/GoatTracker_V2_1982.sid").unwrap();
            let header = crate::header::parse(&bytes).unwrap();
            let mut emulator = Emulator::new();
            emulator.load(&header, &bytes).unwrap();
            emulator
                .call_init(header.init_address, SubtuneIndex(1), header.songs)
                .unwrap();
            emulator.ram_image()
        };
        assert!(locate(&other).is_some());
        ram[0x1000..0x1600].copy_from_slice(&other[0x1000..0x1600]);
        assert!(locate(&ram).is_none());
    }

    #[test]
    fn locator_ignores_frequency_signature_whose_state_is_outside_module() {
        let mut ram = post_init();
        let false_writer = [0xBD, 0x76, 0xBF, 0x9D, 0x00, 0xD4, 0x60];
        ram[0x3000..0x3000 + false_writer.len()].copy_from_slice(&false_writer);
        let layout = locate_in_module(&ram, 0xF000, u16::MAX, None).unwrap();
        assert_eq!(layout.resolved_frequencies, 0xF840);
    }
}
