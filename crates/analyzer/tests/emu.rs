use sid_analyzer::emu::bus::Bus;
use sid_analyzer::emu::runner::{call, make_cpu};
use sid_analyzer::emu::{CallRate, CiaTimerPeriod, PlaybackTiming};
use sid_analyzer::header::{self, SidModel, SubtuneIndex};
use sid_analyzer::trace::{SID_REGISTER_LAST, SidRegister, SubFrameOffset};

/// Assemble a tiny 6502 program at `$0800` that:
///   LDA #$AA  ; A9 AA
///   STA $D400 ; 8D 00 D4
///   LDA #$BB  ; A9 BB
///   STA $D41C ; 8D 1C D4  -- highest trapped register
///   LDA #$CC  ; A9 CC
///   STA $D41D ; 8D 1D D4  -- one past trap window, must NOT be captured
///   RTS       ; 60
const PROGRAM: &[u8] = &[
    0xA9, 0xAA, 0x8D, 0x00, 0xD4, 0xA9, 0xBB, 0x8D, 0x1C, 0xD4, 0xA9, 0xCC, 0x8D, 0x1D, 0xD4, 0x60,
];

#[test]
fn bus_traps_sid_window_only() {
    let mut cpu = make_cpu();
    cpu.memory.load(0x0800, PROGRAM);

    let writes = call(&mut cpu, 0x0800, 0, 0, 0).expect("call returns");

    assert_eq!(
        writes.len(),
        2,
        "only the two SID-window writes are trapped"
    );
    assert_eq!(writes[0].reg, SidRegister(0x00));
    assert_eq!(writes[0].value, 0xAA);
    assert_eq!(writes[1].reg, SID_REGISTER_LAST);
    assert_eq!(writes[1].value, 0xBB);

    // The non-trapped write at $D41D still hits RAM (writes are always
    // forwarded; only logging is gated).
    assert_eq!(cpu.memory.ram[0xD41D], 0xCC);

    // Steps are monotonic.
    assert!(writes[0].offset <= writes[1].offset);
}

#[test]
fn second_call_clears_previous_writes() {
    let mut cpu = make_cpu();
    cpu.memory.load(0x0800, PROGRAM);

    let first = call(&mut cpu, 0x0800, 0, 0, 0).unwrap();
    assert_eq!(first.len(), 2);

    // Re-run; the previous call's writes must not leak into this one, and
    // the step counter resets so identical programs yield identical steps.
    let second = call(&mut cpu, 0x0800, 0, 0, 0).unwrap();
    assert_eq!(second.len(), 2);
    assert_eq!(second[0].offset, first[0].offset);
    assert_eq!(second[1].offset, first[1].offset);
}

#[test]
fn registers_are_seeded_for_call() {
    // STA $D400 ; 8D 00 D4
    // STX $D401 ; 8E 01 D4
    // STY $D402 ; 8C 02 D4
    // RTS       ; 60
    let prog: &[u8] = &[0x8D, 0x00, 0xD4, 0x8E, 0x01, 0xD4, 0x8C, 0x02, 0xD4, 0x60];
    let mut cpu = make_cpu();
    cpu.memory.load(0x0900, prog);

    let writes = call(&mut cpu, 0x0900, 0x11, 0x22, 0x33).unwrap();
    assert_eq!(writes.len(), 3);
    assert_eq!((writes[0].reg, writes[0].value), (SidRegister(0x00), 0x11));
    assert_eq!((writes[1].reg, writes[1].value), (SidRegister(0x01), 0x22));
    assert_eq!((writes[2].reg, writes[2].value), (SidRegister(0x02), 0x33));
}

