//! GoatTracker V1 orderlist and pattern grammar.
//!
//! A pattern row is one of three shapes, distinguished by its first byte:
//!
//! | first byte | meaning                                                   |
//! |------------|-----------------------------------------------------------|
//! | `< $60`    | note number, followed by `instrument << 3 \| command` and a command byte |
//! | `$60..$BF` | note number `byte - $60`, nothing else                    |
//! | `>= $C0`   | hold the previous row for `256 - byte` further ticks       |
//!
//! Note numbers `$5E` and `$5F` are key-off and rest, in either note shape.
//! A `$FF` byte ends the pattern.
//!
//! The orderlist grammar depends on the generation — see [`OrderLayout`].

use super::super::{
    NativeFrameSample, VOICE_OFFSETS, runtime_instances, table_pointer, voice_placements,
};
use super::{GoatTrackerV1Layout, OrderLayout};
use crate::analysis::VoiceId;
use crate::export::{
    FrequencyTableIndex, InstrumentNumber, NativeDriverOpcode, NativeEffectByte, OrderOffset,
    PatternByteOffset, PatternDuration, PatternNumber, PatternRepeatCount, PatternTranspose,
    RecoveredOrderCommand, RecoveredPatternEvent, RecoveredStructure, RecoveredVoiceStructure,
    VoicePlacements,
};
use std::collections::BTreeMap;
use thiserror::Error;

/// Lowest byte that carries its own note number without an instrument row.
const PLAIN_NOTE: u8 = 0x60;
/// Lowest byte that extends the previous row instead of starting a new one.
const FIRST_PACKED_HOLD: u8 = 0xC0;
const KEY_OFF: u8 = 0x5E;
const REST: u8 = 0x5F;
const PATTERN_END: u8 = 0xFF;

/// Orderlist bytes of the indexed generation.
const REPEAT: u8 = 0xD0;
const TRANSDOWN: u8 = 0xE0;
const TRANSPOSE_ZERO: u8 = 0xF0;
const ORDER_LOOP: u8 = 0xFF;

/// Every cursor the player keeps is one byte, so a pattern, an orderlist, and
/// the pattern table itself are all bounded by 256.
const MAX_PATTERN_BYTES: u16 = 256;
const MAX_ORDER_BYTES: usize = 256;
const MAX_PATTERNS: u16 = 256;

#[derive(Debug, Error)]
pub(super) enum DecodeError {
    #[error("GoatTracker V1 pattern pointer table is empty or inverted")]
    NoPatterns,
    #[error("GoatTracker V1 table address ${address:04X} is outside the loaded module")]
    AddressOutsideModule { address: u16 },
    #[error("GoatTracker V1 pattern {pattern} does not terminate within 256 bytes")]
    UnterminatedPattern { pattern: u8 },
    #[error("GoatTracker V1 orderlist for voice {voice} has no terminator")]
    UnterminatedOrder { voice: u8 },
}

fn read(ram: &[u8], address: u16, module_end: u16) -> Result<u8, DecodeError> {
    if address >= module_end {
        return Err(DecodeError::AddressOutsideModule { address });
    }
    Ok(ram[usize::from(address)])
}

fn decode_pattern(
    ram: &[u8],
    address: u16,
    module_end: u16,
    pattern: PatternNumber,
) -> Result<Vec<RecoveredPatternEvent>, DecodeError> {
    let mut events = Vec::new();
    let mut cursor = address;
    let limit = address.saturating_add(MAX_PATTERN_BYTES).min(module_end);
    loop {
        if cursor >= limit {
            return Err(DecodeError::UnterminatedPattern { pattern: pattern.0 });
        }
        let row_start = cursor;
        let byte = read(ram, cursor, module_end)?;
        cursor = cursor.wrapping_add(1);
        // The player only tests for the terminator after a note row, because a
        // hold row is always followed by one. Testing before every row reaches
        // the same end on well-formed data and keeps a malformed pattern from
        // walking past it.
        if byte == PATTERN_END {
            return Ok(events);
        }
        if byte >= FIRST_PACKED_HOLD {
            events.push(RecoveredPatternEvent {
                offset: PatternByteOffset(row_start.wrapping_sub(address)),
                duration: PatternDuration(u16::from(byte.wrapping_neg())),
                frequency_index: None,
                instrument: None,
                hold: true,
                slide: None,
                command: None,
                command_data: None,
                duration_index: None,
                operand: None,
            });
            continue;
        }
        let (note, instrument, command, command_data) = if byte < PLAIN_NOTE {
            let packed = read(ram, cursor, module_end)?;
            let data = read(ram, cursor.wrapping_add(1), module_end)?;
            cursor = cursor.wrapping_add(2);
            let opcode = packed & 0x07;
            (
                byte,
                (packed & 0xF8 != 0).then_some(InstrumentNumber(packed >> 3)),
                (opcode != 0).then_some(NativeDriverOpcode(opcode)),
                Some(NativeEffectByte(data)),
            )
        } else {
            (byte - PLAIN_NOTE, None, None, None)
        };
        events.push(RecoveredPatternEvent {
            offset: PatternByteOffset(row_start.wrapping_sub(address)),
            duration: PatternDuration(1),
            frequency_index: (note != KEY_OFF && note != REST).then_some(FrequencyTableIndex(note)),
            instrument,
            hold: note == REST,
            slide: command
                .filter(|opcode| (1..=3).contains(&opcode.0))
                .and(command_data),
            command,
            command_data,
            duration_index: None,
            operand: None,
        });
    }
}

