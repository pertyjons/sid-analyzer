//! Modern Pertylizer sound profiles layered on the analyzer's musical model.
//!
//! Profiles own synthesis recipes and mix treatment. The parent exporter owns
//! notes, recovered song structure, expression, and automation. Keeping that
//! boundary explicit lets a new sound variant restyle the same analyzed
//! program without duplicating SID decoding or arrangement lowering.

use super::*;

trait ModernProfile {
    fn recipe(&self, plan: &TrackPlan<'_>) -> VoiceRecipe;
    fn global_state(&self) -> GlobalProjectState;
}

struct ModernAnalog;

const MODERN_INSTRUMENT_GAIN: f32 = 2.6;

impl ModernProfile for ModernAnalog {
    fn recipe(&self, plan: &TrackPlan<'_>) -> VoiceRecipe {
        let arpeggiated = plan_arpeggiated(plan);
        match plan_role(plan) {
            InstrumentRole::Bass => VoiceRecipe::bass(),
            InstrumentRole::Lead if arpeggiated => VoiceRecipe::arp_lead(plan.shape.waveform),
            InstrumentRole::Lead => VoiceRecipe::lead(plan.shape.waveform),
            InstrumentRole::Pad => VoiceRecipe::pad(),
            InstrumentRole::Stab => VoiceRecipe::stab(),
            InstrumentRole::Bell => VoiceRecipe::bell(),
            InstrumentRole::Drum(subclass) => VoiceRecipe::drum(subclass),
            InstrumentRole::Untagged if plan.drum_drop || plan.shape.is_noise => {
                VoiceRecipe::drum(DrumSubclass::PercMetallic)
            }
            InstrumentRole::Untagged => VoiceRecipe::untagged(plan.shape.waveform),
        }
    }

    fn global_state(&self) -> GlobalProjectState {
        modern_analog_global_state()
    }
}

#[derive(Clone, Copy)]
struct VoiceRecipe {
    primary: OscillatorRecipe,
    secondary: SecondaryRecipe,
    filter: FilterRecipe,
    effects: EffectRecipe,
    volume_trim: f32,
}

impl VoiceRecipe {
    fn bass() -> Self {
        Self {
            primary: OscillatorRecipe::analog("sawtooth", 2.0, 7.0, 0.18, 0.72),
            secondary: SecondaryRecipe::Sub {
                octave: "minus1",
                waveform: "triangle",
                level: 0.42,
            },
            filter: FilterRecipe::lowpass(880.0, 0.18, 1.8, 0.48),
            effects: EffectRecipe::Bass,
            volume_trim: 1.0,
        }
    }

    fn lead(source_waveform: &'static str) -> Self {
        Self {
            primary: OscillatorRecipe::analog(
                modern_waveform(source_waveform),
                3.0,
                12.0,
                0.62,
                0.60,
            ),
            secondary: SecondaryRecipe::Wavetable {
                table: "digital",
                position: 0.36,
                octave: 0.0,
                detune: 7.0,
                level: 0.32,
            },
            filter: FilterRecipe::lowpass(3_600.0, 0.27, 1.35, 0.32),
            effects: EffectRecipe::Lead,
            volume_trim: 0.90,
        }
    }

    fn arp_lead(source_waveform: &'static str) -> Self {
        Self {
            primary: OscillatorRecipe::analog(
                modern_waveform(source_waveform),
                2.0,
                9.0,
                0.45,
                0.58,
            ),
            secondary: SecondaryRecipe::Wavetable {
                table: "harmonics",
                position: 0.58,
                octave: 1.0,
                detune: -5.0,
                level: 0.23,
            },
            filter: FilterRecipe::lowpass(4_800.0, 0.16, 1.15, 0.38),
            effects: EffectRecipe::Arp,
            volume_trim: 0.82,
        }
    }

