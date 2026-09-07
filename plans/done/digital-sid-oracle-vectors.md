# Digital SID oracle vectors — implementation plan

> **Status:** completed 2026-08-05.
> This is the completed execution plan for the reSID/reSIDfp oracle-vector
> work. Current roadmap ordering lives in [`PLAN.md`](../PLAN.md).
>
> **Planning baseline:** sid-analyzer `96caa2c`, 2026-08-05. The primary
> external reference is `libresidfp` v1.1.2, tag commit
> `a5cd8f2486d627c40ea8c7c7a25827db73837002`. Regeneration must additionally
> pin and verify the downloaded source archive by SHA-256 before any vector is
> accepted.

## Goal

Add an independent, reproducible validation layer for
`emu::sid::DigitalSid`. A pinned external SID implementation consumes ordered,
cycle-stamped register operations and produces committed golden observations.
The normal Rust test suite replays the same operations through `DigitalSid` and
compares the digital state and readable SID outputs at every observation.

The work is complete when envelope, oscillator, sync, TEST, and noise behavior
has external oracle coverage, every comparable integer field is checked
exactly, every intentional difference is explicit and narrowly scoped, and CI
does not need a C++ compiler, network access, or the reference implementation.

The central contract is:

```text
named event case
    |
    +-- pinned libresidfp generator --> immutable oracle JSON
    |
    +-- DigitalSid replay -----------> exact field comparison
```

This validates the digital state machine. It is not an audio-render comparison.

## Completion evidence

The implementation landed on `feat/digital-sid-oracle-vectors` with a standalone
GPL-2.0-or-later generator under `tools/digital-sid-oracle` and an offline Rust
replay harness. The source lock pins libresidfp v1.1.2, tag commit
`a5cd8f2486d627c40ea8c7c7a25827db73837002`, release archive SHA-256
`a753d61fb0ae554a0f9224363ea57ed0c43741169edb98ec32c53b96d0412719`,
GCC 16.1.1, `-O2` for the library, and `-std=c++23 -O2` for the generator.
`regenerate.sh --check` sanitizes inherited build variables, verifies the
archive, builds the pinned source, generates twice, proves byte identity,
validates the temporary fixture set through Rust, and compares it with the
committed files.

The completed fixture set contains 37 generated cases plus the handwritten
timeline smoke case, 178 observations, and 773 classified observation fields.
It covers all 16 attack rates, exponential thresholds, hold-zero and ADSR-delay
boundaries, direct Attack-to-Release interruption, both same-cycle ADSR/gate
orders, accumulator and waveform paths, all three hard-sync routes,
simultaneous sync edges, sync/noise ordering, the actual model-specific TEST
fill boundaries, pure and combined noise, and a named OSC3/ENV3 read-integration
window.

The follow-up conformance pass resolved all four original divergence classes:
power-on state, envelope/ENV3 pipelines, oscillator/OSC3 pipelines, and the
noise/TEST/LFSR pipeline. The manifest now classifies every one of the 773
observation fields as exact `must_match`; it contains zero known divergences and
zero not-comparable fields. Negative harness tests retain the policy checks for
future narrowly scoped exceptions, including rejection of broad, stale, or
disappeared divergence selectors.

`DigitalSid` now uses the same right-shifting 23-bit LFSR representation as the
oracle and exposes its two-phase shift pipeline in checkpoints. The sync/noise
ordering case additionally normalizes the state to a cumulative clock count;
the count changes from one to two on the joint bit-19/sync edge and remains an
exact `must_match` field after the accumulator reset.

A 1500-frame census found zero `$D41B` and zero `$D41C` reads in every one of
the 23 analysable PSID assets; the two remaining assets are out-of-scope RSID.
The captured-read oracle therefore uses the named minimal live-read PSID in
`captured_read_fixture_matches_live_read_psid_capture`. A regression test proves
that every oracle write/read operation has the same order, register, value, and
absolute cycle as the emulator's captured bus stream. The final
`sid-corpus-baseline --full` run exported all 23 PSIDs in trace mode, produced
21 structured and one decoded native result with one typed timing rejection,
validated 45 projects with zero schema failures, and left `exports/baseline`
byte-identical. The conformance follow-up changed live `DigitalSid` behavior
and repeated the same full gate in 268.4 seconds; the corpus artifact again
remained byte-identical. A final run after the shift-latch writeback review
completed in 154.2 seconds with the same byte-identical result.

