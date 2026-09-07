//! Native decoder for the relocated GoatTracker V1 player.
//!
//! # What the player looks like
//!
//! V1 keeps three columnar per-voice state arrays — order cursor, pattern
//! cursor, playing frequency — addressed with the voice's SID register offset,
//! and resolves an authored note number through split frequency tables. Most
//! builds use 96 entries; extended players use 104 or 159.
//! The pattern reader is one shape across the whole family:
//!
//! ```text
//! LDY pattern_number,X ; LDA ptr_lo,Y ; STA zp ; LDA ptr_hi,Y ; STA zp+1
//! LDY pattern_position,X ; LDA (zp),Y ; INY ; CMP #$60
//! ```
//!
//! The orderlist reader is not: three generations ship in HVSC. One indexes a
//! split pointer table with a per-voice song index and understands repeat and
//! transpose commands (`CMP #$D0`); two cache the orderlist address in a
//! per-voice cell at init and read a bare pattern list terminated by `$FF` or
//! `$FE`. [`OrderLayout`] captures which, and the decoder switches grammar on
//! it. Every generation reads its own post-`init` state to find the selected
//! subtune's orderlists, so no subtune arithmetic is reimplemented here.
//!
//! # What is recovered
//!
//! Pitch comes from the player's own playing-frequency cells, sampled per
//! call; articulation stays trace-measured. When the orderlist reader is
//! recognised the extractor also recovers three orderlists, the packed
//! patterns they name, and runtime-observed pattern placements. Authored
//! instrument tables are not decoded yet.

mod decode;

use super::super::{DriverExtractor, EmulationStage, NativeContext, NativeError, NativeSong};
use super::{
    CursorLayout, absolute_operand, assemble, decode_pitches, dedup_by_operands, module_end,
    pointer_table_readers, resolve_phase, sample_native_state, trace_context, unique,
};
use crate::emu::Emulator;
use crate::header::PlayAddress;

/// Entries in each split frequency table — GoatTracker V1 spans eight octaves
/// of twelve semitones, and the two halves are always this far apart.
const FREQUENCY_TABLE_LEN: u16 = 0x60;
const EXTENDED_FREQUENCY_TABLE_LEN: u16 = 0x68;
const LONG_FREQUENCY_TABLE_LEN: u16 = 0x9F;

/// How the player reaches a voice's orderlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrderLayout {
    /// A split pointer table indexed by a per-voice song index that `init`
    /// sets from the subtune. Understands repeat, transpose and loop commands.
    Indexed {
        table_lo: u16,
        table_hi: u16,
        index_state: u16,
    },
    /// The orderlist address itself, cached per voice by `init`. The list is a
    /// bare sequence of pattern numbers; `terminal` is the byte that ends it.
    VoicePointer {
        pointer_lo: u16,
        pointer_hi: u16,
        terminal: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) struct GoatTrackerV1Layout {
    pub frequency_lo: u16,
    pub frequency_hi: u16,
    pub playing_frequency_lo: u16,
    pub playing_frequency_hi: u16,
    /// Authored note number cell. Builds that keep the note in a self-modified
    /// immediate instead have none, which costs nothing: pitch is read from
    /// the playing-frequency cells either way.
    pub note_numbers: Option<u16>,
    pub pattern_pointer_lo: u16,
    pub pattern_pointer_hi: u16,
    pub pattern_numbers: u16,
    pub pattern_positions: u16,
    pub order: OrderLayout,
    pub order_positions: u16,
}

impl GoatTrackerV1Layout {
    fn cursors(&self) -> CursorLayout {
        CursorLayout {
            frequency_state_lo: self.playing_frequency_lo,
            frequency_state_hi: self.playing_frequency_hi,
            order_positions: self.order_positions,
            pattern_numbers: self.pattern_numbers,
            pattern_positions: self.pattern_positions,
        }
    }

    fn pattern_count(&self) -> u16 {
        self.pattern_pointer_hi
            .saturating_sub(self.pattern_pointer_lo)
    }
}

