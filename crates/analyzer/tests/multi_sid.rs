use sid_analyzer::analysis::analyze;
use sid_analyzer::emu::capture::SidChipId;
use sid_analyzer::header::{self, SidModel, SubtuneIndex};
use sid_analyzer::trace::SidRegister;

fn psid_v3(play: &[u8]) -> Vec<u8> {
    const HEADER_LEN: usize = 0x7c;
    const LOAD: u16 = 0x1000;
    let mut bytes = vec![0; HEADER_LEN];
    bytes[0..4].copy_from_slice(b"PSID");
    bytes[4..6].copy_from_slice(&3_u16.to_be_bytes());
    bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    bytes[8..10].copy_from_slice(&LOAD.to_be_bytes());
    bytes[10..12].copy_from_slice(&LOAD.to_be_bytes());
    bytes[12..14].copy_from_slice(&(LOAD + 1).to_be_bytes());
    bytes[14..16].copy_from_slice(&1_u16.to_be_bytes());
    bytes[16..18].copy_from_slice(&1_u16.to_be_bytes());
    bytes[0x76..0x78].copy_from_slice(&0x0090_u16.to_be_bytes());
    bytes[0x7a] = 0x42;
    bytes.push(0x60);
    bytes.extend_from_slice(play);
    bytes
}

#[test]
fn configured_second_sid_is_captured_and_projected_independently() {
    let bytes = psid_v3(&[
        0xa9, 0x11, 0x8d, 0x00, 0xd4, 0xa9, 0x22, 0x8d, 0x20, 0xd4, 0x60,
    ]);
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 2).unwrap();

    assert_eq!(trace.capture.chips.len(), 2);
    assert_eq!(trace.capture.chips[0].model, SidModel::Mos6581);
    assert_eq!(trace.capture.chips[1].id, SidChipId(1));
    assert_eq!(trace.capture.chips[1].base_address.0, 0xd420);
    assert_eq!(trace.capture.chips[1].model, SidModel::Mos8580);
    assert_eq!(trace.frames[0].writes.len(), 1);
    assert_eq!(trace.frames[0].writes[0].value, 0x11);

    let second = trace.capture.project_trace_for_chip(SidChipId(1));
    assert_eq!(second.frames[0].writes.len(), 1);
    assert_eq!(second.frames[0].writes[0].reg, SidRegister(0));
    assert_eq!(second.frames[0].writes[0].value, 0x22);
    assert_eq!(analyze(&second).len(), 2);
    assert!(trace.capture.events.iter().any(|event| {
        event.chip == SidChipId(1) && event.address.0 == 0xd420 && event.value == 0x22
    }));
}

#[test]
fn unconfigured_d420_remains_a_primary_sid_mirror() {
    let mut bytes = psid_v3(&[0xa9, 0x33, 0x8d, 0x20, 0xd4, 0x60]);
    bytes[0x7a] = 0;
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).unwrap();

    assert_eq!(trace.capture.chips.len(), 1);
    assert_eq!(trace.frames[0].writes[0].reg, SidRegister(0));
    assert_eq!(trace.frames[0].writes[0].value, 0x33);
    assert_eq!(trace.capture.events[0].chip, SidChipId::PRIMARY);
}

#[test]
fn additional_sid_readback_uses_its_own_data_latch() {
    let bytes = psid_v3(&[
        0xa9, 0x55, 0x8d, 0x20, 0xd4, 0xad, 0x20, 0xd4, 0x8d, 0x00, 0xd4, 0x60,
    ]);
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).unwrap();

    let second_read = trace
        .capture
        .events
        .iter()
        .find(|event| {
            event.chip == SidChipId(1)
                && event.access == sid_analyzer::emu::capture::SidBusAccess::Read
        })
        .unwrap();
    assert_eq!(second_read.value, 0x55);
    assert_eq!(trace.frames[0].writes.last().unwrap().value, 0x55);
}

#[test]
fn unresolved_tail_of_additional_window_never_falls_through_to_primary_mirror() {
    let bytes = psid_v3(&[0xa9, 0x66, 0x8d, 0x3d, 0xd4, 0x60]);
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).unwrap();

    assert!(trace.frames[0].writes.is_empty());
    assert_eq!(trace.capture.events[0].chip, SidChipId(1));
    assert_eq!(trace.capture.events[0].register, None);
}
