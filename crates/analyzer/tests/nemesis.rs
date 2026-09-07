use sid_analyzer::emu;
use sid_analyzer::header::{
    self, Clock, Format, InitAddress, LoadAddress, PlayAddress, SidModel, SpeedBitmask,
    SubtuneCount, SubtuneIndex,
};
use sid_analyzer::trace::{SID_REGISTER_LAST, SidRegister};

const SAMPLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/Nemesis_the_Warlock.sid"
);

#[test]
fn parses_nemesis_header() {
    let bytes = std::fs::read(SAMPLE).expect("sample SID file present");
    let h = header::parse(&bytes).expect("parse succeeds");

    assert_eq!(h.format, Format::Psid);
    assert_eq!(h.version, 2);
    assert_eq!(h.data_offset, 0x7C);
    assert_eq!(h.load_address, LoadAddress(0));
    assert_eq!(h.init_address, InitAddress(0xF160));
    assert_eq!(h.play_address, PlayAddress(0xF190));
    assert_eq!(h.songs, SubtuneCount(15));
    assert_eq!(h.start_song, SubtuneIndex(1));
    assert_eq!(h.speed, SpeedBitmask(0x0000_0000));
    assert_eq!(h.name, "Nemesis the Warlock");
    assert_eq!(h.author, "Rob Hubbard");
    assert_eq!(h.released, "1987 Martech");
    assert_eq!(h.flags.clock, Clock::Pal);
    assert_eq!(h.flags.sid_model, SidModel::Mos6581);
    assert_eq!(
        h.effective_load_address(&bytes).unwrap(),
        LoadAddress(0xE000)
    );
}

#[test]
fn runs_subtune_1_for_50_frames() {
    let bytes = std::fs::read(SAMPLE).unwrap();
    let header = header::parse(&bytes).unwrap();

    let trace = emu::run(&header, &bytes, SubtuneIndex(1), 50).expect("run succeeds");

    assert_eq!(trace.frames.len(), 50);

    let total = trace.total_writes();
    assert!(
        (50..5_000).contains(&total),
        "expected 50-5000 SID writes across 50 frames, got {total}"
    );

    for frame in &trace.frames {
        for w in &frame.writes {
            assert!(
                w.reg <= SID_REGISTER_LAST,
                "register {} out of SID window",
                w.reg
            );
        }
    }

    let any_voice_ctrl_write = trace
        .frames
        .iter()
        .flat_map(|f| f.writes.iter())
        .any(|w| matches!(w.reg, SidRegister(0x04 | 0x0B | 0x12)));
    assert!(
        any_voice_ctrl_write,
        "expected at least one write to a voice control register ($D404/$D40B/$D412)"
    );
}