## Why this is the next correctness layer

The current validation pyramid already has hand-derived unit tests, a slow
per-cycle envelope reference, randomized jump-clock comparisons, capture
checkpoint replay, and named integration fixtures. Those tests can prove
internal consistency but cannot prove that the shared implementation and its
local reference agree with SID behavior.

The missing external layer was recorded as finding F14 in
[`chip-state-emulation-code-review.md`](chip-state-emulation-code-review.md).
It would have exposed earlier errors in decay-to-zero handling, rate-counter
wrap distance, noise clocking relative to hard sync, and TEST semantics without
requiring a corpus symptom first.

`DigitalSid` now has the necessary seam:

- absolute `ChipCycle` time;
- ordered `write`, `read`, and `clock_to` operations;
- complete serializable envelope and oscillator checkpoints;
- the selected `SidModel`;
- capture events with stable sequence IDs for same-cycle ordering.

The oracle must exercise that seam directly. Frame-level state or WAV output is
too coarse: transient states may converge before the frame boundary, and audio
introduces unrelated filter, DAC, calibration, and resampling differences.

## Scope

### Included

- envelope level, phase, rate counter, rate period, exponential counter,
  exponential period, gate, and hold-zero behavior;
- oscillator accumulator advancement and selected simple-waveform `OSC3`
  output;
- all three hard-sync routes, simultaneous-edge suppression, and ring-mod
  observability;
- noise shift-register seed, feedback, bit-19 clocking, output taps, and
  sync/noise ordering;
- TEST set, held, rewritten, and cleared behavior for MOS 6581 and MOS 8580;
- combined-waveform and noise-writeback observations where reSIDfp supplies a
  reference, including an explicit policy for behavior `DigitalSid` does not
  model;
- exact event-order and observation-boundary semantics;
- pinned, deterministic vector regeneration and offline CI consumption;
- a small set of in-situ captured `$D41B`/`$D41C` reads after the synthetic
  state-machine matrix passes.

### Excluded

- analog filter output, waveform-DAC calibration, external filter response,
  PCM, and spectral comparison;
- replacing `sid-abtest` or the render-based representation gate;
- claiming that one emulator revision is physical truth for every SID die;
- making libresidfp, classic reSID, or another GPL implementation a runtime or
  normal workspace dependency;
- running a network download or C++ build during normal CI;
- weakening a digital mismatch with a numeric audio tolerance;
- broad changes to `DigitalSid` before a vector demonstrates the mismatch.

## Reference selection and trust model

### Primary reference

Use the released `libresidfp` v1.1.2 source as the primary oracle. Record all of
the following in a machine-readable lock file and copy them into every generated
fixture set:

- repository URL;
- release/tag name;
- resolved commit;
- source archive URL;
- source archive SHA-256;
- generator source revision;
- compiler identity and relevant build flags;
- chip model and combined-waveform strength;
- vector schema version.

The implementation slice must confirm the exact released API and source layout
before locking the archive hash. A moving branch such as `main` is never an
accepted fixture source.

### Secondary references

Classic reSID or sidera may be used to investigate a disagreement, but they do
not vote a mismatch away. A vector can have more than one named oracle result;
the comparison policy must still select which behavior `DigitalSid` claims and
why. Reference disagreement becomes evidence attached to the vector, not an
averaged answer.

### Observable and internal reference data

Use two fixture classes:

1. **Full-chip observable vectors** use the public reSIDfp interface to clock
   digital cycles, write registers, and read `OSC3`/`ENV3`. These are the
   strongest compatibility contract because they avoid access to private
   implementation details.
2. **Component-state vectors** expose accumulator, shift register, and envelope
   counters only inside the standalone C++ generator, following libresidfp's
   own unit-test technique. These diagnose the first divergent field and
   cycle; they never create a Rust FFI surface.

When public output and private state disagree about the apparent failing cycle,
the public full-chip result owns compatibility and the internal observation is
diagnostic evidence.

## Repository layout

Add the following files:

