# Pertylizer MCP feedback

This repository-local file is the system of record for every Pertylizer gap,
failure, and feature request discovered by sid-analyzer work. Do not write plans
or implementation changes into the Pertylizer repository unless the user asks
for that explicitly.

## 2026-07-19 — Arrangement placement controls

The project model supports `PatternPlacement.transpose`, `gain`, and
`length_override`, but the released MCP `place_pattern` input exposes only
`pattern_id`, `track_id`, and `start_beat`. This prevents MCP clients from
building the same compact transposed arrangements that direct `.ptz`
serialization supports.

The Pertylizer working tree already contains an in-progress implementation of
exact tick placement, transposition, gain, length override, and placement
updates in `synth_mcp`; those unrelated user changes were inspected but not
modified by sid-analyzer work. Once released, the MCP documentation should make
the optional fields visible in both single and batch placement examples.

## 2026-07-20 — Full-song fix analysis exceeds the mix render limit

`suggest_music_fixes` with `include_audio: true` on the 412-second Nemesis
project returned useful symbolic suggestions, but skipped its mix-bus analysis
because it forwarded the full arrangement duration to an analyzer capped at
300 seconds:

> mix-bus analyzer skipped: duration_seconds 412.01376 exceeds the 300-second maximum

Direct `analyze_mix_bus` calls over 20-second windows at multiple arrangement
positions succeeded, so this is an orchestration gap rather than a render or
project failure. `suggest_music_fixes` should automatically sample bounded,
representative windows (for example intro, highest-density section, and late
section), or clamp/chunk the requested duration and report the sampled ranges.
This would allow its mix rules to operate on long songs without requiring the
caller to reproduce the meta-analyzer manually.

## 2026-07-20 — `save_project` silently replaces `.ptz` with `.json`

Saving the loaded project with the explicit absolute path
`exports/Nemesis_the_Warlock_modern.ptz` returned success but actually
wrote `exports/Nemesis_the_Warlock_modern.json`. The tool schema documents the
path as `(.json)`, but the SID exporter and Pertylizer workflow use
`.ptz`, so silently replacing a caller-supplied extension is surprising
and makes round-tripping exported projects awkward.

`save_project` should either preserve supported `.ptz` paths, reject an
unsupported extension before writing, or document and return both the requested
and normalized path explicitly. Supporting `.ptz` directly is preferable
because it is the established project extension in this workflow.

## 2026-07-20 — Masking matrix ranks silent noise-floor tracks as perfect conflicts

`analyze_masking_matrix` over Nemesis bars 81–88 ranked several pairs at a
`conflict_score` of `1.0` even though both tracks were effectively silent in the
window (about `-85 dBFS`, with identical renderer/noise-floor band energies near
`2e-7`). The genuinely audible drum/lead masking pair appeared below these false
positives at `0.642`.

The analyzer should exclude tracks below a documented absolute RMS/LUFS or
band-energy floor before normalizing spectral overlap, and should return a
`tracks_below_floor` list so callers can distinguish silence from missing data.
Without this gate, top-N masking results on sparse arrangements can be dominated
by numerically identical silence rather than actionable conflicts.

## 2026-07-20 — Automation mutation item schemas hide required instrument IDs

During the Nemesis epic-patch rebuild, `clear_automation_lane` rejected
`{pattern_id, target}` with the misleading error that the rebuilt instrument had
no `amp-1`, even though `list_modules` and
`get_instrument_automation_targets` both showed that exact module and target.
Adding `instrument_id` to the item made the same clear operation succeed.

The exposed tool schema describes `items` only as `Array<unknown>`, so the
required disambiguating field is not discoverable. The item schema should expose
`pattern_id`, `instrument_id`, and `target` explicitly, and the validation error
should say that `instrument_id` is missing rather than incorrectly claiming the
module does not exist. The same concrete nested schemas should be exposed for
`set_parameter`, `set_track_send`, and effect-parameter tools, which required
similar trial-and-error over `level`, `param_name`, and top-level instrument
scope.

## 2026-07-20 — Instrument audio validation leaks solo state into the project

After running `validate_instrument_audio` for the Nemesis timpani, taiko, and
brass patches concurrently, `list_tracks` showed the timpani track left with
`solo: true`. Subsequent arrangement analyses therefore reported only timpani,
including silence in sections where that track had no notes. Clearing the solo
flag with `set_track_mixer` immediately restored the expected multi-track
renders.

Offline instrument validation must preserve all mixer mute/solo state even on
errors and concurrent calls. Prefer an isolated render graph; otherwise guard
temporary solo changes with scoped cleanup and serialize access to shared mixer
state. A regression test should run several validations concurrently and assert
that every track mixer flag is byte-for-byte unchanged afterward.