/// Build a minimal PSID v2 whose init/play routine at `$1000` copies the
/// KERNAL PAL/NTSC flag (`$02A6`) into SID register `$D400`, with the given
/// clock bits patched into the header flags word.
fn psid_reading_tvsflg(clock_bits: u16) -> Vec<u8> {
    const HEADER_LEN: usize = 0x7C;
    let mut b = vec![0u8; HEADER_LEN];
    b[0..4].copy_from_slice(b"PSID");
    b[4..6].copy_from_slice(&2u16.to_be_bytes()); // version
    b[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes()); // data offset
    b[8..10].copy_from_slice(&0x1000_u16.to_be_bytes()); // load
    b[10..12].copy_from_slice(&0x1000_u16.to_be_bytes()); // init
    b[12..14].copy_from_slice(&0x1000_u16.to_be_bytes()); // play
    b[14..16].copy_from_slice(&1u16.to_be_bytes()); // songs
    b[16..18].copy_from_slice(&1u16.to_be_bytes()); // start song
    b[0x76..0x78].copy_from_slice(&(clock_bits << 2).to_be_bytes());
    // LDA $02A6 ; STA $D400 ; RTS
    b.extend_from_slice(&[0xAD, 0xA6, 0x02, 0x8D, 0x00, 0xD4, 0x60]);
    b
}

#[test]
fn tvsflg_seeded_from_header_clock() {
    // PAL (clock bits = 0b01) → $02A6 reads 1; NTSC (0b10) → 0.
    for (clock_bits, expected) in [(0b01_u16, 1_u8), (0b10_u16, 0_u8)] {
        let bytes = psid_reading_tvsflg(clock_bits);
        let header = header::parse(&bytes).expect("parse");
        let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).expect("run");

        let write = trace
            .init_writes
            .iter()
            .find(|w| w.reg == SidRegister(0x00))
            .expect("init wrote $D400");
        assert_eq!(
            write.value, expected,
            "clock bits {clock_bits:#04b} should seed TVSFLG to {expected}"
        );
    }
}

#[test]
fn trace_retains_the_header_sid_model() {
    let mut bytes = psid_reading_tvsflg(0b01);
    let flags = (0b01_u16 << 2) | (0b10_u16 << 4);
    bytes[0x76..0x78].copy_from_slice(&flags.to_be_bytes());
    let header = header::parse(&bytes).expect("parse");
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).expect("run");
    assert_eq!(trace.sid_model, SidModel::Mos8580);
}

