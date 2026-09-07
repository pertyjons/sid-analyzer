use super::*;

// --- Pattern / orderlist decoding ---------------------------------------------

/// `$FF` terminates both an orderlist (loop back to the start) and a pattern
/// (advance the orderlist).
pub(super) const END_MARK: u8 = 0xFF;
/// `$FE` in an orderlist stops that voice after invoking the driver's global
/// song-control hook. The order cursor is not advanced, so the marker remains
/// terminal on every later play call.
pub(super) const ORDER_CMD: u8 = 0xFE;
/// Status-byte fields: low 5 bits = duration, bit 6 = tie/rest, bit 7 = an
/// extra (instrument or slide) byte precedes the note.
pub(super) const STATUS_TIE: u8 = 0x40;
pub(super) const STATUS_EXTRA: u8 = 0x80;
/// In five-bit-duration variants, bit 5 keeps the gate open through the row.
pub(super) const STATUS_SUSTAIN: u8 = 0x20;

/// Safety caps so a malformed image cannot loop forever.
pub(super) const MAX_PATTERN_BYTES: u16 = 512;
pub(super) const MAX_ORDER_LEN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) struct InstrumentIndex(pub u8);

impl std::fmt::Display for InstrumentIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) struct FrequencyIndex(pub u8);

impl std::fmt::Display for FrequencyIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(super) struct OrderOffset(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(super) struct RowTick(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(super) struct RepeatCount(u32);

#[derive(Debug, thiserror::Error)]
pub(super) enum HubbardDecodeError {
    #[error("pattern {pattern} at ${address:04X} is not terminated within {limit} bytes")]
    PatternUnterminated {
        pattern: u8,
        address: u16,
        limit: u16,
    },
    #[error("voice {voice} orderlist at ${address:04X} is not terminated within {limit} bytes")]
    OrderUnterminated {
        voice: u8,
        address: u16,
        limit: usize,
    },
    #[error("voice {voice} order command at offset {offset} is missing an operand")]
    MissingOrderOperand { voice: u8, offset: usize },
    #[error("voice {voice} order traversal made no bounded progress at offset {offset}")]
    TraversalLimit { voice: u8, offset: usize },
}

/// One decoded event from a Rob Hubbard pattern stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PatternEvent {
    pub offset: PatternByteOffset,
    /// The event occupies `duration + 1` row-ticks (the masked status bits).
    pub duration: u8,
    /// Frequency-table index for the note, or `None` for a rest.
    pub note: Option<FrequencyIndex>,
    /// `true` for a bit-7 note byte. The flag is retained in recovered source
    /// structure; gate continuity itself is determined by the preceding
    /// status byte's sustain bit.
    pub hold: bool,
    /// The five-bit Hubbard format's status bit 5, which suppresses the normal
    /// gate-off near the end of this event. Six-bit-duration variants use the
    /// same bit as part of the duration instead.
    pub sustain: bool,
    /// Instrument (patch) index this event selects, if any.
    pub instrument: Option<InstrumentIndex>,
    /// Pitch-slide command, if any: bit 0 = direction (set = down), bits 1-6 =
    /// speed; the driver adds/subtracts it from the running frequency each frame.
    pub slide: Option<u8>,
}

#[derive(Debug, Clone)]
pub(super) struct PatternInstance {
    pattern: PatternNumber,
    transpose: PatternTranspose,
    repeat_ordinal: RepeatCount,
    order_offset: OrderOffset,
    start_tick: RowTick,
    events: Vec<PatternEvent>,
}

#[derive(Debug, Clone)]
pub(super) struct HubbardIr {
    voices: Vec<(VoiceId, Vec<PatternInstance>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GateContinuationMode {
    BitSevenNotes,
    EverySustainedRow,
}

/// Address of pattern `num` from the pointer tables. With `pat_stride == 1` the
/// lo and hi bytes come from two separate tables indexed by `num`; with
/// `pat_stride == 2` they are the adjacent bytes of one interleaved table indexed
/// by `num * 2` (where `pat_ptr_hi == pat_ptr_lo + 1`).
pub(super) fn pattern_addr(read: &impl Fn(u16) -> u8, layout: &HubbardLayout, num: u8) -> u16 {
    let n = u16::from(num) * u16::from(layout.pat_stride);
    u16::from(read(layout.pat_ptr_lo.wrapping_add(n)))
        | (u16::from(read(layout.pat_ptr_hi.wrapping_add(n))) << 8)
}

/// Decode pattern `num` into its event list (until the `$FF` end mark).
pub(super) fn decode_pattern_checked(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    num: u8,
) -> Result<Vec<PatternEvent>, HubbardDecodeError> {
    let base = pattern_addr(read, layout, num);
    let mut events = Vec::new();
    let mut pos: u16 = 0;
    while pos < MAX_PATTERN_BYTES {
        let event_offset = pos;
        let status = read(base.wrapping_add(pos));
        if status == END_MARK {
            return Ok(events);
        }
        pos = pos.wrapping_add(1);

        let (mut instrument, mut slide) = (None, None);
        if status & STATUS_EXTRA != 0 {
            let extra = read(base.wrapping_add(pos));
            pos = pos.wrapping_add(1);
            if extra & 0x80 != 0 {
                slide = Some(extra);
                // A multi-byte effect (Sigma) carries extra parameter bytes;
                // skip them so the rest of the stream stays in sync.
                for _ in 1..layout.effect_bytes {
                    pos = pos.wrapping_add(1);
                }
            } else {
                instrument = Some(InstrumentIndex(extra));
            }
        }

        let mut hold = false;
        let note = if status & STATUS_TIE == 0 {
            let n = read(base.wrapping_add(pos));
            pos = pos.wrapping_add(1);
            if n & 0x80 != 0 {
                hold = true;
            }
            Some(FrequencyIndex(n & 0x7F))
        } else {
            None
        };

        events.push(PatternEvent {
            offset: PatternByteOffset(event_offset),
            duration: status & layout.dur_mask,
            note,
            instrument,
            slide,
            hold,
            sustain: layout.dur_mask == 0x1F && status & STATUS_SUSTAIN != 0,
        });
    }
    Err(HubbardDecodeError::PatternUnterminated {
        pattern: num,
        address: base,
        limit: MAX_PATTERN_BYTES,
    })
}

#[cfg(test)]
pub(crate) fn decode_pattern(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    num: u8,
) -> Vec<PatternEvent> {
    decode_pattern_checked(read, layout, num).unwrap_or_default()
}

/// Read voice `voice`'s orderlist through its `$FE` stop marker or up to its
/// `$FF` loop marker. `$FE` remains in the returned stream so source recovery
/// can distinguish a stopped voice from a looping one.
pub(super) fn orderlist_checked(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    voice: u8,
) -> Result<Vec<u8>, HubbardDecodeError> {
    let v = u16::from(voice);
    let base = u16::from(read(layout.seq_ptr_lo.wrapping_add(v)))
        | (u16::from(read(layout.seq_ptr_hi.wrapping_add(v))) << 8);
    let mut out = Vec::new();
    for i in 0..MAX_ORDER_LEN as u16 {
        let b = read(base.wrapping_add(i));
        if b == END_MARK {
            return Ok(out);
        }
        out.push(b);
        if b == ORDER_CMD {
            return Ok(out);
        }
    }
    Err(HubbardDecodeError::OrderUnterminated {
        voice: voice + 1,
        address: base,
        limit: MAX_ORDER_LEN,
    })
}

#[cfg(test)]
pub(crate) fn orderlist(read: &impl Fn(u16) -> u8, layout: &HubbardLayout, voice: u8) -> Vec<u8> {
    orderlist_checked(read, layout, voice).unwrap_or_default()
}

/// Convert a frequency-table index to a [`NoteEvent`] at `start_frame`.
pub(super) fn note_event(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    clock: SystemClock,
    voice: VoiceId,
    note_idx: u8,
    start: u32,
    end: u32,
) -> Option<NoteEvent> {
    let idx = u16::from(note_idx);
    let raw = match layout.freq_hi {
        // Split lo/hi tables, indexed at stride 1 by the raw note index.
        Some(hi_base) => {
            u32::from(read(layout.freq_table.wrapping_add(idx)))
                | (u32::from(read(hi_base.wrapping_add(idx))) << 8)
        }
        // One interleaved table: the lo/hi bytes are adjacent at `idx * 2`.
        None => {
            let off = layout.freq_table.wrapping_add(idx * 2);
            u32::from(read(off)) | (u32::from(read(off.wrapping_add(1))) << 8)
        }
    };
    super::note_from_raw_freq(raw, clock, voice, start, end)
}

/// An orderlist byte with bit 7 set (and not [`END_MARK`]/[`ORDER_CMD`]) is a
/// transpose command: a transpose amount added to every following note's
/// frequency-table index until the next such command. The amount is carried
/// either in a *separate* following byte (Auf Wiedersehen Monty,
/// [`HubbardLayout::embedded_transpose_mask`] `None`) or *embedded* in the marker
/// byte itself (`Some(mask)`: the Jeroen Tel relocation masks low 5 bits, the
/// Magnar / Shape Music player low 7 bits). The driver stores it per voice and
/// adds it after masking the note (`ADC $EB81,X` in Monty, `ADC $191B,X` in Jeroen
/// Tel, `ADC $E0E1,X` in Shape Music). Commando, Sigma Seven and Human Race never
/// emit one (no bit-7 orderlist bytes), so honouring it is backward-compatible.
pub(super) const ORDER_TRANSPOSE: u8 = 0x80;

/// Simulate the play routine's per-frame timing counters and return the frame
/// of each row-tick, anchored so the first tick is frame 0.
///
/// This is the exact 6502 timing logic, not a closed-form rate: a stall gate
/// ([`HubbardLayout::stall_reload`]) can kill a whole frame, a prescale gate
/// ([`HubbardLayout::prescale_reload`]) can skip the tempo divider for a frame,
/// and the divider emits a tick when it underflows (once per `tempo + 1` runs).
/// Reproducing the counters frame-by-frame is what captures the non-integer
/// effective tempos (Warhawk's 16/7, Knucklebusters' 7/3) that no single
/// frames-per-tick value fits — and, with no gates, it collapses back to a tick
/// every `tempo + 1` frames, matching the simpler variants exactly.
///
/// Counters start at their reload value (steady state); the resulting whole-song
/// drift from the true initial phase is a frame or two, well inside
/// [`ONSET_TOLERANCE`].
pub(super) fn row_frames(layout: &HubbardLayout, frames: u32) -> Vec<u32> {
    let tempo = u32::from(layout.tempo);
    let mut div = tempo;
    let mut pre = u32::from(layout.prescale_reload.unwrap_or(0));
    let mut stall = u32::from(layout.stall_reload.unwrap_or(0));

    let mut rows = Vec::new();
    for f in 0..frames {
        if let Some(reload) = layout.stall_reload {
            if stall == 0 {
                stall = u32::from(reload);
                continue; // dead frame: routine returns before touching the song
            }
            stall -= 1;
        }

        let mut skip = false;
        if let Some(reload) = layout.prescale_reload {
            if pre == 0 {
                pre = u32::from(reload);
                skip = true; // underflow frame: tempo divider does not advance
            } else {
                pre -= 1;
            }
        }
        if !skip {
            if div == 0 {
                div = tempo;
            } else {
                div -= 1;
            }
        }

        // The divider signals a row when it sits at its reload value (it has just
        // underflowed). Knucklebusters additionally suppresses the row on the one
        // frame its prescale counter reads zero; absent a prescale that guard is
        // vacuously true.
        let prescale_open = layout.prescale_reload.is_none() || pre != 0;
        if prescale_open && div == tempo {
            rows.push(f);
        }
    }

    let anchor = rows.first().copied().unwrap_or(0);
    rows.iter().map(|f| f - anchor).collect()
}

pub(super) fn walk_pattern_instances(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    voice: u8,
    frames: u32,
    tick_to_frame: &[u32],
) -> Result<Vec<PatternInstance>, HubbardDecodeError> {
    let order = orderlist_checked(read, layout, voice)?;
    if order.is_empty() {
        return Ok(Vec::new());
    }
    let at_tick = |tick: RowTick| -> FrameIndex {
        FrameIndex(
            tick_to_frame
                .get(tick.0 as usize)
                .copied()
                .unwrap_or(frames),
        )
    };
    let mut instances = Vec::new();
    let mut tick = RowTick(0);
    let mut oi = 0usize;
    let mut transpose = PatternTranspose(0);
    let mut repeat = RepeatCount(1);
    let traversal_limit = usize::try_from(frames)
        .unwrap_or(usize::MAX / 4)
        .saturating_add(1)
        .saturating_mul(order.len().saturating_add(1))
        .saturating_mul(2);

    for _ in 0..traversal_limit {
        if at_tick(tick).0 >= frames {
            return Ok(instances);
        }
        let order_offset = OrderOffset(oi);
        let entry = order[oi];
        oi = (oi + 1) % order.len();
        if entry == ORDER_CMD {
            return Ok(instances);
        }
        if entry & ORDER_TRANSPOSE != 0 {
            transpose = if let Some(mask) = layout.embedded_transpose_mask {
                PatternTranspose(i16::from(entry & mask))
            } else {
                if oi == 0 {
                    return Err(HubbardDecodeError::MissingOrderOperand {
                        voice: voice + 1,
                        offset: order_offset.0,
                    });
                }
                let value = order[oi];
                oi = (oi + 1) % order.len();
                PatternTranspose(i16::from(value))
            };
            continue;
        }

        let pattern = PatternNumber(entry);
        let events = decode_pattern_checked(read, layout, entry)?;
        let repetitions = if layout.order_repeat { repeat.0 } else { 1 };
        for ordinal in 0..repetitions {
            if at_tick(tick).0 >= frames {
                return Ok(instances);
            }
            instances.push(PatternInstance {
                pattern,
                transpose,
                repeat_ordinal: RepeatCount(ordinal),
                order_offset,
                start_tick: tick,
                events: events.clone(),
            });
            for event in &events {
                tick.0 = tick.0.saturating_add(u32::from(event.duration) + 1);
            }
        }
        if layout.order_repeat {
            if oi == 0 {
                return Err(HubbardDecodeError::MissingOrderOperand {
                    voice: voice + 1,
                    offset: order_offset.0,
                });
            }
            repeat = RepeatCount(u32::from(order[oi]));
            oi = (oi + 1) % order.len();
        }
    }

    Err(HubbardDecodeError::TraversalLimit {
        voice: voice + 1,
        offset: oi,
    })
}

pub(super) fn decode_ir(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    frames: u32,
) -> Result<HubbardIr, HubbardDecodeError> {
    let tick_to_frame = row_frames(layout, frames);
    let mut voices = Vec::new();
    for voice_index in 0u8..layout.voices {
        let instances = walk_pattern_instances(read, layout, voice_index, frames, &tick_to_frame)?;
        if !instances.is_empty() {
            voices.push((VoiceId::from_index(voice_index as usize), instances));
        }
    }
    Ok(HubbardIr { voices })
}

/// Walk all three voices into a note timeline plus a parallel per-note authored
/// instrument index, stopping each voice once its next event would start at or
/// beyond `frames`.
///
/// Timing model (validated frame-for-frame against the emulator trace): the song
/// advances one row-tick per entry in [`row_frames`], and an event occupies
/// `duration + 1` ticks. Depending on the relocated player's qualified
/// [`GateContinuationMode`], either a bit-7 note or every note row extends the
/// previous note when the prior row's sustain bit kept the gate open. A rest
/// advances time and releases the voice. An orderlist transpose command
/// ([`ORDER_TRANSPOSE`]) shifts every following note's table index.
///
/// The instrument index is sticky per voice — a pattern event only carries one
/// when it changes ([`PatternEvent::instrument`]), so each note inherits the
/// voice's last selected instrument (`None` before any is set). It indexes the
/// instrument table ([`decode_instruments`]) for native patch binding.
#[cfg(test)]
pub(super) fn decode_song_checked(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    clock: SystemClock,
    frames: u32,
) -> Result<(Vec<NoteEvent>, Vec<Option<u8>>), HubbardDecodeError> {
    let ir = decode_ir(read, layout, frames)?;
    Ok(decode_song_from_ir(read, layout, &ir, clock, frames))
}

#[cfg(test)]
pub(super) fn decode_song_from_ir(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    ir: &HubbardIr,
    clock: SystemClock,
    frames: u32,
) -> (Vec<NoteEvent>, Vec<Option<u8>>) {
    decode_song_from_ir_with_mode(
        read,
        layout,
        ir,
        clock,
        frames,
        GateContinuationMode::BitSevenNotes,
    )
}

pub(super) fn decode_song_from_ir_with_mode(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    ir: &HubbardIr,
    clock: SystemClock,
    frames: u32,
    continuation_mode: GateContinuationMode,
) -> (Vec<NoteEvent>, Vec<Option<u8>>) {
    let tick_to_frame = row_frames(layout, frames);
    // Frame of a row-tick, or `frames` (out of range) once the song runs past
    // the analysed window.
    let at_tick =
        |tick: u32| -> u32 { tick_to_frame.get(tick as usize).copied().unwrap_or(frames) };
    let mut notes: Vec<NoteEvent> = Vec::new();
    let mut instruments = Vec::new();

    for (voice, instances) in &ir.voices {
        let mut cur_inst: Option<u8> = None;
        // Index into `notes` of this voice's most recent note while the previous
        // row kept the gate open; cleared by a rest.
        let mut open_note: Option<usize> = None;
        let mut gate_continues = false;
        'voice: for instance in instances {
            let mut tick = instance.start_tick.0;
            for &ev in &instance.events {
                if let Some(i) = ev.instrument {
                    cur_inst = Some(i.0);
                }
                let start = at_tick(tick);
                if start >= frames {
                    break 'voice;
                }
                tick += u32::from(ev.duration) + 1;
                // Exclusive end: the frame the next row begins.
                let end = at_tick(tick).min(frames);
                let zero_duration_continues = continuation_mode
                    == GateContinuationMode::EverySustainedRow
                    && layout.dur_mask == 0x1F
                    && ev.duration == 0;
                if let Some(idx) = ev.note {
                    let continues = match continuation_mode {
                        GateContinuationMode::BitSevenNotes => ev.hold,
                        GateContinuationMode::EverySustainedRow => true,
                    };
                    if continues && gate_continues && open_note.is_some() {
                        if let Some(i) = open_note {
                            notes[i].end_frame = Some(FrameIndex(end));
                        }
                        gate_continues = ev.sustain || zero_duration_continues;
                        continue;
                    }
                    if let Some(ne) = note_event(
                        read,
                        layout,
                        clock,
                        *voice,
                        idx.0.wrapping_add(instance.transpose.0 as u8),
                        start,
                        end,
                    ) {
                        open_note = Some(notes.len());
                        notes.push(ne);
                        instruments.push(cur_inst);
                    } else {
                        open_note = None;
                    }
                    gate_continues = ev.sustain || zero_duration_continues;
                } else {
                    // Rest: the driver releases at this row; the note ended
                    // at its own last extent.
                    open_note = None;
                    gate_continues = false;
                }
            }
        }
    }
    (notes, instruments)
}

#[cfg(test)]
pub(crate) fn decode_song(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    clock: SystemClock,
    frames: u32,
) -> (Vec<NoteEvent>, Vec<Option<u8>>) {
    decode_song_checked(read, layout, clock, frames).unwrap_or_default()
}

/// One step in a voice's orderlist: play pattern `pattern_number`, shifting every
/// note by `transpose` frequency-table indices. On Hubbard's chromatic frequency
/// tables one index step is one semitone, so `transpose` maps directly to a
/// Pertylizer placement transpose. Recovered verbatim from the driver's own
/// orderlist + [`ORDER_TRANSPOSE`] commands — there is no similarity matching:
/// reuse is simply a pattern number recurring in the orderlist.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrderStep {
    pub pattern_number: u8,
    pub transpose: u8,
}

/// A voice's arrangement: the orderlist as one pass of pattern placements (the
/// entries before the `$FF` loop-to-start), each carrying the transpose active
/// when the driver reaches it.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceArrangement {
    pub voice: VoiceId,
    pub steps: Vec<OrderStep>,
}