```text
tools/digital-sid-oracle/
  README.md
  source.lock
  generate.cpp
  regenerate.sh

crates/analyzer/tests/common/oracle.rs
crates/analyzer/tests/digital_sid_oracle.rs
crates/analyzer/tests/fixtures/digital_sid_oracle/v1/
  manifest.json
  envelope.oracle.json
  oscillator.oracle.json
  sync.oracle.json
  test_noise_6581.oracle.json
  test_noise_8580.oracle.json
  captured_reads.oracle.json
```

`tools/digital-sid-oracle` is not a Cargo workspace member. Its only product
consumed by the analyzer is deterministic JSON. `regenerate.sh` downloads or
uses an explicitly supplied source archive, verifies it against `source.lock`,
builds in a temporary directory, runs the generator, and refuses to overwrite
fixtures when verification fails.

The generator must never write into `target/`, depend on a user's installed
`sidplayfp`, or silently fall back to another library version.

## Vector data model

### Immutable oracle document

The generated document contains only case input, oracle output, and generator
provenance. It must not contain a judgment based on current `DigitalSid`
behavior.

Conceptual Rust shape, using existing domain newtypes where available:

```rust
struct SidOracleDocument {
    schema_version: OracleSchemaVersion,
    source: OracleSource,
    cases: Vec<SidOracleCase>,
}

struct SidOracleCase {
    id: OracleCaseId,
    description: String,
    sid_model: SidModel,
    operations: Vec<OracleOperation>,
    observations: Vec<OracleObservation>,
}

enum OracleOperation {
    Write {
        sequence: OracleSequence,
        cycle: ChipCycle,
        register: SidRegister,
        value: SidRegisterValue,
    },
    Observe {
        sequence: OracleSequence,
        cycle: ChipCycle,
        observation: OracleObservationId,
    },
}
```

Raw integers are acceptable only at the JSON serialization boundary. The Rust
loader wraps cycles, register addresses, IDs, sequence numbers, envelope
levels, and counters in existing or dedicated newtypes before replay.

Each observation may carry:

- `env3` and `osc3` public read values;
- per-voice accumulator and shift-register state;
- envelope state, level, rate counter/period, exponential counter/period, gate,
  and hold-zero state;
- selected control and frequency registers needed to make a diff self-contained;
- model-specific TEST countdown or pipeline state when the generator exposes it;
- a stable observation sequence so several operations at one cycle are
  unambiguous.

Arrays are always in physical voice order 1, 2, 3. Case IDs and document order
are deterministic and lexically sorted.

### Separate comparison-policy manifest

`manifest.json` is handwritten and never emitted by the oracle generator. For
every generated observation field it declares one of:

- `must_match` — exact equality is required;
- `known_divergence` — includes a stable issue ID, narrow field/span selector,
  reason, selected project policy, and planned resolution;
- `not_comparable` — includes why the field is not available or not normalized
  between implementations.

The test fails when:

- a generated field has no policy;
- a `must_match` field differs;
- a `known_divergence` stops matching its narrow selector;
- an expected divergence disappears without the manifest being reviewed;
- a policy wildcard covers a whole case or subsystem where a field-level rule
  is possible.

The raw golden value is never edited to equal `DigitalSid`. This separation
keeps reference evidence immutable while allowing the project to state an
honest support boundary.

## Clock and event-order contract

All vectors use absolute chip cycles with `ChipCycle(0)` at oracle reset. The
runner maintains a current cycle and processes operations in `(cycle,
sequence)` order.

For an operation at cycle `t`:

1. advance the reference or `DigitalSid` from the current cycle to `t`;
2. apply the operation;
3. take an observation only after every earlier same-cycle sequence item;
4. retain the resulting cycle as the next operation's origin.

Writes at cycle `t` therefore affect state after clocking to `t`, matching
`DigitalSid::write`. A vector must contain explicit before-write and
after-write observations when that distinction matters. The loader rejects
backwards cycles, duplicate sequence IDs, an observation without an expected
record, and registers outside the modeled SID range.

The first slice must include a fixture that distinguishes clock-then-write from
write-then-clock. Without it, all later off-by-one results are ambiguous.

## Required fixture matrix

### A. Envelope

1. Power-on/reset state and `ENV3` visibility.
2. All 16 attack rate periods, sampled one cycle before, at, and one cycle after
   the first step.
3. `0xFE -> 0xFF` attack transition and entry into decay/sustain.
4. Every exponential-divider threshold: `FF`, `5D`, `36`, `1A`, `0E`, `06`,
   and `00`.