/// The indexed generation's orderlist: repeat and transpose bytes *precede*
/// the pattern they modify, and `$FF` carries the loop target.
fn decode_indexed_order(
    ram: &[u8],
    address: u16,
    module_end: u16,
    voice: VoiceId,
) -> Result<(Option<OrderOffset>, Vec<RecoveredOrderCommand>), DecodeError> {
    let mut commands = Vec::new();
    let mut pending_repeat = PatternRepeatCount(1);
    for offset in 0..MAX_ORDER_BYTES {
        let byte = read(ram, address.wrapping_add(offset as u16), module_end)?;
        let order_offset = OrderOffset(offset);
        if byte == ORDER_LOOP {
            let target = read(ram, address.wrapping_add(offset as u16 + 1), module_end)?;
            commands.push(RecoveredOrderCommand::Loop {
                order_offset,
                target: OrderOffset(usize::from(target)),
            });
            return Ok((Some(OrderOffset(usize::from(target))), commands));
        }
        if byte >= TRANSDOWN {
            commands.push(RecoveredOrderCommand::SetTranspose {
                order_offset,
                transpose: PatternTranspose(i16::from(byte) - i16::from(TRANSPOSE_ZERO)),
            });
        } else if byte >= REPEAT {
            // The counter is how many *extra* times the next pattern plays.
            pending_repeat = PatternRepeatCount(u32::from(byte - REPEAT) + 1);
            commands.push(RecoveredOrderCommand::DriverCommand {
                order_offset,
                opcode: NativeDriverOpcode(byte),
            });
        } else {
            commands.push(RecoveredOrderCommand::Pattern {
                order_offset,
                pattern: PatternNumber(byte),
                repeat: pending_repeat,
            });
            pending_repeat = PatternRepeatCount(1);
        }
    }
    Err(DecodeError::UnterminatedOrder { voice: voice.0 })
}

/// The cached-pointer generations' orderlist: a bare list of pattern numbers.
/// `$FF` loops — to the byte named next where the terminator is `$FF`, to the
/// start where it is `$FE` — and `$FE` stops the voice.
fn decode_plain_order(
    ram: &[u8],
    address: u16,
    module_end: u16,
    voice: VoiceId,
    terminal: u8,
) -> Result<(Option<OrderOffset>, Vec<RecoveredOrderCommand>), DecodeError> {
    let mut commands = Vec::new();
    for offset in 0..MAX_ORDER_BYTES {
        let byte = read(ram, address.wrapping_add(offset as u16), module_end)?;
        let order_offset = OrderOffset(offset);
        if byte < terminal {
            commands.push(RecoveredOrderCommand::Pattern {
                order_offset,
                pattern: PatternNumber(byte),
                repeat: PatternRepeatCount(1),
            });
            continue;
        }
        if byte == ORDER_LOOP {
            let target = if terminal == ORDER_LOOP {
                read(ram, address.wrapping_add(offset as u16 + 1), module_end)?
            } else {
                0
            };
            commands.push(RecoveredOrderCommand::Loop {
                order_offset,
                target: OrderOffset(usize::from(target)),
            });
            return Ok((Some(OrderOffset(usize::from(target))), commands));
        }
        commands.push(RecoveredOrderCommand::Stop { order_offset });
        return Ok((None, commands));
    }
    Err(DecodeError::UnterminatedOrder { voice: voice.0 })
}

fn order_address(ram: &[u8], layout: &GoatTrackerV1Layout, voice_offset: u16) -> u16 {
    match layout.order {
        OrderLayout::Indexed {
            table_lo,
            table_hi,
            index_state,
        } => {
            let index = ram[usize::from(index_state.wrapping_add(voice_offset))];
            table_pointer(ram, table_lo, table_hi, u16::from(index))
        }
        OrderLayout::VoicePointer {
            pointer_lo,
            pointer_hi,
            ..
        } => table_pointer(ram, pointer_lo, pointer_hi, voice_offset),
    }
}

pub(super) fn recover(
    ram: &[u8],
    layout: &GoatTrackerV1Layout,
    module_end: u16,
    samples: &[NativeFrameSample],
) -> Result<(Vec<VoicePlacements>, RecoveredStructure), DecodeError> {
    let pattern_count = layout.pattern_count();
    if pattern_count == 0 || pattern_count > MAX_PATTERNS {
        return Err(DecodeError::NoPatterns);
    }
    let mut patterns = BTreeMap::new();
    for raw_pattern in 0..pattern_count {
        let number = PatternNumber(raw_pattern as u8);
        let address = table_pointer(
            ram,
            layout.pattern_pointer_lo,
            layout.pattern_pointer_hi,
            raw_pattern,
        );
        patterns.insert(number, decode_pattern(ram, address, module_end, number)?);
    }
    let mut voices = Vec::new();
    let mut placements = Vec::new();
    for (voice_index, voice_offset) in VOICE_OFFSETS.into_iter().enumerate() {
        let voice = VoiceId::from_index(voice_index);
        let address = order_address(ram, layout, voice_offset);
        let (order_loop_offset, order_commands) = match layout.order {
            OrderLayout::Indexed { .. } => decode_indexed_order(ram, address, module_end, voice)?,
            OrderLayout::VoicePointer { terminal, .. } => {
                decode_plain_order(ram, address, module_end, voice, terminal)?
            }
        };
        let instances = runtime_instances(samples, voice, &order_commands, &patterns);
        placements.push(voice_placements(voice, &instances));
        voices.push(RecoveredVoiceStructure {
            voice,
            order_loop_offset,
            order_commands,
            instances,
        });
    }
    Ok((placements, RecoveredStructure { patterns, voices }))
}