/// The native song structure recovered straight from the driver: per-voice
/// arrangements over a shared, number-keyed pattern set. The reuse is the
/// driver's own — a pattern number that recurs in an orderlist is the *same*
/// [`Self::patterns`] entry placed again (optionally transposed), not a block the
/// analyser guessed was similar.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct NativeStructure {
    pub arrangements: Vec<VoiceArrangement>,
    pub patterns: std::collections::BTreeMap<u8, Vec<PatternEvent>>,
}

/// Total row-ticks a pattern occupies (each event lasts `duration + 1` ticks).
#[cfg(test)]
pub(crate) fn pattern_ticks(events: &[PatternEvent]) -> u32 {
    events.iter().map(|e| u32::from(e.duration) + 1).sum()
}

/// Recover the song's pattern structure from the driver's per-voice orderlists,
/// mirroring [`decode_song`]'s orderlist walk but recording one pass of pattern
/// placements (with the transpose state) instead of flattening to absolute notes.
///
/// This is pure decode of the driver's own tables — no note timing, no trace, no
/// similarity heuristics. The flat note timeline ([`decode_song`]) and this
/// structure share the same walk, so replaying the steps (looping at the end,
/// pacing rows through [`row_frames`]) reproduces exactly those notes.
#[cfg(test)]
pub(crate) fn decode_structure(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
) -> NativeStructure {
    let mut arrangements = Vec::new();
    let mut patterns = std::collections::BTreeMap::new();

    for vi in 0u8..layout.voices {
        let order = orderlist(read, layout, vi);
        if order.is_empty() {
            continue;
        }
        let mut steps = Vec::new();
        let mut transpose: u8 = 0;
        let mut oi = 0usize;
        while oi < order.len() {
            let entry = order[oi];
            oi += 1;
            if entry == ORDER_CMD {
                break;
            }
            if entry & ORDER_TRANSPOSE != 0 {
                if let Some(mask) = layout.embedded_transpose_mask {
                    transpose = entry & mask;
                } else if oi < order.len() {
                    transpose = order[oi];
                    oi += 1;
                }
                continue;
            }
            patterns
                .entry(entry)
                .or_insert_with(|| decode_pattern(read, layout, entry));
            steps.push(OrderStep {
                pattern_number: entry,
                transpose,
            });
        }
        arrangements.push(VoiceArrangement {
            voice: VoiceId::from_index(vi as usize),
            steps,
        });
    }

    NativeStructure {
        arrangements,
        patterns,
    }
}