5. Gate off during attack and release to zero.
6. Gate on during release, restarting attack from the current level.
7. Decay to sustain without seeking upward after sustain is raised.
8. Decay/sustain reaching zero, sustain then raised while gate remains high.
9. Rate-period rewrite below the current counter, pinning ADSR-delay-bug wrap
   distance.
10. AD/SR/control writes immediately before, at, and after a rate match.
11. Same-cycle AD, SR, and gate writes in both relevant orders.
12. Hold-zero unlock and retrigger behavior.

### B. Oscillator and simple waveform output

1. Reset accumulator and first increment.
2. Several frequencies, including zero, one, high values, and 24-bit wrap.
3. Frequency-low and frequency-high writes at exact cycle boundaries.
4. Saw `OSC3` trajectory.
5. Triangle trajectory on both halves of the accumulator.
6. Pulse output below, at, and above pulse width, including TEST forcing high.
7. Ring-mod triangle with source MSB clear and set.
8. Waveform zero and every single selected waveform.

Combined-waveform output is not added to the simple-waveform equality budget.
It receives separate observations and explicit policy because the current core
does not contain reSIDfp's per-model waveform tables.

### C. Hard sync

1. Voice 1 reset by voice 3.
2. Voice 2 reset by voice 1.
3. Voice 3 reset by voice 2.
4. Source MSB rise one cycle before, at, and one cycle after a destination
   accumulator event.
5. Simultaneous source edges and reSID's source-reset suppression case.
6. Sync disabled at the edge and enabled immediately before the edge.
7. Destination noise bit-19 rise on the same natural increment as a sync reset;
   the LFSR must clock before the accumulator reset is applied.

### D. Noise and TEST

1. Reference reset seed and first eight LFSR transitions.
2. Feedback inputs for all combinations of source bits 22 and 17.
3. `OSC3` output-bit mapping for taps 22, 20, 16, 13, 11, 7, 4, and 2.
4. Multiple bit-19 crossings during a large clock delta.
5. TEST set: accumulator reset and immediate observable state.
6. TEST held shorter than the reset/fill interval.
7. TEST held through the 6581 reset/fill interval.
8. TEST held through the 8580 reset/fill interval.
9. Rewriting control with TEST still set, including countdown restart behavior.
10. TEST falling edge and forced-feedback LFSR transition.
11. Pure noise after TEST recovery.
12. Noise plus triangle, saw, or pulse long enough to trigger destructive
    writeback in the oracle.
13. Return from a combined-noise waveform to pure noise.

Power-on accumulator and shift-register values are deliberately observed rather
than normalized away. If classic reSID, current reSIDfp, and `DigitalSid`
disagree, the manifest records the exact disagreement and the implementation
slice decides whether to change reset policy or retain a named compatibility
choice.

### E. Captured reads

After the synthetic matrix passes, add a small immutable capture derived from a
named test SID or asset window that performs `$D41B` and `$D41C` reads. Replay
the captured ordered bus operations through both systems and compare every
captured read at its absolute cycle.

This is a host-integration check, not a replacement for synthetic isolation.
Do not use a full song or a fixture whose behavior depends on analog output.

## Rust comparison harness

Add a test-only loader under `crates/analyzer/tests/common/oracle.rs`. It:

1. deserializes with `deny_unknown_fields` on every schema type;
2. validates source metadata and the vector schema version;
3. validates deterministic ordering and operation references;
4. builds `DigitalSid::with_model` for the declared model;
5. replays writes and observations using absolute `ChipCycle` values;
6. obtains public reads and `DigitalSidCheckpoint` state;
7. normalizes only representation differences declared by the schema, never
   behavior differences;
8. applies the separate field-level policy;
9. reports the case ID, observation ID, cycle, field path, expected value,
   actual value, and preceding operation on failure.

Prefer one parameterized integration test per fixture family so a failure names
the affected domain. Add a compact diff type rather than comparing serialized
JSON strings. Tests may unwrap; library and CLI code remain free of unwraps.

The harness must also include negative tests for unknown schema fields, missing
policy coverage, duplicate sequence IDs, backwards time, and an overly broad
known-divergence selector.

## Standalone generator

The C++ generator is a development tool, not shipped analyzer code. It must:

