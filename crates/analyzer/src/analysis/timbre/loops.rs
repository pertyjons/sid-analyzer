//! Generic cyclic-loop detection over integer streams.
//!
//! Used by Slice 1b to identify arpeggio patterns in relative-pitch
//! streams and repeating waveform sequences. Slice 2 reuses it for
//! patch fingerprinting.
//!
//! Algorithm: for each candidate prefix offset `o ∈ 0..=MAX_OFFSET`,
//! try period lengths `p` from 2 upward; report the *shortest* one
//! where `tail[i] == tail[i - p]` holds for at least `(1 - tolerance)`
//! of all `i ∈ [p, tail.len())`. We skip period 1 — constants are
//! best represented as `Raw`, not as "loop of one".

/// Maximum pre-loop prefix length to try. Real SID arpeggios rarely
/// have more than a couple of "setup" frames before the cycle starts.
const MAX_OFFSET: usize = 4;

/// Minimum body length. Period 1 = constant; skip it so callers get
/// `Raw` for static streams and `Loop` only for genuine cycles.
const MIN_PERIOD: usize = 2;

/// Default tolerance: ≤ 5 % mismatched elements within the tail.
/// Permissive enough to ignore one-off `$D418`-overlay noise but
/// strict enough to reject "almost-periodic" coincidences.
pub const DEFAULT_TOLERANCE: f32 = 0.05;

/// Find the shortest period (and optional prefix offset) such that
/// the tail `s[offset..]` repeats with that period, within
/// `tolerance`. Returns `(body, offset)` on success.
///
/// `body` is `s[offset..offset + period]`. A `period` of N requires
/// at least `2 * N` tail elements (loop must repeat at least twice).
#[must_use]
pub fn detect_loop<T: PartialEq + Clone>(s: &[T], tolerance: f32) -> Option<(Vec<T>, u8)> {
    let max_off = MAX_OFFSET.min(s.len());
    for offset in 0..=max_off {
        let tail_len = s.len() - offset;
        let max_period = tail_len / 2;
        if max_period < MIN_PERIOD {
            continue;
        }
        for period in MIN_PERIOD..=max_period {
            if is_periodic(&s[offset..], period, tolerance) {
                return Some((s[offset..offset + period].to_vec(), offset as u8));
            }
        }
    }
    None
}

fn is_periodic<T: PartialEq>(tail: &[T], period: usize, tolerance: f32) -> bool {
    let comparisons = tail.len() - period;
    if comparisons == 0 {
        return false;
    }
    // Constant body == period 1 in disguise; reject so callers stay
    // on the `Raw` branch for static streams.
    if tail[..period].iter().all(|x| x == &tail[0]) {
        return false;
    }
    let allowed = (comparisons as f32 * tolerance) as usize;
    // Compare against the canonical body position, not the previous
    // occurrence, so a single perturbation produces exactly one
    // mismatch instead of cascading. Bail out the moment we exceed
    // tolerance — saves work on long pad notes that don't loop.
    let mut mismatches = 0usize;
    for i in period..tail.len() {
        if tail[i] != tail[i % period] {
            mismatches += 1;
            if mismatches > allowed {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_arpeggio_with_zero_offset() {
        let s = vec![0i8, 4, 7, 0, 4, 7, 0, 4, 7];
        let (body, offset) = detect_loop(&s, DEFAULT_TOLERANCE).expect("loop");
        assert_eq!(body, vec![0, 4, 7]);
        assert_eq!(offset, 0);
    }

    #[test]
    fn detects_arpeggio_with_prefix_offset() {
        let s = vec![5i8, 0, 4, 7, 0, 4, 7, 0, 4, 7];
        let (body, offset) = detect_loop(&s, DEFAULT_TOLERANCE).expect("loop");
        assert_eq!(body, vec![0, 4, 7]);
        assert_eq!(offset, 1);
    }

    #[test]
    fn returns_none_for_random_sequence() {
        let s = vec![1i8, 2, 3, 4, 5, 6, 7, 8];
        assert!(detect_loop(&s, DEFAULT_TOLERANCE).is_none());
    }

    #[test]
    fn returns_none_for_constant_sequence() {
        // Constant streams are best represented as Raw — period 1 is
        // semantically "not a loop". We refuse to report it.
        let s = vec![42u8; 16];
        assert!(detect_loop(&s, DEFAULT_TOLERANCE).is_none());
    }

    #[test]
    fn tolerates_single_mismatch_under_threshold() {
        // 30-element 3-step pattern with one perturbation at position
        // 20. 27 comparisons; the non-cascading comparator counts that
        // as exactly 1 mismatch ≈ 3.7 %.
        let mut s: Vec<i8> = (0..30).map(|i| [0i8, 4, 7][(i as usize) % 3]).collect();
        s[20] = 99;
        assert!(detect_loop(&s, DEFAULT_TOLERANCE).is_some());
        assert!(detect_loop(&s, 0.0).is_none());
    }

    #[test]
    fn detects_period_two_oscillation() {
        // Hubbard's classic T-N alternating perc — [0x10, 0x80, 0x10, 0x80, ...]
        let s = vec![0x10u8, 0x80, 0x10, 0x80, 0x10, 0x80, 0x10, 0x80];
        let (body, offset) = detect_loop(&s, DEFAULT_TOLERANCE).expect("loop");
        assert_eq!(body, vec![0x10, 0x80]);
        assert_eq!(offset, 0);
    }

    #[test]
    fn ignores_too_short_sequences() {
        // < 2*MIN_PERIOD = 4 elements: can't fit even one period
        // repeating twice. Return None.
        assert!(detect_loop(&[1u8, 2, 3], DEFAULT_TOLERANCE).is_none());
        assert!(detect_loop::<u8>(&[], DEFAULT_TOLERANCE).is_none());
    }

    #[test]
    fn prefers_shorter_periods() {
        // [1,1,1,1,1,1] could be period 2 or 3, but constants are skipped.
        // [1,2,1,2,1,2,1,2] could be period 2 or 4 — shorter wins.
        let s = vec![1u8, 2, 1, 2, 1, 2, 1, 2];
        let (body, _) = detect_loop(&s, DEFAULT_TOLERANCE).expect("loop");
        assert_eq!(body, vec![1, 2]);
    }
}