/// Resolve the per-voice pattern placement timeline in the **frame domain**,
/// walking (and looping) the orderlist exactly as [`decode_song`] does but
/// recording one [`NativePlacement`] per pattern instance — its number, the
/// transpose active at that step, and the play frame it starts at — instead of
/// emitting notes. Because it shares [`decode_song`]'s tick accounting and
/// [`row_frames`] mapping, every decoded note falls inside its placement's frame
/// span, so the synth exporter can slice the flat note timeline back into these
/// driver-authored blocks (see [`crate::export::synth`]).
pub(super) fn resolve_placements(
    ir: &HubbardIr,
    layout: &HubbardLayout,
    frames: u32,
) -> Vec<VoicePlacements> {
    let tick_to_frame = row_frames(layout, frames);
    let at_tick =
        |tick: u32| -> u32 { tick_to_frame.get(tick as usize).copied().unwrap_or(frames) };
    let mut out = Vec::new();

    for (voice, instances) in &ir.voices {
        if instances.is_empty() {
            continue;
        }
        let mut placements = Vec::new();
        for instance in instances {
            let start = at_tick(instance.start_tick.0);
            if start >= frames {
                break;
            }
            placements.push(NativePlacement {
                pattern_number: instance.pattern,
                start_frame: FrameIndex(start),
                transpose: instance.transpose,
                order_offset: Some(PlacementOrderOffset(instance.order_offset.0)),
                repeat_ordinal: Some(RepeatOrdinal(instance.repeat_ordinal.0)),
            });
        }
        out.push(VoicePlacements {
            voice: *voice,
            placements,
        });
    }
    out
}

