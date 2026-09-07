# Hubbard native export implementation status

This document records the implementation of
`hubbard-native-export-implementation-plan.md` on 2026-07-19. Qualification
snapshots are generated artifacts and are intentionally not checked in; use
`sid-hubbard-qualify` to reproduce one against a local corpus.

## Delivered pipeline

The native CLI path now resolves one `PlaybackTiming` value and uses it for the
init environment, play scheduler, trace analysis, native SID-frequency
conversion, effect rates, and project time. PAL and NTSC vblank rates are
rational rather than nominal 50/60 values. A zero-call time export is invalid,
and CIA-timed native validation is rejected while its timer period is unknown.
Native dispatch happens before the generic trace export path, so there is no
discarded full analysis pass.

Native notes are validated independently per physical SID voice with a
monotonic one-to-one dynamic-programming alignment. The report contains native,
truth, match, insertion, and deletion counts; precision and recall; and
median/p95/max onset, pitch, and duration residuals. A trace note cannot validate
more than one native note. Hubbard row-clock phase is fitted once within ±8
calls and the selected offset is serialized.

Hubbard decode uses typed pattern, instrument, frequency, order-offset,
transpose, repeat, row-tick, and pattern-offset values. One pattern-instance
walker drives note decoding and render placements. Repeat-count orderlists,
separate and embedded transpose, holds/rests, one/two-byte effects, five/six-bit
durations, no-divider timing, prescale timing, and whole-play stalls have focused
tests. Pattern/order safety bounds return typed errors instead of partial data.

The locator generates candidates around a pattern-pointer anchor. Its pattern
fetch, duration mask, sequence pointers, frequency reads/SID writes, timing,
format, and instrument evidence must come from the same bounded relocated
routine region. Candidate instruction reads cannot wrap at `$FFFF`; equal best
candidates produce `LocateAmbiguous`. The selected evidence also drives
`sid-re` labels.

The native census retains exact recovered order commands, loop command,
patterns/events, resolved instances, and source offsets separately from
render-oriented placements. `NativeSong` construction validates all parallel
note arrays and verifies that source instances round-trip into render placements.
ADSR, waveform, pulse-width, vibrato, PWM, and effect flags carry field-level
provenance. ADSR/waveform evidence is compared at aligned onsets. The unproven
Hubbard vibrato-rate nibble remains `authored_partial`; instrument-field evidence
is report-only rather than an additional acceptance threshold.

Split render tracks now obey physical SID voice ownership. A following note on
the same hardware voice caps the older plan's release note and measured
amplifier lane. Census distinguishes source release overlap from rendered
overlap. The checked 1500-call start-song windows have zero rendered overlap;
Knucklebusters, Sigma Seven, and Warhawk respectively choke 1/75, 23/23, and
14/3192 source overlap pairs/calls.

`synth-native` is strict: it never silently falls back to `synth`. Project and
sidecar writes use a named sibling temporary file, flush, sync, and atomic
rename. Failed writes preserve prior artifacts. Former environment switches are
explicit hidden unstable CLI flags and their values are serialized in census.

## Qualification

`sid-hubbard-qualify` recursively scans an asset or HVSC root, filters SIDId
`Rob_Hubbard` matches, runs either start songs or every subtune, and emits sorted
JSON plus an aggregate human summary. Accepted outcomes include complete native
validation, field provenance, exact structure, and deterministic project/census
MD5 hashes.

The checked `assets/music` run uses 1500 calls and the default policy:

- minimum precision and recall: 0.75 each;
- maximum matched-pair onset residual: 8 calls;
- maximum pitch residual: 75 cents;
- maximum duration residual: 64 calls.

All-subtune result: 14 accepted and 106 rejected over 120 outcomes. All 13 AWM
subtunes are recorded: subtune 1 is accepted at 1.0 precision/recall; subtunes
2–13 are rejected for concrete insertion-heavy alignment reports. Eight of
eleven start songs are accepted. Human Race is excluded for unknown CIA period,
Last V8 reaches the emulator cycle limit at play `$0000`, and Nemesis is rejected
at 12.7% precision and 17.4% recall.

## Review finding closure

| Finding | Resolution |
|---|---|
| H1, M1, M3, M8, M10 | One rational timing object, early dispatch, CIA refusal, nonzero export window |
| H2, M6, L3 | Bidirectional one-to-one alignment and adversarial/unit/integration coverage |
| H3, H4, M5, L1 | Typed IR, one walker, repeat parity, zero reload, typed truncation failures |
| M4 | Coherent bounded locator candidates, evidence, ambiguity failure |
| M7 | Exact recovered structure separated from render placements and serialized |
| M9 | Vibrato rate explicitly partial; no authored-verified claim |
| M2, M11, L2 | Strict CLI, atomic artifacts, explicit recorded debug switches |
| M12 | Source overlap diagnosed; physical-voice release ownership enforced and counted |
| L4 | CLI, driver/export docs, PLAN coverage, and failure semantics updated |

CIA timer-period recovery remains a stated non-goal of this implementation. A
full HVSC corpus was not present in this worktree, so the command supports such a
root but the checked snapshot covers the repository fixtures only.
