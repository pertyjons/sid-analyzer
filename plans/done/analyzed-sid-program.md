# Analyzed SID program implementation plan

> **Status:** implemented on `feat/analyzed-sid-program-completion` (2026-08-03).
> This is the detailed execution record for
> [`../PLAN.md`](../PLAN.md) and does not own active roadmap work.
>
> **Planning baseline:** sid-analyzer `a958106`, Pertylizer `3e25e679`,
> 2026-07-29. Target capability checks use the pinned Pertylizer descriptor
> mirror; structured candidates remain render-rejected until matching audio
> evidence is supplied, so absence of an external renderer cannot silently
> replace the known-good serialized baseline.

## Goal

Introduce one owned, target-neutral `AnalyzedSidProgram` between emulation and
synthesis export. It contains:

- an immutable, lossless `CapturedSidExecution`;
- one chip-scoped `SidChipProgram`;
- exactly one continuous `SidVoiceProgram` per physical SID voice;
- additive causal, musical, native-driver, and render-observable views;
- field-level evidence and uncertainty;
- enough state lineage to compile, validate, and compare several Pertylizer
  representations without consulting the live emulator, `Trace`, or
  `FrameState`.

The work is complete when a serialized capture can resume the digital SID from
any retained checkpoint and reproduce all later checkpoints and SID read
results, the analyzed program reconstructs the modeled chip timeline, the
Pertylizer exporter consumes only the analyzed program plus pinned target
capabilities, and deterministic candidate selection passes state, render, and
corpus budgets.

## Scope boundaries

This plan includes the full data path from SID bus capture through Pertylizer
candidate selection and rendered comparison. It includes trace-derived and
strict-native export.

It does not include:

- RSID host support or multi-SID execution;
- an analog SID implementation inside `sid-analyzer`;
- speculative driver semantics without authored or causal evidence;
- replacing exact captured state with notes, patches, averaged descriptors, or
  rendered PCM;
- stabilizing a public API before the first tagged release.

An external SID renderer remains the initial audio oracle. An in-process
renderer may replace that adapter later without changing the analyzed-program
schema.

## Current code and migration pressure

| Concern | Current owner | Required change |
|---|---|---|
| SID accesses | `trace.rs`, `emu::bus`, `emu::runner` | Preserve read/write interleaving, absolute cycle, sequence, call identity, and timestamp validity in one event stream |
| Digital state | `emu::sid::DigitalSid` | Add exhaustive, serializable checkpoint and restore APIs for every private state field |
| Frame projection | `analysis::analyze` and `FrameState` | Rebuild as a derived compatibility/semantic view over capture replay |
| Note/chip programs | `analysis::programs` | Fold the useful note-local results into continuous voice/chip programs |
| Notes/effects/timbre | `analysis::{note,effects,timbre,osc3}` | Attach as indexed views with source spans and evidence instead of parallel unlinked arrays |
| Native intent | `export::native::NativeSong` | Retain the capture and attach recovered structure, instruments, and effects to the same program |
| Pertylizer planning | the monolithic `export/synth.rs` | Split proposal generation, forward prediction, state gates, selection, and serialization |
| Render checks | manual `sidplayfp`, Pertylizer MCP, and `sid-re wavebands` | Add reusable WAV/feature code and an unattended `sid-abtest` binary |

`Trace` currently stores reads and writes in separate vectors, so same-call
interleaving cannot be recovered after emulation. `DigitalSid` already holds
most required state, but public snapshots omit the register file, model cycle,
envelope hold/rate state, oscillator TEST-fill deadline, and active observation
state. `export/synth.rs` reads `FrameState` directly throughout and must be
migrated behind an adapter before its old path can be removed.

## Target architecture

```text
6502 + SID bus
    |
    v
CapturedSidExecution
  events + call spans + checkpoints + emitted observations
    |
    +------------------------+
    | exact replay/projector |
    v                        v
SidChipProgram          [SidVoiceProgram; 3]
  shared topology         continuous physical/control signals
  filter/mixer/digi       regions + semantic/native views
    \                        /
     +---- AnalyzedSidProgram ----+
                                  |
                         candidate compilers
                                  |
                 state gate -> render gate -> cost selector
                                  |
                         PertylizerLoweringPlan
                                  |
                             .ptz / bundle
```

