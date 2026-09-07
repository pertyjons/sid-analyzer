use crate::trace::ChipCycle;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SourceSpan {
    pub start: ChipCycle,
    pub end: ChipCycle,
}

impl SourceSpan {
    #[must_use]
    pub fn new(start: ChipCycle, end: ChipCycle) -> Option<Self> {
        (start <= end).then_some(Self { start, end })
    }

    #[must_use]
    pub fn contains(self, cycle: ChipCycle) -> bool {
        self.start <= cycle && cycle < self.end
    }
}
