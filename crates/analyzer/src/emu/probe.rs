//! Differential semantics probe — the engine behind `sid-re probe`.
//!
//! Taint (data flow) reads ADDRESSES out of a driver but cannot attribute
//! command SEMANTICS: what a sequence byte *means* flows through branches,
//! not through data, and implicit-flow tracking proved too noisy to ship.
//! This module answers the semantics question causally instead: mutate one
//! stream byte in the post-`init` RAM image, replay the tune, and diff the
//! SID write trace against the unmutated baseline. The first divergent
//! frame, the registers affected, and whether the tail is merely
//! time-shifted classify the byte's role — note pitch, duration, gate/ctrl,
//! envelope, pulse, filter, or structural (orderlist/dispatch) — with zero
//! knowledge of the driver's code.
//!
//! A mutated byte can also derail the player; the runner's cycle guard
//! bounds that, and the crash itself is a verdict (the byte is load-bearing
//! for control flow). When the primary `+1` mutation lands on a grammar
//! boundary and explodes structurally, a `-1` retry often yields the
//! cleaner within-class verdict; the milder of the two is reported.

use super::{EmuError, Emulator};
use crate::header::{Header, SubtuneIndex};
use crate::trace::{FrameIndex, FrameTrace};

/// How many frames after the first divergence are scanned for affected
/// registers.
const DIFF_WINDOW: usize = 128;
/// Largest pure time-shift (in frames) recognised as a Timing verdict.
const MAX_SHIFT: usize = 32;
/// Minimum overlapping frames that must match to accept a time-shift.
const MIN_SHIFT_MATCH: usize = 8;

/// One byte to probe: its address and the value the unmutated image holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeTarget {
    pub addr: u16,
    pub orig: u8,
}

/// Outcome of probing one byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub addr: u16,
    pub orig: u8,
    /// The mutated value that produced `verdict`.
    pub mutated: u8,
    pub verdict: Verdict,
}

/// SID parameter family a register belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    Freq,
    Pulse,
    Ctrl,
    Envelope,
    Filter,
    Volume,
}

impl Category {
    /// Category and voice (0..2, `None` for global) of a SID register offset.
    fn of(reg: u8) -> Option<(Self, Option<u8>)> {
        match reg {
            0..=20 => {
                let cat = match reg % 7 {
                    0 | 1 => Self::Freq,
                    2 | 3 => Self::Pulse,
                    4 => Self::Ctrl,
                    _ => Self::Envelope,
                };
                Some((cat, Some(reg / 7)))
            }
            21..=23 => Some((Self::Filter, None)),
            24 => Some((Self::Volume, None)),
            _ => None,
        }
    }
}

/// The byte's empirically measured role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The traces are identical — the byte never influences the SID output
    /// in the probed window (padding, or a value the player overwrites).
    NoEffect,
    /// The mutated player tripped the cycle guard or errored — the byte is
    /// load-bearing for control flow (jump target, loop count).
    Crash,
    /// The affected voices' write streams are the baseline's shifted by
    /// `shift` frames (other voices untouched) — the byte is a duration /
    /// tempo value. Positive = events happen later. `voices` is a bitmask
    /// (bit 0 = voice 1); `0b111` with global registers also shifted means
    /// the whole tune moved.
    Timing {
        shift: i32,
        voices: u8,
        first_frame: u32,
    },
    /// A bounded set of SID parameters changed — the byte is a note or an
    /// effect/instrument parameter. `voices` is a bitmask (bit 0 = voice 1);
    /// global-only effects (filter/volume) leave it zero.
    Param {
        categories: Vec<Category>,
        voices: u8,
        first_frame: u32,
    },
    /// The trace diverges broadly across voices and parameter families —
    /// the byte steers structure (orderlist entry, pattern jump, dispatch).
    Structural { first_frame: u32 },
}

impl Verdict {
    /// Severity order used to pick the milder, more informative of two
    /// mutations' outcomes.
    fn rank(&self) -> u8 {
        match self {
            Self::NoEffect => 0,
            Self::Timing { .. } => 1,
            Self::Param { .. } => 2,
            Self::Structural { .. } => 3,
            Self::Crash => 4,
        }
    }
}

