use crate::analysis::FrameState;
use crate::trace::{CpuCycles, SidRegister};
use serde::Serialize;

const OSC3: SidRegister = SidRegister(0x1B);
const CAUSAL_WINDOW: CpuCycles = CpuCycles(96);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[must_use]
pub enum Osc3Target {
    Pitch,
    PulseWidth,
    FilterCutoff,
    Volume,
    RandomOnly,
    Multiple,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct Osc3Attribution {
    pub target: Osc3Target,
    pub reads: u32,
    pub evidence: u32,
    pub confidence_percent: u8,
}

impl Default for Osc3Attribution {
    fn default() -> Self {
        Self {
            target: Osc3Target::Unknown,
            reads: 0,
            evidence: 0,
            confidence_percent: 0,
        }
    }
}

pub fn attribute_osc3(states: &[FrameState]) -> Osc3Attribution {
    let mut reads = 0u32;
    let mut votes = [0u32; 4];
    for state in states {
        for read in state.register_reads.iter().filter(|read| read.reg == OSC3) {
            reads += 1;
            let mut matched = [false; 4];
            for write in state.register_writes.iter().filter(|write| {
                write.offset.0 >= read.offset.0
                    && u64::from(write.offset.0 - read.offset.0) <= CAUSAL_WINDOW.0
            }) {
                if let Some(target) = target_index(write.reg) {
                    matched[target] = true;
                }
            }
            for (target, did_match) in matched.into_iter().enumerate() {
                if did_match {
                    votes[target] += 1;
                }
            }
        }
    }
    if reads == 0 {
        return Osc3Attribution {
            target: Osc3Target::Unknown,
            reads,
            evidence: 0,
            confidence_percent: 0,
        };
    }
    let evidence = votes.iter().sum();
    if evidence == 0 {
        return Osc3Attribution {
            target: if reads >= 3 {
                Osc3Target::RandomOnly
            } else {
                Osc3Target::Unknown
            },
            reads,
            evidence,
            confidence_percent: if reads >= 3 { 100 } else { 0 },
        };
    }
    let mut ranked: Vec<(usize, u32)> = votes.into_iter().enumerate().collect();
    ranked.sort_unstable_by_key(|&(_, count)| std::cmp::Reverse(count));
    let (best, best_count) = ranked[0];
    let second_count = ranked[1].1;
    let coverage_percent = ((best_count * 100) / reads.max(1)).min(100) as u8;
    let dominance_percent = ((best_count * 100) / evidence.max(1)).min(100) as u8;
    let confidence_percent = coverage_percent.min(dominance_percent);
    let target = if best_count < 3 {
        Osc3Target::Unknown
    } else if second_count.saturating_mul(4) >= best_count || confidence_percent < 70 {
        Osc3Target::Multiple
    } else {
        [
            Osc3Target::Pitch,
            Osc3Target::PulseWidth,
            Osc3Target::FilterCutoff,
            Osc3Target::Volume,
        ][best]
    };
    Osc3Attribution {
        target,
        reads,
        evidence,
        confidence_percent,
    }
}

fn target_index(register: SidRegister) -> Option<usize> {
    match register.0 {
        0x00 | 0x01 | 0x07 | 0x08 | 0x0E | 0x0F => Some(0),
        0x02 | 0x03 | 0x09 | 0x0A | 0x10 | 0x11 => Some(1),
        0x15 | 0x16 => Some(2),
        0x18 => Some(3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{RegisterRead, RegisterWrite, SubFrameOffset};

    fn state(read_offset: u32, writes: &[(u8, u32)]) -> FrameState {
        FrameState {
            register_reads: vec![RegisterRead {
                reg: OSC3,
                value: 0x55,
                offset: SubFrameOffset(read_offset),
            }],
            register_writes: writes
                .iter()
                .map(|&(reg, offset)| RegisterWrite {
                    reg: SidRegister(reg),
                    value: 0,
                    offset: SubFrameOffset(offset),
                })
                .collect(),
            ..FrameState::default()
        }
    }

    #[test]
    fn classifies_each_supported_target() {
        for (register, target) in [
            (0x00, Osc3Target::Pitch),
            (0x02, Osc3Target::PulseWidth),
            (0x15, Osc3Target::FilterCutoff),
            (0x18, Osc3Target::Volume),
        ] {
            assert_eq!(
                attribute_osc3(&[
                    state(10, &[(register, 30)]),
                    state(10, &[(register, 30)]),
                    state(10, &[(register, 30)]),
                ])
                .target,
                target
            );
        }
    }

    #[test]
    fn distinguishes_rng_multiple_and_out_of_window_writes() {
        assert_eq!(
            attribute_osc3(&[state(10, &[])]).target,
            Osc3Target::Unknown
        );
        assert_eq!(
            attribute_osc3(&[state(10, &[]), state(10, &[]), state(10, &[])]).target,
            Osc3Target::RandomOnly
        );
        assert_eq!(
            attribute_osc3(&[
                state(10, &[(0x00, 20), (0x02, 30)]),
                state(10, &[(0x00, 20), (0x02, 30)]),
                state(10, &[(0x00, 20), (0x02, 30)]),
            ])
            .target,
            Osc3Target::Multiple
        );
        assert_eq!(
            attribute_osc3(&[state(10, &[(0x00, 200)])]).target,
            Osc3Target::Unknown
        );
    }
}