- build only against the source identified by `source.lock`;
- use reSIDfp's digital clocking path, avoiding audio resampling;
- emit public `OSC3`/`ENV3` observations from the full-chip API;
- emit diagnostic internal fields through an oracle-only adapter patterned on
  upstream tests;
- define each named case once and emit both its operations and observations;
- use integer formatting, fixed field order, and no locale-dependent output;
- sort cases and observations deterministically;
- write to a temporary output tree, validate it, and replace the committed
  fixture tree only after every case succeeds;
- support `--check`, which regenerates to a temporary tree and byte-compares it
  with committed fixtures;
- print the complete source and generator identity in its report.

`regenerate.sh` must use a temporary directory and explicit paths. It must not
delete or overwrite a broad directory, trust an unverified download, or modify
fixtures after a failed generation.

Document the exact regeneration command in both the tool README and the
fixture manifest. The normal repository verification commands do not invoke
regeneration.

## Handling mismatches

Every first-run mismatch follows this sequence:

1. Minimize it to the shortest event stream and earliest divergent observation.
2. Confirm event ordering and model selection.
3. Compare the public full-chip read where the state is externally observable.
4. Check the pinned oracle source and at least one secondary reference or
   published hardware result when the references differ.
5. Classify it as a `DigitalSid` bug, timeline-stamping limitation,
   model-specific behavior, intentionally unsupported behavior, or oracle
   adapter error.
6. Fix the bug or add the narrow policy record plus a roadmap follow-up.
7. Add a direct Rust regression beside the oracle vector when the mismatch
   changes implementation behavior.
8. Run named real-tune checks when `$D41B`/`$D41C` reads or exported state can
   change.

Do not update the raw oracle document as part of accepting a `DigitalSid`
result. Do not use “both sound plausible” as a classification.

Known areas expected to require an explicit decision include:

- power-on accumulator and LFSR values;
- reSIDfp's envelope and waveform pipelines versus instruction-start SID write
  timestamps;
- model-dependent TEST reset/fill timing;
- combined-waveform oscillator output and destructive noise writeback;
- waveform-zero floating DAC retention, which is currently outside
  `DigitalSid` scope.

## Implementation slices

Each slice lands independently with the normal repository gate green.

### Slice 0 — freeze contracts and baseline

1. Record the current `DigitalSid` field inventory and existing relevant tests.
2. Add the vector schema, policy schema, typed test loader, and schema-negative
   tests without oracle data.
3. Add one hand-authored clock/write-order smoke vector solely to validate the
   harness.
4. Record current asset-corpus `$D41B`/`$D41C` read counts and full digital-state
   digests for the named control tunes.

Exit gate: the harness rejects malformed or incompletely classified vectors;
normal analysis and exports are unchanged.

### Slice 1 — reproducible oracle generator

1. Add `source.lock` with the release, resolved commit, archive URL, and verified
   archive hash.
2. Add the standalone generator and safe regeneration wrapper.
3. Generate the reset and event-order cases twice in clean temporary
   directories and prove byte-identical output.
4. Add `--check` and document regeneration.

Exit gate: a developer can regenerate the seed fixtures from a verified source;
CI consumes them without downloading or compiling the oracle.

### Slice 2 — envelope vectors

1. Implement the complete envelope fixture matrix.
2. Compare public `ENV3` and every exposed internal envelope field.
3. Minimize and classify all mismatches.
4. Add direct Rust regressions for each implementation correction.

Named checks: Nemesis subtune 1 and Auf Wiedersehen Monty retain their existing
delay-bug and envelope-state behavior unless a reviewed oracle mismatch proves
the current result wrong.

Exit gate: all comparable envelope fields match exactly or have narrow,
reviewed divergence records; all 16 rates and every exponential threshold are
externally covered.

### Slice 3 — oscillator and sync vectors

1. Add accumulator and simple-waveform vectors.
2. Add all three sync routes and simultaneous-edge cases.
3. Add ring-mod observability cases.
4. Add the sync/noise same-cycle ordering case before broad noise coverage.

Exit gate: accumulator, simple `OSC3`, sync reset timing, and natural-increment
noise ordering match the oracle for every claimed model.

### Slice 4 — noise and TEST vectors