pub(super) fn recovered_order_commands(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    voice: u8,
) -> Result<Vec<RecoveredOrderCommand>, HubbardDecodeError> {
    let order = orderlist_checked(read, layout, voice)?;
    let mut commands = Vec::new();
    let mut offset = 0usize;
    let mut repeat = PatternRepeatCount(1);
    while offset < order.len() {
        let command_offset = PlacementOrderOffset(offset);
        let entry = order[offset];
        offset += 1;
        if entry == ORDER_CMD {
            commands.push(RecoveredOrderCommand::Stop {
                order_offset: command_offset,
            });
            return Ok(commands);
        } else if entry & ORDER_TRANSPOSE != 0 {
            let transpose = if let Some(mask) = layout.embedded_transpose_mask {
                PatternTranspose(i16::from(entry & mask))
            } else {
                let Some(value) = order.get(offset).copied() else {
                    return Err(HubbardDecodeError::MissingOrderOperand {
                        voice: voice + 1,
                        offset: command_offset.0,
                    });
                };
                offset += 1;
                PatternTranspose(i16::from(value))
            };
            commands.push(RecoveredOrderCommand::SetTranspose {
                order_offset: command_offset,
                transpose,
            });
        } else {
            commands.push(RecoveredOrderCommand::Pattern {
                order_offset: command_offset,
                pattern: PatternNumber(entry),
                repeat,
            });
            if layout.order_repeat {
                let Some(value) = order.get(offset).copied() else {
                    return Err(HubbardDecodeError::MissingOrderOperand {
                        voice: voice + 1,
                        offset: command_offset.0,
                    });
                };
                repeat = PatternRepeatCount(u32::from(value));
                offset += 1;
            }
        }
    }
    commands.push(RecoveredOrderCommand::Loop {
        order_offset: PlacementOrderOffset(order.len()),
        target: PlacementOrderOffset(0),
    });
    Ok(commands)
}