Source facts, semantic interpretations, target candidates, and serialized
Pertylizer objects are separate types. No Pertylizer module ID, parameter name,
or capability flag may appear in `CapturedSidExecution`, `SidChipProgram`, or
`SidVoiceProgram`.

## Data-model decisions

### Capture

Add these types under `emu::capture`, reusing existing domain newtypes and
adding newtypes for every new ID or domain value:

- `CapturedSidExecution`
- `CaptureSchemaVersion`
- `CapturedSourceIdentity`
- `SidBusEventId`
- `SidCallId` (`Init` or `Play(FrameIndex)`)
- `SidBusEvent`
- `SidBusAccess` (`Read` or `Write`)
- `SidAddress` and `SidAddressClass` (`Base` or `Mirror`)
- `EventTimestampQuality` (`Exact` or `InstructionStartBounded`)
- `CapturedCallSpan`
- `CheckpointId` and `CheckpointRef`
- `SidBusCheckpoint`
- `CapturedObservation`

`SidBusEvent` carries event ID, call ID, raw address, resolved register when
modeled, value, access kind, absolute `ChipCycle`, `SubFrameOffset`, and
timestamp quality. IDs increase across init and every play call. Repeated
accesses and accesses at the same offset remain distinct and ordered. Capture
base-window reads, including write-only-register data-latch reads used by RMW
digis, as well as the existing OSC3/ENV3 reads. Retain mirror accesses as
explicit unsupported events until mirror decoding is implemented.

The capture header retains source digest and subtune, SID model and system
clock, resolved rational call rate and timing validity, init duration/overrun,
and every play call's frame index, absolute start/end, duration, and overrun.
Emulator diagnostics such as mirror accesses remain part of the capture rather
than stderr-only information.

Keep one canonical event vector. During migration, `Trace.init_reads`,
`Trace.init_writes`, and each `FrameTrace` read/write vector are projections
from that vector, never a second independently mutated source.

### Digital checkpoints

Define checkpoint DTOs beside `DigitalSid` in `emu::sid` so exhaustive
conversion can see private fields:

- `DigitalSidCheckpoint`
- `EnvelopeCheckpoint`
- `EnvelopeObservationCheckpoint`
- `OscillatorCheckpoint`

They contain the SID model, register file, absolute cycle, all envelope fields
including `hold_zero`, selected ADSR/rate period and in-progress observation,
and all oscillator fields including accumulator, LFSR, cumulative counters,
poison state, and TEST-fill deadline.

`SidBusCheckpoint` wraps the digital checkpoint with host-visible SID bus state,
including the shared data latch and the model status of mirror/external-input
behavior. Resuming the digital core alone is insufficient for a later
write-only-register read.

`DigitalSid::checkpoint()` and `DigitalSid::restore()` use exhaustive field
destructuring without `..`. Adding a private emulator field must therefore
break compilation until its checkpoint policy is explicit. Round-trip and
field-inventory tests provide the runtime half of the contract.

Capture checkpoints at:

1. init return;
2. the first scheduled play boundary;
3. every play-call start and sampling boundary initially;
4. optional selected event boundaries for diagnostic fixtures.

Retain a checkpoint policy in capture metadata so checkpoint density can change
without changing event semantics.

### Physical and semantic program

Add `analysis::sid_program` with small focused submodules:

- `ids` — stable typed IDs;
- `time` — integer/rational points and half-open spans;
- `evidence` — provenance, confidence, source references, validity, and bounds;
- `signal` — lossless event signals and derived stepped/curve/table views;
- `topology` — oscillator/envelope/filter/bypass/mixer nodes and live edges;
- `region` — continuous and possibly overlapping `SoundRegion`s;
- `semantic` — notes, articulation, instruments, programs, occurrences, and
  native structure;
- `observable` — render artifacts, configurations, profiles, and feature
  versions;
- `builder` and `validate`.

The top-level owned type is:

```rust
pub struct AnalyzedSidProgram {
    pub capture: CapturedSidExecution,
    pub chip: SidChipProgram,
    pub voices: [SidVoiceProgram; 3],
    pub semantic: SemanticProgramView,
    pub observables: RenderObservableSet,
    pub revisions: ProgramRevisions,
}
```