/// A `LDY position,X / LDA pointer_lo,X / LDA pointer_hi,X / LDA (zp),Y /
/// CMP #terminal` reader: the orderlist address is already in RAM.
struct VoicePointerReader {
    pointer_lo: u16,
    pointer_hi: u16,
    position_state: u16,
    terminal: u8,
    code_address: usize,
}

fn voice_pointer_readers(ram: &[u8], terminal: u8) -> Vec<VoicePointerReader> {
    let mut readers = Vec::new();
    for start in 0..ram.len().saturating_sub(17) {
        let code = &ram[start..start + 17];
        if code[0] != 0xBC
            || code[3] != 0xBD
            || code[6] != 0x85
            || code[8] != 0xBD
            || code[11] != 0x85
            || code[13] != 0xB1
            || code[15] != 0xC9
            || code[16] != terminal
            || code[12] != code[7].wrapping_add(1)
            || code[14] != code[7]
        {
            continue;
        }
        readers.push(VoicePointerReader {
            pointer_lo: absolute_operand(&code[4..6]),
            pointer_hi: absolute_operand(&code[9..11]),
            position_state: absolute_operand(&code[1..3]),
            terminal,
            code_address: start,
        });
    }
    dedup_by_operands(&mut readers, |reader| {
        (reader.pointer_lo, reader.pointer_hi, reader.position_state)
    });
    readers
}

/// `LDA freq_lo,Y / STA playing_lo,X … LDA freq_hi,Y / STA playing_hi,X`. Some
/// builds mirror the low byte straight to `$D400,X` between the two loads, and
/// some store the high byte below the low one, so both are tolerated. The
/// table spacing pins the match down: an unrelated pair of adjacent cells
/// (instrument attack/decay into `$D405`/`$D406`, or the init-time orderlist
/// pointer copy) is never fed from one of the observed table spacings.
#[derive(Clone, Copy)]
struct FrequencyReader {
    table_lo: u16,
    table_hi: u16,
    playing_lo: u16,
    playing_hi: u16,
    code_address: usize,
}

fn is_register_page(address: u16) -> bool {
    (0xD000..0xE000).contains(&address)
}

fn frequency_readers(ram: &[u8]) -> Vec<FrequencyReader> {
    let mut readers = Vec::new();
    for start in 0..ram.len().saturating_sub(21) {
        let code = &ram[start..start + 21];
        if code[0] != 0xB9 || code[3] != 0x9D {
            continue;
        }
        let second = if code[6] == 0xB9 {
            6
        } else if code[6..9] == [0x9D, 0x00, 0xD4] && code[9] == 0xB9 {
            9
        } else {
            continue;
        };
        if code[second + 3] != 0x9D {
            continue;
        }
        let table_lo = absolute_operand(&code[1..3]);
        let table_hi = absolute_operand(&code[second + 1..second + 3]);
        let playing_lo = absolute_operand(&code[4..6]);
        let playing_hi = absolute_operand(&code[second + 4..second + 6]);
        let table_spacing = table_lo.wrapping_sub(table_hi);
        if !matches!(
            table_spacing,
            FREQUENCY_TABLE_LEN | EXTENDED_FREQUENCY_TABLE_LEN | LONG_FREQUENCY_TABLE_LEN
        ) || playing_lo.abs_diff(playing_hi) != 1
            || is_register_page(playing_lo)
            || is_register_page(playing_hi)
        {
            continue;
        }
        readers.push(FrequencyReader {
            table_lo,
            table_hi,
            playing_lo,
            playing_hi,
            code_address: start,
        });
    }
    dedup_by_operands(&mut readers, |reader| {
        (
            reader.table_lo,
            reader.table_hi,
            reader.playing_lo,
            reader.playing_hi,
        )
    });
    readers
}

/// `CMP #$5E / BEQ keyoff / BCS rest / [ADC transpose,X] / STA note,X` — the
/// note number the pattern reader just decoded, after the orderlist transpose
/// where the generation has one.
fn note_number_states(ram: &[u8]) -> Vec<u16> {
    let mut states = Vec::new();
    for start in 0..ram.len().saturating_sub(12) {
        let code = &ram[start..start + 12];
        if code[0..2] != [0xC9, 0x5E] || code[2] != 0xF0 || code[4] != 0xB0 {
            continue;
        }
        let store = if code[6] == 0x7D { 9 } else { 6 };
        if code[store] != 0x9D {
            continue;
        }
        states.push(absolute_operand(&code[store + 1..store + 3]));
    }
    states.sort_unstable();
    states.dedup();
    states
}