## 2026-07-20 — Full-arrangement masking silently analyzes only the first 300 seconds

Running `analyze_masking_matrix` without an explicit range on the 412-second
Nemesis arrangement reported the full arrangement scope in its top-level fields,
but every per-track render was clamped to the first 300 seconds. The warnings
explain the clamp, yet the returned `end_tick` and `end_bar` still describe the
full song, which makes the pair scores look full-arrangement-derived when the
last 112 seconds were not analyzed.

The tool should either select and report a representative 300-second window as
`suggest_music_fixes` does, aggregate multiple bounded windows, or return the
actual analyzed range in the main scope fields. A separate requested scope and
analyzed scope would make partial coverage unambiguous.

## 2026-07-20 — Return-send levels are not automation targets

While building transition-only reverse reverb, granular, and spectral-blur
effects for Nemesis, the exposed automation target DSL supported instrument
macros, module parameters, track mixer parameters, and global master volume,
but not a track's send level or a return bus's mute/volume. `set_track_send` can
only establish a static value, so creative effects intended for a few section
transitions had to remain at a very low constant send instead.

Expose return routing as automatable targets, for example
`track:Send:<return_id>` and `return:<return_id>:Volume`/`Mute`, and include them
in automation discovery. This would allow precise effect throws, reverse swells,
and temporary granular or spectral processing without editing notes or creating
duplicate tracks.

## 2026-07-20 — `set_track_send` silently clamps levels above unity

To create an exaggerated drum-hall experiment, `set_track_send` was called with
levels `1.2` and later `3.0`. Both calls returned `OK`, but `list_tracks` showed
the stored level was silently clamped to `1.0`; the `1.2` render was therefore
byte-for-metric identical to the prior unity-send render. The exposed schema
only describes `1.0` as unity and does not state that it is the maximum.

The schema should declare the accepted range, and the mutation should either
reject values above it or return the applied/clamped value with a warning. If
boosted sends are intentionally unsupported, make that explicit. For this task,
two duplicate hall returns were required to approximate a requested 3x send.

## 2026-07-20 — No dedicated transient-shaper effect

The Nemesis drum patches needed a slightly stronger attack without shortening
their existing frame-derived amplitude automation. `search_modules` for
`transient` returned no matches; a broader search for `attack` exposed only the
compressor. A slow, low-ratio parallel compressor was a workable substitute,
but it cannot independently control attack and sustain gain as precisely as a
transient shaper.

Add a transient-shaper effect with at least Attack, Sustain, sensitivity/window,
and Mix parameters. It would be useful for reconstructed drums whose detailed
amplitude lanes should remain intact while their macro punch and tail are tuned.

## 2026-07-20 — `build_instrument` accepts invalid parameters only as partial errors

While prototyping a discarded chord layer, `build_instrument` was given a
Ladder Filter parameter named `filter_type`. The instrument was still created
and the call returned `isError: false`, with the unknown parameter reported only
inside an `errors` array. This makes it easy for clients to treat a partially
configured patch as successful.

The tool should either reject and roll back an instrument containing unknown
parameters, or expose an explicit partial-success status at the top level. Its
schema could also point directly to `get_module_type_info` and note that
parameter names are module-specific and must match the returned names.

## 2026-07-20 — No automation-lane simplification tool

After converting imported SID automation to S-curves, the project contained
25,974 points and needed a safe redundancy audit. Pertylizer exposes lane read,
add, remove, clear, copy, offset, and scale operations, but no curve-aware
simplifier. `optimize_project` only removes unused project objects.

Add a read-only preview plus apply workflow for automation simplification. It
should support a normalized or native-unit error tolerance, preserve Step
segments and their boundary points, understand Linear/Exponential/SCurve
interpolation, report per-lane before/after counts and maximum error, and permit
filtering by pattern/target. A dry-run mode is important before rewriting large
frame-derived lanes.

## 2026-07-20 — Loading one shortened pattern also truncates its sibling at runtime

The Nemesis V13 project contains pattern 6 with `length_ticks: 619560` and
pattern 7 with `length_ticks: 245760`. After `load_project`, `list_patterns` and
`list_arrangement` report **both** patterns as 245760 ticks (256 beats / 64 bars).
The underlying notes and automation remain present: pattern 6 has notes through
beat 645.375 and pattern 7 through beat 564.5, with thousands of points after
beat 256. Loading the V12 project, where both serialized lengths are 619560,
reports both correctly at 645.375 beats. Reloading V13 reproduces the paired
truncation.