The exact field layout is allowed to evolve while implementing the early
slices, but these invariants are fixed:

- source time is `ChipCycle` or an exact rational mapping, never floating-point
  seconds;
- the lossless access/event signal retains repeated equal writes;
- a derived `StepSignal<T>` may compact adjacent equal state;
- a detected table/program retains every step, duration, loop point, initial
  position, and continuation;
- chip-global filter, routing, voice-3-off, volume, model, and mixer state occur
  once in `SidChipProgram`;
- cross-voice sync/ring edges reference live `VoiceId`s;
- notes and instruments reference physical regions and never own the only copy
  of their state;
- program definitions are separate from occurrences; occurrence state includes
  starting phase/LFSR/table position, transpose, velocity, local automation,
  and continuation links;
- stable IDs are assigned in deterministic source/time order; reusable program
  content also receives a deterministic content digest;
- provenance attaches at the smallest field or homogeneous signal span that
  shares it.

Unify the existing native `FieldProvenance` with a program-wide provenance
enum covering exact emulation, authored verified/decoded/partial,
trace-measured/corrected, render-measured, inferred, approximated, unsupported,
and unknown. Preserve multiple interpretations rather than overwriting weaker
evidence.

### Lowering

Add target-only types under `export::synth`:

- `TargetCapabilities` and `TargetRevision`;
- `CandidateId`, `RepresentationClass`, `Coverage`, `Requirement`, `KnownLoss`,
  and `EditCost`;
- `RepresentationCandidate`;
- per-domain `StateResiduals`;
- `CandidateDecision` with accepted/rejected reasons;
- `PertylizerLoweringPlan`.

Capabilities are loaded from the pinned mirrors in `docs/pertylizer/` plus
behavior probes for semantics the schemas cannot prove: trigger/retrigger,
phase reset, automation application, port compatibility, sequence carry, and
bundle/sample loading.

Selection is deterministic. Candidates are ordered by a documented
lexicographic cost tuple: known loss class, editability class, graph
complexity, automation density, serialized size, then stable candidate ID.
Only candidates inside all mandatory state budgets reach audio rendering.

## Implementation slices

Every slice lands as one or more logical commits. Each commit must pass the
normal repository gate. Do not combine emulator capture changes with audible
export changes.

### Slice 0 — pin baselines and create seams

1. Regenerate the full corpus baseline and retain its report outside git under
   `exports/baseline/`.
2. Record golden `.ptz`, census, serialized size, elapsed time, and peak memory
   for the named fixtures used by `synth_fidelity_budget`.
3. Refresh `docs/pertylizer/{project.schema.json,patch.schema.json,
   bundle-metadata.schema.json,descriptors.json}` from Pertylizer and record both
   repository revisions in the baseline report.
4. Extract the CLI's repeated trace/notes/effects/timbre assembly into one
   internal `AnalysisInputs` builder without changing output.
5. Add a minimal owned `AnalyzedSidProgram` shell and an internal exporter
   entry accepting `&AnalyzedSidProgram`; initially its legacy payload delegates
   through the current exporter. Replace that payload slice by slice rather
   than designing two public entry points.

Exit gate:

- trace and native `.ptz` files remain byte-identical;
- census and forward residuals remain unchanged;
- the baseline command records revisions and performance;
- no new behavior is enabled.

### Slice 1 — capture one ordered SID bus stream

1. Add `SidBusEvent` and a monotonically increasing event counter to
   `emu::bus`.
2. Record base-window and mirror reads/writes through the same append path,
   including data-latch reads. Preserve the current instruction-start offset
   and mark it `InstructionStartBounded`; do not claim cycle-exact
   intra-instruction timing.
3. Carry absolute chip cycle, call identity, and call-relative offset through
   `CapturedCall`, `FrameTrace`, and `Trace`.
4. Derive the old read/write vectors from the unified events during the
   compatibility period.
5. Add capture serialization and deserialization with explicit schema version
   and strict unknown-field handling.

Tests:

