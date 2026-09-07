use super::*;

pub(super) const PHASE_FIT_RADIUS: i64 = 8;

pub(super) fn shift_frame(frame: FrameIndex, offset: i64, frames: u32) -> FrameIndex {
    FrameIndex(
        (i64::from(frame.0) + offset)
            .clamp(0, i64::from(frames))
            .try_into()
            .unwrap_or(frames),
    )
}

pub(super) fn shift_notes(notes: &[NoteEvent], offset: i64, frames: u32) -> Vec<NoteEvent> {
    notes
        .iter()
        .copied()
        .map(|mut note| {
            note.start_frame = shift_frame(note.start_frame, offset, frames);
            note.end_frame = note
                .end_frame
                .map(|frame| shift_frame(frame, offset, frames));
            note
        })
        .collect()
}

pub(super) fn validate_with_phase_fit(
    notes: &[NoteEvent],
    truth: &[NoteEvent],
    timing: crate::emu::PlaybackTiming,
    frames: u32,
) -> (Vec<NoteEvent>, NativeValidationReport, i64) {
    let policy = NativeValidationPolicy::default();
    let mut best_notes = notes.to_vec();
    let mut best_report = validate_native_notes(&best_notes, truth, timing, policy);
    let mut best_offset = 0i64;
    for offset in -PHASE_FIT_RADIUS..=PHASE_FIT_RADIUS {
        if offset == 0 {
            continue;
        }
        let shifted = shift_notes(notes, offset, frames);
        let report = validate_native_notes(&shifted, truth, timing, policy);
        let report_onset = report.onset.median.map_or(i64::MAX, |value| value.0);
        let best_onset = best_report.onset.median.map_or(i64::MAX, |value| value.0);
        let better = report.matched.0 > best_report.matched.0
            || (report.matched == best_report.matched
                && (report_onset < best_onset
                    || (report_onset == best_onset && offset.abs() < best_offset.abs())));
        if better {
            best_notes = shifted;
            best_report = report;
            best_offset = offset;
        }
    }
    best_report.decoder_phase = DecoderPhaseResolution::BoundedFit {
        offset: CallResidual(best_offset),
        search_radius: CallResidual(PHASE_FIT_RADIUS),
    };
    (best_notes, best_report, best_offset)
}