/// Probe every target byte of `header`/`bytes` and classify each one's role.
///
/// Runs one unmutated baseline plus one (occasionally two) mutated replays
/// per target, all from a fresh `load` + `init`, so results are deterministic
/// and order-independent.
pub fn probe(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    frames: u32,
    targets: &[ProbeTarget],
) -> Result<Vec<ProbeResult>, EmuError> {
    let baseline = run_variant(header, bytes, subtune, frames, None)?;
    let mut results = Vec::with_capacity(targets.len());
    for t in targets {
        let primary = t.orig.wrapping_add(1);
        let verdict = run_and_diff(header, bytes, subtune, frames, t.addr, primary, &baseline)?;
        let (mutated, verdict) = if matches!(verdict, Verdict::Structural { .. } | Verdict::Crash) {
            // A +1 mutation that crosses a grammar boundary (note -> command)
            // reads as structural even when the byte is an ordinary
            // parameter; the -1 neighbour usually stays within the class.
            let alt = t.orig.wrapping_sub(1);
            let alt_verdict = run_and_diff(header, bytes, subtune, frames, t.addr, alt, &baseline)?;
            if alt_verdict.rank() < verdict.rank() && alt_verdict != Verdict::NoEffect {
                (alt, alt_verdict)
            } else {
                (primary, verdict)
            }
        } else {
            (primary, verdict)
        };
        results.push(ProbeResult {
            addr: t.addr,
            orig: t.orig,
            mutated,
            verdict,
        });
    }
    Ok(results)
}

fn run_and_diff(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    frames: u32,
    addr: u16,
    value: u8,
    baseline: &[Vec<(u8, u8)>],
) -> Result<Verdict, EmuError> {
    match run_variant(header, bytes, subtune, frames, Some((addr, value))) {
        Ok(variant) => Ok(diff(baseline, &variant)),
        Err(EmuError::Run(_)) => Ok(Verdict::Crash),
        Err(e) => Err(e),
    }
}

/// Load + init + patch + replay, reduced to per-frame `(reg, value)` write
/// sequences (sub-frame cycle offsets are irrelevant to the diff).
fn run_variant(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    frames: u32,
    patch: Option<(u16, u8)>,
) -> Result<Vec<Vec<(u8, u8)>>, EmuError> {
    let mut emu = Emulator::new();
    emu.load(header, bytes)?;
    emu.call_init(header.init_address, subtune, header.songs)?;
    if let Some((addr, value)) = patch {
        emu.poke(addr, value);
    }
    let mut out = Vec::with_capacity(frames as usize);
    for n in 0..frames {
        let frame = emu.run_play_frame(header.play_address, FrameIndex(n))?;
        out.push(frame_key(&frame));
    }
    Ok(out)
}

fn frame_key(frame: &FrameTrace) -> Vec<(u8, u8)> {
    frame.writes.iter().map(|w| (w.reg.0, w.value)).collect()
}

fn diff(baseline: &[Vec<(u8, u8)>], variant: &[Vec<(u8, u8)>]) -> Verdict {
    let n = baseline.len().min(variant.len());
    let Some(first) = (0..n).find(|&i| baseline[i] != variant[i]) else {
        return Verdict::NoEffect;
    };
    let first_frame = first as u32;

    if let Some(shift) = detect_shift(baseline, variant, first) {
        return Verdict::Timing {
            shift,
            voices: 0b111,
            first_frame,
        };
    }

    // A duration byte shifts only its own voice's events while the other
    // voices play on unchanged, so the whole-trace comparison above misses
    // it: re-run the shift detection per voice track.
    if let Some(v) = per_voice_shift(baseline, variant, n) {
        return v;
    }

    // Collect the registers whose writes differ over the window.
    let end = (first + DIFF_WINDOW).min(n);
    let mut regs: Vec<u8> = Vec::new();
    for i in first..end {
        if baseline[i] == variant[i] {
            continue;
        }
        for reg in differing_regs(&baseline[i], &variant[i]) {
            if !regs.contains(&reg) {
                regs.push(reg);
            }
        }
    }
    let mut categories: Vec<Category> = Vec::new();
    let mut voices = 0u8;
    for &reg in &regs {
        if let Some((cat, voice)) = Category::of(reg) {
            if !categories.contains(&cat) {
                categories.push(cat);
            }
            if let Some(v) = voice {
                voices |= 1 << v;
            }
        }
    }
    categories.sort();
    // Broad damage = structure; a bounded change = a parameter byte. Freq+Ctrl
    // on one voice is still a note (gate retrigger moves with the pitch), so
    // the bound is on families *and* voices.
    if categories.len() > 2 || (voices.count_ones() > 1 && categories.len() > 1) {
        return Verdict::Structural { first_frame };
    }
    Verdict::Param {
        categories,
        voices,
        first_frame,
    }
}