/// Minimal PSID whose init programs CIA 1 timer A to `$4025` and whose play
/// routine pokes `$D400`. `speed_mask` selects vblank (0) or CIA (1) for
/// subtune 1.
fn psid_programming_cia_timer(speed_mask: u32) -> Vec<u8> {
    const HEADER_LEN: usize = 0x7C;
    let mut b = vec![0u8; HEADER_LEN];
    b[0..4].copy_from_slice(b"PSID");
    b[4..6].copy_from_slice(&2u16.to_be_bytes());
    b[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    b[8..10].copy_from_slice(&0x1000_u16.to_be_bytes()); // load
    b[10..12].copy_from_slice(&0x1000_u16.to_be_bytes()); // init
    b[12..14].copy_from_slice(&0x100B_u16.to_be_bytes()); // play
    b[14..16].copy_from_slice(&1u16.to_be_bytes()); // songs
    b[16..18].copy_from_slice(&1u16.to_be_bytes()); // start song
    b[18..22].copy_from_slice(&speed_mask.to_be_bytes()); // speed bitmask
    b.extend_from_slice(&[
        0xA9, 0x25, 0x8D, 0x04, 0xDC, // LDA #$25 ; STA $DC04
        0xA9, 0x40, 0x8D, 0x05, 0xDC, // LDA #$40 ; STA $DC05
        0x60, // RTS
        0xA9, 0x01, 0x8D, 0x00, 0xD4, // LDA #$01 ; STA $D400
        0x60, // RTS
    ]);
    b
}

fn psid_temporarily_reprogramming_cia_timer() -> Vec<u8> {
    let mut bytes = psid_programming_cia_timer(1);
    bytes.truncate(0x7C + 11);
    bytes.extend_from_slice(&[
        0xA9, 0x35, 0x8D, 0x04, 0xDC, // change timer low byte
        0xA9, 0x25, 0x8D, 0x04, 0xDC, // restore it before the call returns
        0x60,
    ]);
    bytes
}

fn psid_interrupt_player(vector: u16, handler_tail: &[u8]) -> Vec<u8> {
    const HEADER_LEN: usize = 0x7C;
    const HANDLER: u16 = 0x100B;
    let mut bytes = vec![0u8; HEADER_LEN];
    bytes[0..4].copy_from_slice(b"PSID");
    bytes[4..6].copy_from_slice(&2u16.to_be_bytes());
    bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    bytes[8..10].copy_from_slice(&0x1000_u16.to_be_bytes());
    bytes[10..12].copy_from_slice(&0x1000_u16.to_be_bytes());
    bytes[12..14].copy_from_slice(&0u16.to_be_bytes());
    bytes[14..16].copy_from_slice(&1u16.to_be_bytes());
    bytes[16..18].copy_from_slice(&1u16.to_be_bytes());
    let [vector_lo, vector_hi] = vector.to_le_bytes();
    let [handler_lo, handler_hi] = HANDLER.to_le_bytes();
    bytes.extend_from_slice(&[
        0xA9,
        handler_lo,
        0x8D,
        vector_lo,
        vector_hi,
        0xA9,
        handler_hi,
        0x8D,
        vector_lo.wrapping_add(1),
        vector_hi,
        0x60,
        0xA9,
        0xAA,
        0x8D,
        0x00,
        0xD4,
    ]);
    bytes.extend_from_slice(handler_tail);
    bytes
}

fn psid_chained_kernal_interrupt_player() -> Vec<u8> {
    const HEADER_LEN: usize = 0x7C;
    let mut bytes = vec![0u8; HEADER_LEN];
    bytes[0..4].copy_from_slice(b"PSID");
    bytes[4..6].copy_from_slice(&2u16.to_be_bytes());
    bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    bytes[8..10].copy_from_slice(&0x1000_u16.to_be_bytes());
    bytes[10..12].copy_from_slice(&0x1000_u16.to_be_bytes());
    bytes[12..14].copy_from_slice(&0u16.to_be_bytes());
    bytes[14..16].copy_from_slice(&1u16.to_be_bytes());
    bytes[16..18].copy_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&[
        0xAD, 0x14, 0x03, 0x8D, 0x1D, 0x10, // save old IRQ vector low
        0xAD, 0x15, 0x03, 0x8D, 0x1E, 0x10, // save old IRQ vector high
        0xA9, 0x17, 0x8D, 0x14, 0x03, // install $1017
        0xA9, 0x10, 0x8D, 0x15, 0x03, 0x60, // RTS
        0xA9, 0xAA, 0x8D, 0x00, 0xD4, // handler writes frequency
        0x4C, 0x00, 0x00, // chain to the saved default vector
    ]);
    bytes
}

#[test]
fn cia_timed_subtune_adopts_the_init_programmed_timer_period() {
    let bytes = psid_programming_cia_timer(1);
    let header = header::parse(&bytes).expect("parse");
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 3).expect("run");
    assert!(
        trace.timing_exact,
        "a captured CIA rate is ground-truth eligible"
    );
    let period = 0x4025 + 1;
    assert_eq!(trace.call_rate, CallRate::new(985_248, period));
    let timing = PlaybackTiming::for_subtune(&header, SubtuneIndex(1)).resolved_from_trace(&trace);
    assert!(timing.exact());
    assert_eq!(timing.cia_period, Some(CiaTimerPeriod::new(period)));
    assert_eq!(trace.frames[0].start_cycle.0, period);
    for pair in trace.frames.windows(2) {
        assert_eq!(pair[1].start_cycle.0 - pair[0].start_cycle.0, period);
    }
}

#[test]
fn high_subtunes_use_the_header_policy_to_resolve_cia_playback() {
    for (specific, speed, subtune, cia) in [
        (false, 0x80000000u32, 32, true),
        (false, 0x80000000, 33, true),
        (false, 0x80000000, 256, true),
        (true, 0x80000000, 33, false),
        (true, 1, 33, true),
        (true, 0x80000000, 256, true),
    ] {
        let mut bytes = psid_programming_cia_timer(speed);
        bytes[14..16].copy_from_slice(&256u16.to_be_bytes());
        bytes[0x77] = if specific { 2 } else { 0 };
        let header = header::parse(&bytes).unwrap();
        let subtune = SubtuneIndex(subtune);
        let timing = PlaybackTiming::for_subtune(&header, subtune);
        assert_eq!(timing.cia_timed, cia);
        let trace = sid_analyzer::emu::run(&header, &bytes, subtune, 3).unwrap();
        let period = if cia { 0x4025 + 1 } else { 19656 };
        assert!(trace.timing_exact);
        assert_eq!(trace.call_rate, CallRate::new(985_248, period));
        for pair in trace.frames.windows(2) {
            assert_eq!(pair[1].start_cycle.0 - pair[0].start_cycle.0, period);
        }
    }
}