    fn pad() -> Self {
        Self {
            primary: OscillatorRecipe::analog("sawtooth", 4.0, 18.0, 0.82, 0.46),
            secondary: SecondaryRecipe::Wavetable {
                table: "warm",
                position: 0.42,
                octave: 0.0,
                detune: -8.0,
                level: 0.34,
            },
            filter: FilterRecipe::lowpass(2_200.0, 0.20, 1.25, 0.50),
            effects: EffectRecipe::Pad,
            volume_trim: 0.76,
        }
    }

    fn stab() -> Self {
        Self {
            primary: OscillatorRecipe::analog("sawtooth", 2.0, 10.0, 0.36, 0.62),
            secondary: SecondaryRecipe::Wavetable {
                table: "basic",
                position: 0.72,
                octave: 1.0,
                detune: 0.0,
                level: 0.20,
            },
            filter: FilterRecipe::lowpass(2_800.0, 0.34, 1.55, 0.44),
            effects: EffectRecipe::Stab,
            volume_trim: 0.86,
        }
    }

    fn bell() -> Self {
        Self {
            primary: OscillatorRecipe::analog("triangle", 1.0, 0.0, 0.0, 0.58),
            secondary: SecondaryRecipe::Wavetable {
                table: "harmonics",
                position: 0.78,
                octave: 1.0,
                detune: 3.0,
                level: 0.30,
            },
            filter: FilterRecipe::lowpass(6_200.0, 0.12, 1.0, 0.24),
            effects: EffectRecipe::Bell,
            volume_trim: 0.80,
        }
    }

    fn drum(subclass: DrumSubclass) -> Self {
        let (waveform, octave, noise_level, cutoff, resonance, trim) = match subclass {
            DrumSubclass::Kick => ("sine", -24.0, 0.10, 1_500.0, 0.10, 1.05),
            DrumSubclass::Tom => ("triangle", -12.0, 0.16, 2_200.0, 0.18, 0.96),
            DrumSubclass::Snare => ("triangle", 0.0, 0.72, 7_200.0, 0.24, 0.88),
            DrumSubclass::HihatClosed => ("square", 12.0, 0.92, 11_000.0, 0.16, 0.70),
            DrumSubclass::HihatOpen => ("square", 12.0, 0.88, 10_500.0, 0.14, 0.72),
            DrumSubclass::PercMetallic => ("square", 0.0, 0.60, 8_600.0, 0.36, 0.78),
        };
        Self {
            primary: OscillatorRecipe::analog(waveform, 1.0, 0.0, 0.0, 0.56).octave(octave),
            secondary: SecondaryRecipe::Noise {
                kind: "chip",
                level: noise_level,
            },
            filter: FilterRecipe::lowpass(cutoff, resonance, 1.45, 0.18),
            effects: EffectRecipe::Drum,
            volume_trim: trim,
        }
    }

    fn untagged(source_waveform: &'static str) -> Self {
        Self {
            primary: OscillatorRecipe::analog(
                modern_waveform(source_waveform),
                2.0,
                6.0,
                0.20,
                0.62,
            ),
            secondary: SecondaryRecipe::Wavetable {
                table: "basic",
                position: 0.50,
                octave: 0.0,
                detune: -4.0,
                level: 0.24,
            },
            filter: FilterRecipe::lowpass(3_200.0, 0.18, 1.25, 0.30),
            effects: EffectRecipe::Utility,
            volume_trim: 0.86,
        }
    }
}

#[derive(Clone, Copy)]
struct OscillatorRecipe {
    waveform: &'static str,
    octave: f32,
    unison: f32,
    unison_detune: f32,
    unison_spread: f32,
    level: f32,
}

impl OscillatorRecipe {
    fn analog(
        waveform: &'static str,
        unison: f32,
        unison_detune: f32,
        unison_spread: f32,
        level: f32,
    ) -> Self {
        Self {
            waveform,
            octave: 0.0,
            unison,
            unison_detune,
            unison_spread,
            level,
        }
    }

