# Pertylizer patch dissection (Layer 1)

Reverse-engineered facts about `.ptz` patches, derived from the Pertylizer
example projects under `assets/examples/projects/` and cross-checked against
`project.schema.json`. The format-idiom bedrock for the SID exporter
(`docs/export.md`).

The schema and descriptor mirror was refreshed on 2026-07-27 from Pertylizer
commit `c89f672d`. See [`module-audit.md`](module-audit.md) for the complete
module capability review.

Authoritative reference file: **`SID Export v0 Test.json`** — handcrafted,
two SID-flavoured instruments, **validates with 0 errors against the current
schema** (re-confirmed 2026-05-27, i.e. post the 2026-05-22 schema change)
and plays in Pertylizer. Everything below is verified against it unless
marked otherwise.

## Connections are 2-string tuples, not objects

The COMPOSE.md guess (`{label, module_id, port}`) was wrong. The real shape:

```json
{
  "from": [
    "osc-1",
    "out"
  ],
  "to": [
    "amp-1",
    "in"
  ]
}
```

`from`/`to` are each a 2-element array `[module_id, port_name]`, both strings
(schema: `prefixItems: [string, string]`, `minItems/maxItems: 2`). Ports are
named string labels.

## Pitch and gate are IMPLICIT — no wiring needed

This was COMPOSE.md open questions #2 and #3, now answered:

- **Pitch**: the gold file wires *no* pitch/frequency CV. The oscillator
  carries a static `frequency: 440.0`, yet `Note.pitch = 72` plays a C5.
  Pertylizer drives oscillator pitch from the active note automatically.
  (The oscillator's only CV inputs are `fm` and `pwm` — there is no pitch
  input port to wire.)
- **Gate**: the envelope module has **no input ports at all** (`IN[-]`).
  Note-on/note-off drives the envelope implicitly. No gate connection exists
  or is needed.

So the smallest playable voice is just an audio path; the keyboard/gate plumbing
is handled by the engine.

## Smallest playable voice (the gold skeleton)

Four modules, three connections:

```
osc-1 (oscillator) --out--> amp-1.in
env-1 (envelope)   --out--> amp-1.cv
amp-1 (amplifier)  --out--> out-1.in
out-1 (stereo_output)
```

```json
"connections": [
{"from": ["osc-1", "out"], "to": ["amp-1", "in"]},
{"from": ["env-1", "out"], "to": ["amp-1", "cv"]},
{"from": ["amp-1", "out"], "to": ["out-1", "in"]}
]
```

That is the entire subtractive voice for v0. Add a `filter` between osc and amp
for post-v0.

## Port catalog per module type

Harvested from `connections` across all 13 example projects (OUT = ports seen
as a connection source, IN = as a destination). Covers the modules SID export
needs plus the common ones:

| Module type       | id prefix | OUT ports           | IN ports                  | 
|-------------------|-----------|---------------------|---------------------------|
| `oscillator`      | `osc-`    | `out, out_l, out_r` | `fm, pwm`                 |
| `noise`           | `nse-`    | `out`               | —                         |
| `sub_oscillator`  | `sub-`    | `out`               | —                         |
| `wavetable_osc`   | `wtb-`    | `out`               | `pos_cv`                  |
| `math_oscillator` | `mth-`    | `out`               | `fm, param_a`             |
| `envelope`        | `env-`    | `out`               | — (gate is implicit)      |
| `lfo`             | `lfo-`    | `out`               | —                         |
| `mseg`            | `msg-`    | `out`               | —                         |
| `filter`          | `flt-`    | `out`               | `in, cutoff_cv, res_cv`   |
| `amplifier`       | `amp-`    | `out, left, right`  | `in, cv, in_l, in_r`      |
| `ring_mod`        | `rng-`    | `out`               | `in`                      |
| `mixer`           | `mix-`    | `out`               | `in1, in2, in3, in4, in5` |
| `stereo_output`   | `out-`    | —                   | `in, in_l, in_r`          |
| `mod_matrix`      | `mmx-`    | (not graph-wired)   | (not graph-wired)         |

The full id-prefix → type map (38 module types seen): `add`=additive_osc,
`amp`=amplifier, `bdy`=body_resonance, `chr`=chorus, `cmp`=compressor,
`dly`=delay, `drf`=drift_generator, `dst`=distortion, `enc`=ensemble_chorus,
`env`=envelope, `equ`=eq, `euc`=euclidean, `flt`=filter, `frc`=fractal_osc,
`kbp`=keyboard_panner, `las`=la_synth, `lfo`=lfo, `mdr`=modal_resonator,
`mds`=mid_side, `mec`=mechanical_noise, `mix`=mixer, `mmx`=mod_matrix,
`msg`=mseg, `mth`=math_oscillator, `nse`=noise, `osc`=oscillator,
`out`=stereo_output, `pvc`=phase_vocoder, `rev`=reverb,
`rgr`=reverse_gate_reverb, `rgt`=random_gates, `rng`=ring_mod,
`shr`=shimmer_reverb, `sub`=sub_oscillator, `tur`=turing_machine,
`uvb`=univibe, `wsh`=waveshaper, `wtb`=wavetable_osc.