/// Per-register write-sequence comparison within one frame: a register is
/// affected when its own ordered values differ (other registers' extra
/// writes must not implicate it).
fn differing_regs(a: &[(u8, u8)], b: &[(u8, u8)]) -> Vec<u8> {
    let mut regs: Vec<u8> = a.iter().chain(b).map(|&(r, _)| r).collect();
    regs.sort_unstable();
    regs.dedup();
    regs.retain(|&r| {
        let seq = |fr: &[(u8, u8)]| -> Vec<u8> {
            fr.iter()
                .filter(|&&(reg, _)| reg == r)
                .map(|&(_, v)| v)
                .collect()
        };
        seq(a) != seq(b)
    });
    regs
}

/// Split per-frame writes into one stream per voice. Global registers
/// (filter/volume) are dropped: per-voice shift detection tolerates their
/// divergence as collateral, so they never enter a track comparison.
fn voice_tracks(frames: &[Vec<(u8, u8)>], n: usize) -> [Vec<Vec<(u8, u8)>>; 3] {
    let mut tracks: [Vec<Vec<(u8, u8)>>; 3] = std::array::from_fn(|_| Vec::with_capacity(n));
    for frame in &frames[..n] {
        let mut per: [Vec<(u8, u8)>; 3] = std::array::from_fn(|_| Vec::new());
        for &(reg, value) in frame {
            if let Some((_, Some(voice))) = Category::of(reg) {
                per[usize::from(voice)].push((reg, value));
            }
        }
        for (t, w) in per.into_iter().enumerate() {
            tracks[t].push(w);
        }
    }
    tracks
}

/// Timing verdict for a mutation that shifts some voices' write streams in
/// time while leaving the others byte-identical. Every differing voice must
/// shift by the same amount; a diverging global track (filter/volume) is
/// tolerated as collateral — command drains move with the row they ride on —
/// but at least one voice must carry a clean shift.
fn per_voice_shift(
    baseline: &[Vec<(u8, u8)>],
    variant: &[Vec<(u8, u8)>],
    n: usize,
) -> Option<Verdict> {
    let base_tracks = voice_tracks(baseline, n);
    let var_tracks = voice_tracks(variant, n);
    let mut voices = 0u8;
    let mut common_shift: Option<i32> = None;
    let mut first_frame = u32::MAX;
    for t in 0..3 {
        let first = (0..n).find(|&i| base_tracks[t][i] != var_tracks[t][i]);
        let Some(first) = first else { continue };
        let shift = detect_shift(&base_tracks[t], &var_tracks[t], first)
            .or_else(|| content_slip(&base_tracks[t], &var_tracks[t]))
            .or_else(|| gate_slip(&base_tracks[t], &var_tracks[t]))?;
        if common_shift.is_some_and(|s| s != shift) {
            return None;
        }
        common_shift = Some(shift);
        voices |= 1 << t;
        first_frame = first_frame.min(first as u32);
    }
    Some(Verdict::Timing {
        shift: common_shift?,
        voices,
        first_frame,
    })
}

/// Accumulating time-shift: a duration byte inside a looped pattern is
/// re-read every pass, so the lag grows each loop and no constant offset
/// matches. The track still plays the *identical write sequence*, only at
/// drifting frame positions — compare the non-empty frame contents in order
/// and report the final slip.
fn content_slip(base: &[Vec<(u8, u8)>], var: &[Vec<(u8, u8)>]) -> Option<i32> {
    fn keyed(track: &[Vec<(u8, u8)>]) -> Vec<(&Vec<(u8, u8)>, usize)> {
        track
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.is_empty())
            .map(|(i, w)| (w, i))
            .collect()
    }
    pair_slip(&keyed(base), &keyed(var), usize::MAX)
}

/// Last-resort timing detector: free-running effects (a PWM counter that
/// ignores row position) change *values* when a note shifts onto a different
/// phase, and dense every-frame writes defeat the empty-frame slip logic.
/// The gate (ctrl register) *transitions* are row-driven and value-stable,
/// so a duration byte shows as the identical sequence of ctrl value changes
/// at drifted frame positions. The length tolerance absorbs the 1-2
/// transitions that drift across the probed window's end.
fn gate_slip(base: &[Vec<(u8, u8)>], var: &[Vec<(u8, u8)>]) -> Option<i32> {
    let transitions = |track: &[Vec<(u8, u8)>]| -> Vec<(u8, usize)> {
        let mut out = Vec::new();
        let mut cur: Option<u8> = None;
        for (i, frame) in track.iter().enumerate() {
            for &(reg, v) in frame {
                if matches!(Category::of(reg), Some((Category::Ctrl, _))) && cur != Some(v) {
                    out.push((v, i));
                    cur = Some(v);
                }
            }
        }
        out
    };
    pair_slip(&transitions(base), &transitions(var), 2)
}

