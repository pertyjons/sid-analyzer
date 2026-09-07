use super::*;
use crate::analysis::effects::EffectSpan;
use crate::analysis::note::{GmProgram, Velocity};
use crate::emu::PlaybackTiming;
use crate::header::SubtuneIndex;

fn synthetic_layout() -> HubbardLayout {
    HubbardLayout {
        evidence: LocatorEvidence {
            pattern_pointer_anchor: 0,
            pattern_read_anchor: 0,
            sequence_pointer_tables: (0x2200, 0x2300),
            frequency_table: 0x3000,
            tempo_divider_anchor: None,
        },
        note_mask: 0,
        dur_mask: 0x1F,
        pat_ptr_lo: 0x2000,
        pat_ptr_hi: 0x2100,
        pat_stride: 1,
        zp_ptr: 0,
        pat_read: 0,
        freq_table: 0x3000,
        freq_hi: None,
        seq_ptr_lo: 0x2200,
        seq_ptr_hi: 0x2300,
        tempo: 0,
        prescale_reload: None,
        stall_reload: None,
        voices: 1,
        effect_bytes: 1,
        embedded_transpose_mask: None,
        order_repeat: false,
        inst_table: None,
    }
}

fn write_locator_candidate(ram: &mut [u8], anchor: u16, table_bias: u16) {
    let write = |ram: &mut [u8], address: u16, bytes: &[u8]| {
        let start = usize::from(address);
        ram[start..start + bytes.len()].copy_from_slice(bytes);
    };
    let ptr_lo = 0x6000u16 + table_bias;
    let ptr_hi = 0x6100u16 + table_bias;
    write(
        ram,
        anchor,
        &[
            0xB9,
            ptr_lo as u8,
            (ptr_lo >> 8) as u8,
            0x85,
            0x20,
            0xB9,
            ptr_hi as u8,
            (ptr_hi >> 8) as u8,
            0x85,
            0x21,
        ],
    );
    write(ram, anchor + 0x20, &[0xB1, 0x20]);
    write(ram, anchor + 0x28, &[0x29, 0x1F]);
    let seq_lo = 0x6200u16 + table_bias;
    let seq_hi = seq_lo + 3;
    write(
        ram,
        anchor + 0x40,
        &[
            0xBD,
            seq_lo as u8,
            (seq_lo >> 8) as u8,
            0x85,
            0x30,
            0xBD,
            seq_hi as u8,
            (seq_hi >> 8) as u8,
            0x85,
            0x31,
        ],
    );
    let freq = 0x6400u16 + table_bias;
    write(
        ram,
        anchor + 0x60,
        &[
            0xB9,
            freq as u8,
            (freq >> 8) as u8,
            0xEA,
            0xB9,
            freq.wrapping_add(1) as u8,
            (freq.wrapping_add(1) >> 8) as u8,
        ],
    );
    write(ram, anchor + 0x70, &[0x99, 0x00, 0xD4]);
}

#[test]
fn locator_reports_equally_coherent_candidates_as_ambiguous() {
    let mut ram = vec![0u8; 0x10000];
    write_locator_candidate(&mut ram, 0x1000, 0);
    write_locator_candidate(&mut ram, 0x4000, 0x0800);
    let read = |address| ram[usize::from(address)];
    let error = locate_checked(&read, SCAN_LO, SCAN_HI).unwrap_err();
    assert!(matches!(
        error,
        HubbardLocateError::Ambiguous { evidence } if evidence.len() == 2
    ));
}

#[test]
fn locator_does_not_wrap_instruction_windows_at_top_of_ram() {
    let mut ram = vec![0u8; 0x10000];
    ram[0xFFFF] = 0xB9;
    write_locator_candidate(&mut ram, 0x1000, 0);
    let read = |address| ram[usize::from(address)];
    let layout = locate_checked(&read, SCAN_LO, SCAN_HI).unwrap();
    assert_eq!(layout.evidence.pattern_pointer_anchor, 0x1000);
}

/// Notes-only decode, dropping the parallel instrument indices that the
/// production path ([`HubbardExtractor::extract`]) carries through.
fn decode_notes(
    read: &impl Fn(u16) -> u8,
    layout: &HubbardLayout,
    clock: SystemClock,
    frames: u32,
) -> Vec<NoteEvent> {
    decode_song(read, layout, clock, frames).0
}

/// Apply [`arpeggio_survivors`] to the notes alone (the production path
/// applies the same survivor set to the instrument indices in lockstep).
fn collapse_arpeggios(notes: Vec<NoteEvent>, effects: &[EffectSpan]) -> Vec<NoteEvent> {
    arpeggio_survivors(&notes, effects)
        .into_iter()
        .map(|(i, end)| {
            let mut n = notes[i];
            n.end_frame = end;
            n
        })
        .collect()
}

/// Build a post-init RAM reader for Commando.
fn commando_ram() -> impl Fn(u16) -> u8 {
    let bytes = std::fs::read("../../assets/music/Commando.sid").unwrap();
    let header = crate::header::parse(&bytes).unwrap();
    let mut emu = Emulator::new();
    emu.load(&header, &bytes).unwrap();
    emu.call_init(header.init_address, SubtuneIndex(1), header.songs)
        .unwrap();
    let ram = emu.ram_image();
    move |a: u16| ram[a as usize]
}

/// Triage tool for the AWM V1 lead truncation (uncovered-tail class): dumps
/// a pattern's raw bytes + decoded events. Run:
/// `cargo test -p sid-analyzer --lib dbg_awm_pattern -- --ignored --nocapture`.
#[test]
#[ignore]
fn dbg_awm_pattern() {
    let bytes = std::fs::read("../../assets/music/Auf_Wiedersehen_Monty.sid").unwrap();
    let header = crate::header::parse(&bytes).unwrap();
    let mut emu = Emulator::new();
    emu.load(&header, &bytes).unwrap();
    emu.call_init(header.init_address, SubtuneIndex(1), header.songs)
        .unwrap();
    let ram = emu.ram_image();
    let read = |a: u16| ram[a as usize];
    let layout = locate(&read, SCAN_LO, SCAN_HI).unwrap();
    eprintln!("layout: tempo={} voices={}", layout.tempo, layout.voices);
    for vi in 0..layout.voices {
        eprintln!(
            "V{} orderlist: {:02X?}",
            vi + 1,
            orderlist(&read, &layout, vi)
        );
    }
    for num in [52u8] {
        let base = pattern_addr(&read, &layout, num);
        let raw: Vec<u8> = (0..96).map(|i| read(base.wrapping_add(i))).collect();
        eprintln!("pattern {num} @${base:04X} raw: {raw:02X?}");
        for (i, ev) in decode_pattern(&read, &layout, num).iter().enumerate() {
            eprintln!(
                "  ev{i}: dur={} note={:?} inst={:?} slide={:?}",
                ev.duration, ev.note, ev.instrument, ev.slide
            );
        }
    }
}