fn closest_unique<T>(
    mut candidates: Vec<T>,
    anchor: usize,
    address: impl Fn(&T) -> usize,
) -> Option<T> {
    candidates.sort_by_key(|candidate| address(candidate).abs_diff(anchor));
    if candidates.len() > 1
        && address(&candidates[0]).abs_diff(anchor) == address(&candidates[1]).abs_diff(anchor)
    {
        return None;
    }
    candidates.into_iter().next()
}

fn resolved_entry(ram: &[u8], mut address: PlayAddress) -> usize {
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
    usize::from(address.0)
}

fn locate_impl(ram: &[u8], play_address: Option<PlayAddress>) -> Option<GoatTrackerV1Layout> {
    let frequency_candidates = frequency_readers(ram);
    let frequency = match play_address {
        Some(play_address) => closest_unique(
            frequency_candidates,
            resolved_entry(ram, play_address),
            |reader| reader.code_address,
        )?,
        None => unique(frequency_candidates)?,
    };
    let pattern_candidates = pointer_table_readers(ram, 0x60);
    let pattern = if play_address.is_some() {
        closest_unique(pattern_candidates, frequency.code_address, |reader| {
            reader.code_address
        })?
    } else {
        unique(pattern_candidates)?
    };
    let mut orders: Vec<(OrderLayout, u16, usize)> = Vec::new();
    for reader in pointer_table_readers(ram, 0xD0) {
        let layout = OrderLayout::Indexed {
            table_lo: reader.table_lo,
            table_hi: reader.table_hi,
            index_state: reader.index_state,
        };
        orders.push((layout, reader.position_state, reader.code_address));
    }
    for terminal in [0xFF, 0xFE] {
        for reader in voice_pointer_readers(ram, terminal) {
            let layout = OrderLayout::VoicePointer {
                pointer_lo: reader.pointer_lo,
                pointer_hi: reader.pointer_hi,
                terminal: reader.terminal,
            };
            orders.push((layout, reader.position_state, reader.code_address));
        }
    }
    let (order, order_positions, _) = if play_address.is_some() {
        closest_unique(orders, pattern.code_address, |candidate| candidate.2)?
    } else {
        unique(orders)?
    };
    Some(GoatTrackerV1Layout {
        frequency_lo: frequency.table_lo,
        frequency_hi: frequency.table_hi,
        playing_frequency_lo: frequency.playing_lo,
        playing_frequency_hi: frequency.playing_hi,
        note_numbers: unique(note_number_states(ram)),
        pattern_pointer_lo: pattern.table_lo,
        pattern_pointer_hi: pattern.table_hi,
        pattern_numbers: pattern.index_state,
        pattern_positions: pattern.position_state,
        order,
        order_positions,
    })
}

pub(crate) fn locate(ram: &[u8]) -> Option<GoatTrackerV1Layout> {
    locate_impl(ram, None)
}

fn locate_pitch_only(ram: &[u8]) -> Option<GoatTrackerV1Layout> {
    let frequency = unique(frequency_readers(ram))?;
    Some(GoatTrackerV1Layout {
        frequency_lo: frequency.table_lo,
        frequency_hi: frequency.table_hi,
        playing_frequency_lo: frequency.playing_lo,
        playing_frequency_hi: frequency.playing_hi,
        note_numbers: unique(note_number_states(ram)),
        pattern_pointer_lo: 0,
        pattern_pointer_hi: 0,
        pattern_numbers: 0,
        pattern_positions: 0,
        order: OrderLayout::VoicePointer {
            pointer_lo: 0,
            pointer_hi: 0,
            terminal: 0,
        },
        order_positions: 0,
    })
}