- read-before-write and write-before-read within one instruction offset;
- multiple accesses at one offset retain insertion order;
- init and play events share one monotonic ID domain;
- RMW `$D418` access ordering;
- capture JSON round-trip and deterministic serialization;
- current note/effect/timbre outputs remain unchanged.

Exit gate: every currently trapped SID access appears exactly once in the
unified stream, and the legacy projections reproduce the prior `Trace`.

### Slice 2 — complete checkpoints and replay

1. Implement exhaustive `DigitalSid` and SID-bus checkpoint/restore.
2. Capture init-return, first-boundary, frame-start, and frame-end checkpoints
   from the live SID core used by `Bus`.
3. Add a replay engine that restores a checkpoint, clocks to each later event,
   applies writes, executes reads, advances through idle spans, and compares
   the result with captured values and later checkpoints.
4. Capture emitted envelope/oscillator observations with source event/span
   references rather than recomputing them only into `FrameState`.
5. Add a configurable sparse checkpoint cadence after correctness is proven;
   retain dense diagnostic checkpoints for synthetic tests.

Tests:

- every envelope phase, rate carry, hold-zero, ADSR write, gate transition, and
  active observation resumes identically;
- oscillator accumulator, sync counters, ring source state, TEST fill, LFSR,
  poison recovery, and model-specific TEST timing resume identically;
- replay reproduces `$D41B`, `$D41C`, POT, and write-only data-latch reads;
- replay from each retained checkpoint reaches byte-equal later checkpoints;
- adding a private `DigitalSid`, `Envelope`, `Observation`, or `Oscillator`
  field cannot compile without an explicit checkpoint mapping.

Named checks: the sound-engine synthetic matrix, Nemesis, Auf Wiedersehen
Monty, Warhawk, and one CIA-timed tune.

Exit gate: capture replay needs neither the 6502 nor the original SID file and
reproduces all modeled SID observables.

### Slice 3 — build the exact physical program

1. Implement the program time, ID, evidence, signal, and topology primitives.
2. Build lossless per-voice event signals for frequency, pulse width, control,
   ADSR, gate, TEST, sync, and ring.
3. Build derived state signals for envelope state/level, accumulator phase,
   LFSR/validity, waveform, and source/destination activity.
4. Build chip signals for cutoff, resonance, modes, routing, voice-3-off,
   volume, external-input status, and digi/sample writes.
5. Represent pre-filter voice taps, filtered bus, bypass bus, and final mix in
   one explicit topology.
6. Add a projector from `AnalyzedSidProgram` back to current `FrameState` and
   `ChipFilterProgram` sampling boundaries.
7. Add a versioned deterministic debug JSON format.

Tests:

- reconstruct all 29 modeled SID registers at every event and frame boundary;
- reconstruct current `FrameState`s and filter programs exactly;
- preserve repeated equal program writes and step durations;
- cover all sync/ring source-destination pairs and silent modulators;
- local timing uncertainty does not demote unrelated spans.

Exit gate: the exact physical program reconstructs the modeled chip timeline
without reading raw `Trace` or calling the live emulator.

### Slice 4 — attach causal and semantic views

1. Move note, effect, timbre, `NoteProgram`, and OSC3 results behind program
   builders that return source-linked views.
2. Segment continuous `SoundRegion`s: tonal attack, sustain, release tail,
   noise transient, waveform transient, continuous texture, silent modulator,
   silent/parked state, and digi stream. Regions may overlap and cross note or
   gate boundaries.
3. Detect constant, ramp, periodic, table, loop, envelope, and bounded-script
   interpretations without discarding the underlying event/step signal.
4. Attach native instruments, authored effects, recovered patterns/orderlists,
   and validation evidence to the same physical spans.
5. Separate reusable program definitions from occurrences and carry hidden
   initial state plus continuation.
6. Replace the current time-window-only OSC3 attribution with dependencies
   over ordered read/write events. Extend causal sources through the existing
   taint/probe machinery to ENV3, RAM/table cells, timers, accumulators, and RNG
   only when a producer-transform-consumer chain is supported.
7. Keep ambiguous hypotheses side by side with confidence and residual bounds.

Tests:

