# Export fidelity — corpus-driven next pass

> **Status: superseded.** Remaining work is consolidated in
> [`../PLAN.md`](../PLAN.md).

Turn the successful chip-state export rollout into a measured improvement loop.
The first full-length corpus export made most tunes audibly better, but also
exposed a small number of high-value outliers. This plan fixes those outliers
first, then generalizes the solutions into release-tail, OSC3, waveform-program,
and native-extraction improvements.

Companion plans:

- [`chip-state-export.md`](chip-state-export.md) records the measured envelope and
  oscillator-state rollout.
- [`../PLAN.md`](../PLAN.md) owns rendered-audio comparison.
- [`forward-model-gate.md`](forward-model-gate.md) records trace-to-export pitch
  verification.
- [`export.md`](../../docs/export.md) owns the overall SID → Pertylizer architecture.

## Goal

Improve the worst audible exports without regressing the tunes that already
benefit from chip-state-driven export. Every change must be attributable to a
measured trace or render difference; no new driver-specific audio heuristics land
without a reusable classification and a corpus census.

The work is complete when:

- the named outliers meet their pinned pitch and render budgets;
- audible release tails retain both their envelope and pitch motion;
- OSC3 consumers are causally classified or explicitly unknown;
- noise/waveform programs preserve all state the target engine can represent;
- native extractors cover the named unsupported variants or reject them cheaply;
- the full export corpus remains schema-valid and no tracked fixture regresses.

## Baseline and fixtures

Pin the 2026-07-19 full-length export census before changing behavior. The first
priority fixtures are:

| Tune | Baseline signal | Primary investigation |
|---|---:|---|
| Sigma Seven | 144 failed events, 180 degraded, 206.8 ct mean | pitch ownership, percussion relocation, silence boundaries |
| Auf Wiedersehen Monty | 67 failed, 1,235 degraded | filter/percussion articulation, glide and hard-sync spans |
| Warhawk | 5 failed, 1,276 degraded | ring modulation, plan fragmentation, release motion |
| Knucklebusters | 5 failed, 299 degraded; very slow export | long-trace scaling, native mismatch, sync/ring density |
| Nemesis the Warlock | native onset agreement 1% | unsupported Hubbard variant |
| Comic Bakery | Galway tables not located | post-init table discovery |
| Defcom / Glider Rider | no David Whittaker extractor | new native driver family |

Keep Monty on the Run, Commando, Neverending Story, Ocean Loader 1, and 720
Degrees as non-regression controls. Arkanoid and Last V8 remain outside this
plan until the RSID host environment exists.

## Slice 1 — diagnose and close the outliers

### Implementation

- Add a repeatable corpus command that exports the named fixtures, validates
  every `.ptz`, and writes a compact before/after census table.
- Extend fidelity reporting with the worst event ranges: tune, voice, plan,
  frame span, proposed representation, degradation rung, and residual.
- Use `sid-re watch`, native decode dumps, and the forward model to classify the
  Sigma Seven, AWM, and Warhawk failures before changing export behavior.
- Fix reusable causes in the exporter. Keep tune-specific knowledge in native
  driver decoding only when it is genuinely authored data.
- Record representative short windows for render A/B so a full tune is not
  required during normal development.

### Exit gate

- Sigma Seven mean pitch error is below 25 cents and failed events fall by at
  least 75%, without hiding frames as unverifiable or silent.
- AWM and Warhawk degraded-event counts fall by at least 30% with no increase in
  failed events.
- All non-regression controls stay within their pinned forward-model budgets.
- Each improvement has a synthetic regression test and a named real-song test.

## Slice 2 — complete release-tail articulation

### Implementation

- Represent gate release, audible sound end, envelope zero, and trace censoring
  as separate export-side events.
- Continue synth notes to measured sound end only when the digital state is
  exact and the tail can be rendered faithfully.
- Track frequency and waveform changes during release. Bake pitch runs or use a
  glide when the release moves; never sustain a parked register value merely to
  keep an amplifier lane alive.
- Preserve ordered retriggers inside release and split/retrigger the target note
  only where the engine requires it.
- Let the amplifier lane carry the measured 8-bit envelope while MIDI continues
  to end at gate release.

### Exit gate

- Synthetic tests cover stable release, moving release, retriggered release,
  waveform change, parking, and trace-end censoring.
- AWM stabs and percussion reach measured zero without phantom notes.
- Release extension adds no failures to `synth_fidelity_budget`.
- Render A/B holds or improves release duration, decay slope, and tail pitch.

