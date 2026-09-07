//! Pertylizer parameter descriptors — the response curve and value range of
//! every module parameter, mirrored from Pertylizer's generated
//! `docs/pertylizer/descriptors.json`.
//!
//! The synth exporter normalizes SID register values into Pertylizer
//! automation-lane values (`0..1`, through each param's response curve) and
//! clamps its static on-disk parameter values into each param's `[min, max]`.
//! Both the ranges and the curve kinds are sourced from the descriptor file
//! rather than hand-copied constants that can silently drift when Pertylizer
//! retunes a parameter. The file is embedded at compile time, so a re-synced
//! mirror is picked up on the next build; the `descriptors_cover_exporter_params`
//! test guards the specific params the exporter depends on.
//!
//! Rule (`docs/export.md` §A2): on-disk *parameter values* are real
//! values within `[min, max]` ([`ParamDescriptor::clamp`]); only
//! *automation-lane values* are the normalized `0..1` through the curve
//! ([`ParamDescriptor::normalize`], mirroring the engine's
//! `synth_core::ResponseCurve`).

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

/// The mirrored descriptor file, embedded so the exporter's ranges/curves track
/// the schema mirror at build time (a re-sync triggers a rebuild).
const DESCRIPTORS_JSON: &str = include_str!("../../../../docs/pertylizer/descriptors.json");

/// A Pertylizer parameter's response curve (engine `synth_core::ResponseCurve`).
/// Selects how a real value in `[min, max]` maps to the normalized `0..1`
/// automation-lane value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseCurve {
    Linear,
    Logarithmic,
    Exponential,
}

impl ResponseCurve {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "linear" => Some(Self::Linear),
            "logarithmic" => Some(Self::Logarithmic),
            "exponential" => Some(Self::Exponential),
            _ => None,
        }
    }
}

/// Range + response curve for one module parameter.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParamDescriptor {
    pub min: f32,
    pub max: f32,
    pub curve: ResponseCurve,
}

impl ParamDescriptor {
    /// Identity fallback used when a queried parameter is absent from the
    /// descriptor file. Only reachable if the embedded mirror is malformed or a
    /// key was renamed — guarded by the `descriptors_cover_exporter_params`
    /// test — so a linear `[0, 1]` param keeps `clamp`/`normalize` no-ops on a
    /// value already in `0..1` rather than silently mis-scaling.
    const IDENTITY: Self = Self {
        min: 0.0,
        max: 1.0,
        curve: ResponseCurve::Linear,
    };

    /// Clamp a real value into the parameter's `[min, max]` — the on-disk
    /// parameter-value rule.
    pub fn clamp(self, value: f32) -> f32 {
        value.clamp(self.min, self.max)
    }

    /// Normalize a real value into its `0..1` automation-lane value through the
    /// response curve, mirroring the engine's `ResponseCurve::normalize`. The
    /// value is first clamped into `[min, max]`.
    pub fn normalize(self, value: f32) -> f32 {
        let span = self.max - self.min;
        if span <= 0.0 {
            return 0.0;
        }
        let v = self.clamp(value);
        let lane = match self.curve {
            ResponseCurve::Linear => (v - self.min) / span,
            // Logarithmic needs a positive lower bound; every parameter that
            // uses it (e.g. `filter.cutoff` `[20, 20000]`) has `min > 0`. A
            // non-positive min would be a descriptor bug, so fall back to linear.
            ResponseCurve::Logarithmic if self.min > 0.0 => {
                (v / self.min).ln() / (self.max / self.min).ln()
            }
            ResponseCurve::Logarithmic => (v - self.min) / span,
            ResponseCurve::Exponential => {
                let lin = (v - self.min) / span;
                (lin * (std::f32::consts::E - 1.0) + 1.0).ln()
            }
        };
        lane.clamp(0.0, 1.0)
    }
}

#[derive(Deserialize)]
struct DescriptorsFile {
    modules: HashMap<String, ModuleEntry>,
}

#[derive(Deserialize)]
struct ModuleEntry {
    #[serde(default)]
    parameters: HashMap<String, ParamEntry>,
}

/// A parameter entry as it appears in the file. Numeric params carry
/// `min`/`max`/`response_curve`; enum params (waveform, filter type, …) carry
/// none of these, so they parse with all-`None` and are skipped.
#[derive(Deserialize, Default)]
struct ParamEntry {
    min: Option<f64>,
    max: Option<f64>,
    response_curve: Option<String>,
}

type Table = HashMap<String, HashMap<String, ParamDescriptor>>;

/// Parse the embedded descriptor file once into a `(module, param)` lookup of
/// numeric parameters. A parse failure yields an empty table, so every lookup
/// falls back to [`ParamDescriptor::IDENTITY`] (guarded by the drift test).
fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = Table::new();
        let Ok(file) = serde_json::from_str::<DescriptorsFile>(DESCRIPTORS_JSON) else {
            return table;
        };
        for (module, entry) in file.modules {
            let mut params = HashMap::new();
            for (name, p) in entry.parameters {
                if let (Some(min), Some(max), Some(curve)) = (
                    p.min,
                    p.max,
                    p.response_curve.as_deref().and_then(ResponseCurve::parse),
                ) {
                    params.insert(
                        name,
                        ParamDescriptor {
                            min: min as f32,
                            max: max as f32,
                            curve,
                        },
                    );
                }
            }
            table.insert(module, params);
        }
        table
    })
}

/// The descriptor for `module.param`, or [`ParamDescriptor::IDENTITY`] when the
/// parameter is absent from the descriptor file.
pub(crate) fn param(module: &str, param: &str) -> ParamDescriptor {
    table()
        .get(module)
        .and_then(|m| m.get(param))
        .copied()
        .unwrap_or(ParamDescriptor::IDENTITY)
}