/// Triage tool for the Family A (timing-only) HVSC clump: dumps each tune's
/// header, recovered timing layout, and onset agreement. Reads the HVSC root
/// from `SID_HVSC_ROOT` and no-ops when unset. Run:
/// `SID_HVSC_ROOT=… cargo test -p sid-analyzer --lib dbg_family_a -- --ignored --nocapture`.
#[test]
#[ignore]
fn dbg_family_a() {
    let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
        eprintln!("SID_HVSC_ROOT unset; skipping");
        return;
    };
    // Override the tune list with `SID_DBG_TUNES` (comma-separated paths
    // relative to the HVSC root); defaults to the Family A timing clump.
    let default_tunes = "MUSICIANS/B/Bjerregaard_Johannes/Camel_Riders_Inc.sid,\
             MUSICIANS/B/Bjerregaard_Johannes/Ragtime_Anno_87.sid,\
             MUSICIANS/H/Hubbard_Rob/Kentilla.sid,\
             MUSICIANS/H/Hubbard_Rob/Spellbound.sid"
        .to_string();
    let tunes_var = std::env::var("SID_DBG_TUNES").unwrap_or(default_tunes);
    for rel in tunes_var.split(',').map(str::trim) {
        let path = format!("{root}/{rel}");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("{rel}: missing");
            continue;
        };
        let header = crate::header::parse(&bytes).unwrap();
        let mut emu = Emulator::new();
        emu.load(&header, &bytes).unwrap();
        emu.call_init(header.init_address, header.start_song, header.songs)
            .unwrap();
        let ram = emu.ram_image();
        let read = |a: u16| ram[a as usize];
        let layout = locate(&read, SCAN_LO, SCAN_HI);
        let name = rel.rsplit('/').next().unwrap();
        eprintln!(
            "\n=== {name}  init=${:04X} play=${:04X} songs={} start={} speed=${:08X} clock={:?} ===",
            header.init_address.0,
            header.play_address.0,
            header.songs.0,
            header.start_song.0,
            header.speed.0,
            header.flags.clock,
        );
        match &layout {
            Some(l) => {
                eprintln!(
                    "  tempo={} prescale={:?} stall={:?} voices={} seq_lo=${:04X} seq_hi=${:04X}",
                    l.tempo,
                    l.prescale_reload,
                    l.stall_reload,
                    l.voices,
                    l.seq_ptr_lo,
                    l.seq_ptr_hi
                );
                let frames = 400u32;
                let native = decode_notes(&read, l, SystemClock::Pal, frames);
                let trace = crate::emu::run(&header, &bytes, header.start_song, frames).unwrap();
                let states = crate::analysis::analyze(&trace);
                let effects = crate::analysis::effects::detect_effects(
                    &trace,
                    &states,
                    crate::analysis::effects::EffectThresholds::default(),
                );
                let native = collapse_arpeggios(native, &effects);
                let truth = crate::analysis::note::detect_notes(&states, SystemClock::Pal);
                let onset = onset_agreement(&native, &truth);
                eprintln!(
                    "  onset={onset:.3}  native={} truth={}",
                    native.len(),
                    truth.len()
                );
                let fmt = |ns: &[NoteEvent]| {
                    let mut v: Vec<NoteEvent> = ns.to_vec();
                    v.sort_by_key(|n| (n.start_frame.0, n.voice.0));
                    v.iter()
                        .take(18)
                        .map(|n| format!("{}:v{}m{}", n.start_frame.0, n.voice.0, n.midi.0))
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                eprintln!("    nat: {}", fmt(&native));
                eprintln!("    tru: {}", fmt(&truth));
                if let Ok(vs) = std::env::var("SID_DBG_VOICE") {
                    let voice: u8 = vs.parse().unwrap();
                    eprintln!(
                        "  layout: pat_lo=${:04X} pat_hi=${:04X} stride={} freq=${:04X} freq_hi={:?} dur_mask=${:02X} eb={} note_mask=${:02X} pat_read=${:04X}",
                        l.pat_ptr_lo,
                        l.pat_ptr_hi,
                        l.pat_stride,
                        l.freq_table,
                        l.freq_hi,
                        l.dur_mask,
                        l.effect_bytes,
                        l.note_mask,
                        l.pat_read,
                    );
                    let ol = orderlist(&read, l, voice);
                    eprintln!("  V{voice} orderlist: {ol:02X?}");
                    for &num in ol.iter().take(4) {
                        let base = pattern_addr(&read, l, num);
                        let raw: Vec<u8> = (0..32).map(|i| read(base.wrapping_add(i))).collect();
                        eprintln!("    pat {num:02X} @${base:04X}: {raw:02X?}");
                        let evs = decode_pattern(&read, l, num);
                        let s: Vec<String> = evs
                            .iter()
                            .take(12)
                            .map(|e| {
                                format!(
                                    "d{}{}{}{}",
                                    e.duration,
                                    e.note.map_or(String::new(), |n| format!("n{n}")),
                                    e.instrument.map_or(String::new(), |i| format!("i{i}")),
                                    e.slide.map_or(String::new(), |s| format!("s{s:02X}")),
                                )
                            })
                            .collect();
                        eprintln!("      decoded: {}", s.join(" "));
                    }
                }
            }
            None => eprintln!("  NO LAYOUT"),
        }
        std::fs::write(format!("/tmp/fa_{name}.bin"), &ram).unwrap();
    }
}

fn ikari_ram() -> impl Fn(u16) -> u8 {
    let ram = ram_for("../../assets/music/Ikari_Union.sid").unwrap();
    move |a: u16| ram[a as usize]
}

/// Build a post-init RAM reader for any asset SID.
fn ram_for(path: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    let header = crate::header::parse(&bytes).ok()?;
    let mut emu = Emulator::new();
    emu.load(&header, &bytes).ok()?;
    emu.call_init(header.init_address, SubtuneIndex(1), header.songs)
        .ok()?;
    Some(emu.ram_image())
}