- release motion and free-running tables survive note boundaries;
- repeated sequences such as `T,T,N,T,N` retain all five steps;
- native authored values and trace-measured values coexist when they disagree;
- program reuse never resets phase, LFSR, or table position implicitly;
- synthetic OSC3/ENV3/table/timer/RNG fixtures distinguish single, multiple,
  random-only, and unknown consumers.

Exit gate: current musical analysis is available through source-linked program
views, and no semantic pass can delete or rewrite captured evidence.

### Slice 5 — migrate both Pertylizer input paths

1. Change trace export to construct one `AnalyzedSidProgram`.
2. Change every native extractor result to retain its capture and contribute a
   native semantic overlay to the same builder. Remove `NativeSong`'s role as a
   parallel synthesis data model.
3. Introduce a legacy `RepresentationCandidate` that wraps today's
   `MergeShape`, `TrackPlan`, instrument builders, automation, forward model,
   and serializer.
4. Move all raw `FrameState` scans behind program query APIs and cached indexes.
5. Change the final synth API to
   `write_synth(program: &AnalyzedSidProgram, capabilities, options, out)`.
6. Keep JSON, text, and MIDI behavior stable; optionally expose the analyzed
   debug JSON through a separate explicit CLI format.

Exit gate:

- all existing trace and native `.ptz` goldens are byte-identical;
- `export/synth` imports neither `Trace` nor `FrameState`;
- no native extractor reruns emulation to rebuild the program;
- long exports perform no repeated full-timeline scan per note or candidate.

Only after this gate may audible candidate improvements begin.

### Slice 6 — state-gated candidate compiler

Implement candidates domain by domain, keeping the legacy candidate as a
non-regression control.

#### 6A. Envelope and amplitude

Generate, simulate, and compare:

1. native ADSR;
2. normalized ADSR plus `envelope.time_scale`;
3. MSEG;
4. gate-driven Script/Mod Matrix;
5. sparse amplifier automation;
6. dense amplifier automation.

Residuals cover start/peak/end level, onset/peak/release time, sustain,
pointwise maximum/mean error, retriggers, rate carry, and censoring. Reject MSEG
or reusable envelopes whenever reset-from-zero, legato, or
retrigger-from-current-level semantics disagree.

#### 6B. Pitch, pulse width, filter, and level motion

Generate:

- Arpeggiator for verified periodic semitone tables;
- Glide or note expression for verified note-local ramps;
- LFO for centered periodic controls;
- Kinetic Modulator for one-shot/looping/ping-pong easing curves;
- direct CV or Mod Matrix for causally supported scaled routing;
- Script for deterministic tables and bounded algorithms;
- sparse or dense automation as the measured fallback.

One candidate may share a modulator across targets only when the dependency
graph identifies the same source and transforms.

#### 6C. Oscillator, noise, topology, and filter

Generate:

- static native SID oscillator;
- stateful SID waveform/frequency sequence;
- deterministic noise seed/restart;
- live cross-voice sync/ring graph;
- calibrated combined-waveform fallback;
- current legacy graph as an explicit approximation.

Represent the shared SID filter and routing as one chip-scoped candidate when
Pertylizer capabilities support it. Until then, retain the voice-local filter
copy only as a declared-loss fallback; never describe it as exact.

#### 6D. Digi and rendered fallback

Reconstruct `$D418` PCM where the event stream and timing are sufficient.
Generate a sampler/bundle candidate for reconstructed PCM and for bounded
one-shot rendered fallback. The sample artifact retains its source region,
renderer, model, sample rate, normalization, and content digest.

#### Selection

Expand `export::forward` into per-domain state prediction. A candidate declares
coverage, required capabilities, initial-state/reset behavior, continuation,
known losses, and edit cost. Reject missing coverage, wrong notes, causal
resets, or out-of-budget state before rendering. Retain every rejected reason
and residual in the census/debug report.

Exit gate:

- synthetic fixtures choose the known minimal representation in every family;
- deliberate near-misses are rejected for the expected residual;
- selection is deterministic across repeated and parallel corpus runs;
- unsupported target semantics produce explicit fallbacks, never silent
  omission;
- the legacy candidate remains selectable until the render gate proves each
  replacement class.

### Slice 7 — continuous regions and safe reuse

1. Make candidate planning operate on continuous regions and program
   occurrences rather than note containers.
