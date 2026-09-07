# Chip-state-driven export — slice plan

Use the shipped digital SID model as export ground truth. The analyzer now
computes the envelope level and phase, ordered attack/release/zero events,
interval-active cycles, oscillator phase, sync resets, source-MSB edges, and the
noise LFSR. The synth exporter still derives important choices from register
intent or static ADSR heuristics. This plan migrates those choices to measured
chip state without turning the exporter into a second SID emulator.

Companion plans:

- [`chip-state-emulation.md`](chip-state-emulation.md) owns the digital model.
- [`export.md`](../../docs/export.md) owns the SID → Pertylizer architecture and backlog.
- [`forward-model-gate.md`](forward-model-gate.md) owns pitch fidelity.
- [`PLAN.md`](../PLAN.md) owns current rendered-audio qualification.

## Implementation status (2026-07-19)

- Exact melodic release tails now extend to measured envelope zero when the
  release waveform is stable. Moving pitch is checked by the forward model and
  degrades to trace-derived pitch runs; waveform-changing and trace-censored
  tails remain explicit census cases rather than fabricated notes.
- Waveform programs carry the analyzer's deterministic LFSR seed and measured
  per-noise-step frequency. TEST transitions that would need a sequence reset
  mask are counted as deferred, and all 11 combined-waveform masks have an
  explicit 6581/8580 native-support policy.
- Timestamped OSC3 reads and SID writes are retained at the analysis boundary.
  A bounded causal classifier distinguishes pitch, pulse width, filter cutoff,
  volume, RNG-only, multiple, and unknown consumers. Only dominant targets with
  at least three observations and 70% confidence are marked as exported through
  the existing measured pitch/PW/filter/volume paths.
- Structured export now canonicalizes leading silence into placement start,
  lifts pitch into placement transpose, and content-addresses the remaining
  notes. Driver pattern numbers and transposition no longer prevent exact reuse.
- Identical tonal SID arpeggiators are emitted as pooled Pertylizer Note Graphs;
  stateful waveform/noise programs remain on the SID oscillator. Redundant step
  holds are removed after the existing RDP/curve automation fit, and the census
  reports patterns, placements, graphs, and final automation-point count.

## Goal

Improve audible fidelity in three steps:

1. export the envelope that actually ran;
2. enable hardware effects only when the oscillator actually performed them;
3. reproduce noise programs faithfully and make every combined-waveform
   approximation explicit.

Each slice lands independently with the normal repository gate green. Changes
must also pass `synth_fidelity_budget`; render-facing changes add a named A/B
fixture or a pinned render-budget result.

## Scope and boundaries

In scope:

- synth and synth-native note duration, gain contour, velocity, and retrigger;
- measured sync/ring activity and OSC3-derived modulation;
- noise onset/program state and combined-waveform fallback policy;
- census, JSON sidecar, and provenance needed to audit those decisions.

Out of scope:

- analogue SID filtering, DAC bleed, and transistor-level combined waves;
- replacing Pertylizer's native `sid` oscillator;
- CIA-timed exactness before CIA scheduling lands;
- `$D418` PCM reconstruction and bundle-format work;
- changing MIDI into an audio-faithful renderer.

MIDI continues to end notes at `release_frame()`. Synth export may continue to
`sound_end_frame(states)` and carry release-phase pitch motion. CIA-timed tunes
may use the existing export path, but chip-state-driven decisions are labelled
inexact and excluded from exactness gates.

## Shared export representation

Introduce one export-side articulation record rather than reading `FrameState`
ad hoc in instrument builders:

```rust
struct ChipArticulation {
    onset: FrameIndex,
    release: Option<FrameIndex>,
    sound_end: Option<FrameIndex>,
    envelope: EnvelopeProgram,
    oscillator: OscillatorActivity,
    exact: bool,
}
```

`EnvelopeProgram` contains normalized, monotonic-time points derived from the
8-bit envelope level and ordered events. `OscillatorActivity` contains
frame-local sync-reset/source-edge counts, ring applicability, OSC3-read
provenance, noise state at onset, and combined-waveform support status.

The record belongs at the boundary between analysis and export. Native driver
data remains authored intent; chip articulation is measured execution. When
they disagree, the exported value records its provenance as `Authored`,
`Measured`, or `Fallback` in the fidelity census.

