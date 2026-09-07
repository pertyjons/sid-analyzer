# Pertylizer module audit for SID export

Reviewed against Pertylizer commit `c89f672d` on 2026-07-27. The generated
descriptor catalog contains 75 module types. This audit classifies every type
by whether it can represent measured SID state without turning Pertylizer into
a SID-specific DAW.

The selection rule is state first: use a module only when its predicted
parameter or envelope trajectory matches the analyzed SID program. Spectral
approximations are candidates only behind the render gate.

## Already emitted

The exporter currently serializes these nine types:

| Module | Current role |
|---|---|
| `sid_oscillator` | SID waveform, pulse width, frequency, TEST, noise, sync, and ring |
| `envelope` | Ordinary reusable ADSR |
| `amplifier` | Envelope and measured level application |
| `filter` | Voice-local approximation of SID filtering |
| `mixer` | Voice graph summing |
| `stereo_output` | Instrument output |
| `distortion` | Calibrated timbre fallback |
| `eq` | Calibrated spectral correction |
| `script` | Deterministic control-rate SID register programs |

The refreshed schema adds the modulatable `envelope.time_scale` parameter. It
may reduce duplicate envelope definitions, but it must pass the same measured
envelope gate as direct attack, decay, sustain, and release parameters.

## Recommended general additions

| Priority | Module | SID-derived use | Important boundary |
|---|---|---|---|
| 1 | `mseg` | Repeatable measured envelopes that are not well represented by ADSR | A note trigger starts from zero, so rate carry and retrigger-from-current-level normally need measured automation |
| 2 | `lfo` | Periodic vibrato, PWM, filter sweeps, and tremolo | Free-running phase works directly; note-retriggered phase needs a `script` output driven by `gate_on` |
| 3 | `mod_matrix` | Scaled LFO/MSEG/Script routing to frequency, pulse width, cutoff, resonance, or level | Use only for a single causally identified destination |
| 4 | `kinetic_modulator` | One-shot, looping, or ping-pong pitch/PW/filter trajectories | Suitable only when one easing curve reproduces the measured program |
| 5 | `sampler` | Reconstructed `$D418` PCM and rendered one-shot transient fallback | Requires Pertylizer bundle/sample serialization; plain project JSON cannot carry the audio asset |

The first implementation ladder should therefore be:

```text
measured state
  -> ADSR
  -> MSEG
  -> LFO or Kinetic Modulator through direct CV / Mod Matrix
  -> Script
  -> sparse or dense automation
  -> Sampler fallback
```

## Conditional render-gated candidates

These modules can help a bounded sound class, but must not be selected from a
name or role heuristic alone.

| Modules | Possible use | Why they are not primary state representations |
|---|---|---|
| `transient_shaper`, `la_synth` | Percussion punch or an onset transient that survives the state-level envelope fit | Signal-dependent or synthetic transient shaping can alter overlapping tails |
| `delay` | A confidently identified repeated-note/echo relation | A delay effect can change voice ownership, polyphony, and tail length |
| `waveshaper`, `tilt_eq`, `crossover_splitter`, `ladder_filter` | Render-calibrated DAC, combined-waveform, or filter correction | They do not describe SID register or digital state directly |
| `compressor`, `limiter` | Output safety or a measured dynamics correction | Their signal-dependent gain is not SID envelope state |
| `audio_script` | A bounded deterministic DSP fallback for a target feature no native module can express | Lower editability and a risk of becoming a hidden SID instrument |
| `oscillator`, `wavetable_osc`, `additive_osc`, `math_oscillator`, `noise`, `sub_oscillator`, `vector_mixer` | Alternative spectral reconstruction when `sid_oscillator` has a documented unsupported case | They lose SID accumulator, LFSR, TEST, and combined-waveform semantics |
| `ring_mod`, `frequency_shifter` | Last-resort metallic spectral approximation | Neither implements SID's triangle-MSB ring semantics; use `sid_oscillator` cross-voice wiring when evidence exists |

The newly added `transient_shaper` resolves the earlier absence of a dedicated
attack/sustain shaper, but it belongs after the measured envelope candidate
fails a render budget, not before MSEG or automation.

## Not suitable for faithful automatic translation

These 42 types were reviewed and intentionally excluded from automatic SID
representation selection:

- Generative or uncontrolled modulation: `chaotic_osc`, `drift_generator`,
  `euclidean`, `random_gates`, `turing_machine`.
- Input, analysis, and visualization: `audio_input`, `beat_detector`,
  `envelope_follower`, `level_meter`, `oscilloscope`, `pitch_tracker`,
  `signal_monitor`, `spectrum_analyzer`.
- Unrelated synthesis models: `am_formant`, `body_resonance`, `fof`,
  `fooglers`, `formant_filter`, `fractal_osc`, `granular_osc`,
  `mechanical_noise`, `pad_synth`, `vocal_tract`, `voice_synth`.
- Stylistic, spatial, or spectral effects without a SID-state equivalent:
  `bbd_delay`, `chorus`, `convolver`, `ensemble_chorus`, `flanger`,
  `granular_fx`, `keyboard_panner`, `mid_side`, `modal_resonator`,
  `phase_vocoder`, `phaser`, `reverb`, `reverse_gate_reverb`,
  `shimmer_reverb`, `spatial_panner`, `spectral_blur`, `univibe`, `vocoder`.

An excluded module may still be chosen manually by a Pertylizer user. The
classification only prevents the analyzer from inventing production effects
that are not supported by SID data.

## Pertylizer discovery limitations found during the audit

`descriptors.json` catalogs parameters but not ports. The project schema also
accepts connection port names as arbitrary strings, so an offline exporter
cannot discover or validate port direction and signal type from the mirrored
artifacts alone. The audit had to read each module's Rust descriptor.

The LFO's `retrigger` parameter enables its `retrigger` input; it does not
restart automatically in `note_on()`. Per-note restart is possible with a
control `script` that emits `gate_on`, but the LFO descriptor currently suggests
an `Envelope Gate` source even though the envelope exposes gate only as an
input. This is a documentation/discoverability issue, not a missing engine
capability.