## Parameter blocks emitted in the gold file

`additionalProperties: false` on every parameter block, so emit only
schema-listed keys. The gold file emits these (all the keys, even defaults):

- **oscillator**: `anti_alias` (`"polyblep"`), `detune`, `fm_amt`, `fm_mode`
  (`"exponential"`), `frequency`, `level`, `pulse_width` (0..1), `uni_detune`,
  `uni_phase`, `uni_spread`, `unison`, `waveform` (`"pulse"`/`"triangle"`/…),
  `x_mod`.
- **envelope**: `atk_curve`, `attack`, `dec_curve`, `decay`, `rel_curve`,
  `release`, `sustain` (0..1), `vel_sens`.
- **amplifier**: `cv_bipolar`, `level`, `pan`.
- **stereo_output**: `dither`, `limit`, `master`, `mute`, `pan`.

Emission policy: the gold file emits **all** keys explicitly. Schema accepts
omitted keys (fall back to defaults), but emitting everything is the verified-safe
choice — do that for v0.

**Numeric parameter bounds bite even when the shape is right** (found while
building the v0 exporter against real Nemesis data):

- `oscillator.pulse_width` ∈ `[0.01, 0.99]`. SID PW=0 → `0.0` fails validation;
  clamp into the band.
- `envelope.attack`/`decay`/`release` are capped at `10.0` s. The SID
  decay/release table reaches 24 s — clamp.
- `noise` accepts a `type` key (color enum, e.g. `"white"`); emit it under the
  "all keys" policy.

Top-level `author` and `global` objects are **optional** (not in the root
`required`). The v0 exporter emits a static `global` block and omits top-level
`author` (not derivable from SID data); both validate.

## Note → pulse-width / waveform conventions

- `pulse_width` is **normalized 0.0–1.0**, not a 12-bit SID value. SID 50% =
  `0.5`; the gold lead uses `0.25`.
- `waveform` is a **string enum** (`"pulse"`, `"triangle"`, `"sawtooth"`,
  `"sine"`, …). Numeric form no longer validates (2026-05-22 schema change).
- Noise is a separate `noise` module, not an oscillator waveform.

## Song / pattern / track / note shape (current schema)

Confirmed against the gold file (arrays of objects, not keyed objects):

- `song.patterns`: **array** of `{id, name, length, notes[], automation[], next_note_id}`.
- `song.tracks`: **array** of `{id, name, instrument, volume, pan, mute, solo, color{r,g,b}, mode}`.
- `song.arrangement`: **array** of `{pattern_id, track_id, start, transpose, gain, length_override}`.
- Song-level counters: `next_pattern_id`, `next_track_id`.
- `song.row_resolution`: `{rows, ticks_per_row}` (editor grid; the gold file uses 240).
- `Note`: `{id, start, duration, pitch, velocity, instrument, track}`.
    - `start`/`duration` are **absolute ticks** (960 PPQN).
    - `pattern.length == rows * ticks_per_row` (3840 = 16 × 240 in the gold file).
    - `instrument: 0` (track binds the instrument); `track: null`.
    - `velocity`: f32 in 0.0–1.0 (gold uses 0.85 / 0.9).
- `SequencerTrack.pan`: **BipolarValue** (-1 left, 0 center, **0.0 = center**).
  The older memory note saying track pan is 0..1 with 0.5 center is **wrong**;
  the gold file uses `0.0`.

## Instrument shape

`{id, name, channel, volume, pan, muted, solo, key_range:[0,127], transpose,
oversampling, category (int), description, allocation_mode ("Polyphonic"),
stealing_strategy ("Oldest"), max_voices, velocity_amp_sensitivity,
velocity_filter_sensitivity, patch}`.

`patch.settings`: `{master_volume, bpm, octave_offset, glide_time,
canvas_size:{width,height}}` — the gold file omits `awe` and
`effect_chain_order` entirely and still validates and plays, so they are
safely omittable for v0.

## Resolved COMPOSE.md open questions

| # | Question                              | Answer                                                         |
|---|---------------------------------------|----------------------------------------------------------------|
| 1 | Port labels — strings or indices?     | **Strings** (`"out"`, `"in"`, `"cv"`, `"cutoff_cv"`)           |
| 2 | Per-voice pitch CV wired or implicit? | **Implicit** — no pitch wire; engine drives pitch              |
| 3 | Gate wired or implicit?               | **Implicit** — envelope has no input ports                     |
| 5 | Omitting `PatchSettings`?             | Partial `settings` works; `awe`/`effect_chain_order` omittable |
| 9 | Emit defaults or only non-defaults?   | Gold file emits **all** keys explicitly — do that              |

Still open (need a patch that uses them): Mod Matrix slot syntax (#6),
`ExposedPortState` semantics (#7), how the 8 instrument macros bind to module
params. The SID exporter's v0 doesn't need these; post-v0 PWM/filter work does.
