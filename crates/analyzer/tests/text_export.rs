use sid_analyzer::analysis::voice::VoiceState;
use sid_analyzer::analysis::{FrameState, SystemClock};
use sid_analyzer::export::text::write_text;
use sid_analyzer::trace::FrameIndex;

mod common;
use common::pulse_gated;

#[test]
fn table_shows_note_name_on_gate_rise_and_dots_when_sustained() {
    let states = vec![
        FrameState {
            frame: FrameIndex(0),
            voices: [
                pulse_gated(0x1D44, true),
                VoiceState::default(),
                VoiceState::default(),
            ],
            ..FrameState::default()
        },
        FrameState {
            frame: FrameIndex(1),
            voices: [
                pulse_gated(0x1D44, true),
                VoiceState::default(),
                VoiceState::default(),
            ],
            ..FrameState::default()
        },
    ];

    let mut buf = Vec::new();
    write_text(&states, SystemClock::Pal, &mut buf).unwrap();
    let s = String::from_utf8(buf).unwrap();
    let lines: Vec<&str> = s.lines().collect();

    assert!(lines[0].starts_with("Frame |"), "header: {}", lines[0]);
    assert!(
        lines[1].contains("A-4"),
        "gate rise emits note name: {}",
        lines[1]
    );
    assert!(
        lines[2].contains("..."),
        "sustained gate shows dots: {}",
        lines[2]
    );
}

#[test]
fn table_shows_dashes_when_gate_is_off() {
    let states = vec![FrameState {
        frame: FrameIndex(0),
        voices: [
            VoiceState::default(),
            VoiceState::default(),
            VoiceState::default(),
        ],
        ..FrameState::default()
    }];

    let mut buf = Vec::new();
    write_text(&states, SystemClock::Pal, &mut buf).unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("---"), "silent voice shows dashes:\n{s}");
}

#[test]
fn header_and_row_have_identical_width() {
    let states = vec![FrameState {
        frame: FrameIndex(0),
        voices: [pulse_gated(0x1D44, true); 3],
        ..FrameState::default()
    }];

    let mut buf = Vec::new();
    write_text(&states, SystemClock::Pal, &mut buf).unwrap();
    let s = String::from_utf8(buf).unwrap();
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(
        lines[0].len(),
        lines[1].len(),
        "header and row widths differ:\nheader: {:?}\nrow:    {:?}",
        lines[0],
        lines[1]
    );
}