#[test]
fn decode_pattern_honours_six_bit_duration_mask() {
    // The Jeroen Tel relocation masks the status byte with AND #$3F, so a
    // duration of up to 63 ticks must survive; a 5-bit mask would truncate it.
    let mut mem = vec![0u8; 0x1_0000];
    mem[0x1000] = 0x25; // status: duration 0x25 (37), no tie, no extra
    mem[0x1001] = 0x10; // note index
    mem[0x1002] = END_MARK;
    mem[0x2000] = 0x00; // pat_ptr_lo[0]
    mem[0x2100] = 0x10; // pat_ptr_hi[0]  -> pattern 0 at $1000
    let read = move |a: u16| mem[a as usize];
    let layout = HubbardLayout {
        evidence: LocatorEvidence {
            pattern_pointer_anchor: 0,
            pattern_read_anchor: 0,
            sequence_pointer_tables: (0, 0),
            frequency_table: 0,
            tempo_divider_anchor: None,
        },
        note_mask: 0,
        dur_mask: 0x3F,
        pat_ptr_lo: 0x2000,
        pat_ptr_hi: 0x2100,
        pat_stride: 1,
        zp_ptr: 0,
        pat_read: 0,
        freq_table: 0,
        freq_hi: None,
        seq_ptr_lo: 0,
        seq_ptr_hi: 0,
        tempo: 0,
        prescale_reload: None,
        stall_reload: None,
        voices: 3,
        effect_bytes: 1,
        embedded_transpose_mask: None,
        order_repeat: false,
        inst_table: None,
    };
    let events = decode_pattern(&read, &layout, 0);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].duration, 0x25,
        "6-bit duration must not be truncated"
    );
    assert_eq!(events[0].note, Some(FrequencyIndex(0x10)));
    assert!(!events[0].sustain, "bit 5 belongs to the six-bit duration");
}

#[test]
fn gate_continuation_modes_preserve_both_player_dialects() {
    let mut mem = vec![0u8; 0x1_0000];
    mem[0x1000..0x1009].copy_from_slice(&[
        STATUS_SUSTAIN | 0x01,
        0x00,
        STATUS_SUSTAIN | 0x01,
        0x81,
        0x01,
        0x02,
        STATUS_SUSTAIN | 0x01,
        0x83,
        END_MARK,
    ]);
    mem[0x2000] = 0x00;
    mem[0x2100] = 0x10;
    mem[0x2200] = 0x00;
    mem[0x2300] = 0x24;
    mem[0x2400..0x2402].copy_from_slice(&[0x00, END_MARK]);
    for (index, frequency) in [0x1D44u16, 0x2266, 0x28A0, 0x2EE0].into_iter().enumerate() {
        let offset = 0x3000 + index * 2;
        mem[offset] = frequency as u8;
        mem[offset + 1] = (frequency >> 8) as u8;
    }
    let read = |address: u16| mem[usize::from(address)];
    let layout = synthetic_layout();

    let events = decode_pattern(&read, &layout, 0);
    assert_eq!(events[1].note, Some(FrequencyIndex(1)));
    assert!(events[1].hold);
    assert!(events[1].sustain);

    let (notes, _) = decode_song(&read, &layout, SystemClock::Pal, 8);
    let spans: Vec<_> = notes
        .iter()
        .map(|note| (note.start_frame, note.end_frame, note.midi))
        .collect();
    assert_eq!(spans.len(), 3);
    assert_eq!(spans[0].0, FrameIndex(0));
    assert_eq!(spans[0].1, Some(FrameIndex(4)));
    assert_eq!(spans[1].0, FrameIndex(4));
    assert_eq!(spans[1].1, Some(FrameIndex(6)));
    assert_eq!(spans[2].0, FrameIndex(6));
    assert_eq!(spans[2].1, Some(FrameIndex(8)));

    let ir = decode_ir(&read, &layout, 8).unwrap();
    let (notes, _) = decode_song_from_ir_with_mode(
        &read,
        &layout,
        &ir,
        SystemClock::Pal,
        8,
        GateContinuationMode::EverySustainedRow,
    );
    let spans: Vec<_> = notes
        .iter()
        .map(|note| (note.start_frame, note.end_frame, note.midi))
        .collect();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].0, FrameIndex(0));
    assert_eq!(spans[0].1, Some(FrameIndex(6)));
    assert_eq!(spans[1].0, FrameIndex(6));
    assert_eq!(spans[1].1, Some(FrameIndex(8)));
}

#[test]
fn zero_duration_rows_keep_the_gate_open_until_a_longer_row() {
    let mut mem = vec![0u8; 0x1_0000];
    mem[0x1000..0x1009]
        .copy_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x01, 0x03, END_MARK]);
    mem[0x2000] = 0x00;
    mem[0x2100] = 0x10;
    mem[0x2200] = 0x00;
    mem[0x2300] = 0x24;
    mem[0x2400..0x2402].copy_from_slice(&[0x00, END_MARK]);
    for (index, frequency) in [0x1D44u16, 0x2266, 0x28A0, 0x2EE0].into_iter().enumerate() {
        let offset = 0x3000 + index * 2;
        mem[offset] = frequency as u8;
        mem[offset + 1] = (frequency >> 8) as u8;
    }
    let read = |address: u16| mem[usize::from(address)];
    let layout = synthetic_layout();
    let ir = decode_ir(&read, &layout, 6).unwrap();

    let (alternate, _) = decode_song_from_ir_with_mode(
        &read,
        &layout,
        &ir,
        SystemClock::Pal,
        6,
        GateContinuationMode::EverySustainedRow,
    );

    assert_eq!(alternate.len(), 2);
    assert_eq!(alternate[0].start_frame, FrameIndex(0));
    assert_eq!(alternate[0].end_frame, Some(FrameIndex(4)));
    assert_eq!(alternate[1].start_frame, FrameIndex(4));
    assert_eq!(alternate[1].end_frame, Some(FrameIndex(6)));

    let (established, _) = decode_song_from_ir_with_mode(
        &read,
        &layout,
        &ir,
        SystemClock::Pal,
        6,
        GateContinuationMode::BitSevenNotes,
    );
    assert_eq!(established.len(), 4);
    assert_eq!(established[0].end_frame, Some(FrameIndex(1)));
    assert_eq!(established[1].start_frame, FrameIndex(1));
    assert_ne!(established[0].midi, established[1].midi);

    let six_bit_layout = HubbardLayout {
        dur_mask: 0x3F,
        ..layout
    };
    let six_bit_ir = decode_ir(&read, &six_bit_layout, 6).unwrap();
    let (six_bit, _) = decode_song_from_ir_with_mode(
        &read,
        &six_bit_layout,
        &six_bit_ir,
        SystemClock::Pal,
        6,
        GateContinuationMode::EverySustainedRow,
    );
    assert_eq!(six_bit.len(), 4);
}

