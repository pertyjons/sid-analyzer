//! Native decoder for the David Whittaker bytecode-player family used by
//! Defcom, Glider Rider, and related tunes.
//!
//! The player keeps three relocated 36-byte voice states. Its playroutine
//! transforms each authored note index before resolving it through an
//! interleaved SID-frequency table. This extractor locates the voice states,
//! table, and actual lookup instruction from code/data signatures, then records
//! the effective lookup offset while replaying the player. Variants without
//! coherent evidence for all three pieces are rejected before decoding.
//! The same replay observes the musical byte fetch: pointer discontinuities
//! delimit source patterns and revisits recover native placements and reuse.

use super::{
    DriverExtractor, FieldProvenance, NativeContext, NativeError, NativeSong,
    NativeValidationPolicy, ProvenanceEvidence, note_from_raw_freq, validate_native_notes,
};
use crate::analysis::effects::{EffectThresholds, detect_effects};
use crate::analysis::note::{GmProgram, NoteEvent, detect_notes};
use crate::analysis::timbre::{
    apply_voice3_lfo_detection, extract_characteristics, extract_patches,
};
use crate::analysis::{SystemClock, analyze};
use crate::emu::{self, Emulator};
use crate::export::{
    FrequencyTableIndex, NativeDriverOpcode, NativeEffectByte, NativePlacement, NativeRowTick,
    OrderOffset, PatternByteOffset, PatternDuration, PatternNumber, PatternRepeatCount,
    PatternTranspose, RecoveredOrderCommand, RecoveredPatternEvent, RecoveredPatternInstance,
    RecoveredStructure, RecoveredVoiceStructure, RepeatOrdinal, VoicePlacements,
};
use crate::trace::FrameIndex;