## Slice 3 — causally attribute OSC3 modulation

### Implementation

- Carry timestamped `$D41B` reads from `FrameTrace` into an analysis-side OSC3
  read stream without embedding raw trace traversal in the exporter.
- Correlate each read with subsequent frequency, pulse-width, cutoff, and volume
  writes inside a bounded CPU-cycle window.
- Classify consumers as `Pitch`, `PulseWidth`, `FilterCutoff`, `Volume`,
  `RandomOnly`, `Multiple`, or `Unknown`, with a confidence and evidence count.
- Export automation only for a single high-confidence musical target. Multiple
  and unknown consumers remain report-only.
- Add causal-target counts and worst unknown consumers to the census.

### Exit gate

- Synthetic fixtures distinguish all four musical targets from RNG use.
- At least one captured tune exports a trace-confirmed OSC3 contour.
- A read-only RNG fixture receives no musical automation.
- Tunes without OSC3 reads serialize identically apart from census additions.

## Slice 4 — stateful noise and waveform programs

### Implementation

- Build a per-note program from waveform mask, duration, frequency register,
  pulse width, envelope level, test transition, LFSR state, and loop/free-run
  semantics.
- Preserve noise→tonal, tonal→noise, and test recovery as distinct transitions.
- Lower every field supported by Pertylizer. Record unsupported LFSR seed and
  per-step frequency/level as explicit capability fallbacks.
- Preserve sequence phase across legato pitch splits.
- Assign every combined-waveform mask a chip-model-specific support policy:
  native exact, calibrated approximation, rendered fallback, or low fidelity.

### Exit gate

- Golden tests pin LFSR/test behavior used at the export boundary.
- Named noise drums hold transient timing, frequency, level, and sequence phase.
- All 11 multi-waveform selector classes have an explicit 6581 and 8580 policy.
- No raw note silently becomes pulse; every fallback has a reason in census.
- When Pertylizer gains deterministic seed and per-step fields, add capability
  tests before enabling them and retire the corresponding fallback counts.

## Slice 5 — native extractor coverage and speed

### Implementation

- Diagnose Nemesis and Knucklebusters Hubbard onset disagreement using the
  existing taint/probe and driver RE tools.
- Generalize Galway table discovery so Comic Bakery locates its post-init data.
- Add a David Whittaker extractor starting with Defcom and Glider Rider.
- Profile Knucklebusters end to end. Remove repeated full-trace scans and
  quadratic plan/event work; cache only derived data with clear ownership.
- Fail native agreement gates early enough that an unsupported 50,100-frame tune
  does not pay for two complete expensive exports before trace fallback.

### Exit gate

- Nemesis and Knucklebusters either pass the normal native agreement threshold
  or produce a precise, tested unsupported-variant reason.
- Comic Bakery exports natively with trace agreement at the normal gate.
- Defcom and Glider Rider share one Whittaker extractor and pass onset/pitch
  agreement fixtures.
- Knucklebusters full export completes in under one minute on the baseline
  development machine, with peak memory and elapsed time printed by the corpus
  command.

## Slice 6 — render-based automatic selection

### Implementation

- Complete the `sid-abtest` path in [`../PLAN.md`](../PLAN.md).
- Measure onset time, peak time, decay slope, release duration, pitch residual,
  spectral distance, centroid, and zero-crossing rate per note/voice.
- Compare native recipe, measured automation, and fallback candidates for the
  same fixture window.
- Select the cheapest editable representation that passes its class budget.
  Selection must be deterministic and reported in the census.
- Pin tool and renderer versions in every report.

### Exit gate

- The named fixture matrix runs unattended from SID input to ranked report.
- Candidate selection chooses the known-good representation on synthetic and
  captured fixtures.
- No tracked class regresses outside its confidence interval.
- The full 18-song exportable asset corpus remains schema-valid, and its worst
  render failures are listed with reproducible commands.

## Implementation order

1. Pin baselines and fix Sigma Seven, AWM, and Warhawk.
2. Complete release tails without relaxing the forward-model budget.
3. Add OSC3 causal attribution.
4. Improve stateful noise and combined-waveform programs.
5. Expand native coverage and remove long-trace scaling problems.
6. Make render A/B the final automatic representation gate.

Slices 1 and 5 may share diagnostics, but fixes land separately. Do not delay
release-tail correctness on Pertylizer sequence extensions, and do not guess an
OSC3 target when causal evidence is ambiguous.