#[test]
fn established_gate_dialect_stays_preferred_after_acceptance() {
    let note = NoteEvent {
        voice: VoiceId::V1,
        start_frame: FrameIndex(0),
        end_frame: Some(FrameIndex(4)),
        midi: crate::analysis::note::MidiNote(60),
        cents: crate::analysis::note::Cents(0.0),
        program: GmProgram::SQUARE_LEAD,
        velocity: Velocity(100),
    };
    let established = validate_native_notes(
        &[note],
        &[note],
        PlaybackTiming::vblank(SystemClock::Pal),
        NativeValidationPolicy::default(),
    );
    let mut alternate = established.clone();
    alternate.matched.0 = alternate.matched.0.saturating_add(1);
    assert!(!candidate_validation_is_better(&alternate, &established));

    let mut rejected = established.clone();
    rejected.accepted = false;
    rejected.inserted.0 = 1;
    assert!(candidate_validation_is_better(&established, &rejected));
}

#[test]
fn unterminated_pattern_is_a_typed_failure() {
    let mut mem = vec![0u8; 0x1_0000];
    mem[0x2000] = 0x00;
    mem[0x2100] = 0x10;
    let read = |address: u16| mem[address as usize];
    let error = decode_pattern_checked(&read, &synthetic_layout(), 0).unwrap_err();
    assert!(matches!(
        error,
        HubbardDecodeError::PatternUnterminated { .. }
    ));
}

#[test]
fn repeat_instances_drive_notes_and_placements_from_one_ir() {
    let mut mem = vec![0u8; 0x1_0000];
    mem[0x1000..0x1003].copy_from_slice(&[0x00, 0x00, END_MARK]);
    mem[0x1010..0x1013].copy_from_slice(&[0x00, 0x01, END_MARK]);
    mem[0x2000] = 0x00;
    mem[0x2100] = 0x10;
    mem[0x2001] = 0x10;
    mem[0x2101] = 0x10;
    mem[0x2200] = 0x00;
    mem[0x2300] = 0x24;
    mem[0x2400..0x2405].copy_from_slice(&[0, 2, 1, 1, END_MARK]);
    mem[0x3000..0x3004].copy_from_slice(&[0x00, 0x10, 0x00, 0x20]);
    let read = |address: u16| mem[address as usize];
    let layout = HubbardLayout {
        order_repeat: true,
        ..synthetic_layout()
    };
    let ir = decode_ir(&read, &layout, 5).unwrap();
    let (notes, _) = decode_song_from_ir(&read, &layout, &ir, SystemClock::Pal, 5);
    let placements = resolve_placements(&ir, &layout, 5);
    let starts: Vec<_> = placements[0]
        .placements
        .iter()
        .map(|placement| placement.start_frame.0)
        .collect();
    assert_eq!(starts, vec![0, 1, 2, 3, 4]);
    assert_eq!(notes.len(), placements[0].placements.len());
    assert_eq!(
        placements[0].placements[1].repeat_ordinal,
        Some(RepeatOrdinal(0))
    );
    assert_eq!(
        placements[0].placements[2].repeat_ordinal,
        Some(RepeatOrdinal(1))
    );
}