#[test]
fn init_only_timing_resolution_drives_duration_call_count() {
    let bytes = psid_programming_cia_timer(1);
    let header = header::parse(&bytes).unwrap();
    let requested = PlaybackTiming::for_subtune(&header, SubtuneIndex(1));
    let resolved =
        sid_analyzer::emu::resolve_playback_timing(&header, &bytes, SubtuneIndex(1), requested)
            .unwrap();
    assert_eq!(resolved.cia_period, Some(CiaTimerPeriod::new(0x4026)));
    assert_eq!(
        resolved.calls_for_duration(std::time::Duration::from_secs(2)),
        120
    );

    let one_hundred_hz = PlaybackTiming {
        call_rate: CallRate::new(100, 1),
        cia_period: Some(CiaTimerPeriod::new(9_852)),
        ..requested
    };
    assert_eq!(
        one_hundred_hz.calls_for_duration(std::time::Duration::from_secs(2)),
        200
    );
}

#[test]
fn temporary_cia_period_change_marks_timing_inexact() {
    let bytes = psid_temporarily_reprogramming_cia_timer();
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).unwrap();
    assert_eq!(trace.call_rate, CallRate::new(985_248, 0x4026));
    assert!(!trace.timing_exact);
    let timing = PlaybackTiming::for_subtune(&header, SubtuneIndex(1)).resolved_from_trace(&trace);
    assert!(!timing.exact());
    assert_eq!(timing.cia_period, None);
}

#[test]
fn vblank_subtune_ignores_a_programmed_cia_timer() {
    let bytes = psid_programming_cia_timer(0);
    let header = header::parse(&bytes).expect("parse");
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 2).expect("run");
    assert!(trace.timing_exact);
    assert_eq!(trace.call_rate, CallRate::vblank(header.flags.clock.into()));
    assert_eq!(
        trace.frames[1].start_cycle.0 - trace.frames[0].start_cycle.0,
        19_656
    );
}

#[test]
fn play_address_zero_without_an_irq_vector_fails_with_intent() {
    let mut bytes = psid_reading_tvsflg(0b01);
    bytes[12..14].copy_from_slice(&0u16.to_be_bytes()); // play = 0
    let header = header::parse(&bytes).expect("parse");
    let result = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1);
    assert!(matches!(
        result,
        Err(sid_analyzer::emu::EmuError::InterruptHandlerNotInstalled)
    ));
}

#[test]
fn play_address_zero_runs_the_installed_kernal_irq_vector() {
    let bytes = psid_interrupt_player(0x0314, &[0x4C, 0x7E, 0xEA]);
    let header = header::parse(&bytes).expect("parse");
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 2).expect("run");
    assert!(!trace.timing_exact);
    assert_eq!(trace.frames.len(), 2);
    for frame in trace.frames {
        assert_eq!(frame.writes.len(), 1);
        assert_eq!(frame.writes[0].reg, SidRegister(0));
        assert_eq!(frame.writes[0].value, 0xAA);
    }
}

#[test]
fn play_address_zero_can_chain_to_the_preinitialized_kernal_irq_vector() {
    let bytes = psid_chained_kernal_interrupt_player();
    let header = header::parse(&bytes).expect("parse");
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).expect("run");
    assert_eq!(trace.frames[0].writes[0].value, 0xAA);
}

#[test]
fn play_address_zero_runs_a_direct_hardware_irq_vector() {
    let bytes = psid_interrupt_player(0xFFFE, &[0x40]);
    let header = header::parse(&bytes).expect("parse");
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).expect("run");
    assert_eq!(trace.frames[0].writes[0].value, 0xAA);
}