2. Preserve audible release, moving pitch/waveform after gate-off, and
   retriggers.
3. Preserve silent sync/ring modulators and shared causal sources.
4. Reuse programs only when hidden-state continuation and instance overrides
   are representable.
5. Canonicalize exact repeated programs before Pertylizer pattern/instrument
   grouping; keep physical voice ownership and chip-global topology.
6. Add a program-level complexity and serialization-size census.

Exit gate:

- release tails, legato, free-running tables, and noise sequences do not reset
  at artificial note or pattern boundaries;
- program reuse reduces structured export duplication without changing state
  residuals or rendered physical-voice overlap;
- Warhawk and Auf Wiedersehen Monty show a material pattern/size reduction or
  retain an explicit report explaining why safe reuse is impossible.

### Slice 8 — render-observable profiles and `sid-abtest`

1. Move the reusable WAV reader, FFT, and band analysis from `sid-re` into a
   library module.
2. Add feature-versioned measurement for:
   - multiscale RMS/peak envelope;
   - onset, peak, decay, release, and transient density;
   - fundamental frequency, confidence, and pitch trajectory;
   - harmonic/noise balance and spectral envelope;
   - log-spectral distance, centroid, rolloff, flatness, and zero-crossing rate;
   - low-rate amplitude, pitch, and brightness modulation.
3. Add `RenderArtifactRef`, `RenderConfig`, `TapId`, and
   `RenderObservableProfile`. Store content-addressed references and complete
   render configuration, not embedded PCM, in analyzed-program debug data.
4. Add `sid-abtest` with adapters for:
   - `sidplayfp`/reSID reference rendering, including per-voice mute;
   - a stable headless Pertylizer renderer;
   - already-rendered WAV inputs for reproducible offline comparison.
5. Align windows by source region and onset before comparison. Record when a
   voice cannot be isolated because filter nonlinearity, combined waveforms, or
   `$D418` interaction makes stems non-additive.
6. Cache renders and profiles by input digest, source span, renderer revision,
   model, sample rate, and feature version.
7. Render only candidates that passed state gates; choose the cheapest
   candidate within the class-specific audio budget.

Pertylizer now exposes the versioned `pertylizer render` protocol. `sid-abtest`
invokes it directly, requests lossless 32-bit float output and a JSON receipt,
validates the effective duration, format, input/output identity, and mix, and
maps one physical SID voice to every matching stable Pertylizer track ID. The
remaining work is named-matrix coverage and capability testing for shared
master/return filter automation, dynamic routing, LFO gate retrigger, SID
sequence continuation, and sampler bundle loading.

Tests:

- synthetic time shift, gain, pitch shift, noise, and filtering produce known
  metric directions;
- cached and uncached reports are byte-identical apart from elapsed time;
- the 6581/8580 sound-engine fixture matrix has pinned budgets;
- named short windows cover envelope, release, arpeggio, glide, PWM, filter,
  sync, ring, combined waveform, noise, and percussion;
- render-tool failures are typed and include reproducible commands.

Exit gate: the named matrix runs unattended from SID input to ranked candidate
report and selects its known-good candidates.

### Slice 9 — switch the default and remove transitional paths

Completed. Both trace and native extraction now cross the synth boundary as an
`AnalyzedSidProgram`; native recovery attaches a semantic overlay directly.
The old synth inputs, `program.legacy`, the public `NativeSong` handoff, legacy
representation class, and frame-state projection name were removed. Automatic
selection is enabled for every export and records its state/render decisions in
the census.

1. Enable new candidate selection by default one representation class at a
   time, only after that class passes state and render budgets.
2. Delete the legacy `FrameState` synth input, direct trace rescans, duplicate
   native synthesis arrays, and superseded special-case builders.
3. Keep explicit fallback candidate builders for capabilities that remain
   unsupported.
4. Add capture/program schema migration policy. Before the first release,
   incompatible debug schema changes may fail loudly instead of migrating.
5. Regenerate documentation, schema mirrors, corpus baseline, and module audit.

Final exit gate:

- capture serialization round-trips every modeled SID input and observable;
- replay from every retained checkpoint reproduces later checkpoints and SID
  reads;
- the analyzed program reconstructs the complete modeled chip timeline and
  retains continuous state lineage;