#[test]
fn collapse_arpeggios_keeps_first_in_span_extends_and_drops_rest() {
    use crate::analysis::effects::{Effect, EffectSpan};
    use crate::analysis::note::{Cents, MidiNote};

    let note = |voice, start: u32, end: u32, midi: u8| NoteEvent {
        voice,
        start_frame: FrameIndex(start),
        end_frame: Some(FrameIndex(end)),
        midi: MidiNote(midi),
        cents: Cents(0.0),
        program: GmProgram::SQUARE_LEAD,
        velocity: Velocity(100),
    };
    let notes = vec![
        note(VoiceId::V1, 0, 7, 60),   // before the span — untouched
        note(VoiceId::V1, 10, 11, 72), // span opener — kept, extended
        note(VoiceId::V1, 12, 13, 75), // inside the span — dropped
        note(VoiceId::V1, 14, 15, 72), // inside the span — dropped
        note(VoiceId::V2, 12, 13, 48), // other voice — untouched
    ];
    let arps = [EffectSpan {
        effect: Effect::Arpeggio,
        voice: Some(VoiceId::V1),
        start_frame: FrameIndex(10),
        end_frame: FrameIndex(20),
    }];

    let out = collapse_arpeggios(notes, &arps);
    let v1: Vec<_> = out.iter().filter(|n| n.voice == VoiceId::V1).collect();
    assert_eq!(
        v1.len(),
        2,
        "span folds to its opener plus the pre-span note"
    );
    assert_eq!(v1[0].start_frame.0, 0);
    assert_eq!(
        v1[0].end_frame,
        Some(FrameIndex(7)),
        "pre-span note untouched"
    );
    assert_eq!(v1[1].start_frame.0, 10);
    assert_eq!(
        v1[1].end_frame,
        Some(FrameIndex(21)),
        "opener extended past the span's inclusive end (exclusive note end)"
    );
    assert!(
        out.iter().any(|n| n.voice == VoiceId::V2),
        "other voices untouched"
    );
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn locates_all_hubbard_assets() {
    let db = crate::playerid::PlayerDb::embedded();
    let dir = std::fs::read_dir("../../assets/music").unwrap();
    let mut hubbard = 0;
    let mut located = 0;
    for entry in dir {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("sid") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        if db.identify(&bytes) != Some("Rob_Hubbard") {
            continue;
        }
        hubbard += 1;
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let Some(ram) = ram_for(path.to_str().unwrap()) else {
            eprintln!("{name}: emulation failed");
            continue;
        };
        let read = |a| ram[a as usize];
        match locate(&read, SCAN_LO, SCAN_HI) {
            Some(l) => {
                located += 1;
                eprintln!(
                    "{name}: pat_lo=${:04X} pat_hi=${:04X} zp=${:02X} pat_read=${:04X} mask=${:04X} freq=${:04X} inst={}",
                    l.pat_ptr_lo,
                    l.pat_ptr_hi,
                    l.zp_ptr,
                    l.pat_read,
                    l.note_mask,
                    l.freq_table,
                    l.inst_table
                        .map_or("none".to_string(), |t| format!("{t:?}"))
                );
                // The `AND #$1F` note mask sits exactly 8 bytes after the
                // pattern fetch in every variant of this routine.
                assert_eq!(
                    l.note_mask,
                    l.pat_read + 8,
                    "{name}: mask not at pat_read+8"
                );
            }
            None => eprintln!("{name}: NO LAYOUT"),
        }
    }
    eprintln!("Hubbard assets: {hubbard}, located: {located}");
    assert!(hubbard >= 1, "expected at least one Rob_Hubbard asset");
    assert_eq!(located, hubbard, "every Rob_Hubbard asset must locate");
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn locates_commando_tables() {
    let read = commando_ram();
    let layout = locate(&read, SCAN_LO, SCAN_HI).expect("Hubbard layout located");
    // Exact addresses from the Commando disassembly (load base $5000).
    assert_eq!(layout.pat_ptr_lo, 0x5711); // LDA $5711,Y @ $50AB
    assert_eq!(layout.pat_ptr_hi, 0x573E); // LDA $573E,Y @ $50B0
    assert_eq!(layout.zp_ptr, 0x5F); // STA $5F / STA $60
    assert_eq!(layout.freq_table, 0x5428); // LDA $5428,Y @ $50FA
    assert_eq!(layout.pat_read, 0x50C2); // LDA ($5F),Y
    assert_eq!(layout.note_mask, 0x50CA); // AND #$1F
    assert_eq!(layout.seq_ptr_lo, 0x56F9); // LDA $56F9,X @ $506E
    assert_eq!(layout.seq_ptr_hi, 0x56FC); // LDA $56FC,X @ $5073
    assert_eq!(layout.dur_mask, 0x1F); // 5-bit duration (AND #$1F)
    assert_eq!(layout.tempo, 2); // $5517 = 2 -> tick every 3 frames
    assert_eq!(layout.prescale_reload, None); // no prescale frame-skip gate
    assert_eq!(layout.stall_reload, None); // no whole-play stall gate
    assert_eq!(layout.voices, 3); // seq_hi - seq_lo = 3 voices
    assert_eq!(layout.effect_bytes, 1); // Commando effects are 1 byte
    assert_eq!(layout.pat_stride, 1); // separate lo/hi pattern-ptr tables
    assert_eq!(layout.freq_hi, None); // one interleaved frequency table
    assert_eq!(layout.embedded_transpose_mask, None); // 2-byte orderlist transpose (unused here)
    assert_eq!(
        layout.inst_table,
        Some(InstrumentTable::Packed { base: 0x5591 })
    ); // @ $5591
    // Both live inside the play loop $5086-$515D.
    assert!((0x5086..=0x515D).contains(&layout.note_mask));
    assert!((0x5086..=0x515D).contains(&layout.pat_read));
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn decode_instruments_matches_commando_table() {
    let read = commando_ram();
    let layout = locate(&read, SCAN_LO, SCAN_HI).expect("Hubbard layout located");
    let insts = decode_instruments(&read, &layout, 12);
    assert_eq!(insts.len(), 12);

    // inst 0: 00 09 41 29 5F 02 E0 00 -> PW $0900, pulse+gate, ADSR 2/9/5/15,
    // effect bytes 02/E0/00: vibrato depth 2, continuous PWM (rate 0, step
    // $E0), no flag effects.
    let i0 = insts[0];
    assert_eq!(i0.pulse_width, PulseWidth(0x0900));
    assert!(i0.control.waveform.pulse && i0.control.gate);
    assert!(!i0.control.sync && !i0.control.ring_mod);
    assert_eq!(
        i0.adsr,
        Adsr {
            attack: 2,
            decay: 9,
            sustain: 5,
            release: 15
        }
    );
    assert_eq!(i0.effects.vibrato_depth, Some(2));
    assert_eq!(
        i0.effects.pwm,
        Some(Pwm {
            rate: 0,
            step: 0xE0
        })
    );
    assert_eq!(i0.effects.pw_offset, None);
    assert!(!i0.effects.drum_drop && !i0.effects.chirp_up && !i0.effects.arp);

    // The control byte decodes to a varied waveform palette (the doc's table).
    assert!(insts[3].control.waveform.noise); // ctrl $81
    assert!(insts[4].control.waveform.pulse && insts[4].control.sync); // $43
    assert!(insts[7].control.waveform.triangle && insts[7].control.ring_mod); // $15
    assert!(insts[9].control.waveform.sawtooth); // $21

    // inst 3: p7 $05 -> drum pitch-drop + arp, no vibrato/PWM.
    assert_eq!(insts[3].effects.vibrato_depth, None);
    assert_eq!(insts[3].effects.pwm, None);
    assert!(insts[3].effects.drum_drop && insts[3].effects.arp);
    assert!(!insts[3].effects.chirp_up);

    // inst 2: p6 $16 with p7 bit3 ($08) -> one-shot PW offset, not PWM.
    assert_eq!(insts[2].effects.pwm, None);
    assert_eq!(insts[2].effects.pw_offset, Some(0x16));

    // inst 4: p7 $03 -> drum pitch-drop + chirp-up.
    assert!(insts[4].effects.drum_drop && insts[4].effects.chirp_up);
    assert!(!insts[4].effects.arp);
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn authored_vibrato_uses_resolved_cia_call_rate() {
    let read = commando_ram();
    let layout = locate(&read, SCAN_LO, SCAN_HI).expect("Hubbard layout located");
    let inst = decode_instruments(&read, &layout, 12)[0];
    let vblank = PlaybackTiming::vblank(SystemClock::Pal);
    let cia = PlaybackTiming {
        cia_timed: true,
        ..vblank
    }
    .with_cia_period(crate::emu::CiaTimerPeriod::new(9_828));
    let vblank_rate = authored_patch_effects(&inst, vblank)
        .vibrato
        .expect("authored vibrato")
        .rate_hz;
    let cia_rate = authored_patch_effects(&inst, cia)
        .vibrato
        .expect("authored vibrato")
        .rate_hz;

    assert!((cia_rate - 2.0 * vblank_rate).abs() < 1e-4);
}

#[test]
fn decode_effects_splits_pwm_from_one_shot_offset() {
    // +7 bit 3 clear: +6 is a continuous PWM (rate = low 5, step = high 3).
    let e = decode_effects(0, 0x45, 0);
    assert_eq!(
        e.pwm,
        Some(Pwm {
            rate: 0x05,
            step: 0x40
        })
    );
    assert_eq!(e.pw_offset, None);
    // +7 bit 3 set: the same +6 is instead a one-shot offset.
    let e = decode_effects(0, 0x45, EFFECT_PW_OFFSET);
    assert_eq!(e.pwm, None);
    assert_eq!(e.pw_offset, Some(0x45));
    // +5/+6 zero: vibrato + pulse effects absent; +7 flags still decode.
    let e = decode_effects(0, 0, 0x07);
    assert_eq!(e.vibrato_depth, None);
    assert_eq!(e.pwm, None);
    assert!(e.drum_drop && e.chirp_up && e.arp);
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn decode_instruments_empty_without_table() {
    let read = commando_ram();
    let mut layout = locate(&read, SCAN_LO, SCAN_HI).unwrap();
    layout.inst_table = None;
    assert!(decode_instruments(&read, &layout, 12).is_empty());
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn locates_ikari_columnar_table() {
    // The Jeroen Tel relocation packs its instrument fields columnarly: the
    // SR table sits `instrument_count` (6) past the AD table, not +1, so the
    // packed confirm fails and `locate_columnar_instruments` recovers the
    // per-field bases from the $D403/$D405/$D406 writes ($1141/$115A/$1160).
    let read = ikari_ram();
    let layout = locate(&read, SCAN_LO, SCAN_HI).expect("Hubbard layout located");
    assert_eq!(
        layout.inst_table,
        Some(InstrumentTable::Columnar {
            pwhi: 0x1633,
            ad: 0x163F,
            sr: 0x1645,
        })
    );
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn decode_instruments_columnar_authors_adsr_only() {
    // Columnar Ikari: ADSR is read from its field-tables; the waveform and
    // effects are program-driven (not statically authored) so they default.
    let read = ikari_ram();
    let layout = locate(&read, SCAN_LO, SCAN_HI).unwrap();
    let insts = decode_instruments(&read, &layout, 6);
    assert_eq!(insts.len(), 6);
    // Authored ADSR varies per instrument (read straight from $163F/$1645,X).
    assert_eq!(
        insts[1].adsr,
        Adsr {
            attack: 0,
            decay: 15,
            sustain: 0,
            release: 9
        }
    );
    assert_ne!(insts[2].adsr, insts[3].adsr);
    // Waveform + effects are not authored in this layout.
    for inst in &insts {
        assert_eq!(inst.control, ControlBits::default());
        assert_eq!(inst.effects, InstrumentEffects::default());
    }
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn decoded_notes_match_emulator_trace() {
    use crate::analysis::analyze;
    use crate::analysis::note::detect_notes;
    use crate::header::SubtuneIndex;

    let frames = 400u32;
    let bytes = std::fs::read("../../assets/music/Commando.sid").unwrap();
    let header = crate::header::parse(&bytes).unwrap();

    // Native decode from the post-init RAM image.
    let mut img = Emulator::new();
    img.load(&header, &bytes).unwrap();
    img.call_init(header.init_address, SubtuneIndex(1), header.songs)
        .unwrap();
    let read = |a| img.read_ram(a);
    let layout = locate(&read, SCAN_LO, SCAN_HI).unwrap();
    let native = decode_notes(&read, &layout, SystemClock::Pal, frames);

    // Ground truth: emulate + analyze + detect notes the standard way.
    let trace = crate::emu::run(&header, &bytes, SubtuneIndex(1), frames).unwrap();
    let states = analyze(&trace);
    let truth = detect_notes(&states, SystemClock::Pal);

    // Voice 3 (the bass) has no slides, so its onsets line up exactly.
    let onsets = |ns: &[NoteEvent]| -> Vec<(u32, u8)> {
        ns.iter()
            .filter(|n| n.voice == VoiceId::V3)
            .map(|n| (n.start_frame.0, n.midi.0))
            .collect()
    };
    let nat = onsets(&native);
    let tru = onsets(&truth);
    assert!(
        nat.len() >= 14,
        "expected a bass timeline, got {}",
        nat.len()
    );
    // Compare the first 14 (start_frame, midi) pairs — both must agree.
    assert_eq!(nat[..14], tru[..14], "native vs emulator V3 onsets differ");
}

/// Decode agreement vs the emulated trace for one asset subtune, or `None`
/// if it cannot be located/emulated.
fn decode_agreement_sub(file: &str, sub: u16, frames: u32) -> Option<f64> {
    use crate::analysis::analyze;
    use crate::analysis::note::detect_notes;
    use crate::header::SubtuneIndex;

    let bytes = std::fs::read(format!("../../assets/music/{file}")).ok()?;
    let header = crate::header::parse(&bytes).ok()?;
    let mut img = Emulator::new();
    img.load(&header, &bytes).ok()?;
    img.call_init(header.init_address, SubtuneIndex(sub), header.songs)
        .ok()?;
    let read = |a| img.read_ram(a);
    let layout = locate(&read, SCAN_LO, SCAN_HI)?;
    let native = decode_notes(&read, &layout, SystemClock::Pal, frames);
    let trace = crate::emu::run(&header, &bytes, SubtuneIndex(sub), frames).ok()?;
    let states = analyze(&trace);
    // Mirror `extract`: collapse manual arpeggios before measuring so the
    // gate sees the same notes the export ships.
    let effects = crate::analysis::effects::detect_effects(
        &trace,
        &states,
        crate::analysis::effects::EffectThresholds::default(),
    );
    let native = collapse_arpeggios(native, &effects);
    let truth = detect_notes(&states, SystemClock::Pal);
    Some(onset_agreement(&native, &truth))
}

/// Dump the recovered native structure ([`decode_structure`]) for every
/// supported asset: per-voice orderlist as `pattern#@transpose` steps, the
/// deduplicated pattern table with each pattern's tick length and reuse count,
/// and a semitone-mapping sanity check (does `+1` frequency-table index equal
/// `+1` MIDI semitone, so a transpose maps to a Pertylizer placement
/// transpose?). A triage tool to eyeball the driver's own reuse before any
/// export wiring — not an assertion.
/// Run: `cargo test -p sid-analyzer --lib dbg_dump_structure -- --ignored --nocapture`.
#[test]
#[ignore]
fn dbg_dump_structure() {
    use crate::header::SubtuneIndex;
    use std::collections::BTreeMap;

    let assets = [
        "Commando.sid",
        "Sigma_Seven.sid",
        "Auf_Wiedersehen_Monty.sid",
        "Knucklebusters.sid",
        "Warhawk.sid",
        "Human_Race.sid",
        "Ikari_Union.sid",
    ];

    for file in assets {
        let Ok(bytes) = std::fs::read(format!("../../assets/music/{file}")) else {
            continue;
        };
        let Ok(header) = crate::header::parse(&bytes) else {
            continue;
        };
        let mut img = Emulator::new();
        if img.load(&header, &bytes).is_err() {
            continue;
        }
        if img
            .call_init(header.init_address, SubtuneIndex(1), header.songs)
            .is_err()
        {
            continue;
        }
        let read = |a| img.read_ram(a);
        let Some(layout) = locate(&read, SCAN_LO, SCAN_HI) else {
            println!("\n=== {file}: locate failed ===");
            continue;
        };
        let st = decode_structure(&read, &layout);

        println!("\n=== {file} ({} voices) ===", layout.voices);
        for arr in &st.arrangements {
            let mut counts: BTreeMap<u8, usize> = BTreeMap::new();
            for s in &arr.steps {
                *counts.entry(s.pattern_number).or_default() += 1;
            }
            let seq: Vec<String> = arr
                .steps
                .iter()
                .map(|s| {
                    if s.transpose == 0 {
                        format!("{}", s.pattern_number)
                    } else {
                        format!("{}@{}", s.pattern_number, s.transpose)
                    }
                })
                .collect();
            let distinct = counts.len();
            println!(
                "  V{}: {} steps, {distinct} distinct patterns, max reuse {}x",
                arr.voice.0,
                arr.steps.len(),
                counts.values().copied().max().unwrap_or(0),
            );
            println!("    order: [{}]", seq.join(" "));
        }
        println!("  patterns:");
        for (num, evs) in &st.patterns {
            let notes = evs.iter().filter(|e| e.note.is_some()).count();
            println!(
                "    #{num}: {} events ({notes} notes), {} ticks",
                evs.len(),
                pattern_ticks(evs),
            );
        }

        // Semitone-mapping check: for the index range the patterns use, does
        // a +1 table index move the note up exactly one MIDI semitone?
        let used: Vec<u8> = st
            .patterns
            .values()
            .flat_map(|evs| evs.iter().filter_map(|e| e.note.map(|note| note.0)))
            .collect();
        if let (Some(&lo), Some(&hi)) = (used.iter().min(), used.iter().max()) {
            let midi = |idx: u8| {
                note_event(&read, &layout, SystemClock::Pal, VoiceId::V1, idx, 0, 1)
                    .map(|n| n.midi.0)
            };
            let mut deltas: BTreeMap<i16, usize> = BTreeMap::new();
            for idx in lo..hi {
                if let (Some(a), Some(b)) = (midi(idx), midi(idx + 1)) {
                    *deltas.entry(i16::from(b) - i16::from(a)).or_default() += 1;
                }
            }
            println!("  semitone-per-index deltas over [{lo}..={hi}]: {deltas:?}  (want all = 1)",);
        }
    }
}

/// Categorization sweep over every HVSC Rob_Hubbard tune: runs the
/// locate + decode pipeline in parallel (rayon) under a per-tune wall-clock
/// deadline, buckets each tune by located/decoded/agreement, and for the
/// decoded-but-low clump prints onset-agreement vs pitch-set-agreement
/// (a timing problem vs a pitch/format problem) plus the recovered layout,
/// so unsupported sub-families stand out. A triage tool, not an assertion.
///
/// Point `SID_HVSC_ROOT` at the HVSC tree (the directory holding `DEMOS/`,
/// `MUSICIANS/`); the test no-ops when it is unset, so it never runs in CI.
/// Run: `SID_HVSC_ROOT=/path/to/C64Music cargo test -p sid-analyzer --lib
/// dbg_hvsc_sweep -- --ignored --nocapture`.
#[test]
#[ignore]
fn dbg_hvsc_sweep() {
    use crate::analysis::analyze;
    use crate::analysis::note::detect_notes;
    use rayon::prelude::*;
    use std::collections::BTreeMap;

    let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
        eprintln!("SID_HVSC_ROOT unset; skipping HVSC sweep");
        return;
    };

    // Recursively collect .sid paths.
    let mut paths = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("sid") {
                paths.push(p);
            }
        }
    }
    paths.sort();

    let frames = 400u32;

    // Per-tune outcome categories. The whole identify + emulate + decode
    // pipeline runs in parallel (the slow part is identify over ~60k files).
    enum Cat {
        EmuFail,
        LocateFail,
        Empty,
        // A handful of digi/raster tunes spin to the per-frame CYCLE_GUARD
        // (1M cycles) every frame; in a debug build 400 such frames take
        // minutes. They are not decode targets anyway, so a per-tune wall
        // clock deadline abandons them instead of stalling the sweep.
        Timeout,
        // (onset, pitchset, name, layout-summary, n_native, n_truth)
        Decoded(f64, f64, String, String, usize, usize),
    }

    // The heavy work for one already-identified Rob_Hubbard tune. Owns its
    // bytes so it can run on a detached worker thread under a deadline.
    fn analyze_one(bytes: Vec<u8>, name: String, frames: u32) -> Cat {
        let Ok(header) = crate::header::parse(&bytes) else {
            return Cat::EmuFail;
        };
        let sub = header.start_song;
        let mut img = Emulator::new();
        if img.load(&header, &bytes).is_err()
            || img
                .call_init(header.init_address, sub, header.songs)
                .is_err()
        {
            return Cat::EmuFail;
        }
        let ram = img.ram_image();
        let read = |a: u16| ram[a as usize];
        let Some(layout) = locate(&read, SCAN_LO, SCAN_HI) else {
            return Cat::LocateFail;
        };
        let native = decode_notes(&read, &layout, SystemClock::Pal, frames);
        let Ok(trace) = crate::emu::run(&header, &bytes, sub, frames) else {
            return Cat::EmuFail;
        };
        let states = analyze(&trace);
        let effects = crate::analysis::effects::detect_effects(
            &trace,
            &states,
            crate::analysis::effects::EffectThresholds::default(),
        );
        let native = collapse_arpeggios(native, &effects);
        if native.is_empty() {
            return Cat::Empty;
        }
        let truth = detect_notes(&states, SystemClock::Pal);
        let onset = onset_agreement(&native, &truth);
        // Pitch-set agreement: fraction of decoded notes whose (voice, midi)
        // appears ANYWHERE in the trace, ignoring timing. High pitch-set but
        // low onset => a timing/tempo format problem; both low => a
        // pitch/pattern format problem.
        let truth_set: std::collections::HashSet<(VoiceId, u8)> =
            truth.iter().map(|t| (t.voice, t.midi.0)).collect();
        let pitch_hits = native
            .iter()
            .filter(|n| truth_set.contains(&(n.voice, n.midi.0)))
            .count();
        let pitchset = pitch_hits as f64 / native.len() as f64;
        let summary = format!(
            "str{} fh{} dm{:02X} pre{} stl{} eb{} te{:02X} v{} t{}",
            layout.pat_stride,
            if layout.freq_hi.is_some() { 1 } else { 0 },
            layout.dur_mask,
            layout.prescale_reload.map_or(-1, |x| x as i32),
            layout.stall_reload.map_or(-1, |x| x as i32),
            layout.effect_bytes,
            layout.embedded_transpose_mask.unwrap_or(0),
            layout.voices,
            layout.tempo,
        );
        Cat::Decoded(onset, pitchset, name, summary, native.len(), truth.len())
    }

    // Per-tune wall-clock deadline. A spinning tune is abandoned (its
    // detached worker finishes on its own and the result is dropped).
    let deadline = std::time::Duration::from_secs(5);

    let outcomes: Vec<Cat> = paths
        .par_iter()
        .filter_map(|path| {
            let db = crate::playerid::PlayerDb::embedded();
            let bytes = std::fs::read(path).ok()?;
            if db.identify(&bytes) != Some("Rob_Hubbard") {
                return None;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(analyze_one(bytes, name, frames));
            });
            Some(rx.recv_timeout(deadline).unwrap_or(Cat::Timeout))
        })
        .collect();

    let hub = outcomes.len();
    let (mut located, mut locate_fail, mut emu_fail, mut empty, mut decoded, mut timeout) =
        (0, 0, 0, 0, 0, 0);
    let mut buckets = [0u32; 11];
    let mut pass = 0;
    let mut clump: Vec<(f64, f64, String, String, usize, usize)> = Vec::new();
    for c in outcomes {
        match c {
            Cat::Timeout => timeout += 1,
            Cat::EmuFail => emu_fail += 1,
            Cat::LocateFail => locate_fail += 1,
            Cat::Empty => {
                located += 1;
                empty += 1;
            }
            Cat::Decoded(onset, pitchset, name, summary, nn, nt) => {
                located += 1;
                decoded += 1;
                let b = ((onset * 10.0).round() as usize).min(10);
                buckets[b] += 1;
                if onset >= MIN_AGREEMENT {
                    pass += 1;
                } else {
                    clump.push((onset, pitchset, name, summary, nn, nt));
                }
            }
        }
    }

    eprintln!(
        "\n=== HVSC Rob_Hubbard sweep (start_song, {frames}f) ===\n\
             hub={hub} located={located} locate_fail={locate_fail} emu_fail={emu_fail} \
             timeout={timeout} empty={empty} decoded={decoded} PASS={pass}\n\
             buckets[0.0..1.0]={buckets:?}\n"
    );

    // Group the clump by layout summary to surface sub-families.
    let mut by_layout: BTreeMap<String, usize> = BTreeMap::new();
    for (_, _, _, s, _, _) in &clump {
        *by_layout.entry(s.clone()).or_default() += 1;
    }
    eprintln!("--- clump ({}) grouped by layout ---", clump.len());
    for (s, n) in &by_layout {
        eprintln!("  {n:3}  {s}");
    }
    eprintln!("\n--- clump rows (onset / pitchset / nN / nT  name  layout) ---");
    clump.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (onset, pitchset, name, summary, nn, nt) in &clump {
        eprintln!("  o{onset:.2} p{pitchset:.2}  {nn:4}/{nt:<4}  {name:36}  {summary}");
    }
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn decode_agreement_separates_supported_variants() {
    // These variants now decode correctly: Commando (1-byte effects), Sigma
    // Seven (2-byte effects + bit7 note ties), Auf Wiedersehen Monty
    // (orderlist transpose commands), Knucklebusters (a prescale frame-skip
    // gate) and Warhawk (a whole-play stall gate). The self-validation gate
    // must accept all of them.
    for (tune, sub) in [
        ("Commando.sid", 1),
        ("Sigma_Seven.sid", 1),
        ("Auf_Wiedersehen_Monty.sid", 1),
        ("Knucklebusters.sid", 1),
        // Subtune 2 runs the same prescale gate with a larger reload (a
        // faster effective tempo): the per-frame timing simulation handles
        // it, where the old fixed frames-per-tick model decoded it ~6x slow.
        ("Knucklebusters.sid", 2),
        ("Warhawk.sid", 1),
        // Human Race writes manual (in-pattern) arpeggios that decode to one
        // note per step; collapse_arpeggios folds each back to the single
        // gated note the chip plays, matching the trace.
        ("Human_Race.sid", 1),
        // Ikari Union (a Jeroen Tel relocation): a 6-bit duration mask, an
        // interleaved pattern-pointer table (`pat_stride == 2`), separate
        // lo/hi frequency tables and a one-byte embedded orderlist transpose
        // — four differences from the Commando layout, all auto-detected.
        ("Ikari_Union.sid", 1),
        // Shape Music 2 (the Magnar player): a one-byte embedded orderlist
        // transpose with a 7-bit mask (`AND #$7F`, vs the Jeroen Tel
        // relocation's 5-bit mask). Reading it as Monty's 2-byte command
        // consumed a pattern byte and applied a per-voice constant pitch
        // offset, scoring 0.0; the auto-detected mask brings it to 1.0.
        ("Shape_Music_2.sid", 1),
    ] {
        let a = decode_agreement_sub(tune, sub, 400)
            .unwrap_or_else(|| panic!("{tune} sub{sub} decodes"));
        assert!(
            a > MIN_AGREEMENT,
            "{tune} sub{sub} agreement {a} should pass the gate"
        );
    }
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn full_length_tail_variants_remain_native_accepted() {
    let db = crate::playerid::PlayerDb::embedded();
    for (file, subtune, frames) in [
        ("Sigma_Seven.sid", SubtuneIndex(1), 3_710),
        ("Knucklebusters.sid", SubtuneIndex(2), 10_126),
        ("Monty_on_the_Run.sid", SubtuneIndex(2), 602),
        ("Nemesis_the_Warlock.sid", SubtuneIndex(1), 20_651),
    ] {
        let bytes = std::fs::read(format!("../../assets/music/{file}")).unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let timing = PlaybackTiming::for_subtune(&header, subtune);
        let (_, extractor, song) = crate::export::native::extract_native_song(
            &db, &header, &bytes, subtune, timing, frames,
        )
        .unwrap_or_else(|error| panic!("{file} full-length native extraction: {error}"));
        assert_eq!(extractor, "hubbard");
        assert!(song.validation.accepted);

        if file == "Sigma_Seven.sid" {
            let recovered = song.recovered_structure.unwrap();
            assert!(recovered.voices.iter().all(|voice| {
                matches!(
                    voice.order_commands.last(),
                    Some(crate::export::RecoveredOrderCommand::Stop { .. })
                )
            }));
        }
    }
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn nemesis_embedded_transpose_is_located() {
    let ram = ram_for("../../assets/music/Nemesis_the_Warlock.sid").unwrap();
    let read = |address| ram[usize::from(address)];
    let layout = locate(&read, SCAN_LO, SCAN_HI).unwrap();
    assert_eq!(layout.embedded_transpose_mask, Some(0x7F));
}