const VOICE_STATE_LEN: u16 = 0x24;
const FREQUENCY_PREFIX: [u8; 10] = [0x16, 0x01, 0x26, 0x01, 0x38, 0x01, 0x4B, 0x01, 0x60, 0x01];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) struct ZeroPagePointer(u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WhittakerLayout {
    pub frequency_table: u16,
    pub(crate) variant: WhittakerVariant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WhittakerVariant {
    StateRecords {
        voice_states: [u16; 3],
        frequency_lookup: u16,
        voice_state_pointer: ZeroPagePointer,
        musical_fetch: u16,
        musical_pointer: ZeroPagePointer,
    },
    NibbleNotes {
        note_calls: [u16; 3],
        musical_fetches: [u16; 3],
        musical_pointer: ZeroPagePointer,
    },
}

fn byte_at(ram: &[u8], address: u16) -> u8 {
    ram[usize::from(address)]
}

fn state_candidates(ram: &[u8]) -> Vec<[u16; 3]> {
    const PREFIX: [u8; 4] = [0xA0, 0x23, 0xA9, 0x00];
    let mut candidates = Vec::new();
    for start in 0..=ram.len().saturating_sub(19) {
        if ram[start..start + PREFIX.len()] != PREFIX {
            continue;
        }
        let bytes = &ram[start + 4..start + 19];
        if bytes[0] != 0x99
            || bytes[3] != 0x99
            || bytes[6] != 0x99
            || bytes[9] != 0x88
            || bytes[10] != 0x10
            || bytes[11] != 0xF4
        {
            continue;
        }
        let first = u16::from_le_bytes([bytes[1], bytes[2]]);
        let second = u16::from_le_bytes([bytes[4], bytes[5]]);
        let third = u16::from_le_bytes([bytes[7], bytes[8]]);
        if second == first.wrapping_add(VOICE_STATE_LEN)
            && third == second.wrapping_add(VOICE_STATE_LEN)
        {
            candidates.push([first, second, third]);
        }
    }
    candidates
}

fn voice_state_pointer(ram: &[u8], frequency_lookup: u16) -> Option<ZeroPagePointer> {
    let lookup = usize::from(frequency_lookup);
    let start = lookup.saturating_sub(0x80);
    ram[start..lookup]
        .windows(5)
        .rev()
        .find_map(|bytes| {
            (bytes[..3] == [0xA0, 0x12, 0xB1] && bytes[4] == 0xAA).then_some(bytes[3])
        })
        .map(ZeroPagePointer)
}

fn musical_fetch(
    ram: &[u8],
    frequency_lookup: u16,
    voice_state_pointer: ZeroPagePointer,
) -> Option<(u16, ZeroPagePointer)> {
    let pointer = ZeroPagePointer(voice_state_pointer.0.wrapping_sub(2));
    let lookup = usize::from(frequency_lookup);
    let start = lookup.saturating_sub(0x300);
    let pattern = [
        0xA0,
        0x00,
        0xB1,
        pointer.0,
        0xAA,
        0xE6,
        pointer.0,
        0xD0,
        0x02,
        0xE6,
        pointer.0.wrapping_add(1),
        0x8A,
        0x30,
    ];
    let mut matches = ram[start..lookup]
        .windows(pattern.len())
        .enumerate()
        .filter_map(|(offset, bytes)| (bytes == pattern).then_some(start + offset + 2));
    let fetch = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some((u16::try_from(fetch).ok()?, pointer))
}

type FrequencyCandidate = (u16, u16, ZeroPagePointer, u16, ZeroPagePointer);

fn referenced_frequency_base(ram: &[u8], canonical: u16) -> Option<FrequencyCandidate> {
    let mut candidates = Vec::new();
    for at in 0..ram.len().saturating_sub(16) {
        if ram[at] != 0xBD {
            continue;
        }
        let base = u16::from_le_bytes([ram[at + 1], ram[at + 2]]);
        if !(canonical..=canonical.wrapping_add(0x60)).contains(&base) {
            continue;
        }
        let paired = (at + 3..=(at + 13).min(ram.len().saturating_sub(3))).any(|next| {
            ram[next] == 0xBD
                && u16::from_le_bytes([ram[next + 1], ram[next + 2]]) == base.wrapping_add(1)
        });
        if paired
            && !candidates
                .iter()
                .any(|(candidate, _, _, _, _)| *candidate == base)
        {
            let lookup = u16::try_from(at).ok()?;
            let state_pointer = voice_state_pointer(ram, lookup)?;
            let (musical_fetch, musical_pointer) = musical_fetch(ram, lookup, state_pointer)?;
            candidates.push((base, lookup, state_pointer, musical_fetch, musical_pointer));
        }
    }
    let [candidate] = candidates.as_slice() else {
        return None;
    };
    Some(*candidate)
}

fn frequency_candidates(ram: &[u8]) -> Vec<FrequencyCandidate> {
    ram.windows(FREQUENCY_PREFIX.len())
        .enumerate()
        .filter_map(|(address, bytes)| {
            (bytes == FREQUENCY_PREFIX)
                .then(|| u16::try_from(address).ok())
                .flatten()
                .and_then(|canonical| referenced_frequency_base(ram, canonical))
        })
        .collect()
}

fn locate_state_records(ram: &[u8]) -> Option<WhittakerLayout> {
    let states = state_candidates(ram);
    let frequencies = frequency_candidates(ram);
    match (states.as_slice(), frequencies.as_slice()) {
        (
            [voice_states],
            [
                (
                    frequency_table,
                    frequency_lookup,
                    voice_state_pointer,
                    musical_fetch,
                    musical_pointer,
                ),
            ],
        ) => Some(WhittakerLayout {
            frequency_table: *frequency_table,
            variant: WhittakerVariant::StateRecords {
                voice_states: *voice_states,
                frequency_lookup: *frequency_lookup,
                voice_state_pointer: *voice_state_pointer,
                musical_fetch: *musical_fetch,
                musical_pointer: *musical_pointer,
            },
        }),
        _ => None,
    }
}

fn nibble_frequency_conversion(ram: &[u8]) -> Option<(u16, u16)> {
    let mut candidates = Vec::new();
    for at in 0..=ram.len().saturating_sub(26) {
        let bytes = &ram[at..at + 26];
        if bytes[0..5] != [0x48, 0x29, 0x0F, 0x0A, 0xAA]
            || bytes[5] != 0xBD
            || bytes[8] != 0x8D
            || bytes[11] != 0xBD
            || bytes[14] != 0x8D
            || bytes[17..26] != [0x68, 0x4A, 0x4A, 0x4A, 0x4A, 0xAA, 0xCA, 0x30, 0x09]
        {
            continue;
        }
        let table = u16::from_le_bytes([bytes[6], bytes[7]]);
        let high = u16::from_le_bytes([bytes[12], bytes[13]]);
        if high == table.wrapping_add(1) {
            candidates.push((u16::try_from(at).ok()?, table));
        }
    }
    let [candidate] = candidates.as_slice() else {
        return None;
    };
    Some(*candidate)
}

fn sid_frequency_store_after(ram: &[u8], call: usize, voice: usize) -> bool {
    let target = [0xD400_u16, 0xD407, 0xD40E][voice];
    ram[call..(call + 32).min(ram.len())]
        .windows(3)
        .any(|bytes| bytes[0] == 0x8D && u16::from_le_bytes([bytes[1], bytes[2]]) == target)
}

fn nibble_note_sites(ram: &[u8], conversion: u16) -> Option<([u16; 3], [u16; 3], u8)> {
    let target = conversion.to_le_bytes();
    let mut call_indices = [0_usize; 3];
    let mut fetches = [0_u16; 3];
    let mut pointer = None;
    for voice in 0..3 {
        let mut candidates = Vec::new();
        for call in 0..=ram.len().saturating_sub(3) {
            if ram[call..call + 3] != [0x20, target[0], target[1]]
                || !sid_frequency_store_after(ram, call, voice)
            {
                continue;
            }
            let start = call.saturating_sub(0x70);
            let fetches: Vec<_> = ram[start..call]
                .windows(6)
                .enumerate()
                .filter_map(|(offset, bytes)| {
                    (bytes[0] == 0xB1 && bytes[2..6] == [0xC9, 0x7F, 0xD0, 0x1D])
                        .then_some((start + offset, bytes[1]))
                })
                .collect();
            if let [(fetch, fetch_pointer)] = fetches.as_slice() {
                candidates.push((call, *fetch, *fetch_pointer));
            }
        }
        let [(call, fetch, fetch_pointer)] = candidates.as_slice() else {
            return None;
        };
        if pointer.is_some_and(|candidate| candidate != *fetch_pointer) {
            return None;
        }
        pointer = Some(*fetch_pointer);
        call_indices[voice] = *call;
        fetches[voice] = u16::try_from(*fetch).ok()?;
    }
    let [Some(first), Some(second), Some(third)] =
        call_indices.map(|call| u16::try_from(call).ok())
    else {
        return None;
    };
    Some(([first, second, third], fetches, pointer?))
}

fn locate_nibble_notes(ram: &[u8]) -> Option<WhittakerLayout> {
    let (conversion, frequency_table) = nibble_frequency_conversion(ram)?;
    let (note_calls, musical_fetches, musical_pointer) = nibble_note_sites(ram, conversion)?;
    Some(WhittakerLayout {
        frequency_table,
        variant: WhittakerVariant::NibbleNotes {
            note_calls,
            musical_fetches,
            musical_pointer: ZeroPagePointer(musical_pointer),
        },
    })
}

pub(crate) fn locate(ram: &[u8]) -> Option<WhittakerLayout> {
    match (locate_state_records(ram), locate_nibble_notes(ram)) {
        (Some(layout), None) | (None, Some(layout)) => Some(layout),
        _ => None,
    }
}

fn native_pitch(clock: SystemClock, note: &NoteEvent, raw_frequency: u16) -> Option<NoteEvent> {
    let end = note.end_frame.unwrap_or(note.start_frame).0;
    let mut decoded = note_from_raw_freq(
        u32::from(raw_frequency),
        clock,
        note.voice,
        note.start_frame.0,
        end,
    )?;
    decoded.end_frame = note.end_frame;
    decoded.program = note.program;
    decoded.velocity = note.velocity;
    Some(decoded)
}

fn decode_notes(
    clock: SystemClock,
    truth: &[NoteEvent],
    frequencies: &[[u16; 3]],
) -> Vec<NoteEvent> {
    let mut notes = Vec::with_capacity(truth.len());
    for note in truth {
        let Some(frame) = frequencies.get(note.start_frame.0 as usize) else {
            continue;
        };
        let raw_frequency = frame[note.voice.to_index()];
        if let Some(decoded) = native_pitch(clock, note, raw_frequency) {
            notes.push(decoded);
        }
    }
    notes
}

fn validation_notes(native: &[NoteEvent], truth: &[NoteEvent]) -> Vec<NoteEvent> {
    native
        .iter()
        .map(|authored| {
            let traced = truth.iter().find(|traced| {
                authored.voice == traced.voice && authored.start_frame == traced.start_frame
            });
            if let Some(traced) = traced.filter(|traced| traced.program == GmProgram::SYNTH_DRUM) {
                return NoteEvent {
                    midi: traced.midi,
                    cents: traced.cents,
                    ..*authored
                };
            }
            *authored
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct StreamObservation {
    source: u16,
    value: u8,
    duration: Option<u16>,
    duration_index: Option<u8>,
    melodic: bool,
    frame: FrameIndex,
}

struct WhittakerReplay {
    native_frequencies: Vec<[u16; 3]>,
    streams: [Vec<StreamObservation>; 3],
}

fn nibble_note_frequency(ram: &[u8], frequency_table: u16, note: u8) -> u16 {
    let semitone_offset = u16::from(note & 0x0F) * 2;
    let lo = byte_at(ram, frequency_table.wrapping_add(semitone_offset));
    let hi = byte_at(ram, frequency_table.wrapping_add(semitone_offset + 1));
    let base = u16::from_le_bytes([lo, hi]);
    let octave = note >> 4;
    base.checked_shl(u32::from(octave)).unwrap_or_default()
}

fn replay_player(
    emulator: &mut Emulator,
    layout: &WhittakerLayout,
    play: crate::header::PlayAddress,
    frames: u32,
) -> Result<WhittakerReplay, crate::emu::EmuError> {
    let mut frequencies = Vec::with_capacity(frames as usize);
    let mut streams: [Vec<StreamObservation>; 3] = std::array::from_fn(|_| Vec::new());
    let mut current_frequencies = [0; 3];
    for frame in 0..frames {
        emulator.run_play_frame_observed(play, FrameIndex(frame), |cpu| match layout.variant {
            WhittakerVariant::StateRecords {
                voice_states,
                frequency_lookup,
                voice_state_pointer,
                musical_fetch,
                musical_pointer,
            } => {
                let state_pointer = usize::from(voice_state_pointer.0);
                let voice_state = u16::from(cpu.memory.ram[state_pointer])
                    | (u16::from(cpu.memory.ram[(state_pointer + 1) & 0xFF]) << 8);
                let Some(voice) = voice_states
                    .iter()
                    .position(|candidate| *candidate == voice_state)
                else {
                    return;
                };
                if cpu.registers.program_counter == frequency_lookup {
                    let offset = u16::from(cpu.registers.index_x);
                    let lo = byte_at(
                        &cpu.memory.ram[..],
                        layout.frequency_table.wrapping_add(offset),
                    );
                    let hi = byte_at(
                        &cpu.memory.ram[..],
                        layout.frequency_table.wrapping_add(offset + 1),
                    );
                    current_frequencies[voice] = u16::from_le_bytes([lo, hi]);
                }
                if cpu.registers.program_counter == musical_fetch {
                    let pointer = usize::from(musical_pointer.0);
                    let source = u16::from(cpu.memory.ram[pointer])
                        | (u16::from(cpu.memory.ram[(pointer + 1) & 0xFF]) << 8);
                    streams[voice].push(StreamObservation {
                        source,
                        value: cpu.memory.ram[usize::from(source)],
                        duration: Some(
                            match cpu.memory.ram[usize::from(voice_state.wrapping_add(0x11))] {
                                0 => 256,
                                duration => u16::from(duration),
                            },
                        ),
                        duration_index: Some(
                            cpu.memory.ram[usize::from(voice_state.wrapping_add(0x11))],
                        ),
                        melodic: cpu.memory.ram[usize::from(source)] < 0x80,
                        frame: FrameIndex(frame),
                    });
                }
            }
            WhittakerVariant::NibbleNotes {
                note_calls,
                musical_fetches,
                musical_pointer,
            } => {
                let pc = cpu.registers.program_counter;
                if let Some(voice) = note_calls.iter().position(|candidate| *candidate == pc) {
                    current_frequencies[voice] = nibble_note_frequency(
                        &cpu.memory.ram[..],
                        layout.frequency_table,
                        cpu.registers.accumulator,
                    );
                }
                if let Some(voice) = musical_fetches
                    .iter()
                    .position(|candidate| *candidate == pc)
                {
                    let pointer = usize::from(musical_pointer.0);
                    let base = u16::from(cpu.memory.ram[pointer])
                        | (u16::from(cpu.memory.ram[(pointer + 1) & 0xFF]) << 8);
                    let source = base.wrapping_add(u16::from(cpu.registers.index_y));
                    let value = cpu.memory.ram[usize::from(source)];
                    streams[voice].push(StreamObservation {
                        source,
                        value,
                        duration: None,
                        duration_index: None,
                        melodic: value < 0x7F,
                        frame: FrameIndex(frame),
                    });
                }
            }
        })?;
        frequencies.push(current_frequencies);
    }
    Ok(WhittakerReplay {
        native_frequencies: frequencies,
        streams,
    })
}

fn recovered_structure(
    ram: &[u8],
    streams: &[Vec<StreamObservation>; 3],
) -> Option<(Vec<VoicePlacements>, RecoveredStructure)> {
    let mut patterns = std::collections::BTreeMap::new();
    let mut address_owner = std::collections::HashMap::new();
    let mut pattern_starts = std::collections::HashMap::new();
    let mut placements: [Vec<NativePlacement>; 3] = std::array::from_fn(|_| Vec::new());
    let mut instances: [Vec<RecoveredPatternInstance>; 3] = std::array::from_fn(|_| Vec::new());
    let mut commands: [Vec<RecoveredOrderCommand>; 3] = std::array::from_fn(|_| Vec::new());

    for voice in 0..3 {
        let mut active: Option<PatternNumber> = None;
        let mut previous_source = None;
        for observation in &streams[voice] {
            let contiguous = previous_source.is_some_and(|previous: u16| {
                let delta = observation.source.wrapping_sub(previous);
                (1..=4).contains(&delta)
            });
            if active.is_none() || !contiguous {
                let pattern = if let Some(pattern) = address_owner.get(&observation.source) {
                    *pattern
                } else {
                    let number = u8::try_from(pattern_starts.len()).ok()?;
                    let pattern = PatternNumber(number);
                    pattern_starts.insert(pattern, observation.source);
                    pattern
                };
                active = Some(pattern);
                let order_offset = OrderOffset(commands[voice].len());
                let repeat_ordinal = RepeatOrdinal(
                    instances[voice]
                        .iter()
                        .filter(|instance| instance.pattern == pattern)
                        .count() as u32,
                );
                commands[voice].push(RecoveredOrderCommand::Pattern {
                    order_offset,
                    pattern,
                    repeat: PatternRepeatCount(1),
                });
                placements[voice].push(NativePlacement {
                    pattern_number: pattern,
                    start_frame: observation.frame,
                    transpose: PatternTranspose(0),
                    order_offset: Some(order_offset),
                    repeat_ordinal: Some(repeat_ordinal),
                });
                instances[voice].push(RecoveredPatternInstance {
                    pattern,
                    transpose: PatternTranspose(0),
                    repeat_ordinal,
                    order_offset,
                    start_tick: NativeRowTick(observation.frame.0),
                    start_frame: observation.frame,
                });
            }

            let pattern = active?;
            let start = *pattern_starts.get(&pattern)?;
            address_owner.entry(observation.source).or_insert(pattern);
            let melodic = observation.melodic;
            let event = RecoveredPatternEvent {
                offset: PatternByteOffset(observation.source.wrapping_sub(start)),
                duration: PatternDuration(if melodic {
                    observation.duration.unwrap_or_default()
                } else {
                    0
                }),
                frequency_index: melodic.then_some(FrequencyTableIndex(observation.value)),
                instrument: None,
                hold: false,
                slide: None,
                command: (!melodic).then_some(NativeDriverOpcode(observation.value)),
                command_data: (!melodic).then_some(NativeEffectByte(
                    ram[usize::from(observation.source.wrapping_add(1))],
                )),
                duration_index: observation
                    .duration_index
                    .filter(|_| melodic)
                    .map(NativeEffectByte),
                operand: None,
            };
            let events = patterns.entry(pattern).or_insert_with(Vec::new);
            if !events.contains(&event) {
                events.push(event);
            }
            previous_source = Some(observation.source);
        }
        if !commands[voice].is_empty() {
            let order_offset = OrderOffset(commands[voice].len());
            commands[voice].push(RecoveredOrderCommand::Stop { order_offset });
        }
    }

    let mut structure = Vec::new();
    let mut voices = Vec::new();
    for voice in 0..3 {
        if instances[voice].is_empty() {
            continue;
        }
        structure.push(VoicePlacements {
            voice: crate::analysis::VoiceId::from_index(voice),
            placements: std::mem::take(&mut placements[voice]),
        });
        voices.push(RecoveredVoiceStructure {
            voice: crate::analysis::VoiceId::from_index(voice),
            order_loop_offset: None,
            order_commands: std::mem::take(&mut commands[voice]),
            instances: std::mem::take(&mut instances[voice]),
        });
    }
    Some((structure, RecoveredStructure { patterns, voices }))
}

pub struct WhittakerExtractor;

impl DriverExtractor for WhittakerExtractor {
    fn name(&self) -> &'static str {
        "whittaker"
    }

    fn handles(&self, driver: &str) -> bool {
        driver == "David_Whittaker"
    }

    fn extract(&self, ctx: &NativeContext<'_>) -> Result<NativeSong, NativeError> {
        let emu_err = |error: crate::emu::EmuError| NativeError::Emulation {
            driver: ctx.driver.to_owned(),
            stage: super::EmulationStage::ExtractorSetup,
            reason: error.to_string(),
        };

        let mut image = Emulator::with_timing(ctx.timing);
        image.load(ctx.header, ctx.bytes).map_err(emu_err)?;
        image
            .call_init(ctx.header.init_address, ctx.subtune, ctx.header.songs)
            .map_err(emu_err)?;
        let ram = image.ram_image();
        let layout = locate(&ram).ok_or(NativeError::LocateFailed {
            driver: ctx.driver.to_owned(),
            extractor: self.name(),
            reason: "required Whittaker signatures were not unique".to_owned(),
        })?;

        let replay = replay_player(&mut image, &layout, ctx.header.play_address, ctx.frames)
            .map_err(emu_err)?;
        let Some((structure, recovered_structure)) = recovered_structure(&ram, &replay.streams)
        else {
            return Err(NativeError::StructureInvariant {
                driver: ctx.driver.to_owned(),
                extractor: self.name(),
                reason: "musical stream recovery exceeded the pattern-number domain".to_owned(),
            });
        };
        let trace =
            emu::run_with_timing(ctx.header, ctx.bytes, ctx.subtune, ctx.frames, ctx.timing)
                .map_err(emu_err)?;
        let validation_timing = ctx.validation_timing(&trace, self.name())?;
        let states = analyze(&trace);
        let effects = detect_effects(&trace, &states, EffectThresholds::default());
        let truth = detect_notes(&states, ctx.timing.clock);
        let notes = decode_notes(ctx.timing.clock, &truth, &replay.native_frequencies);
        if notes.is_empty() {
            return Err(NativeError::DecodeEmpty {
                driver: ctx.driver.to_owned(),
                extractor: self.name(),
            });
        }

        let notes_for_validation = validation_notes(&notes, &truth);
        let validation = validate_native_notes(
            &notes_for_validation,
            &truth,
            validation_timing,
            NativeValidationPolicy::default(),
        );
        if !validation.accepted {
            return Err(NativeError::DecodeUnreliable {
                driver: ctx.driver.to_owned(),
                extractor: self.name(),
                reason: validation.reason_summary(),
            });
        }

        let voice3_reads = trace.voice3_reads_per_frame();
        let mut characteristics: Vec<_> = notes
            .iter()
            .map(|note| extract_characteristics(note, &states, &effects, ctx.timing.clock))
            .collect();
        apply_voice3_lfo_detection(&mut characteristics, &notes, &voice3_reads);
        let (patches, patch_assignments) = extract_patches(&notes, &characteristics);
        let provenance = vec![
            ProvenanceEvidence {
                field: "note.pitch".to_owned(),
                provenance: FieldProvenance::AuthoredDecoded,
                samples: notes.len(),
                mismatches: validation.inserted.0,
            },
            ProvenanceEvidence {
                field: "note.articulation".to_owned(),
                provenance: FieldProvenance::TraceMeasured,
                samples: notes.len(),
                mismatches: 0,
            },
            ProvenanceEvidence {
                field: "song.structure".to_owned(),
                provenance: FieldProvenance::AuthoredDecoded,
                samples: recovered_structure.patterns.len(),
                mismatches: 0,
            },
        ];

        Ok(NativeSong {
            capture: trace.capture.clone(),
            states,
            notes,
            patches,
            patch_assignments,
            characteristics,
            effects,
            structure: Some(structure),
            recovered_structure: Some(recovered_structure),
            validation,
            provenance,
        })
    }
}

#[cfg(all(test, feature = "asset-tests"))]
mod tests {
    use super::*;
    use crate::header::SubtuneIndex;
    use crate::playerid::PlayerDb;

    fn post_init(asset: &str) -> Vec<u8> {
        let bytes = std::fs::read(format!("../../assets/music/{asset}")).unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut emulator = Emulator::new();
        emulator.load(&header, &bytes).unwrap();
        emulator
            .call_init(header.init_address, SubtuneIndex(1), header.songs)
            .unwrap();
        emulator.ram_image()
    }

    #[test]
    fn locates_defcom_and_glider_rider() {
        let defcom = locate(&post_init("Defcom.sid")).unwrap();
        assert!(matches!(
            defcom.variant,
            WhittakerVariant::StateRecords {
                voice_states: [0xE9C6, 0xE9EA, 0xEA0E],
                ..
            }
        ));
        assert_eq!(defcom.frequency_table, 0xEF91);

        let glider = locate(&post_init("Glider_Rider.sid")).unwrap();
        assert!(matches!(
            glider.variant,
            WhittakerVariant::StateRecords {
                voice_states: [0xE0C5, 0xE0E9, 0xE10D],
                ..
            }
        ));
        assert_eq!(glider.frequency_table, 0xE6BC);
    }

    #[test]
    fn locates_nibble_note_generation_from_code_references() {
        let mut ram = vec![0; 0x1_0000];
        let conversion = 0x5000_usize;
        ram[conversion..conversion + 26].copy_from_slice(&[
            0x48, 0x29, 0x0F, 0x0A, 0xAA, 0xBD, 0x00, 0x60, 0x8D, 0x00, 0x20, 0xBD, 0x01, 0x60,
            0x8D, 0x01, 0x20, 0x68, 0x4A, 0x4A, 0x4A, 0x4A, 0xAA, 0xCA, 0x30, 0x09,
        ]);
        for voice in 0..3_usize {
            let call = 0x2000 + voice * 0x100;
            let fetch = call - 0x50;
            ram[fetch..fetch + 6].copy_from_slice(&[0xB1, 0x40, 0xC9, 0x7F, 0xD0, 0x1D]);
            ram[call..call + 3].copy_from_slice(&[0x20, 0x00, 0x50]);
            let sid = (0xD400_u16 + u16::try_from(voice).unwrap() * 7).to_le_bytes();
            ram[call + 8..call + 11].copy_from_slice(&[0x8D, sid[0], sid[1]]);
        }

        let layout = locate(&ram).unwrap();
        assert_eq!(layout.frequency_table, 0x6000);
        assert!(matches!(
            layout.variant,
            WhittakerVariant::NibbleNotes {
                note_calls: [0x2000, 0x2100, 0x2200],
                musical_fetches: [0x1FB0, 0x20B0, 0x21B0],
                musical_pointer: ZeroPagePointer(0x40),
            }
        ));
    }

    #[test]
    fn extracts_both_repository_whittaker_tunes() {
        let db = PlayerDb::embedded();
        for asset in ["Defcom.sid", "Glider_Rider.sid"] {
            let bytes = std::fs::read(format!("../../assets/music/{asset}")).unwrap();
            let header = crate::header::parse(&bytes).unwrap();
            let timing = crate::emu::PlaybackTiming::for_subtune(&header, SubtuneIndex(1));
            let (_, extractor, song) = super::super::extract_native_song(
                &db,
                &header,
                &bytes,
                SubtuneIndex(1),
                timing,
                400,
            )
            .unwrap_or_else(|error| panic!("{asset}: {error}"));
            assert_eq!(extractor, "whittaker");
            assert!(song.validation.accepted);
            assert!(!song.notes.is_empty());
            let recovered = song
                .recovered_structure
                .as_ref()
                .expect("Whittaker source structure");
            assert!(!recovered.patterns.is_empty());
            assert!(
                song.structure
                    .as_ref()
                    .is_some_and(|voices| !voices.is_empty())
            );
        }
    }

    #[test]
    fn stream_revisits_reuse_the_original_pattern() {
        let mut ram = vec![0; 0x1_0000];
        ram[0x100] = 1;
        ram[0x101] = 2;
        ram[0x200] = 3;
        let streams = [
            vec![
                StreamObservation {
                    source: 0x100,
                    value: 1,
                    duration: Some(4),
                    duration_index: Some(4),
                    melodic: true,
                    frame: FrameIndex(0),
                },
                StreamObservation {
                    source: 0x101,
                    value: 2,
                    duration: Some(4),
                    duration_index: Some(4),
                    melodic: true,
                    frame: FrameIndex(4),
                },
                StreamObservation {
                    source: 0x200,
                    value: 3,
                    duration: Some(4),
                    duration_index: Some(4),
                    melodic: true,
                    frame: FrameIndex(8),
                },
                StreamObservation {
                    source: 0x100,
                    value: 1,
                    duration: Some(4),
                    duration_index: Some(4),
                    melodic: true,
                    frame: FrameIndex(12),
                },
            ],
            Vec::new(),
            Vec::new(),
        ];
        let (structure, recovered) = recovered_structure(&ram, &streams).unwrap();
        assert_eq!(recovered.patterns.len(), 2);
        assert_eq!(structure[0].placements.len(), 3);
        assert_eq!(
            structure[0].placements[0].pattern_number,
            structure[0].placements[2].pattern_number
        );
        assert_eq!(
            structure[0].placements[2].repeat_ordinal,
            Some(RepeatOrdinal(1))
        );
    }

    #[test]
    fn full_length_primary_tunes_pass_native_validation() {
        let db = PlayerDb::embedded();
        for (asset, subtune, frames) in [
            ("Defcom.sid", 1, 5_400),
            ("Glider_Rider.sid", 1, 5_100),
            ("Glider_Rider.sid", 2, 5_100),
            ("Glider_Rider.sid", 3, 5_100),
        ] {
            let bytes = std::fs::read(format!("../../assets/music/{asset}")).unwrap();
            let header = crate::header::parse(&bytes).unwrap();
            let subtune = SubtuneIndex(subtune);
            let timing = crate::emu::PlaybackTiming::for_subtune(&header, subtune);
            let (_, extractor, song) =
                super::super::extract_native_song(&db, &header, &bytes, subtune, timing, frames)
                    .unwrap_or_else(|error| panic!("{asset} subtune {}: {error}", subtune.0));
            assert_eq!(extractor, "whittaker");
            assert!(song.validation.accepted);
        }
    }

    #[test]
    fn locator_rejects_ambiguous_frequency_tables() {
        let mut ram = post_init("Defcom.sid");
        ram[0x4000..0x4000 + FREQUENCY_PREFIX.len()].copy_from_slice(&FREQUENCY_PREFIX);
        ram[0x4FE0..0x4FED].copy_from_slice(&[
            0xA0, 0x00, 0xB1, 0xF8, 0xAA, 0xE6, 0xF8, 0xD0, 0x02, 0xE6, 0xF9, 0x8A, 0x30,
        ]);
        ram[0x4FFB..0x5000].copy_from_slice(&[0xA0, 0x12, 0xB1, 0xFA, 0xAA]);
        ram[0x5000..0x5009]
            .copy_from_slice(&[0xBD, 0x00, 0x40, 0x8D, 0x00, 0xD4, 0xBD, 0x01, 0x40]);
        assert!(locate(&ram).is_none());
    }

    #[test]
    fn frequency_locator_uses_the_code_referenced_table_view() {
        let mut ram = vec![0; 0x1_0000];
        ram[0x1000..0x1000 + FREQUENCY_PREFIX.len()].copy_from_slice(&FREQUENCY_PREFIX);
        ram[0x1FE0..0x1FED].copy_from_slice(&[
            0xA0, 0x00, 0xB1, 0xF8, 0xAA, 0xE6, 0xF8, 0xD0, 0x02, 0xE6, 0xF9, 0x8A, 0x30,
        ]);
        ram[0x1FFB..0x2000].copy_from_slice(&[0xA0, 0x12, 0xB1, 0xFA, 0xAA]);
        ram[0x2000..0x2009]
            .copy_from_slice(&[0xBD, 0x18, 0x10, 0x8D, 0x00, 0xD4, 0xBD, 0x19, 0x10]);
        assert_eq!(
            frequency_candidates(&ram),
            vec![(
                0x1018,
                0x2000,
                ZeroPagePointer(0xFA),
                0x1FE2,
                ZeroPagePointer(0xF8)
            )]
        );
    }

    #[test]
    #[ignore = "manual triage tool; needs SID_DBG_TUNE"]
    fn dbg_whittaker_tune() {
        let Ok(path) = std::env::var("SID_DBG_TUNE") else {
            eprintln!("SID_DBG_TUNE unset; skipping");
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut emulator = Emulator::new();
        emulator.load(&header, &bytes).unwrap();
        emulator
            .call_init(header.init_address, header.start_song, header.songs)
            .unwrap();
        let ram = emulator.ram_image();
        let layout = locate(&ram).unwrap();
        let frames = std::env::var("SID_DBG_FRAMES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(400);
        let replay = replay_player(&mut emulator, &layout, header.play_address, frames).unwrap();
        let trace = emu::run(&header, &bytes, header.start_song, frames).unwrap();
        let truth = detect_notes(&analyze(&trace), SystemClock::Pal);
        let native = decode_notes(SystemClock::Pal, &truth, &replay.native_frequencies);
        eprintln!("{layout:04X?}");
        for voice in [
            crate::analysis::VoiceId::V1,
            crate::analysis::VoiceId::V2,
            crate::analysis::VoiceId::V3,
        ] {
            let authored: Vec<_> = native
                .iter()
                .filter(|note| note.voice == voice)
                .take(12)
                .map(|note| (note.start_frame.0, note.midi.0))
                .collect();
            let traced: Vec<_> = truth
                .iter()
                .filter(|note| note.voice == voice)
                .take(12)
                .map(|note| (note.start_frame.0, note.midi.0))
                .collect();
            eprintln!("V{} native {authored:?}", voice.0);
            eprintln!("V{} truth  {traced:?}", voice.0);
        }
        let checked = validation_notes(&native, &truth);
        for (authored, traced) in checked.iter().zip(&truth) {
            if authored.voice != traced.voice
                || authored.start_frame != traced.start_frame
                || authored.midi != traced.midi
            {
                eprintln!(
                    "first mismatch: native V{} {}@{} truth V{} {}@{}",
                    authored.voice.0,
                    authored.midi.0,
                    authored.start_frame.0,
                    traced.voice.0,
                    traced.midi.0,
                    traced.start_frame.0
                );
                break;
            }
        }
    }

    #[test]
    #[ignore = "manual coverage tool; needs SID_HVSC_ROOT"]
    fn dbg_whittaker_hvsc_sweep() {
        use rayon::prelude::*;

        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping HVSC sweep");
            return;
        };
        let root = std::path::PathBuf::from(root);
        let mut paths = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|extension| extension.to_str()) == Some("sid") {
                    paths.push(path);
                }
            }
        }
        paths.sort();

        enum Outcome {
            EmulationFailed(String),
            LocateFailed(String),
            Empty(String),
            Decoded(String, f64),
        }

        let db = PlayerDb::embedded();
        let outcomes: Vec<_> = paths
            .par_iter()
            .filter_map(|path| {
                let bytes = std::fs::read(path).ok()?;
                if db.identify(&bytes) != Some("David_Whittaker") {
                    return None;
                }
                let name = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .display()
                    .to_string();
                let Ok(header) = crate::header::parse(&bytes) else {
                    return Some(Outcome::EmulationFailed(name));
                };
                let mut emulator = Emulator::new();
                if emulator.load(&header, &bytes).is_err()
                    || emulator
                        .call_init(header.init_address, header.start_song, header.songs)
                        .is_err()
                {
                    return Some(Outcome::EmulationFailed(name));
                }
                let ram = emulator.ram_image();
                let Some(layout) = locate(&ram) else {
                    return Some(Outcome::LocateFailed(name));
                };
                let Ok(replay) = replay_player(&mut emulator, &layout, header.play_address, 400)
                else {
                    return Some(Outcome::EmulationFailed(name));
                };
                let Ok(trace) = emu::run(&header, &bytes, header.start_song, 400) else {
                    return Some(Outcome::EmulationFailed(name));
                };
                let truth = detect_notes(&analyze(&trace), SystemClock::Pal);
                let native = decode_notes(SystemClock::Pal, &truth, &replay.native_frequencies);
                if native.is_empty() {
                    return Some(Outcome::Empty(name));
                }
                Some(Outcome::Decoded(
                    name,
                    super::super::onset_agreement(&native, &truth),
                ))
            })
            .collect();

        let total = outcomes.len();
        let mut emulation_failed = Vec::new();
        let mut locate_failed = Vec::new();
        let mut empty = Vec::new();
        let mut decoded = Vec::new();
        for outcome in outcomes {
            match outcome {
                Outcome::EmulationFailed(name) => emulation_failed.push(name),
                Outcome::LocateFailed(name) => locate_failed.push(name),
                Outcome::Empty(name) => empty.push(name),
                Outcome::Decoded(name, agreement) => decoded.push((name, agreement)),
            }
        }
        decoded.sort_by(|left, right| left.0.cmp(&right.0));
        let accepted = decoded
            .iter()
            .filter(|(_, agreement)| *agreement >= super::super::MIN_AGREEMENT)
            .count();
        eprintln!(
            "Whittaker sweep: total={total} decoded={} accepted={accepted} locate_failed={} \
             emulation_failed={} empty={}",
            decoded.len(),
            locate_failed.len(),
            emulation_failed.len(),
            empty.len()
        );
        for (name, agreement) in decoded {
            eprintln!("decoded {agreement:.3} {name}");
        }
        for name in locate_failed {
            eprintln!("locate-failed {name}");
        }
        for name in emulation_failed {
            eprintln!("emulation-failed {name}");
        }
        for name in empty {
            eprintln!("empty {name}");
        }
    }
}