fn locate_failure_reason(ram: &[u8]) -> String {
    let patterns = pointer_table_readers(ram, 0x60).len();
    let frequencies = frequency_readers(ram).len();
    let indexed_orders = pointer_table_readers(ram, 0xD0).len();
    let voice_orders = [0xFF, 0xFE]
        .into_iter()
        .map(|terminal| voice_pointer_readers(ram, terminal).len())
        .sum::<usize>();
    format!(
        "expected one candidate for each V1 signature, found pattern={patterns}, frequency={frequencies}, indexed_order={indexed_orders}, voice_order={voice_orders}"
    )
}

/// Extractor for the relocated GoatTracker V1 player.
pub struct GoatTrackerV1;

impl DriverExtractor for GoatTrackerV1 {
    fn name(&self) -> &'static str {
        "goattracker-v1"
    }

    fn handles(&self, driver: &str) -> bool {
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
        // Discovery runs after `init` because some builds ship the player
        // packed and unpack it there; the signatures are not in the loaded
        // image until then.
        let ram = image.ram_image();
        let (layout, full_pattern_grammar) = match locate_impl(&ram, Some(ctx.header.play_address))
        {
            Some(layout) => (layout, true),
            None => (
                locate_pitch_only(&ram).ok_or_else(|| NativeError::LocateFailed {
                    driver: ctx.driver.to_owned(),
                    extractor: self.name(),
                    reason: locate_failure_reason(&ram),
                })?,
                false,
            ),
        };
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
        // Taken after the player has run: the cached-pointer generations fill
        // their per-voice orderlist pointers on the first `play` call, not in
        // `init`, and the indexed generation's song index is written by `init`.
        // Reading the state back is what keeps subtune selection the player's
        // business rather than arithmetic reimplemented here.
        let module_ram = image.ram_image();
        let trace = trace_context(ctx, self.name())?;
        let decode = |delay| decode_pitches(ctx.timing.clock, &trace.truth, &samples, delay);
        let (notes, validation) = resolve_phase(decode, &trace.truth, trace.timing);
        let (structure, recovered_structure) = if full_pattern_grammar {
            match decode::recover(
                &module_ram,
                &layout,
                module_end(ctx, self.name())?,
                &samples,
            ) {
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

    fn post_init(asset: &str) -> Vec<u8> {
        let bytes = std::fs::read(format!("../../assets/music/{asset}")).unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut emulator = Emulator::new();
        emulator.load(&header, &bytes).unwrap();
        emulator
            .call_init(header.init_address, header.start_song, header.songs)
            .unwrap();
        emulator.ram_image()
    }

    /// The indexed generation: a split song-pointer table plus repeat and
    /// transpose orderlist commands.
    const INDEXED: &str = "GoatTracker_V1_Jamaik2.sid";
    /// A cached-pointer generation whose orderlists end with `$FF` plus a
    /// loop target.
    const LOOPING_POINTER: &str = "GoatTracker_V1_Lazy_Jones.sid";
    /// The other cached-pointer generation, whose orderlists end with `$FE`.
    const STOPPING_POINTER: &str = "GoatTracker_V1_MW1_Title.sid";

    #[test]
    fn locates_every_orderlist_generation() {
        for (asset, expected) in [
            (INDEXED, "indexed"),
            (LOOPING_POINTER, "pointer-ff"),
            (STOPPING_POINTER, "pointer-fe"),
        ] {
            let layout = locate(&post_init(asset)).unwrap();
            assert_eq!(layout.frequency_lo, layout.frequency_hi + 0x60, "{asset}");
            assert!(layout.pattern_count() > 0, "{asset}");
            let found = match layout.order {
                OrderLayout::Indexed { .. } => "indexed",
                OrderLayout::VoicePointer { terminal: 0xFF, .. } => "pointer-ff",
                OrderLayout::VoicePointer { terminal: 0xFE, .. } => "pointer-fe",
                OrderLayout::VoicePointer { .. } => "pointer-other",
            };
            assert_eq!(found, expected, "{asset}");
        }
    }

    /// Two generations of the player in one image name two different sets of
    /// tables and two different orderlist grammars, with no evidence for
    /// choosing between them — so discovery must refuse rather than pick. (One
    /// player reached from several code sites is not this: its operands agree,
    /// and `dedup_by_operands` folds it back into a single candidate.)
    #[test]
    fn locator_rejects_two_players_in_one_image() {
        let mut ram = post_init(LOOPING_POINTER);
        assert!(locate(&ram).is_some());
        let other = post_init(INDEXED);
        assert!(locate(&other).is_some());
        // The indexed fixture loads at $8000; its player and tables fit in the
        // free RAM above the looping-pointer fixture's own module.
        ram[0x4000..0x4A00].copy_from_slice(&other[0x8000..0x8A00]);
        assert!(locate(&ram).is_none());
    }

    #[test]
    fn frequency_reader_accepts_observed_table_lengths() {
        for spacing in [0x60_u16, 0x68, 0x9F] {
            let mut ram = vec![0; 0x10000];
            let [lo, hi] = 0x2000_u16.wrapping_add(spacing).to_le_bytes();
            ram[0x1000..0x100c].copy_from_slice(&[
                0xB9, lo, hi, 0x9D, 0x01, 0x30, 0xB9, 0x00, 0x20, 0x9D, 0x02, 0x30,
            ]);
            let readers = frequency_readers(&ram);
            assert_eq!(readers.len(), 1);
            assert_eq!(readers[0].table_lo, 0x2000 + spacing);
            assert_eq!(readers[0].table_hi, 0x2000);
        }
    }

    fn extract(asset: &str, frames: u32) -> crate::export::native::NativeSong {
        let bytes = std::fs::read(format!("../../assets/music/{asset}")).unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let subtune = SubtuneIndex(1);
        let timing = crate::emu::PlaybackTiming::for_subtune(&header, subtune);
        let (_, extractor, song) = extract_native_song(
            &PlayerDb::embedded(),
            &header,
            &bytes,
            subtune,
            timing,
            frames,
        )
        .unwrap();
        assert_eq!(extractor, "goattracker-v1");
        song
    }

    #[test]
    fn every_generation_recovers_orderlists_and_patterns() {
        for asset in [INDEXED, LOOPING_POINTER, STOPPING_POINTER] {
            let song = extract(asset, 3_000);
            assert!(song.validation.accepted, "{asset}");
            assert!(!song.notes.is_empty(), "{asset}");
            assert!(song.structure.is_some(), "{asset}");
            let recovered = song.recovered_structure.unwrap();
            assert_eq!(recovered.voices.len(), 3, "{asset}");
            assert!(!recovered.patterns.is_empty(), "{asset}");
            assert!(
                recovered.patterns.values().all(|events| !events.is_empty()),
                "{asset}"
            );
            assert!(
                recovered
                    .patterns
                    .values()
                    .flatten()
                    .any(|event| event.duration.0 > 1),
                "{asset}"
            );
            assert!(
                recovered
                    .voices
                    .iter()
                    .any(|voice| !voice.instances.is_empty()),
                "{asset}"
            );
            assert!(
                recovered
                    .voices
                    .iter()
                    .flat_map(|voice| &voice.instances)
                    .all(|instance| recovered.patterns.contains_key(&instance.pattern)),
                "{asset}"
            );
        }
    }

    #[test]
    fn the_indexed_generation_recovers_repeat_and_pattern_commands() {
        let recovered = extract(INDEXED, 3_000).recovered_structure.unwrap();
        let commands: Vec<_> = recovered
            .voices
            .iter()
            .flat_map(|voice| &voice.order_commands)
            .collect();
        assert!(commands.iter().any(|command| matches!(
            command,
            crate::export::RecoveredOrderCommand::DriverCommand { .. }
        )));
        assert!(commands.iter().any(
            |command| matches!(command, crate::export::RecoveredOrderCommand::Pattern {
                    repeat,
                    ..
                } if repeat.0 > 1)
        ));
        assert!(
            recovered
                .voices
                .iter()
                .all(|voice| voice.order_loop_offset.is_some())
        );
    }

    #[test]
    fn a_row_carrying_an_instrument_also_carries_its_note() {
        let recovered = extract(LOOPING_POINTER, 3_000).recovered_structure.unwrap();
        assert!(
            recovered
                .patterns
                .values()
                .flatten()
                .any(|event| event.instrument.is_some() && event.frequency_index.is_some())
        );
        assert!(
            recovered
                .patterns
                .values()
                .flatten()
                .any(|event| event.command.is_some())
        );
    }
}