This is a persistence/runtime reconstruction bug, not missing musical data. A
loader regression test should create two long patterns on separate tracks,
shorten only one serialized pattern, round-trip it, and assert that each
pattern/placement retains its independent effective length. `lint_project`
should also warn whenever notes or automation extend beyond a pattern's active
length, since the current V13 state passes with zero warnings despite silently
inaudible content.

Follow-up narrowed the failure further: `set_pattern_length(pattern 6,
645.375)` made repeated `list_patterns`/`list_arrangement` calls report 619560
ticks, but the immediately following `save_project` serialized pattern 6 back
as 245760 ticks. Pattern 7, repaired in the same session, serialized correctly.
Directly changing only pattern 6's JSON `length` to 619560 and loading that file
made both runtime patterns and placements correct and restored audible bass
after beat 256. The save path is therefore reading stale/divergent song state
for at least some pattern-length edits; this is not solely a loader bug.

Further observation showed the GUI/runtime later forced pattern ID 6 back to
245760 even when its project-file JSON remained 619560. Pattern 7 stayed long.
Pattern 6's metadata, processors, track, and placement were structurally
identical to the stable V12 version, suggesting stale state keyed by pattern ID.
A working workaround was to duplicate pattern 6 to new ID 11, set ID 11 to
619560, remove ID 6's placement, and place ID 11 on the same track with an
explicit 619560-tick `length_override`. Save/reload and eight subsequent runtime
checks all kept ID 11 and its placement at 645.375 beats.

## 2026-07-27 — Schema artifacts do not expose module ports

The generated `descriptors.json` contains the complete parameter catalog but no
module ports. `project.schema.json` validates connection tuples structurally but
accepts arbitrary port-name strings. An offline project producer therefore
cannot discover or validate port direction, signal type, or compatibility from
the mirrored artifacts and must read Pertylizer source or query a live MCP
session.

Include each module's ports in the generated descriptor catalog, with name,
direction, signal type, and description. This would let sid-analyzer validate
LFO, MSEG, Script, Mod Matrix, and SID-oscillator graphs at build time without a
runtime Pertylizer dependency.

## 2026-07-27 — LFO note retrigger is possible but poorly discoverable

The LFO's `retrigger` parameter only enables rising-edge handling on its
`retrigger` input; `Lfo::note_on()` is intentionally empty. A per-note restart
can be built with a control Script that emits the `gate_on` context value into
the LFO port, so this is not an engine capability gap.

The descriptor nevertheless suggests connecting `Envelope Gate`, while the
Envelope exposes gate only as an input and has no gate output. Document the
Script `gate_on` adapter in the LFO port description, or add a standard
per-voice note-gate source/output so the common retrigger graph does not require
an otherwise empty Script module.

## 2026-07-27 — Dedicated transient shaper is now available

The earlier 2026-07-20 feedback requesting a transient shaper is resolved in
Pertylizer commit `c89f672d`: module `transient_shaper` (`tsh`) now exposes
Attack, Sustain, Sensitivity, Window, and Mix. For SID export it should remain a
render-gated percussion correction after measured envelope fitting, because its
signal-dependent detector is not a replacement for SID envelope state.

## 2026-07-29 — Rendering exists, but unattended project rendering has no one-shot CLI

This is not a missing renderer or MCP-tool gap. Pertylizer exposes
`render_to_wav` through MCP, and `crates/pertylizer/src/mcp_bridge.rs` contains
the tested `render_to_wav_impl`. The `pertylizer --headless` entry point starts
a long-lived stdio JSON-RPC server, however; it does not provide a stable
one-command interface that loads a `.ptz`, renders a named tap for a fixed
duration and sample rate, writes a WAV, reports its renderer revision, and
exits with a typed status.

That makes deterministic external A/B runners unnecessarily implement an MCP
client and session lifecycle. `sid-abtest` can consume pre-rendered WAV files
and now defines a temporary version-1 external renderer argument protocol, but
its automatic SID-to-project gate cannot be closed against Pertylizer itself
until a supported entry point exists. Add a one-shot command backed by the
existing project loader and offline arrangement renderer, with explicit input,
output, sample rate, duration/source span, tap, deterministic seed, tail,
normalization, and machine-readable result fields. This entry is the local
implementation-plan record for that request.

## 2026-08-08 — Headless render silently skips a master effect with an invalid module ID

While calibrating a Nemesis master limiter, a schema-valid effect with type
`limiter` and ID `lim-1` produced a successful render receipt with no warning,
but had no audible or measured effect. Pertylizer's canonical short key is
`lmt`; changing only the ID to `lmt-1` made the limiter work. This was exporter
misuse rather than a missing limiter or broken render chain.