## Locked decisions

- **Release and sound end stay separate.** MIDI uses release. Pertylizer note
  duration uses sound end only when the envelope trace is exact and uncensored.
- **Envelope level is amplitude, not velocity.** Velocity represents onset
  strength; a gain lane carries the time-varying contour.
- **Subframe events are preserved until lowering.** Multiple attack/release
  events inside one play call must not collapse before the exporter chooses its
  tick resolution.
- **Register bits express intent; measured counters express activity.** Sync and
  ring classification keeps the configured bits but export enforcement requires
  compatible measured activity.
- **OSC3 reads need causal attribution.** A changing OSC3 signal is not assumed
  to modulate pitch, PW, or filter merely because `$D41B` was read. Use captured
  reads plus write correlation; otherwise report an unknown modulation target.
- **Combined-waveform uncertainty is never silent.** Exact native support,
  measured fallback, and low-fidelity approximation are separate census states.
- **No trace-end fabrication.** A censored release keeps unknown sound end; it
  is not closed at the last captured frame for fidelity accounting.

## Slice 1 — envelope-native articulation

### Implementation

- Build `ChipArticulation` for every traced `NoteEvent`.
- Use `release_frame()` and `sound_end_frame(states)` consistently. Extend synth
  notes through audible release while keeping MIDI note-off unchanged.
- Derive per-note onset level, peak, attack duration, decay/sustain contour,
  release contour, and retrigger points from envelope snapshots, activity, and
  ordered events.
- Replace `velocity_for_envelope(Adsr)` with a measured onset/peak velocity when
  exact state exists; retain the static ADSR rule as an explicit fallback.
- Lower the 8-bit contour to the cheapest faithful Pertylizer primitive:
  native ADSR when it matches within tolerance, otherwise a module gain lane or
  script curve. Simplify only within a named amplitude-error tolerance.
- Split or retrigger Pertylizer notes for multiple `EnteredAttack` events while
  keeping driver-level `HardRestart` as a separate characteristic.
- Add census fields for measured/static velocity, native/automated envelope,
  censored sound ends, release extension, and intra-call retriggers.

### Exit gate

- Synthetic fixtures cover attack interrupted by release, retrigger during
  release, sustain changes, zero-length gate blips, multiple attacks in one
  call, and release censored at trace end.
- MIDI bytes are unchanged except where an existing release-endpoint bug is
  explicitly classified.
- Named AWM stab and drum-drop notes continue to envelope zero without creating
  parking notes.
- Render A/B improves or holds onset-time, peak-time, decay-slope, and
  release-duration errors from `render-abtest`; no tracked class regresses
  outside its confidence interval.
- `synth_fidelity_budget` holds or improves, and every duration change appears
  in the sidecar census.

## Slice 2 — measured hardware effects

### Implementation

- Thread frame-local `sync_resets` and `source_msb_edges` into effect spans and
  `ChipArticulation`.
- Keep sync/ring register-intent detection, but distinguish `Configured`,
  `Active`, and `ConfiguredInactive`.
- Enable Pertylizer hard sync only for spans with source edges and destination
  resets. Preserve configured-but-inactive intent in JSON/census without
  forcing an audible effect.
- Enable ring modulation only for triangle-compatible spans with a changing
  source MSB. Carry the correct cyclic source voice into the native `sid`
  oscillator topology.
- Build an OSC3 read stream from captured `$D41B` values and cycles. Correlate it
  with subsequent frequency, pulse-width, cutoff, and volume writes inside a
  bounded causal window.
- For a confidently identified target, lower the measured contour to pitch,
  PWM, filter, or gain automation. Unknown/multi-target consumers remain
  report-only rather than receiving guessed modulation.
- Add census counts for active/inactive sync and ring, OSC3 target confidence,
  measured-contour exports, and unknown targets.

### Exit gate

- Synthetic sync fixtures cover all three source/destination pairs,
  simultaneous-edge suppression, no-source-edge spans, and test-bit resets.
- Ring fixtures prove that source-MSB activity changes only applicable triangle
  output and that an inactive source does not create modulation.