1. Add LFSR transition and output-tap vectors.
2. Add model-specific TEST set/hold/rewrite/fall cases.
3. Add combined-noise writeback cases and field-level support policy.
4. Verify that poisoned or unsupported LFSR state cannot be classified as exact
   by analysis or export.

Exit gate: deterministic pure-noise claims have oracle-backed seed, phase, and
TEST behavior; combined-waveform limitations are explicit and machine-checked.

### Slice 5 — captured read integration and closure

1. Add the named, short `$D41B`/`$D41C` capture fixture.
2. Compare every read value at its captured cycle.
3. Re-run the asset baseline and classify every changed tune or export.
4. Add the oracle check to the documented digital-SID verification gate.
5. Update `PLAN.md`, move this plan to `plans/done/`, and record closure only
   after all required domains meet their exit gates.

Exit gate: synthetic vectors isolate the state machines, captured reads verify
host integration, normal output remains stable except for reviewed corrections,
and the roadmap can truthfully claim external oracle coverage.

## Verification

Every implementation commit runs:

```bash
cargo fmt --check
cargo build --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
```

Oracle-specific checks:

```bash
cargo test -p sid-analyzer --test digital_sid_oracle
tools/digital-sid-oracle/regenerate.sh --check
```

The second command is required when the generator, source lock, schemas, case
definitions, or committed vectors change. It is not required for unrelated
normal CI jobs.

For any fix that changes live SID reads or analyzed state, additionally run:

```bash
cargo run --release -p sid-analyzer --bin sid-corpus-baseline -- --full
```

Compare the report and serialized outputs with the pinned pre-change baseline.
Every changed tune must trace back to a changed oracle-covered field or read.

## Exit criteria

The plan is complete only when:

- the primary oracle source and generator are cryptographically pinned;
- regeneration is byte-identical and fails closed on source mismatch;
- normal CI is offline and independent of the C++ toolchain;
- envelope, oscillator, sync, TEST, and noise each have named synthetic vectors;
- public `OSC3`/`ENV3` observations and comparable internal integer state match
  exactly;
- every generated field is covered by a field-level comparison policy;
- no broad wildcard suppresses an oracle mismatch;
- every known divergence has evidence, an issue ID, and an explicit project
  policy;
- captured read integration covers at least one `$D41B` and one `$D41C` path;
- checkpoint replay and current property tests remain green;
- the full asset baseline has no unexplained changes;
- `PLAN.md` records the item as done and points to the completed evidence.

## Risks and controls

| Risk | Control |
|---|---|
| Oracle version drift changes results silently | Pin release, resolved commit, archive hash, generator revision, and reject all mismatches |
| Generator accidentally validates itself against current Rust output | Oracle documents contain no `DigitalSid` result or acceptance policy |
| Same-cycle ordering creates false off-by-one failures | Pin clock-then-write semantics with before/after observations in Slice 0 |
| Private oracle fields couple to one reSIDfp layout | Treat them as diagnostics; public full-chip reads own compatibility |
| Known-divergence rules become a blanket skip mechanism | Require field/span selectors, issue IDs, and fail on unmatched or disappeared divergences |
| GPL reference code leaks into the analyzer runtime | Keep the generator standalone; commit data vectors only; no workspace or runtime dependency |
| Analog behavior contaminates a digital test | Use digital clocking and integer state/read observations; no WAV or filter output |
| Power-on behavior differs across references or chip revisions | Preserve the disagreement, add secondary evidence, and select a named model policy |
| Large per-cycle cases make tests slow | Generate sparse observation points; `DigitalSid` keeps jump clocking while vectors retain boundary samples |
| A fix changes tunes that read OSC3/ENV3 | Re-run the captured read fixture and full corpus baseline; require a causal diff for every changed tune |

## Delivery sequence

The intended commit sequence is:

1. schema, policy, loader, and malformed-vector tests;
2. pinned standalone generator and deterministic reset fixtures;
3. envelope oracle matrix and remediations;
4. oscillator and sync oracle matrix and remediations;
5. noise and model-specific TEST matrix and remediations;
6. captured read integration, full baseline, documentation, and closure.

No behavior-changing fix lands in the same commit that first introduces the
oracle evidence for that mismatch. The evidence commit must fail or record the
narrow divergence first; the following commit changes `DigitalSid` and removes
or updates the divergence record. This keeps the external reason for each
correction reviewable.