pub(super) fn recovered_structure(
    read: &impl Fn(u16) -> u8,
    ir: &HubbardIr,
    layout: &HubbardLayout,
    frames: u32,
) -> Result<RecoveredStructure, HubbardDecodeError> {
    let tick_to_frame = row_frames(layout, frames);
    let at_tick = |tick: u32| tick_to_frame.get(tick as usize).copied().unwrap_or(frames);
    let mut patterns = std::collections::BTreeMap::new();
    let mut voices = Vec::new();
    for (voice, instances) in &ir.voices {
        let mut recovered_instances = Vec::new();
        for instance in instances {
            patterns.entry(instance.pattern).or_insert_with(|| {
                instance
                    .events
                    .iter()
                    .map(|event| RecoveredPatternEvent {
                        offset: event.offset,
                        duration: PatternDuration(u16::from(event.duration)),
                        frequency_index: event.note.map(|index| FrequencyTableIndex(index.0)),
                        instrument: event.instrument.map(|index| InstrumentNumber(index.0)),
                        hold: event.hold,
                        slide: event.slide.map(NativeEffectByte),
                        command: None,
                        command_data: None,
                        duration_index: None,
                        operand: None,
                    })
                    .collect()
            });
            recovered_instances.push(RecoveredPatternInstance {
                pattern: instance.pattern,
                transpose: instance.transpose,
                repeat_ordinal: RepeatOrdinal(instance.repeat_ordinal.0),
                order_offset: PlacementOrderOffset(instance.order_offset.0),
                start_tick: NativeRowTick(instance.start_tick.0),
                start_frame: FrameIndex(at_tick(instance.start_tick.0)),
            });
        }
        voices.push(RecoveredVoiceStructure {
            voice: *voice,
            order_loop_offset: Some(PlacementOrderOffset(0)),
            order_commands: recovered_order_commands(read, layout, voice.to_index() as u8)?,
            instances: recovered_instances,
        });
    }
    Ok(RecoveredStructure { patterns, voices })
}