    fn octave(mut self, octave: f32) -> Self {
        self.octave = octave;
        self
    }
}

#[derive(Clone, Copy)]
enum SecondaryRecipe {
    Wavetable {
        table: &'static str,
        position: f32,
        octave: f32,
        detune: f32,
        level: f32,
    },
    Sub {
        octave: &'static str,
        waveform: &'static str,
        level: f32,
    },
    Noise {
        kind: &'static str,
        level: f32,
    },
}

#[derive(Clone, Copy)]
struct FilterRecipe {
    cutoff: f32,
    resonance: f32,
    drive: f32,
    key_track: f32,
}

impl FilterRecipe {
    fn lowpass(cutoff: f32, resonance: f32, drive: f32, key_track: f32) -> Self {
        Self {
            cutoff,
            resonance,
            drive,
            key_track,
        }
    }
}

#[derive(Clone, Copy)]
enum EffectRecipe {
    Bass,
    Lead,
    Arp,
    Pad,
    Stab,
    Bell,
    Drum,
    Utility,
}

pub(super) fn apply_modern_analog(
    plans: &[TrackPlan<'_>],
    instruments: &mut [Instrument],
    song: &mut Song,
) {
    let profile = ModernAnalog;
    if plans.is_empty() {
        if let Some(instrument) = instruments.first_mut() {
            restyle_instrument(instrument, VoiceRecipe::untagged("pulse"));
        }
        return;
    }
    for (plan, instrument) in plans.iter().zip(instruments.iter_mut()) {
        restyle_instrument(instrument, profile.recipe(plan));
    }
    remap_automation(song);
}

pub(super) fn global_state() -> GlobalProjectState {
    ModernAnalog.global_state()
}

fn modern_analog_global_state() -> GlobalProjectState {
    GlobalProjectState {
        master_volume: 1.15,
        octave_offset: 0,
        glide_time: 0.0,
        return_bus_effects: Vec::new(),
        master_effects: vec![
            compressor_module("cmp-1", -12.0, 2.4, 12.0, 180.0, 1.5, 1.0),
            Module {
                id: "equ-1",
                kind: "eq",
                position: Position { x: 200.0, y: 0.0 },
                scripts: None,
                parameters: Parameters::Eq(EqParams {
                    low_freq: 95.0,
                    low_gain: 1.4,
                    mid_freq: 1_800.0,
                    mid_gain: -0.8,
                    mid_q: 0.72,
                    high_freq: 8_500.0,
                    high_gain: 1.1,
                    mix: 1.0,
                }),
            },
            Module {
                id: "lmt-1",
                kind: "limiter",
                position: Position { x: 400.0, y: 0.0 },
                scripts: None,
                parameters: Parameters::Limiter(LimiterParams {
                    ceiling: -0.6,
                    look_ahead: 3.0,
                    release: 90.0,
                    mix: 1.0,
                }),
            },
        ],
    }
}

fn restyle_instrument(instrument: &mut Instrument, recipe: VoiceRecipe) {
    let old_modules = std::mem::take(&mut instrument.patch.modules);
    let mut envelopes = Vec::new();
    let mut amplifiers = Vec::new();
    let mut output = None;
    for mut module in old_modules {
        match module.kind {
            "envelope" => {
                module.position = Position {
                    x: 530.0,
                    y: 280.0 + envelopes.len() as f32 * 110.0,
                };
                envelopes.push(module);
            }
            "amplifier" => {
                module.position = Position {
                    x: 760.0,
                    y: amplifiers.len() as f32 * 150.0,
                };
                amplifiers.push(module);
            }
            "stereo_output" => {
                module.position = Position {
                    x: 1_030.0,
                    y: 80.0,
                };
                output = Some(module);
            }
            _ => {}
        }
    }

    let mut modules = vec![oscillator_module(recipe.primary)];
    modules.push(secondary_module(recipe.secondary));
    modules.push(mixer_module());
    modules.push(filter_module(recipe.filter));
    modules.append(&mut envelopes);
    modules.append(&mut amplifiers);
    if let Some(output) = output {
        modules.push(output);
    }
    let (effects, effect_order) = effect_modules(recipe.effects);
    modules.extend(effects);

    let dual_amplifier = modules
        .iter()
        .filter(|module| module.kind == "amplifier")
        .count()
        >= 2
        && modules
            .iter()
            .filter(|module| module.kind == "envelope")
            .count()
            >= 2;
    let secondary_is_noise = matches!(recipe.secondary, SecondaryRecipe::Noise { .. });
    instrument.patch.connections = if dual_amplifier && secondary_is_noise {
        dual_path_connections()
    } else {
        layered_connections(recipe.secondary)
    };
    instrument.patch.modules = modules;
    instrument.patch.settings.effect_chain_order = effect_order;
    instrument.patch.settings.canvas_size = CanvasSize {
        width: 1_680.0,
        height: 720.0,
    };
    instrument.volume = (instrument.volume * recipe.volume_trim * MODERN_INSTRUMENT_GAIN).min(1.2);
    instrument.name = format!("{} · Modern Analog", instrument.name);
    instrument.patch.name = instrument.name.clone();
    instrument.description =
        "Modern Analog profile: role-aware layered synthesis from analyzed SID music data."
            .to_string();
    instrument.patch.description = "Editable modern profile: layered oscillator, filter, analyzed envelopes, and role effects.";
}

fn remap_automation(song: &mut Song) {
    for pattern in &mut song.patterns {
        pattern.automation.retain_mut(|lane| {
            let AutomationTarget::Module(target) = &mut lane.target else {
                return true;
            };
            if target.module.module_type == "sid_oscillator" && target.module.param_id == "pw_reg" {
                target.module.module_type = "oscillator";
                target.module.param_id = "pulse_width";
            }
            target.module.module_type != "sid_oscillator"
        });
    }
}

fn modern_waveform(source: &'static str) -> &'static str {
    match source {
        "pulse" => "pulse",
        "triangle" => "triangle",
        "sawtooth" => "sawtooth",
        _ => "sawtooth",
    }
}

fn oscillator_module(recipe: OscillatorRecipe) -> Module {
    Module {
        id: "osc-1",
        kind: "oscillator",
        position: Position { x: 30.0, y: 20.0 },
        scripts: None,
        parameters: Parameters::Oscillator(OscillatorParams {
            anti_alias: "polyblep",
            detune: 0.0,
            fm_amt: 0.0,
            fm_mode: "exponential",
            frequency: 440.0,
            glide_time: 0.0,
            level: recipe.level,
            octave: recipe.octave,
            phase: 0.0,
            pulse_width: 0.48,
            uni_detune: recipe.unison_detune,
            uni_phase: 0.0,
            uni_spread: recipe.unison_spread,
            unison: recipe.unison,
            waveform: recipe.waveform,
            x_mod: 0.0,
        }),
    }
}

fn secondary_module(recipe: SecondaryRecipe) -> Module {
    match recipe {
        SecondaryRecipe::Wavetable {
            table,
            position,
            octave,
            detune,
            level,
        } => Module {
            id: "wtb-1",
            kind: "wavetable_osc",
            position: Position { x: 30.0, y: 160.0 },
            scripts: None,
            parameters: Parameters::WavetableOsc(WavetableOscParams {
                detune,
                glide_time: 0.0,
                level,
                octave,
                position,
                table,
            }),
        },
        SecondaryRecipe::Sub {
            octave,
            waveform,
            level,
        } => Module {
            id: "sub-1",
            kind: "sub_oscillator",
            position: Position { x: 30.0, y: 160.0 },
            scripts: None,
            parameters: Parameters::SubOscillator(SubOscillatorParams {
                glide_time: 0.0,
                level,
                octave,
                waveform,
            }),
        },
        SecondaryRecipe::Noise { kind, level } => Module {
            id: "nse-1",
            kind: "noise",
            position: Position { x: 30.0, y: 160.0 },
            scripts: None,
            parameters: Parameters::Noise(NoiseParams { level, kind }),
        },
    }
}

fn mixer_module() -> Module {
    Module {
        id: "mix-1",
        kind: "mixer",
        position: Position { x: 250.0, y: 80.0 },
        scripts: None,
        parameters: Parameters::Mixer(MixerParams {
            input_1: 1.0,
            input_2: 1.0,
            input_3: 1.0,
            input_4: 1.0,
            input_5: 1.0,
            input_6: 1.0,
            input_7: 1.0,
            input_8: 1.0,
            master: 0.82,
        }),
    }
}

fn filter_module(recipe: FilterRecipe) -> Module {
    Module {
        id: "flt-1",
        kind: "filter",
        position: Position { x: 500.0, y: 80.0 },
        scripts: None,
        parameters: Parameters::Filter(FilterParams {
            cutoff: recipe.cutoff,
            cv_amt: 0.0,
            drive: recipe.drive,
            env_amt: 0.0,
            key_track: recipe.key_track,
            model: "fluid",
            morph: 0.0,
            resonance: recipe.resonance,
            kind: "lowpass",
        }),
    }
}

fn layered_connections(secondary: SecondaryRecipe) -> Vec<Connection> {
    let secondary_id = match secondary {
        SecondaryRecipe::Wavetable { .. } => "wtb-1",
        SecondaryRecipe::Sub { .. } => "sub-1",
        SecondaryRecipe::Noise { .. } => "nse-1",
    };
    vec![
        Connection {
            from: ["osc-1", "out"],
            to: ["mix-1", "in1"],
        },
        Connection {
            from: [secondary_id, "out"],
            to: ["mix-1", "in2"],
        },
        Connection {
            from: ["mix-1", "out"],
            to: ["flt-1", "in"],
        },
        Connection {
            from: ["flt-1", "out"],
            to: ["amp-1", "in"],
        },
        Connection {
            from: ["env-1", "out"],
            to: ["amp-1", "cv"],
        },
        Connection {
            from: ["amp-1", "out"],
            to: ["out-1", "in"],
        },
    ]
}

fn dual_path_connections() -> Vec<Connection> {
    vec![
        Connection {
            from: ["osc-1", "out"],
            to: ["flt-1", "in"],
        },
        Connection {
            from: ["flt-1", "out"],
            to: ["amp-1", "in"],
        },
        Connection {
            from: ["env-1", "out"],
            to: ["amp-1", "cv"],
        },
        Connection {
            from: ["nse-1", "out"],
            to: ["amp-2", "in"],
        },
        Connection {
            from: ["env-2", "out"],
            to: ["amp-2", "cv"],
        },
        Connection {
            from: ["amp-1", "out"],
            to: ["mix-1", "in1"],
        },
        Connection {
            from: ["amp-2", "out"],
            to: ["mix-1", "in2"],
        },
        Connection {
            from: ["mix-1", "out"],
            to: ["out-1", "in"],
        },
    ]
}

fn effect_modules(recipe: EffectRecipe) -> (Vec<Module>, Vec<&'static str>) {
    match recipe {
        EffectRecipe::Bass => (
            vec![compressor_module(
                "cmp-1", -16.0, 3.2, 7.0, 130.0, 1.0, 0.78,
            )],
            vec!["cmp-1"],
        ),
        EffectRecipe::Lead => (
            vec![
                chorus_module("chr-1", 0.34, 0.42, 0.32, 3.0),
                delay_module("dly-1", 0.28, 0.24, 0.34, "ping_pong"),
            ],
            vec!["chr-1", "dly-1"],
        ),
        EffectRecipe::Arp => (
            vec![
                chorus_module("chr-1", 0.22, 0.25, 0.48, 2.0),
                delay_module("dly-1", 0.36, 0.30, 0.24, "ping_pong"),
            ],
            vec!["chr-1", "dly-1"],
        ),
        EffectRecipe::Pad => (
            vec![
                ensemble_module(),
                reverb_module("rev-1", 0.42, 0.62, 0.76, 0.90),
            ],
            vec!["enc-1", "rev-1"],
        ),
        EffectRecipe::Stab => (
            vec![
                chorus_module("chr-1", 0.18, 0.22, 0.28, 2.0),
                reverb_module("rev-1", 0.18, 0.34, 0.58, 0.72),
            ],
            vec!["chr-1", "rev-1"],
        ),
        EffectRecipe::Bell => (
            vec![
                delay_module("dly-1", 0.32, 0.26, 0.45, "stereo"),
                reverb_module("rev-1", 0.34, 0.55, 0.82, 0.88),
            ],
            vec!["dly-1", "rev-1"],
        ),
        EffectRecipe::Drum => (
            vec![compressor_module("cmp-1", -14.0, 4.0, 4.0, 95.0, 1.2, 0.88)],
            vec!["cmp-1"],
        ),
        EffectRecipe::Utility => (
            vec![chorus_module("chr-1", 0.16, 0.18, 0.25, 2.0)],
            vec!["chr-1"],
        ),
    }
}

pub(super) fn chorus_module(
    id: &'static str,
    depth: f32,
    mix: f32,
    rate: f32,
    voices: f32,
) -> Module {
    Module {
        id,
        kind: "chorus",
        position: Position {
            x: 1_260.0,
            y: 40.0,
        },
        scripts: None,
        parameters: Parameters::Chorus(ChorusParams {
            depth,
            mix,
            rate,
            voices,
        }),
    }
}

fn ensemble_module() -> Module {
    Module {
        id: "enc-1",
        kind: "ensemble_chorus",
        position: Position {
            x: 1_260.0,
            y: 40.0,
        },
        scripts: None,
        parameters: Parameters::EnsembleChorus(EnsembleChorusParams {
            base_delay: 13.0,
            depth: 1.5,
            mix: 0.42,
            noise: 0.06,
            rate: 0.38,
            stereo_width: 0.92,
            tone: 0.52,
            voices: 3.0,
        }),
    }
}

pub(super) fn delay_module(
    id: &'static str,
    feedback: f32,
    mix: f32,
    time: f32,
    mode: &'static str,
) -> Module {
    Module {
        id,
        kind: "delay",
        position: Position {
            x: 1_450.0,
            y: 40.0,
        },
        scripts: None,
        parameters: Parameters::Delay(DelayParams {
            feedback,
            mix,
            mode,
            sync_division: 0.5,
            tempo_sync: 0.0,
            time,
            time_left: time,
            time_right: time * 1.5,
            tone: 0.58,
        }),
    }
}

pub(super) fn reverb_module(
    id: &'static str,
    mix: f32,
    decay: f32,
    room_size: f32,
    width: f32,
) -> Module {
    Module {
        id,
        kind: "reverb",
        position: Position {
            x: 1_450.0,
            y: 180.0,
        },
        scripts: None,
        parameters: Parameters::Reverb(ReverbParams {
            damping: 0.48,
            decay,
            diffusion: 0.72,
            low_cut: 110.0,
            mix,
            pre_delay: 0.018,
            room_size,
            width,
        }),
    }
}

pub(super) fn compressor_module(
    id: &'static str,
    threshold: f32,
    ratio: f32,
    attack: f32,
    release: f32,
    makeup: f32,
    mix: f32,
) -> Module {
    Module {
        id,
        kind: "compressor",
        position: Position {
            x: 1_260.0,
            y: 40.0,
        },
        scripts: None,
        parameters: Parameters::Compressor(CompressorParams {
            attack,
            makeup,
            mix,
            ratio,
            release,
            sc_filter: 20.0,
            sidechain: 0.0,
            threshold,
        }),
    }
}

#[derive(Serialize)]
pub(super) struct OscillatorParams {
    anti_alias: &'static str,
    detune: f32,
    fm_amt: f32,
    fm_mode: &'static str,
    frequency: f32,
    glide_time: f32,
    level: f32,
    octave: f32,
    phase: f32,
    pulse_width: f32,
    uni_detune: f32,
    uni_phase: f32,
    uni_spread: f32,
    unison: f32,
    waveform: &'static str,
    x_mod: f32,
}

#[derive(Serialize)]
pub(super) struct WavetableOscParams {
    detune: f32,
    glide_time: f32,
    level: f32,
    octave: f32,
    position: f32,
    table: &'static str,
}

#[derive(Serialize)]
pub(super) struct SubOscillatorParams {
    glide_time: f32,
    level: f32,
    octave: &'static str,
    waveform: &'static str,
}

#[derive(Serialize)]
pub(super) struct NoiseParams {
    level: f32,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
pub(super) struct ChorusParams {
    depth: f32,
    mix: f32,
    rate: f32,
    voices: f32,
}

#[derive(Serialize)]
pub(super) struct EnsembleChorusParams {
    base_delay: f32,
    depth: f32,
    mix: f32,
    noise: f32,
    rate: f32,
    stereo_width: f32,
    tone: f32,
    voices: f32,
}

#[derive(Serialize)]
pub(super) struct DelayParams {
    feedback: f32,
    mix: f32,
    mode: &'static str,
    sync_division: f32,
    tempo_sync: f32,
    time: f32,
    time_left: f32,
    time_right: f32,
    tone: f32,
}

#[derive(Serialize)]
pub(super) struct ReverbParams {
    damping: f32,
    decay: f32,
    diffusion: f32,
    low_cut: f32,
    mix: f32,
    pre_delay: f32,
    room_size: f32,
    width: f32,
}

#[derive(Serialize)]
pub(super) struct CompressorParams {
    pub(super) attack: f32,
    pub(super) makeup: f32,
    pub(super) mix: f32,
    pub(super) ratio: f32,
    pub(super) release: f32,
    pub(super) sc_filter: f32,
    pub(super) sidechain: f32,
    pub(super) threshold: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modern_profile_recipes_remain_role_specific() {
        let bass = VoiceRecipe::bass();
        let pad = VoiceRecipe::pad();
        let kick = VoiceRecipe::drum(DrumSubclass::Kick);

        assert!(matches!(bass.secondary, SecondaryRecipe::Sub { .. }));
        assert!(matches!(
            pad.secondary,
            SecondaryRecipe::Wavetable { table: "warm", .. }
        ));
        assert!(matches!(kick.secondary, SecondaryRecipe::Noise { .. }));
        assert!(bass.filter.cutoff < pad.filter.cutoff);
    }

    #[test]
    fn pulse_width_automation_is_retained_for_modern_oscillator() {
        let mut song = Song {
            name: String::new(),
            author: String::new(),
            patterns: vec![Pattern {
                id: 0,
                name: String::new(),
                length: 1,
                notes: Vec::new(),
                automation: vec![AutomationLane {
                    target: AutomationTarget::Module(ModuleTarget::new(
                        1,
                        "sid_oscillator",
                        "pw_reg",
                    )),
                    points: Vec::new(),
                }],
                next_note_id: 0,
                processors: Vec::new(),
                note_graph: None,
            }],
            next_pattern_id: 1,
            tracks: Vec::new(),
            next_track_id: 0,
            return_busses: Vec::new(),
            next_return_bus_id: None,
            arrangement: Vec::new(),
            note_graphs: Vec::new(),
            next_note_graph_id: 0,
            tempo_changes: Vec::new(),
            time_signature_changes: Vec::new(),
            default_tempo: 120.0,
            default_time_signature: TimeSignature {
                numerator: 4,
                denominator: 4,
            },
            row_resolution: RowResolution {
                rows: 4,
                ticks_per_row: 24,
            },
        };

        remap_automation(&mut song);

        let AutomationTarget::Module(target) = &song.patterns[0].automation[0].target else {
            panic!("expected module target");
        };
        assert_eq!(target.module.module_type, "oscillator");
        assert_eq!(target.module.param_id, "pulse_width");
    }
}