The diagnostic gap is in project application: `project_apply.rs` uses
`fx.id.parse::<ModuleId>()` and silently `continue`s when parsing fails. The
headless render receipt therefore cannot distinguish a fully reconstructed
project from one that dropped modules. Project load/render should report the
effect path, invalid ID, expected module short key, and whether the module was
skipped. The concrete acceptance requirements are tracked in this entry.

## 2026-08-08 — Load diagnostics caught exporter IDs, but the working tree did not build

The first `synth-modern` Nemesis render used valid module types but noncanonical
IDs (`wav-1`, `cho-1`, `noi-1`, and named master-effect instances). The latest
previously-built `pertylizer render` binary loaded and rendered the project while
reporting every affected path plus the expected short keys (`wtb`, `chr`, `nse`,
and numeric instances). This confirms that the project-apply diagnostics plan is
closing the earlier silent-skip gap and gave the exporter enough information to
correct its output.

Rebuilding the current Pertylizer working tree failed before rendering because
`gui/egui_backend/project_flow.rs` calls `log_load_diagnostics` without that
function being in scope. This appears to be an incomplete concurrent change,
not a missing render capability; the existing binary was a safe read-only
fallback. No separate plan was opened because the preceding local entry already
owns the work and the compile break is local to its in-progress implementation.

## 2026-08-27 — The synth MCP catalog was unavailable in this Codex session

The enabled tool inventory was searched for both `synth` and `pertylizer`, but
contained no Pertylizer MCP tools. It was therefore impossible to run the normal
`list_module_types` / `search_modules` / `get_module_type_info` verification
loop. This is a session/tool-exposure gap, not evidence that the Pertylizer
server lacks those capabilities. The work used the pinned schema/descriptors
and the one-shot headless renderer instead.

The Codex integration should expose the configured `synth` server or return a
typed connection diagnostic that names the missing server, endpoint, and setup
action. An absent catalog is otherwise indistinguishable from a server with no
tools.

## 2026-08-27 — A shared SID filter needs construction, routing, and automation

Pertylizer can persist a return bus and apply a static track send. The project
schema also accepts a Filter in the return's effect chain, but
`module_factory::create_effect` cannot construct it: project application emits
`unsupported-module-type` and drops the saved effect. Its `AutomationTarget`
can target only instrument, instrument-module, track, and global parameters.
There is no target for a track-send level, return fader, or return-effect
parameter, even though `EngineCommand::SetReturnEffectParameter` proves that
the runtime effect path already accepts parameter changes.

The sid-analyzer exporter now uses one true shared return filter when routing
changes only at static track/note boundaries and the cutoff/resonance/mode are
constant. A cutoff sweep or an in-track route change must still fall back to
duplicated per-instrument filters. Add RT-safe `TrackSend`, `Return`, and
`ReturnEffect` automation targets with live/offline parity, persistence, target
discovery, and MCP validation. Static Filter construction must land first and
must have warning-free project-load and headless-render coverage.

## 2026-08-27 — Native 6581 pulse+triangle remains spectrally too dark

Yie Ar Kung Fu II, subtune 1, voice 1 was rendered for ten seconds through one
native `sid_oscillator` with the actual pulse+triangle mask and compared with
the SID reference. Replacing two summed oscillators with the combined bus model
improved RMS error from `0.06755` to `0.00573`, pitch error from `5.383 Hz` to
`0`, and log-spectral distance from `10.278` to `2.254`. The remaining centroid
error is `689.8 Hz` and rolloff error `2121.0 Hz`, so the current 6581
`neighbour_support` fit is still materially too dark.

This needs a measured, frequency- and pulse-width-spanning calibration set for
the native combined-waveform model rather than another exporter EQ workaround.
This entry is the local implementation-plan record for that request.

## 2026-08-28 — SID vibrato needs a distinct onset-hold representation

SID drivers commonly keep pitch stable for a measured interval and then start
a full-depth periodic contour. Pertylizer's persisted per-note
`Vibrato.delay` is a linear depth fade-in: the LFO phase advances from note
onset while the modulation depth ramps. Reinterpreting that field as a hold
would silently change existing project semantics.

Add a versioned representation that distinguishes onset hold from depth
fade-in, either as a separate `onset_delay` field or as an explicit timing
object. The engine, schema, MCP construction/discovery, live playback, and
headless renderer must agree. Until that contract exists, sid-analyzer uses
placement-relative track-pitch automation for delayed contours and reserves
per-note vibrato for zero-delay cases.

## 2026-08-28 — Saved connections are acknowledged before graph validation

