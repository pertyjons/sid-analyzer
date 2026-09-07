use super::super::{
    NativeFrameSample, VOICE_OFFSETS, runtime_instances, table_pointer, voice_placements,
};
use super::GoatTrackerV2Layout;
use crate::analysis::VoiceId;
use crate::export::{
    FrequencyTableIndex, InstrumentNumber, NativeDriverOpcode, NativeEffectByte, OrderOffset,
    PatternByteOffset, PatternDuration, PatternNumber, PatternRepeatCount, PatternTranspose,
    RecoveredOrderCommand, RecoveredPatternEvent, RecoveredStructure, RecoveredVoiceStructure,
    VoicePlacements,
};
use crate::header::SubtuneIndex;
use std::collections::BTreeMap;
use thiserror::Error;

const REPEAT: u8 = 0xD0;
const TRANSDOWN: u8 = 0xE0;
const TRANSPOSE_ZERO: u8 = 0xF0;
const LOOP: u8 = 0xFF;
const FX: u8 = 0x40;
const FX_ONLY: u8 = 0x50;
const FIRST_NOTE: u8 = 0x60;
const REST: u8 = 0xBD;
const KEY_OFF: u8 = 0xBE;
const FIRST_PACKED_REST: u8 = 0xC0;

#[derive(Debug, Error)]
pub(super) enum DecodeError {
    #[error("GoatTracker V2 compact pattern grammar is not structurally decoded yet")]
    CompactGrammar,
    #[error("GoatTracker V2 table address ${address:04X} is outside the loaded module")]
    AddressOutsideModule { address: u16 },
    #[error("GoatTracker V2 pattern {pattern} does not terminate within 256 bytes")]
    UnterminatedPattern { pattern: PatternNumber },
    #[error("GoatTracker V2 pattern {pattern} ends inside a packed row")]
    TruncatedPattern { pattern: PatternNumber },
    #[error(
        "GoatTracker V2 pattern {pattern} has invalid row byte ${byte:02X} at offset ${offset:02X}"
    )]
    InvalidPatternByte {
        pattern: PatternNumber,
        offset: PatternByteOffset,
        byte: u8,
    },
    #[error("GoatTracker V2 orderlist for {voice} has no loop marker")]
    UnterminatedOrder { voice: VoiceId },
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
    let limit = address.saturating_add(256).min(module_end);
    loop {
        if cursor >= limit {
            return Err(DecodeError::UnterminatedPattern { pattern });
        }
        let row_start = cursor;
        let mut byte = read(ram, cursor, module_end)?;
        cursor = cursor.wrapping_add(1);
        let mut instrument = None;
        let mut command = None;
        let mut command_data = None;
        if byte < FX {
            instrument = Some(InstrumentNumber(byte));
            byte = read(ram, cursor, module_end)
                .map_err(|_| DecodeError::TruncatedPattern { pattern })?;
            cursor = cursor.wrapping_add(1);
        }
        if (FX..FIRST_NOTE).contains(&byte) {
            let opcode = byte & 0x0F;
            if opcode != 0 {
                command_data = Some(NativeEffectByte(read(ram, cursor, module_end)?));
                cursor = cursor.wrapping_add(1);
            }
            command = Some(NativeDriverOpcode(opcode));
            if byte < FX_ONLY {
                byte = read(ram, cursor, module_end)?;
                cursor = cursor.wrapping_add(1);
            } else {
                byte = REST;
            }
        }
        if byte < FIRST_NOTE {
            return Err(DecodeError::InvalidPatternByte {
                pattern,
                offset: PatternByteOffset(row_start.wrapping_sub(address)),
                byte,
            });
        }
        let (duration, frequency_index, hold) = if byte >= FIRST_PACKED_REST {
            (byte.wrapping_neg(), None, true)
        } else {
            (
                1,
                (FIRST_NOTE..REST)
                    .contains(&byte)
                    .then(|| FrequencyTableIndex(byte - FIRST_NOTE)),
                byte == REST,
            )
        };
        events.push(RecoveredPatternEvent {
            offset: PatternByteOffset(row_start.wrapping_sub(address)),
            duration: PatternDuration(u16::from(duration)),
            frequency_index,
            instrument,
            hold: hold && byte != KEY_OFF,
            slide: command
                .filter(|opcode| (1..=3).contains(&opcode.0))
                .and(command_data),
            command,
            command_data,
            duration_index: None,
            operand: None,
        });
        if read(ram, cursor, module_end)? == 0 {
            return Ok(events);
        }
    }
}