- both trace and strict-native exports consume the same target-neutral program;
- candidate selection is deterministic and records coverage, capability,
  residual, cost, and rejection evidence;
- the full exportable corpus is schema-valid and inside pinned forward/render
  budgets;
- no known captured field is silently dropped; unsupported output has a typed
  census reason.

## Pertylizer dependencies

The analyzer must continue to compile without a runtime Pertylizer dependency.
Use mirrored schemas/descriptors for serialization and validation, plus a
versioned external renderer for A/B tests.

These target capabilities decide whether exact candidates can be enabled:

| Capability | Required for | Availability or fallback |
|---|---|---|
| Stable headless project/bundle render command | unattended `sid-abtest` | Available through `pertylizer render` protocol v1 with validated receipts |
| Master/return effect automation and dynamic voice routing | one shared SID filter | declared-loss per-instrument filter copies |
| Gate-wired LFO retrigger with documented phase | note-local periodic modulation | gate-driven Script or measured automation |
| SID sequence initial phase/LFSR and continuation | exact free-running waveform/noise programs | measured automation or rendered fallback |
| Bundle writing and sampler asset load | `$D418` PCM and rendered one-shots | explicit unsupported/low-fidelity classification |

No candidate is enabled from a schema field alone. Each row needs a serialized
fixture, live/offline render, and reload test against the pinned Pertylizer
revision.

## Verification matrix

| Layer | Unit/synthetic | Named integration | Corpus |
|---|---|---|---|
| Capture order | same-offset read/write and RMW | Nemesis init/play | event-count and deterministic-size census |
| Checkpoint/replay | every envelope/oscillator hidden state | Monty, Warhawk, CIA tune | sampled replay audit |
| Physical program | register/signal/topology reconstruction | sync/ring/noise fixtures | no unmapped-field census |
| Semantic program | regions, reuse, causality, ambiguity | Nemesis, AWM, OSC3 tunes | provenance and unknown-consumer census |
| State gate | known pass/fail candidates | current fidelity fixtures | `synth_fidelity_budget` |
| Render gate | controlled WAV perturbations | short pinned musical windows | ranked worst-window report |
| Serialization | strict round-trip and deterministic order | trace + native `.ptz` | all projects schema-valid |

Before each commit:

```bash
cargo fmt --check
cargo build --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
```

Run the full corpus and render matrix at the exit of a slice, not on every
small commit.

## Delivery sequence

The intended commit sequence is:

1. baseline and pipeline seam;
2. unified bus events;
3. capture serialization;
4. digital checkpoint/restore;
5. replay verification;
6. physical program primitives and builder;
7. `FrameState`/filter projection parity;
8. semantic regions, evidence, and native overlays;
9. legacy exporter through `AnalyzedSidProgram`;
10. envelope candidates and state gate;
11. modulation/effect candidates;
12. oscillator/topology/filter/digi candidates;
13. continuous reuse;
14. audio features and render adapters;
15. deterministic selector rollout;
16. transitional-path removal and final baseline.

If a commit changes rendered output, its predecessor must already contain the
state residual and fixture that justify the change.

## Risks and controls

| Risk | Control |
|---|---|
| Capture/checkpoint size makes corpus work impractical | Measure in Slice 0; keep events lossless, make checkpoint cadence configurable, cache derived views rather than duplicate state |
| New IR merely copies every old parallel array | Make capture canonical, require source refs, and delete compatibility projections after exporter parity |
| Semantic inference is mistaken for truth | Preserve measured signals, provenance, confidence, alternatives, and rejection residuals |
| Candidate combinations explode | Compile per domain, reject cheaply, combine only capability-compatible survivors, use deterministic cost order |
| Pertylizer behavior drifts beyond mirrored schemas | Pin revisions, run behavioral capability fixtures, and include revisions in every report |
| Native and trace paths diverge again | Build one physical program from capture; native extraction contributes only additive semantic evidence |
| Render metrics reward a perceptual match with wrong notes/state | Mandatory state gates precede render ranking and cannot be waived by audio score |
| External tools make CI flaky | Unit/state gates remain normal CI; pinned short render fixtures are feature-gated, cached, and required at slice/release gates |