/// Walk two `(key, frame)` sequences that must agree key-for-key; the slip is
/// the final position offset. `max_len_diff` bounds the unpaired tail.
fn pair_slip<K: PartialEq>(
    base: &[(K, usize)],
    var: &[(K, usize)],
    max_len_diff: usize,
) -> Option<i32> {
    let len = base.len().min(var.len());
    if len < MIN_SHIFT_MATCH || base.len().abs_diff(var.len()) > max_len_diff {
        return None;
    }
    let mut slip = 0i32;
    for i in 0..len {
        if base[i].0 != var[i].0 {
            return None;
        }
        slip = var[i].1 as i32 - base[i].1 as i32;
    }
    (slip != 0).then_some(slip)
}

/// A pure time-shift: the variant's tail equals the baseline's tail offset
/// by `k` frames. Positive shift = the variant runs late (a lengthened
/// duration); negative = early (shortened).
fn detect_shift(
    baseline: &[Vec<(u8, u8)>],
    variant: &[Vec<(u8, u8)>],
    first: usize,
) -> Option<i32> {
    for k in 1..=MAX_SHIFT {
        if tail_matches(variant, first + k, baseline, first) {
            return Some(i32::try_from(k).unwrap_or(i32::MAX));
        }
        if tail_matches(baseline, first + k, variant, first) {
            return Some(-i32::try_from(k).unwrap_or(i32::MAX));
        }
    }
    None
}

/// Does `a[a_from..]` equal `b[b_from..]` over their overlap (requiring a
/// meaningful overlap)?
fn tail_matches(a: &[Vec<(u8, u8)>], a_from: usize, b: &[Vec<(u8, u8)>], b_from: usize) -> bool {
    let len = a
        .len()
        .saturating_sub(a_from)
        .min(b.len().saturating_sub(b_from));
    if len < MIN_SHIFT_MATCH {
        return false;
    }
    (0..len).all(|i| a[a_from + i] == b[b_from + i])
}

#[cfg(all(test, feature = "asset-tests"))]
mod tests {
    use super::*;

    fn probe_cobra(addrs: &[u16]) -> Vec<ProbeResult> {
        let bytes = std::fs::read("../../assets/music/Cobra.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut emu = Emulator::new();
        emu.load(&header, &bytes).unwrap();
        emu.call_init(header.init_address, header.start_song, header.songs)
            .unwrap();
        let image = emu.ram_image();
        let targets: Vec<ProbeTarget> = addrs
            .iter()
            .map(|&addr| ProbeTarget {
                addr,
                orig: image[usize::from(addr)],
            })
            .collect();
        probe(&header, &bytes, header.start_song, 1500, &targets).unwrap()
    }

    /// Ground-truth check against the hand-RE'd Crowther Table generation
    /// (Cobra): the probe classifies sequence bytes by *causally measured*
    /// role — the semantics half taint's data-flow could never attribute.
    /// $F118 is the first v1 row's note byte, $F119 its duration, $F315 the
    /// v2 initial 16-bit duration's low byte, $F10B an instrument envelope
    /// field, $F104 the volume command's operand.
    #[test]
    fn cobra_byte_roles_match_hand_re() {
        let r = probe_cobra(&[0xF118, 0xF119, 0xF315, 0xF10B, 0xF104]);
        let verdict = |addr: u16| &r.iter().find(|p| p.addr == addr).expect("probed").verdict;
        assert!(
            matches!(
                verdict(0xF118),
                Verdict::Param { categories, voices, .. }
                    if categories.contains(&Category::Freq) && *voices == 0b001
            ),
            "note byte -> v1 pitch, got {:?}",
            verdict(0xF118)
        );
        assert!(
            matches!(verdict(0xF119), Verdict::Timing { voices: 0b001, .. }),
            "row duration -> v1 timing, got {:?}",
            verdict(0xF119)
        );
        assert!(
            matches!(
                verdict(0xF315),
                Verdict::Timing {
                    voices: 0b010,
                    shift: 1,
                    ..
                }
            ),
            "v2 initial duration low byte -> +1 frame, got {:?}",
            verdict(0xF315)
        );
        assert!(
            matches!(
                verdict(0xF10B),
                Verdict::Param { categories, voices, .. }
                    if categories.contains(&Category::Envelope) && *voices == 0b001
            ),
            "instrument AD field -> v1 envelope, got {:?}",
            verdict(0xF10B)
        );
        assert!(
            matches!(
                verdict(0xF104),
                Verdict::Param { categories, voices: 0, .. }
                    if categories.contains(&Category::Volume)
            ),
            "volume operand -> global volume, got {:?}",
            verdict(0xF104)
        );
    }
}