`SynthSession::connect` reports success after queue insertion, so project
application increments its installed-connection count before
`ModuleGraph::connect` validates endpoints, port direction/type, or cycles.
`SynthEngine::handle_connect` then discards the `GraphError`. A saved cable can
therefore be reported as installed, disappear from the rendered graph, and be
lost on a later save without a typed diagnostic.

The fix must be atomic across project hydration, the GUI, MCP, and the render
graph. It must distinguish installable audio/control cables from intentionally
editor-only visualizer or `SignalMonitor` cables, preserve typed endpoint/type
and cycle failures, increment counts only after acceptance, and prevent rejected
GUI cables from remaining drawn. An endpoint-only precheck is insufficient
because it cannot validate cycles and would falsely reject editor-only cables.

## 2026-08-28 — 6581 pulse+saw should collapse to the measured floor

The source-aligned `0x61` matrix was remeasured at A1, A2, and A4 over the
sustained 1–2 second window. The SID references are zero or about `4.35e-6`
RMS. Pertylizer's current one-code MSB residue renders at roughly
`0.0097..0.0101` RMS with a `0.323` peak. A total digital-bus collapse, followed
by the existing default DC blocker, matches the reference floor at all three
pitches. The sid-analyzer render matrix now accepts that target and rejects the
current residue.

Change the 6581 pulse+saw combined-bus rule only when Pertylizer work is
explicitly requested. Add an exhaustive digital-bus unit test, a rendered
default-DC-block test, and retain a loud 8580 control. The related 6581
triangle+saw row remains uncalibrated because the current comparison mixes
oscillator shape with export/mix level; do not fit it from these receipts.

## 2026-08-28 — Exact ring-edge timing worsened the source-aligned control

A reviewed additive `msb_edge -> ring_edge` prototype placed the ring fold at
the exact 4x sub-sample slot. It made the 6581 sustained-window
log-spectral distance worse, from `7.896` to `13.302` dB, and the full 0–2
second window worse, from `11.504` to `17.711` dB. Linear 6581-only fold slews
at 10.4, 20.8, and 50 microseconds did not recover the regression. The
prototype and its sid-analyzer cable were removed.

Do not add an oversampled ring-edge port from this hypothesis. A future model
must jointly control oscillator phase initialization, output level, and the
analog fold-transfer shape, then pass both source-aligned windows before any DSP
or project-port change is retained.

## 2026-08-31 — 6581 triangle+saw needs direct-module, frequency-dependent calibration

The 6581 `0x31` calibration matrix now separates absolute level from
gain-normalized spectral shape. Over the sustained 1–2 second window,
Pertylizer is `19.38 dB` too loud at A1, `19.74 dB` at A2, and `21.95 dB` at
A4. Removing total gain from the four-band comparison still leaves `3.86`,
`13.77`, and `16.30 dB` of shape error. PW `0x0200`, `0x0800`, and `0x0E00`
produce byte-identical renders at each pitch, so the native oscillator correctly
ignores pulse width when the pulse bit is inactive.

The named real control is Japanese by Ben Daglish and Max Hall, subtune 1,
voice 3, 111–112 seconds; its triangle+saw transient occurs at frame 5590. The
unadjusted export is `8.33 dB` too loud with `5.18 dB` normalized-shape error.
A temporary project-only instrument trim reduces the level error to `0.12 dB`
but leaves `5.35 dB` normalized-shape error. The synthetic and real windows
therefore require incompatible level corrections, and level matching does not
repair the waveform shape. sid-analyzer deliberately retains its current
export and lowering behavior.

The next target-engine experiment should:

1. provide a headless direct-output tap for `sid_oscillator`, bypassing the
   envelope, track, master chain, and limiter, or an equivalent render mode
   whose gain stages are individually pinned;
2. replay the checked-in A1/A2/A4 and PW-control fixtures against that tap and
   record both absolute level and gain-normalized shape;
3. fit the 6581 triangle+saw combined bus as a frequency-dependent response,
   keeping pulse-width invariance explicit, instead of adding exporter EQ or a
   single global gain constant;
4. retain exhaustive digital-bus tests, add rendered A1/A2/A4 checks, and keep
   8580 combined-waveform controls unchanged; and
5. accept the model only if every synthetic residual and the Japanese window
   improve without regressing pulse+saw, pulse+triangle, ring, or simple-wave
   controls.

A CLI/MCP render option that selects a module output tap and reports the active
gain path would make this calibration reproducible without editing temporary
project volumes. This is the concrete Pertylizer tooling request exposed by the
matrix; no Pertylizer repository files were changed during this work.