#[test]
fn play_address_zero_reports_an_nmi_only_player() {
    let bytes = psid_interrupt_player(0x0318, &[0x40]);
    let header = header::parse(&bytes).expect("parse");
    let result = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1);
    assert!(matches!(
        result,
        Err(
            sid_analyzer::emu::EmuError::NmiInterruptHandlerUnsupported {
                handler: sid_analyzer::header::PlayAddress(0x100B)
            }
        )
    ));
}

#[test]
fn bus_default_is_all_zero() {
    let bus = Bus::new();
    assert!(bus.ram.iter().all(|&b| b == 0));
    assert_eq!(bus.offset, SubFrameOffset(0));
}

#[test]
fn trace_timeline_is_monotonic_and_events_fit_their_calls() {
    let bytes = psid_reading_tvsflg(0b01);
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 8).unwrap();

    assert!(
        trace
            .init_writes
            .iter()
            .all(|write| u64::from(write.offset.0) <= trace.init_duration.0)
    );
    for pair in trace.frames.windows(2) {
        let end = pair[0].start_cycle.0 + pair[0].duration.0;
        assert!(end <= pair[1].start_cycle.0);
    }
    for frame in &trace.frames {
        assert!(
            frame
                .writes
                .iter()
                .all(|write| u64::from(write.offset.0) <= frame.duration.0)
        );
        assert!(
            frame
                .reads
                .iter()
                .all(|read| u64::from(read.offset.0) <= frame.duration.0)
        );
    }
}

#[test]
fn cia_timed_subtune_is_loudly_marked_inexact() {
    let mut bytes = psid_reading_tvsflg(0b01);
    bytes[0x12..0x16].copy_from_slice(&1_u32.to_be_bytes());
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).unwrap();
    assert!(!trace.timing_exact);
}

#[test]
fn voice3_reads_are_timestamped_and_count_is_derived() {
    let mut bytes = psid_reading_tvsflg(0b01);
    let data = bytes.len() - 7;
    bytes.truncate(data);
    bytes.extend_from_slice(&[0xAD, 0x1B, 0xD4, 0xAD, 0x1C, 0xD4, 0x60]);
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).unwrap();
    assert_eq!(trace.init_reads.len(), 2);
    assert_eq!(trace.frames[0].reads.len(), 2);
    assert_eq!(trace.voice3_reads_per_frame(), vec![2]);
    assert!(trace.frames[0].reads[0].offset <= trace.frames[0].reads[1].offset);
}

#[test]
fn live_env3_read_drives_cpu_execution_from_the_envelope_core() {
    const HEADER_LEN: usize = 0x7C;
    let mut bytes = vec![0u8; HEADER_LEN];
    bytes[0..4].copy_from_slice(b"PSID");
    bytes[4..6].copy_from_slice(&2u16.to_be_bytes());
    bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    bytes[8..10].copy_from_slice(&0x1000_u16.to_be_bytes());
    bytes[10..12].copy_from_slice(&0x1000_u16.to_be_bytes());
    bytes[12..14].copy_from_slice(&0x1010_u16.to_be_bytes());
    bytes[14..16].copy_from_slice(&1u16.to_be_bytes());
    bytes[16..18].copy_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&[
        0xA9, 0x00, 0x8D, 0x13, 0xD4, // AD = fastest attack/decay
        0xA9, 0xF0, 0x8D, 0x14, 0xD4, // sustain = $FF
        0xA9, 0x01, 0x8D, 0x12, 0xD4, // gate on
        0x60, 0xEA, 0xEA, 0xEA, // RTS + pad to $1010
        0xAD, 0x1C, 0xD4, // LDA ENV3
        0x8D, 0x00, 0xD4, // STA V1 FREQ LO
        0x60,
    ]);
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).unwrap();
    let env_read = trace.frames[0]
        .reads
        .iter()
        .find(|read| read.reg == SidRegister(0x1C))
        .unwrap();
    assert_eq!(env_read.value, 0xFF);
    assert!(
        trace.frames[0]
            .writes
            .iter()
            .any(|write| { write.reg == SidRegister(0x00) && write.value == env_read.value })
    );
}