/// Collapse the per-step notes of a manual (in-pattern) arpeggio into the single
/// sustained note the chip actually plays, returning the indices of `notes` (in
/// input order) that survive, each paired with its possibly span-extended end
/// frame.
///
/// Some tunes (Human Race) write an arpeggio as a run of rapid pattern notes
/// rather than an instrument-program offset table. The chip gates once and then
/// only switches frequency, so the trace-derived [`detect_notes`] emits one note
/// for the whole gated region plus an [`Effect::Arpeggio`] span over it — while
/// [`decode_song`], walking the pattern, emits a note per step. Left alone, that
/// is both a self-validation mismatch and a double representation in the export
/// (retriggered notes *and* an arpeggio effect on top).
///
/// Each arpeggio span covers exactly one gate region, so the trace has exactly
/// one note in it; this keeps the first decoded note inside each span (extending
/// it to the span's end so it sounds held) and drops the rest. Notes outside any
/// span survive with their original end. Returning indices (rather than the
/// collapsed notes) lets a parallel per-note array — the instrument indices — be
/// collapsed in lockstep with the notes.
pub(super) fn arpeggio_survivors(
    notes: &[NoteEvent],
    effects: &[crate::analysis::effects::EffectSpan],
) -> Vec<(usize, Option<FrameIndex>)> {
    use crate::analysis::effects::Effect;
    let arps: Vec<_> = effects
        .iter()
        .filter(|e| e.effect == Effect::Arpeggio)
        .collect();
    if arps.is_empty() {
        return (0..notes.len()).map(|i| (i, notes[i].end_frame)).collect();
    }

    let in_span = |n: &NoteEvent, s: &crate::analysis::effects::EffectSpan| {
        s.voice == Some(n.voice)
            && n.start_frame.0 >= s.start_frame.0
            && n.start_frame.0 <= s.end_frame.0
    };

    // Within each span, only the earliest note survives.
    let mut drop = vec![false; notes.len()];
    for span in &arps {
        let mut idxs: Vec<usize> = notes
            .iter()
            .enumerate()
            .filter(|(_, n)| in_span(n, span))
            .map(|(i, _)| i)
            .collect();
        idxs.sort_by_key(|&i| notes[i].start_frame.0);
        for &i in idxs.iter().skip(1) {
            drop[i] = true;
        }
    }

    let mut out = Vec::new();
    for (i, n) in notes.iter().enumerate() {
        if drop[i] {
            continue;
        }
        let mut end = n.end_frame;
        for span in &arps {
            if in_span(n, span) {
                // `EffectSpan.end_frame` is inclusive; the note's is exclusive.
                let span_end = span.end_frame.0 + 1;
                end = Some(FrameIndex(end.map_or(span_end, |e| e.0.max(span_end))));
            }
        }
        out.push((i, end));
    }
    out
}