fn decode_order(
    ram: &[u8],
    address: u16,
    module_end: u16,
    voice: VoiceId,
) -> Result<(OrderOffset, Vec<RecoveredOrderCommand>), DecodeError> {
    let mut commands = Vec::new();
    let mut offset = 0usize;
    while offset < 256 {
        let byte = read(ram, address.wrapping_add(offset as u16), module_end)?;
        let order_offset = OrderOffset(offset);
        if byte == LOOP {
            let target = read(ram, address.wrapping_add(offset as u16 + 1), module_end)?;
            commands.push(RecoveredOrderCommand::Loop {
                order_offset,
                target: OrderOffset(usize::from(target)),
            });
            return Ok((OrderOffset(usize::from(target)), commands));
        }
        if byte >= TRANSDOWN {
            commands.push(RecoveredOrderCommand::SetTranspose {
                order_offset,
                transpose: PatternTranspose(i16::from(byte) - i16::from(TRANSPOSE_ZERO)),
            });
        } else if byte >= REPEAT {
            commands.push(RecoveredOrderCommand::DriverCommand {
                order_offset,
                opcode: NativeDriverOpcode(byte),
            });
        } else {
            let repeat = read(ram, address.wrapping_add(offset as u16 + 1), module_end)
                .ok()
                .filter(|next| (*next > REPEAT) && (*next < TRANSDOWN))
                .map_or(PatternRepeatCount(1), |next| {
                    PatternRepeatCount(u32::from(next - REPEAT))
                });
            commands.push(RecoveredOrderCommand::Pattern {
                order_offset,
                pattern: PatternNumber(byte),
                repeat,
            });
        }
        offset += 1;
    }
    Err(DecodeError::UnterminatedOrder { voice })
}

pub(super) fn recover(
    ram: &[u8],
    layout: &GoatTrackerV2Layout,
    module_end: u16,
    subtune: SubtuneIndex,
    samples: &[NativeFrameSample],
) -> Result<(Vec<VoicePlacements>, RecoveredStructure), DecodeError> {
    if !layout.full_pattern_grammar {
        return Err(DecodeError::CompactGrammar);
    }
    let song_base = subtune.0.saturating_sub(1).saturating_mul(3);
    let mut decoded_orders = Vec::new();
    for voice_index in 0..VOICE_OFFSETS.len() {
        let voice = VoiceId::from_index(voice_index);
        let table_index = song_base.wrapping_add(voice_index as u16);
        let order_address = table_pointer(
            ram,
            layout.song_pointer_lo,
            layout.song_pointer_hi,
            table_index,
        );
        let (order_loop_offset, order_commands) =
            decode_order(ram, order_address, module_end, voice)?;
        decoded_orders.push((voice, order_loop_offset, order_commands));
    }
    let mut patterns = BTreeMap::new();
    for (_, _, commands) in &decoded_orders {
        for number in commands.iter().filter_map(|command| match command {
            RecoveredOrderCommand::Pattern { pattern, .. } => Some(*pattern),
            _ => None,
        }) {
            if patterns.contains_key(&number) {
                continue;
            }
            let address = table_pointer(
                ram,
                layout.pattern_pointer_lo,
                layout.pattern_pointer_hi,
                u16::from(number.0),
            );
            patterns.insert(number, decode_pattern(ram, address, module_end, number)?);
        }
    }
    let mut voices = Vec::new();
    let mut placements = Vec::new();
    for (voice, order_loop_offset, order_commands) in decoded_orders {
        let instances = runtime_instances(samples, voice, &order_commands, &patterns);
        placements.push(voice_placements(voice, &instances));
        voices.push(RecoveredVoiceStructure {
            voice,
            order_loop_offset: Some(order_loop_offset),
            order_commands,
            instances,
        });
    }
    Ok((placements, RecoveredStructure { patterns, voices }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> GoatTrackerV2Layout {
        GoatTrackerV2Layout {
            note_indices: 0,
            resolved_frequencies: 0,
            song_pointer_lo: 0x2000,
            song_pointer_hi: 0x1FF0,
            pattern_pointer_lo: 0x3000,
            pattern_pointer_hi: 0x2FF0,
            order_positions: 0,
            pattern_numbers: 0,
            pattern_positions: 0,
            full_pattern_grammar: true,
        }
    }

    #[test]
    fn invalid_row_byte_is_a_typed_failure() {
        let mut ram = vec![0; 0x1_0000];
        ram[0x1000..0x1003].copy_from_slice(&[0x01, 0x00, 0x00]);
        let result = decode_pattern(&ram, 0x1000, 0x1003, PatternNumber(4));
        assert!(matches!(
            result,
            Err(DecodeError::InvalidPatternByte {
                pattern: PatternNumber(4),
                offset: PatternByteOffset(0),
                byte: 0
            })
        ));
    }

    #[test]
    fn recovery_does_not_infer_pattern_count_from_pointer_table_order() {
        let mut ram = vec![0; 0x1_0000];
        for (index, address) in [0x4000_u16, 0x4100, 0x4200].into_iter().enumerate() {
            let [lo, hi] = address.to_le_bytes();
            ram[0x2000 + index] = lo;
            ram[0x1FF0 + index] = hi;
            ram[usize::from(address)..usize::from(address) + 3]
                .copy_from_slice(&[0x00, LOOP, 0x00]);
        }
        ram[0x3000] = 0x00;
        ram[0x2FF0] = 0x50;
        ram[0x5000..0x5002].copy_from_slice(&[FIRST_NOTE, 0x00]);

        let (_, structure) = recover(&ram, &layout(), 0x6000, SubtuneIndex(1), &[]).unwrap();
        assert_eq!(structure.patterns.len(), 1);
        assert_eq!(structure.voices.len(), 3);
    }
}