- At least one captured OSC3-consuming fixture has a trace-confirmed causal
  target and matching exported automation; a read used only as RNG is not
  misclassified as musical modulation.
- Existing effect spans retain register intent; measured activity only refines
  export behavior and adds data.
- Hard-sync/ring render fixtures hold or improve spectral and modulation error,
  with no regression for tunes that never read OSC3 and never configure the
  effects.

## Slice 3 — noise programs and combined-waveform fidelity

### Implementation

- Capture the LFSR state, test transition, waveform byte, frequency register,
  and envelope level at every noise onset and waveform-program step.
- Seed/restart the Pertylizer `sid` oscillator deterministically where the
  schema supports it. If the engine owns an unseedable free-running LFSR,
  measure the limitation and use timing/program parity as the attainable gate.
- Replace dominant-waveform drum lowering with the measured per-step program:
  waveform, duration, frequency, level, test transition, and loop/free-run
  semantics.
- Classify noise→tonal, tonal→noise, and test-bit recovery as distinct program
  transitions. Preserve free-running programs across legato pitch splits.
- For every combined-waveform class, select one policy:
  native exact support, chip-model-specific calibrated native approximation,
  rendered/sample fallback, or explicitly low-fidelity nearest waveform.
- Prefer editable native `sid` programs. Escalate to sample/render fallback only
  when the native result fails the render budget and the bundle format supports
  a reproducible asset.
- Add per-note fallback reason and combined-waveform support status to JSON and
  the fidelity sidecar.

### Exit gate

- Golden vectors pin LFSR seed, bit-19 clocks, OSC3 output mapping, test
  transitions, and noise recovery used by the export representation.
- Named noise-drum fixtures preserve transient timing, waveform-step frequency,
  free-running/loop behavior, and envelope duration.
- Every one of the 11 multi-waveform selector classes receives an explicit
  policy for both 6581 and 8580; no class silently falls through to pulse.
- The existing 36-fixture reSID matrix holds or improves. Noise-percussion
  centroid/ZCR and combined-waveform spectral-distance budgets are pinned.
- Raw fallback census entries contain waveform, chip model, support status, and
  reason; zero unclassified fallback paths remain.

## Validation and rollout

For every slice:

1. unit tests over synthetic `FrameState`/event streams;
2. analyzer integration fixtures with captured SID reads and writes;
3. `cargo fmt --check`, build, clippy, and workspace tests with zero warnings;
4. `synth_fidelity_budget` and schema validation;
5. named render A/B fixtures using `render-abtest` metrics;
6. corpus census comparing counts and worst offenders before/after.

Roll out behind one temporary `SID_CHIP_STATE_EXPORT` A/B switch. The default
stays off during measurement, flips on when a slice meets its gate, and the
switch is deleted after the following slice confirms no rollback need. Do not
leave permanent dual implementations.

## Dependencies and likely Pertylizer gaps

- A module-level gain automation target or script-controlled amplitude is
  required when native ADSR cannot reproduce measured contours.
- OSC3 contour export needs automation targets for pitch/PW/filter/gain with a
  documented update rate and interpolation rule.
- Pertylizer now provides `noise_seed`, `seq_freq_mask`, and 16 per-step raw
  frequency registers; the exporter uses them for deterministic SID noise and
  measured noise-step pitch. Sequence state survives legato note splits.
- Per-step pulse width, level, and sequence-triggered noise reset remain
  deferred until a captured import demonstrates the need.
- Sample fallback remains blocked until the `.ptz` bundle/audio asset
  format is stable.

When one of these blocks a slice, verify the current schema/module capability,
record the gap in `pertylizer-mcp-feedback.md`, and add a codebase-grounded plan
under the repository policy that applied at the time. Current Pertylizer
requests and plans belong in `pertylizer-mcp-feedback.md` in this repository.

## Implementation order

1. Slice 1 — envelope-native articulation.
2. Slice 2 — measured sync/ring and causally attributed OSC3 modulation.
3. Slice 3 — noise programs and explicit combined-waveform fallbacks.

Do not start Slice 2 by bypassing missing Slice 1 articulation fields; the
shared representation is what keeps hardware effects, duration, and provenance
consistent. Slice 3 may prototype engine capabilities in parallel, but its
export path lands last because it has the highest engine/schema dependency.
