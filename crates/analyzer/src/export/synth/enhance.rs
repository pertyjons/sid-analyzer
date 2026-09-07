use super::*;

const DISTORTION_ID: &str = "dst-1";
const COMPRESSOR_ID: &str = "cmp-1";
const CHORUS_ID: &str = "chr-1";
const DELAY_ID: &str = "dly-1";
const REVERB_ID: &str = "rev-1";

#[derive(Clone, Copy)]
struct EnhancementCurve {
    strength: f32,
    upper: f32,
    ambience: f32,
}

impl EnhancementCurve {
    fn from_amount(amount: EnhancementAmount) -> Self {
        let strength = amount.strength();
        let upper = f32::from(amount.get().saturating_sub(5)) / 5.0;
        let ambience = if upper > 0.0 {
            lerp(0.5, 0.72, upper)
        } else {
            strength
        };
        Self {
            strength,
            upper,
            ambience,
        }
    }
}

pub(super) fn apply(
    plans: &[TrackPlan<'_>],
    instruments: &mut [Instrument],
    global: &mut GlobalProjectState,
    amount: EnhancementAmount,
) {
    if plans.is_empty() {
        if let Some(instrument) = instruments.first_mut() {
            enhance_instrument(instrument, InstrumentRole::Untagged, false, amount);
        }
    } else {
        for (plan, instrument) in plans.iter().zip(instruments.iter_mut()) {
            enhance_instrument(instrument, plan_role(plan), plan_arpeggiated(plan), amount);
        }
    }
    enhance_master(global, amount);
}

fn enhance_instrument(
    instrument: &mut Instrument,
    role: InstrumentRole,
    arpeggiated: bool,
    amount: EnhancementAmount,
) {
    let curve = EnhancementCurve::from_amount(amount);
    let mut effects = vec![distortion_module(role, curve)];
    effects.push(compressor_module(role, curve));

    match role {
        InstrumentRole::Bass => {}
        InstrumentRole::Lead if arpeggiated => {
            effects.push(delay_module(0.16 * curve.ambience, 0.30, 0.18, "ping_pong"));
            effects.push(reverb_module(0.10 * curve.ambience, 0.38, 0.46, 0.82));
        }
        InstrumentRole::Lead => {
            effects.push(modern::chorus_module(
                CHORUS_ID,
                lerp(0.12, 0.34, curve.ambience),
                0.18 * curve.ambience,
                0.32,
                if amount.get() >= 7 { 3.0 } else { 2.0 },
            ));
            effects.push(delay_module(0.12 * curve.ambience, 0.22, 0.12, "stereo"));
            effects.push(reverb_module(0.13 * curve.ambience, 0.42, 0.52, 0.86));
        }
        InstrumentRole::Pad => {
            effects.push(modern::chorus_module(
                CHORUS_ID,
                lerp(0.18, 0.42, curve.ambience),
                0.24 * curve.ambience,
                0.24,
                if amount.get() >= 7 { 3.0 } else { 2.0 },
            ));
            effects.push(reverb_module(0.24 * curve.ambience, 0.62, 0.74, 0.94));
        }
        InstrumentRole::Stab => {
            effects.push(reverb_module(0.12 * curve.ambience, 0.30, 0.42, 0.76));
        }
        InstrumentRole::Bell => {
            effects.push(delay_module(0.18 * curve.ambience, 0.26, 0.28, "stereo"));
            effects.push(reverb_module(0.22 * curve.ambience, 0.66, 0.72, 0.92));
        }
        InstrumentRole::Drum(subclass) => {
            let mix = match subclass {
                DrumSubclass::Kick => 0.04,
                DrumSubclass::Tom => 0.10,
                DrumSubclass::Snare => 0.14,
                DrumSubclass::HihatClosed => 0.07,
                DrumSubclass::HihatOpen => 0.11,
                DrumSubclass::PercMetallic => 0.12,
            } * curve.ambience;
            effects.push(reverb_module(mix, 0.24, 0.30, 0.68));
        }
        InstrumentRole::Untagged => {
            effects.push(modern::chorus_module(
                CHORUS_ID,
                lerp(0.10, 0.24, curve.ambience),
                0.10 * curve.ambience,
                0.28,
                2.0,
            ));
            effects.push(reverb_module(0.09 * curve.ambience, 0.34, 0.44, 0.76));
        }
    }

    instrument
        .patch
        .settings
        .effect_chain_order
        .extend(effects.iter().map(|module| module.id));
    instrument.patch.modules.extend(effects);
    instrument.patch.settings.canvas_size.width = 1_720.0;
    instrument.description = format!(
        "sid-analyzer faithful export with enhancement level {}.",
        amount.get()
    );
}

fn enhance_master(global: &mut GlobalProjectState, amount: EnhancementAmount) {
    let curve = EnhancementCurve::from_amount(amount);
    let low_gain = upper_lerp(1.4 * curve.strength, 0.7, 2.6, curve.upper);
    let mid_freq = upper_lerp(420.0, 420.0, 260.0, curve.upper);
    let mid_gain = upper_lerp(-0.5 * curve.strength, -0.25, 0.8, curve.upper);
    let high_gain = upper_lerp(0.7 * curve.strength, 0.35, 0.65, curve.upper);
    let eq = Module {
        id: "equ-2",
        kind: "eq",
        position: Position { x: 400.0, y: 0.0 },
        scripts: None,
        parameters: Parameters::Eq(EqParams {
            low_freq: 105.0,
            low_gain,
            mid_freq,
            mid_gain,
            mid_q: 0.72,
            high_freq: 7_800.0,
            high_gain,
            mix: 1.0,
        }),
    };
    let threshold = upper_lerp(lerp(-8.0, -15.0, curve.strength), -11.5, -13.0, curve.upper);
    let ratio = upper_lerp(lerp(1.2, 2.4, curve.strength), 1.8, 2.1, curve.upper);
    let mix = upper_lerp(0.52 * curve.strength, 0.26, 0.38, curve.upper);
    let mut compressor = modern::compressor_module(
        "cmp-1",
        threshold,
        ratio,
        24.0,
        190.0,
        0.9 * curve.upper,
        mix,
    );
    compressor.position = Position { x: 600.0, y: 0.0 };

    if curve.upper > 0.0
        && let Some(Parameters::Distortion(distortion)) = global
            .master_effects
            .iter_mut()
            .find(|module| module.kind == "distortion")
            .map(|module| &mut module.parameters)
    {
        distortion.drive = lerp(distortion.drive, 0.42, curve.upper);
        distortion.mix = lerp(distortion.mix, 0.68, curve.upper);
        distortion.tone = lerp(distortion.tone, 0.82, curve.upper);
    }

    let limiter = global
        .master_effects
        .iter()
        .position(|module| module.kind == "limiter")
        .unwrap_or(global.master_effects.len());
    global.master_effects.insert(limiter, eq);
    global.master_effects.insert(limiter + 1, compressor);
}

fn distortion_module(role: InstrumentRole, curve: EnhancementCurve) -> Module {
    let (drive, mix, tone, upper_drive, upper_mix) = match role {
        InstrumentRole::Bass => (0.62, 0.28, 0.48, 0.18, 0.10),
        InstrumentRole::Lead => (0.46, 0.16, 0.64, 0.12, 0.07),
        InstrumentRole::Pad => (0.38, 0.12, 0.56, 0.08, 0.05),
        InstrumentRole::Stab => (0.52, 0.20, 0.60, 0.12, 0.07),
        InstrumentRole::Bell => (0.30, 0.08, 0.72, 0.08, 0.05),
        InstrumentRole::Drum(DrumSubclass::Kick | DrumSubclass::Tom) => {
            (0.68, 0.28, 0.44, 0.18, 0.10)
        }
        InstrumentRole::Drum(_) => (0.56, 0.22, 0.66, 0.12, 0.07),
        InstrumentRole::Untagged => (0.42, 0.14, 0.58, 0.10, 0.06),
    };
    Module {
        id: DISTORTION_ID,
        kind: "distortion",
        position: Position {
            x: 1_060.0,
            y: 40.0,
        },
        scripts: None,
        parameters: Parameters::Distortion(DistortionParams {
            bit_depth: 16.0,
            drive: drive * curve.strength + upper_drive * curve.upper,
            mix: mix * curve.strength + upper_mix * curve.upper,
            tone,
            kind: "tube",
        }),
    }
}

fn compressor_module(role: InstrumentRole, curve: EnhancementCurve) -> Module {
    let (threshold, ratio, attack, release, mix, upper_makeup) = match role {
        InstrumentRole::Bass => (-18.0, 3.4, 18.0, 170.0, 0.68, 1.8),
        InstrumentRole::Lead => (-14.0, 2.4, 16.0, 150.0, 0.46, 1.2),
        InstrumentRole::Pad => (-12.0, 2.0, 30.0, 240.0, 0.36, 0.8),
        InstrumentRole::Stab => (-16.0, 3.0, 8.0, 120.0, 0.58, 1.4),
        InstrumentRole::Bell => (-12.0, 2.0, 24.0, 220.0, 0.30, 0.7),
        InstrumentRole::Drum(DrumSubclass::Kick | DrumSubclass::Tom) => {
            (-18.0, 4.0, 18.0, 130.0, 0.72, 2.0)
        }
        InstrumentRole::Drum(_) => (-16.0, 3.6, 8.0, 110.0, 0.66, 1.5),
        InstrumentRole::Untagged => (-14.0, 2.2, 20.0, 170.0, 0.38, 1.0),
    };
    let threshold_at_five = lerp(-6.0, threshold, 0.5);
    let ratio_at_five = lerp(1.0, ratio, 0.5);
    let resolved_threshold = upper_lerp(
        lerp(-6.0, threshold, curve.strength),
        threshold_at_five,
        lerp(threshold_at_five, threshold, 0.55),
        curve.upper,
    );
    let resolved_ratio = upper_lerp(
        lerp(1.0, ratio, curve.strength),
        ratio_at_five,
        lerp(ratio_at_five, ratio, 0.65),
        curve.upper,
    );
    let resolved_mix = upper_lerp(mix * curve.strength, mix * 0.5, mix * 0.82, curve.upper);
    let makeup = upper_lerp(0.8 * curve.strength, 0.4, upper_makeup, curve.upper);
    modern::compressor_module(
        COMPRESSOR_ID,
        resolved_threshold,
        resolved_ratio,
        attack,
        release,
        makeup,
        resolved_mix,
    )
}

fn delay_module(mix: f32, feedback: f32, time: f32, mode: &'static str) -> Module {
    modern::delay_module(DELAY_ID, feedback, mix, time, mode)
}

fn reverb_module(mix: f32, decay: f32, room_size: f32, width: f32) -> Module {
    modern::reverb_module(REVERB_ID, mix, decay, room_size, width)
}

fn lerp(from: f32, to: f32, position: f32) -> f32 {
    from + (to - from) * position
}

fn upper_lerp(current: f32, at_five: f32, at_ten: f32, upper: f32) -> f32 {
    if upper > 0.0 {
        lerp(at_five, at_ten, upper)
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_is_strictly_bounded() {
        assert!(EnhancementAmount::new(0).is_err());
        assert_eq!(EnhancementAmount::new(1).expect("minimum").get(), 1);
        assert_eq!(EnhancementAmount::new(10).expect("maximum").get(), 10);
        assert!(EnhancementAmount::new(11).is_err());
    }

    #[test]
    fn stronger_amount_increases_wet_processing() {
        let low = distortion_module(
            InstrumentRole::Bass,
            EnhancementCurve::from_amount(EnhancementAmount::new(1).expect("amount")),
        );
        let high = distortion_module(
            InstrumentRole::Bass,
            EnhancementCurve::from_amount(EnhancementAmount::new(10).expect("amount")),
        );
        let Parameters::Distortion(low) = low.parameters else {
            panic!("distortion parameters");
        };
        let Parameters::Distortion(high) = high.parameters else {
            panic!("distortion parameters");
        };
        assert!(low.drive < high.drive);
        assert!(low.mix < high.mix);
    }

    #[test]
    fn level_five_keeps_the_original_recipe_boundary() {
        let curve = EnhancementCurve::from_amount(EnhancementAmount::new(5).expect("amount"));
        assert_eq!(curve.strength, 0.5);
        assert_eq!(curve.upper, 0.0);
        assert_eq!(curve.ambience, 0.5);

        let distortion = distortion_module(InstrumentRole::Bass, curve);
        let Parameters::Distortion(distortion) = distortion.parameters else {
            panic!("distortion parameters");
        };
        assert_eq!(distortion.drive, 0.31);
        assert_eq!(distortion.mix, 0.14);

        let compressor = compressor_module(InstrumentRole::Bass, curve);
        let Parameters::Compressor(compressor) = compressor.parameters else {
            panic!("compressor parameters");
        };
        assert_eq!(compressor.threshold, -12.0);
        assert_eq!(compressor.ratio, 2.2);
        assert_eq!(compressor.makeup, 0.4);
        assert_eq!(compressor.mix, 0.34);
    }

    #[test]
    fn upper_range_adds_body_without_maxing_compression_or_ambience() {
        let curve = EnhancementCurve::from_amount(EnhancementAmount::new(10).expect("amount"));
        assert_eq!(curve.upper, 1.0);
        assert_eq!(curve.ambience, 0.72);

        let compressor = compressor_module(InstrumentRole::Bass, curve);
        let Parameters::Compressor(compressor) = compressor.parameters else {
            panic!("compressor parameters");
        };
        assert!(compressor.threshold > -18.0);
        assert!(compressor.ratio < 3.4);
        assert!(compressor.mix < 0.68);
        assert_eq!(compressor.makeup, 1.8);

        let mut global = GlobalProjectState::default();
        enhance_master(&mut global, EnhancementAmount::new(10).expect("amount"));
        let Parameters::Distortion(distortion) = &global.master_effects[0].parameters else {
            panic!("master distortion parameters");
        };
        assert_eq!(distortion.drive, 0.42);
        assert_eq!(distortion.mix, 0.68);
        assert_eq!(distortion.tone, 0.82);
    }
}
