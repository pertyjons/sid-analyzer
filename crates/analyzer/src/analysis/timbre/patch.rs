//! M7 Slice 2 — patch (instrument) clustering per subtune.
//!
//! Aggregates a subtune's per-note [`NoteCharacteristics`] into a
//! small set of recurring [`Patch`]es. The exact-match v1 algorithm
//! buckets notes by `(adsr, waveform, role_tags)` — Slice 0's
//! baseline (98.5 % `pair_coshare` over `assets/music/`) said this
//! is enough to compress most SID corpora. Looser clustering (v2,
//! K-means / DBSCAN) is only worth building if v1 misses on real
//! HVSC subsets.

use crate::analysis::filter::{Cutoff, FilterMode, Resonance};
use crate::analysis::note::NoteEvent;
use crate::analysis::timbre::characteristics::{
    DrumSubclass, FilterContour, HardwareTrick, NoteCharacteristics, PwEnvelope, RoleTags,
};
use crate::analysis::voice::{Adsr, PulseWidth};
use crate::analysis::{Hertz, VoiceId};
use serde::Serialize;
use std::collections::HashMap;

/// 0-based index into the per-subtune patch table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct PatchId(pub u16);

/// One recurring instrument/timbre profile within a subtune.
///
/// The key-shared fields (`adsr`, `waveform`, `role_tags`) are guaranteed
/// equal across every note in the cluster. The voice-specific timbre (filter
/// routing, pulse width, sync/ring tricks, waveform/arpeggio loops) lives on the
/// per-voice [`PatchVoiceProfile`]s in `voices`, because a cluster spans voices
/// and those are per-voice SID registers — a single representative scalar would
/// be the wrong voice's. Read it via [`Patch::profile`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[must_use]
pub struct Patch {
    pub id: PatchId,

    // Key-shared (identical across all member notes).
    pub adsr: Adsr,
    /// The note cluster's timbral waveform — the control byte the member notes
    /// hold longest ([`NoteCharacteristics::dominant_waveform_byte`]), or the
    /// authored table's waveform for the native grouped path.
    pub waveform: u8,
    pub role_tags: RoleTags,

    /// The authored instrument drives a percussive pitch-drop (a Hubbard
    /// drum/zap: gate, then sweep the pitch down to a low body). Only set by the
    /// native grouped path, which knows the driver's effect flags; `false` for
    /// the trace-clustered heuristic. Lets the exporter give it a percussive
    /// treatment instead of the instrument's slow authored decay.
    pub drum_drop: bool,

    /// The driver's remaining authored per-instrument effects (vibrato / PWM /
    /// chirp / arp), decoded by a native extractor and converted to physical
    /// units. `None` for the trace-clustered heuristic path. The exporter
    /// prefers these over per-note heuristic re-measurement.
    pub authored_effects: Option<AuthoredEffects>,

    /// Complete authored modulation/envelope/filter definition when a native
    /// extractor can decode the driver's instrument program.
    pub authored_definition: Option<AuthoredInstrumentDefinition>,

    pub member_count: u16,

    /// Per-voice timbre, one profile per SID voice that plays this patch.
    pub voices: Vec<PatchVoiceProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[must_use]
pub struct AuthoredInstrumentDefinition {
    pub pitch: AuthoredModulationProgram,
    pub pulse_width: AuthoredModulationProgram,
    pub initial_pulse_width: PulseWidth,
    pub gate_frames: ProgramFrames,
    pub release_frames: ProgramFrames,
    pub filter: AuthoredFilterPreset,
    pub duration_table: Vec<ProgramFrames>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[must_use]
pub struct AuthoredModulationProgram {
    pub stages: Vec<AuthoredModulationStage>,
    pub initial_delay_frames: ProgramFrames,
    pub step_period_frames: ProgramFrames,
    pub enabled: bool,
    pub apply_during_delay: bool,
    pub loop_mode: AuthoredLoopMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct AuthoredModulationStage {
    pub delta: ModulationDelta,
    pub frames: ProgramFrames,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ModulationDelta(pub i16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ProgramFrames(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum AuthoredLoopMode {
    None,
    RestartFromInitial,
    RestartFromCurrent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct AuthoredFilterPreset {
    pub cutoff: Cutoff,
    pub resonance: Resonance,
    pub routing: FilterRoutingMask,
    pub mode: FilterMode,
    pub volume: SidVolume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct FilterRoutingMask(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SidVolume(pub u8);

/// Authored per-instrument effects decoded by a native extractor, in physical
/// units (driver encodings are converted at the extractor, where the driver
/// knowledge lives). Carried on [`Patch`] so the export consumes the driver's
/// *exact* authored parameters instead of re-deriving them per note by
/// heuristic (rollout step E1, `docs/export.md`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[must_use]
pub struct AuthoredEffects {
    /// Continuous pitch vibrato the driver applies every frame of every note.
    pub vibrato: Option<AuthoredVibrato>,
    /// Continuous pulse-width modulation (period + per-period register step).
    pub pwm: Option<AuthoredPwm>,
    /// One-shot pulse-width offset added at note setup (raw register units of
    /// the driver's encoding).
    pub pw_offset: Option<u8>,
    /// Authored 12-bit initial pulse width — the value the driver loads into
    /// the register at note setup, and therefore the phase origin of the
    /// continuous-PWM sweep. Measured A/B vs reSID: regenerating the sweep
    /// from the trace's mid-envelope instead of this origin lands the duty
    /// cycle wrong (Monty V1 lead read 25 dB vs the lane's 12 dB).
    pub pw_init: Option<PulseWidth>,
    /// Upward pitch chirp on alternate frames (an onset/texture effect).
    pub chirp_up: bool,
    /// Fast hardware arpeggio (note ↔ offset alternation).
    pub arp: bool,
}

/// Authored vibrato in physical units: peak deviation in semitones + LFO rate
/// in Hz (and the driver's LFO shape is a triangle — carried implicitly; the
/// exporter maps it to the engine's triangle shape).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[must_use]
pub struct AuthoredVibrato {
    pub depth_semitones: f32,
    pub rate_hz: f32,
}

/// Authored continuous PWM: sweep period in driver frames and the raw
/// pulse-width register step applied each period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[must_use]
pub struct AuthoredPwm {
    pub period_frames: u8,
    pub step: u8,
}

/// The voice-specific timbre of one SID voice within a [`Patch`], aggregated
/// over that voice's member notes (a patch is clustered across voices, but these
/// are per-voice registers).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[must_use]
pub struct PatchVoiceProfile {
    pub voice: VoiceId,
    pub member_count: u16,
    /// Pulse-width envelope unioned over this voice's member notes.
    pub pw_envelope: PwEnvelope,
    /// `true` iff any of this voice's member notes routes through the filter.
    pub filter_routed: bool,
    /// Cutoff contour unioned over the routed member notes (or all members when
    /// none are routed, so a contour is always present).
    pub filter_contour: FilterContour,
    /// Filter mode/resonance from the first routed member (else the first note).
    pub filter_mode: FilterMode,
    pub filter_resonance: Resonance,
    /// Union of the hardware tricks (`RingMod`, `HardSync`, …) seen on this
    /// voice's member notes.
    pub hardware_tricks: Vec<HardwareTrick>,
    /// Ring-mod carrier pitch (the modulating voice's frequency) from the first
    /// member note that carries one. The export holds the ring-source
    /// `sid_oscillator` at this fixed pitch instead of tracking the played note.
    pub ring_source_hz: Option<Hertz>,
    /// Hard-sync master pitch (the syncing voice's frequency) from the first
    /// member note that carries one — the sync-source `sid_oscillator`'s pitch.
    pub sync_source_hz: Option<Hertz>,
    /// Waveform / arpeggio loops from this voice's first member note.
    pub waveform_loop: Option<Vec<u8>>,
    pub arpeggio_loop: Option<Vec<i8>>,
    /// Median noise-frame frequency across member notes (drum click / hihat
    /// tick brightness) — pins the percussion noise source's LFSR clock. See
    /// [`NoteCharacteristics::noise_freq_hz`].
    pub noise_freq_hz: Option<Hertz>,
    /// Mean noise-run length in frames across members with noise frames
    /// (`0.0` when none) — sizes the percussion click envelope.
    pub noise_run_frames: f32,
}

impl Patch {
    /// Convenience accessor — sourced from `role_tags.drum_subclass`.
    #[must_use]
    pub fn drum_subclass(&self) -> Option<DrumSubclass> {
        self.role_tags.drum_subclass
    }

    /// The per-voice timbre profile for `voice`, or `None` if this patch is not
    /// played on that voice.
    #[must_use]
    pub fn profile(&self, voice: VoiceId) -> Option<&PatchVoiceProfile> {
        self.voices.iter().find(|p| p.voice == voice)
    }
}

/// Bucket key for the v1 exact-match clusterer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PatchKey {
    adsr: Adsr,
    waveform: u8,
    role_tags: RoleTags,
}

impl PatchKey {
    fn from_note(c: &NoteCharacteristics) -> Self {
        Self {
            adsr: c.starting_adsr,
            waveform: c.dominant_waveform_byte(),
            role_tags: c.role_tags,
        }
    }
}

/// Run v1 patch extraction over a parallel `(notes, characteristics)`
/// pair. Returns the patch table plus a per-note assignment vector;
/// `None` in the assignment marks the "raw" fallback (singleton
/// cluster dropped, see [`MIN_PATCH_MEMBERS`]).
///
/// `notes.len()` must equal `characteristics.len()`; the two arrays
/// are parallel by index.
///
/// Determinism: patch IDs are assigned in order of each cluster's
/// earliest note (`notes[i].start_frame`), so the output is stable
/// across hash-map iteration order.
pub fn extract_patches(
    notes: &[NoteEvent],
    characteristics: &[NoteCharacteristics],
) -> (Vec<Patch>, Vec<Option<PatchId>>) {
    assert_eq!(
        notes.len(),
        characteristics.len(),
        "extract_patches: notes and characteristics arrays must be parallel"
    );

    let mut groups: HashMap<PatchKey, Vec<usize>> = HashMap::new();
    for (idx, c) in characteristics.iter().enumerate() {
        let key = PatchKey::from_note(c);
        groups.entry(key).or_default().push(idx);
    }

    let mut ordered: Vec<(PatchKey, Vec<usize>)> = groups.into_iter().collect();
    // Primary order is the cluster's earliest-note frame; `indices[0]` (the
    // group's lowest, hence unique, note index) breaks frame ties so the order
    // is total and independent of HashMap iteration order.
    ordered.sort_by_key(|(_, indices)| (notes[indices[0]].start_frame.0, indices[0]));

    let groups: Vec<PatchGroup> = ordered
        .into_iter()
        .filter(|(_, indices)| indices.len() >= MIN_PATCH_MEMBERS)
        .map(|(key, indices)| PatchGroup {
            indices,
            adsr: key.adsr,
            waveform: key.waveform,
            role_tags: key.role_tags,
            drum_drop: false,
            authored_effects: None,
            authored_definition: None,
        })
        .collect();
    // Heuristic clusters: members may be different instruments sharing a key, so
    // keep first-member loops (a stray arp must not spread across the cluster).
    build_patch_table(notes, characteristics, groups, false)
}

/// Singleton clusters (< 2 notes) are dropped to the `None` fallback.
/// A "patch of one" carries no compression benefit and tends to be
/// either a one-shot fill or an edge-case extraction artefact.
const MIN_PATCH_MEMBERS: usize = 2;

/// Build the patch table from an explicit per-note instrument grouping instead
/// of the heuristic [`PatchKey`] clustering. `group[i]` is the authored
/// instrument index of `notes[i]` (`None` leaves the note unclustered, the raw
/// fallback). `authored(idx)` supplies that instrument's authored [`Adsr`] and,
/// optionally, its waveform control byte (`Some` to author it, `None` to keep
/// the trace-derived waveform — e.g. the columnar Jeroen Tel layout authors ADSR
/// only). The per-voice timbre is still aggregated from the trace
/// `characteristics`, unchanged.
///
/// Native extractors use this: they recover the real instrument index per note
/// (from the driver's own tables), so a note binds to its authored instrument
/// directly. Every referenced instrument becomes one patch — there is no
/// singleton drop, because an authored instrument is a real instrument however
/// few notes use it, and patch IDs are assigned in order of each instrument's
/// earliest note for determinism.
pub(crate) fn extract_patches_grouped(
    notes: &[NoteEvent],
    characteristics: &[NoteCharacteristics],
    group: &[Option<u8>],
    authored: impl Fn(
        u8,
    ) -> (
        Adsr,
        Option<u8>,
        bool,
        Option<AuthoredEffects>,
        Option<AuthoredInstrumentDefinition>,
    ),
) -> (Vec<Patch>, Vec<Option<PatchId>>) {
    assert_eq!(
        notes.len(),
        characteristics.len(),
        "extract_patches_grouped: notes and characteristics must be parallel"
    );
    assert_eq!(
        notes.len(),
        group.len(),
        "extract_patches_grouped: notes and group must be parallel"
    );

    let mut groups: HashMap<u8, Vec<usize>> = HashMap::new();
    for (i, g) in group.iter().enumerate() {
        if let Some(idx) = g {
            groups.entry(*idx).or_default().push(i);
        }
    }

    let mut ordered: Vec<(u8, Vec<usize>)> = groups.into_iter().collect();
    // Earliest-note frame first; the authored instrument index breaks frame
    // ties so the order is total and HashMap-iteration-order independent.
    ordered.sort_by_key(|(idx, indices)| (notes[indices[0]].start_frame.0, *idx));

    let groups: Vec<PatchGroup> = ordered
        .into_iter()
        .map(|(idx, indices)| {
            let (adsr, authored_waveform, drum_drop, authored_effects, authored_definition) =
                authored(idx);
            // Waveform is authored when the table carries it, else the trace's
            // longest-held (dominant) waveform.
            let waveform = authored_waveform
                .unwrap_or_else(|| characteristics[indices[0]].dominant_waveform_byte());
            // Role is still trace-derived (authored tables carry no GM role).
            // Take the *modal* tags across the group's members, not the earliest:
            // all members are the same driver instrument, so a few short notes the
            // timbre pass mistags (e.g. an arp step read as a percussive Tom) must
            // not turn the whole instrument into a drum (Ocean Loader sawtooth arps).
            let role_tags = {
                let mut counts: Vec<(RoleTags, usize)> = Vec::new();
                for &i in &indices {
                    let rt = characteristics[i].role_tags;
                    match counts.iter_mut().find(|(k, _)| *k == rt) {
                        Some(c) => c.1 += 1,
                        None => counts.push((rt, 1)),
                    }
                }
                counts
                    .into_iter()
                    .max_by_key(|(_, c)| *c)
                    .map_or_else(|| characteristics[indices[0]].role_tags, |(rt, _)| rt)
            };
            PatchGroup {
                indices,
                adsr,
                waveform,
                role_tags,
                drum_drop,
                authored_effects,
                authored_definition,
            }
        })
        .collect();
    // Authored-instrument groups: all members are the same driver instrument, so
    // take the longest loop any member develops (a short note that never cycles
    // must not erase the instrument's arp).
    build_patch_table(notes, characteristics, groups, true)
}

/// A note cluster ready to become one [`Patch`]: its member note indices (in
/// time order) and the patch's static fields. Patch IDs follow the order these
/// are supplied in, so callers order them before [`build_patch_table`].
struct PatchGroup {
    indices: Vec<usize>,
    adsr: Adsr,
    waveform: u8,
    role_tags: RoleTags,
    drum_drop: bool,
    authored_effects: Option<AuthoredEffects>,
    authored_definition: Option<AuthoredInstrumentDefinition>,
}

/// Materialise an ordered list of [`PatchGroup`]s into the patch table plus the
/// per-note assignment vector — the shared tail of [`extract_patches`] (heuristic
/// `PatchKey` clustering) and [`extract_patches_grouped`] (authored-index
/// binding). Per-voice timbre is aggregated from the trace `characteristics`.
fn build_patch_table(
    notes: &[NoteEvent],
    characteristics: &[NoteCharacteristics],
    groups: Vec<PatchGroup>,
    representative_loops: bool,
) -> (Vec<Patch>, Vec<Option<PatchId>>) {
    let mut patches = Vec::new();
    let mut assignments: Vec<Option<PatchId>> = vec![None; notes.len()];

    for group in groups {
        let id = PatchId(patches.len() as u16);
        let members = u16::try_from(group.indices.len()).unwrap_or(u16::MAX);
        let voices =
            build_voice_profiles(notes, characteristics, &group.indices, representative_loops);
        patches.push(Patch {
            id,
            adsr: group.adsr,
            waveform: group.waveform,
            role_tags: group.role_tags,
            drum_drop: group.drum_drop,
            authored_effects: group.authored_effects,
            authored_definition: group.authored_definition,
            member_count: members,
            voices,
        });
        for &i in &group.indices {
            assignments[i] = Some(id);
        }
    }

    (patches, assignments)
}

/// Build one [`PatchVoiceProfile`] per SID voice that has member notes in this
/// cluster, aggregating the voice-specific timbre over that voice's own notes.
/// Voices are emitted in voice order (1, 2, 3) for determinism; `indices` is
/// already in note (time) order, so each voice's members keep that order.
fn build_voice_profiles(
    notes: &[NoteEvent],
    characteristics: &[NoteCharacteristics],
    indices: &[usize],
    representative_loops: bool,
) -> Vec<PatchVoiceProfile> {
    let mut buckets: [Vec<usize>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for &i in indices {
        if let Some(bucket) = buckets.get_mut(notes[i].voice.to_index()) {
            bucket.push(i);
        }
    }
    buckets
        .iter()
        .enumerate()
        .filter(|(_, bucket)| !bucket.is_empty())
        .map(|(vi, bucket)| {
            profile_for_voice(
                VoiceId::from_index(vi),
                bucket,
                characteristics,
                representative_loops,
            )
        })
        .collect()
}

fn profile_for_voice(
    voice: VoiceId,
    member_indices: &[usize],
    characteristics: &[NoteCharacteristics],
    representative_loops: bool,
) -> PatchVoiceProfile {
    let members: Vec<&NoteCharacteristics> = member_indices
        .iter()
        .map(|&i| &characteristics[i])
        .collect();
    let first = members[0];

    // Filter routing is per-voice: routed iff any of this voice's notes routes.
    let routed: Vec<&NoteCharacteristics> = members
        .iter()
        .copied()
        .filter(|c| c.filter_routed)
        .collect();
    let filter_routed = !routed.is_empty();
    // Mode/resonance from the first routed note (else the voice's first note).
    let filter_repr = routed.first().copied().unwrap_or(first);
    // Cutoff contour spans the routed notes (or all, when none routed).
    let contour_src = if routed.is_empty() { &members } else { &routed };
    let (fc_min, fc_max) = union_range(
        contour_src
            .iter()
            .map(|c| (c.filter_contour.min.0, c.filter_contour.max.0)),
    );
    let filter_contour = FilterContour {
        min: Cutoff(fc_min),
        max: Cutoff(fc_max),
        kind: filter_repr.filter_contour.kind,
    };

    // Pulse width spans all of this voice's notes.
    let (pw_min, pw_max) = union_range(
        members
            .iter()
            .map(|c| (c.pw_envelope.min.0, c.pw_envelope.max.0)),
    );
    let pw_envelope = PwEnvelope {
        min: PulseWidth(pw_min),
        max: PulseWidth(pw_max),
        kind: first.pw_envelope.kind,
    };

    // Hardware tricks: the deduped union over this voice's notes.
    let mut hardware_tricks: Vec<HardwareTrick> = Vec::new();
    for c in &members {
        for trick in &c.hardware_tricks {
            if !hardware_tricks.contains(trick) {
                hardware_tricks.push(*trick);
            }
        }
    }

    PatchVoiceProfile {
        voice,
        member_count: u16::try_from(members.len()).unwrap_or(u16::MAX),
        pw_envelope,
        filter_routed,
        filter_contour,
        filter_mode: filter_repr.filter_mode,
        filter_resonance: filter_repr.filter_resonance,
        hardware_tricks,
        ring_source_hz: members.iter().find_map(|c| c.ring_source_hz),
        sync_source_hz: members.iter().find_map(|c| c.sync_source_hz),
        // Loops: for an authored-instrument group (`representative_loops`) all
        // members ARE the same driver instrument, so take the *longest* loop any
        // develops — else a short first note that never cycles would erase the
        // patch's arp (Galway stabs grouped by instrument). Heuristic clusters
        // keep the first-member loop: their members may be *different* instruments
        // that merely share ADSR+waveform+role, so a stray arp must not spread.
        waveform_loop: if representative_loops {
            members
                .iter()
                .filter_map(|c| c.waveform_sequence.loop_body_cloned())
                .max_by_key(Vec::len)
        } else {
            first.waveform_sequence.loop_body_cloned()
        },
        arpeggio_loop: if representative_loops {
            members
                .iter()
                .filter_map(|c| c.pitch_relative_loop.clone().map(|(b, _)| b))
                .max_by_key(Vec::len)
        } else {
            first.pitch_relative_loop.clone().map(|(b, _)| b)
        },
        noise_freq_hz: median_hertz(members.iter().filter_map(|c| c.noise_freq_hz)),
        noise_run_frames: {
            let runs: Vec<f32> = members
                .iter()
                .map(|c| c.noise_run_frames)
                .filter(|&r| r > 0.0)
                .collect();
            if runs.is_empty() {
                0.0
            } else {
                runs.iter().sum::<f32>() / runs.len() as f32
            }
        },
    }
}

/// Median over an iterator of frequencies; `None` when empty. Median (not the
/// first member, like the ring/sync captures) because drum patches vary the
/// noise register per hit — the middle one is the cluster's colour.
pub(crate) fn median_hertz(it: impl Iterator<Item = Hertz>) -> Option<Hertz> {
    let mut v: Vec<f64> = it.map(|h| h.0).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(Hertz(v[v.len() / 2]))
}

/// Union of `(min, max)` pairs; `(0, 0)` when the iterator is empty.
fn union_range(it: impl Iterator<Item = (u16, u16)>) -> (u16, u16) {
    let mut min = u16::MAX;
    let mut max = 0u16;
    for (lo, hi) in it {
        min = min.min(lo);
        max = max.max(hi);
    }
    if min > max { (0, 0) } else { (min, max) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::VoiceId;
    use crate::analysis::note::{Cents, GmProgram, MidiNote, Velocity};
    use crate::analysis::timbre::characteristics::{
        AttackClass, ContourKind, PitchBehavior, ReleaseClass, SeqOrLoop,
    };
    use crate::analysis::voice::Waveform;
    use crate::trace::FrameIndex;

    fn note_at(start: u32) -> NoteEvent {
        NoteEvent {
            voice: VoiceId(1),
            start_frame: FrameIndex(start),
            end_frame: Some(FrameIndex(start + 9)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        }
    }

    fn pulse_chars() -> NoteCharacteristics {
        NoteCharacteristics {
            length_frames: 10,
            attack: AttackClass::Instant,
            release_behavior: ReleaseClass::NaturalDecay,
            waveform_primary: vec![Waveform {
                pulse: true,
                ..Default::default()
            }],
            dominant_waveform: 0x40,
            ..Default::default()
        }
    }

    fn noise_chars() -> NoteCharacteristics {
        NoteCharacteristics {
            length_frames: 4,
            attack: AttackClass::Instant,
            release_behavior: ReleaseClass::GateCut,
            waveform_primary: vec![Waveform {
                noise: true,
                ..Default::default()
            }],
            dominant_waveform: 0x80,
            noise_share: 1.0,
            role_tags: RoleTags {
                percussive: true,
                drum_subclass: Some(DrumSubclass::HihatClosed),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn note_at_voice(start: u32, voice: u8) -> NoteEvent {
        NoteEvent {
            voice: VoiceId(voice),
            ..note_at(start)
        }
    }

    /// A pulse note (same patch key as `pulse_chars`) carrying voice-specific
    /// filter routing and pulse width.
    fn pulse_chars_on(routed: bool, pw: u16, cutoff: u16) -> NoteCharacteristics {
        let mut c = pulse_chars();
        c.pw_envelope = PwEnvelope {
            min: PulseWidth(pw),
            max: PulseWidth(pw),
            kind: ContourKind::Static,
        };
        c.filter_routed = routed;
        if routed {
            c.filter_contour = FilterContour {
                min: Cutoff(cutoff),
                max: Cutoff(cutoff),
                kind: ContourKind::Static,
            };
            c.filter_mode = FilterMode {
                low_pass: true,
                ..Default::default()
            };
            c.filter_resonance = Resonance(15);
        }
        c
    }

    #[test]
    fn shared_patch_carries_per_voice_profiles() {
        // One pulse cluster played on voice 1 (unrouted, narrow PW) and voice 2
        // (lowpass-routed, wide PW). The voice-1 note is earliest, so the legacy
        // representative scalar reflects voice 1; the per-voice profiles carry
        // the voice-correct values.
        let notes = vec![
            note_at_voice(0, 1),
            note_at_voice(10, 2),
            note_at_voice(20, 1),
            note_at_voice(30, 2),
        ];
        let chars = vec![
            pulse_chars_on(false, 1024, 0),
            pulse_chars_on(true, 3072, 1200),
            pulse_chars_on(false, 1024, 0),
            pulse_chars_on(true, 3072, 1200),
        ];
        let (patches, _) = extract_patches(&notes, &chars);
        assert_eq!(patches.len(), 1, "same key → one cluster across voices");
        let p = &patches[0];
        assert_eq!(p.voices.len(), 2, "one profile per playing voice");

        let v1 = p.profile(VoiceId::V1).expect("voice 1 profile");
        let v2 = p.profile(VoiceId::V2).expect("voice 2 profile");

        // Filter routing is voice-correct.
        assert!(!v1.filter_routed, "voice 1 is not routed");
        assert!(v2.filter_routed, "voice 2 is routed");
        assert!(v2.filter_mode.low_pass);
        assert_eq!(v2.filter_resonance.0, 15);
        assert_eq!(v2.filter_contour.min.0, 1200);

        // Pulse width is each voice's own register, not a shared representative.
        assert_eq!(v1.pw_envelope.min.0, 1024);
        assert_eq!(v2.pw_envelope.min.0, 3072);
    }

    #[test]
    fn identical_characteristics_get_same_patch_id() {
        let notes = vec![note_at(0), note_at(50), note_at(100)];
        let chars = vec![pulse_chars(), pulse_chars(), pulse_chars()];
        let (patches, assignments) = extract_patches(&notes, &chars);
        assert_eq!(patches.len(), 1);
        let expected = Some(patches[0].id);
        assert_eq!(assignments, vec![expected, expected, expected]);
        assert_eq!(patches[0].member_count, 3);
    }

    #[test]
    fn different_drum_subclass_clusters_separately() {
        // Two pulse + two noise → two distinct clusters, both surviving
        // the singleton filter.
        let notes = vec![note_at(0), note_at(20), note_at(40), note_at(60)];
        let chars = vec![pulse_chars(), noise_chars(), pulse_chars(), noise_chars()];
        let (patches, _) = extract_patches(&notes, &chars);
        assert_eq!(patches.len(), 2);
    }

    #[test]
    fn singleton_clusters_are_dropped() {
        // Three distinct notes — each a cluster of one. All assignments None.
        let notes = vec![note_at(0), note_at(50), note_at(100)];
        let mut c2 = pulse_chars();
        c2.dominant_waveform = 0x10; // triangle
        let mut c3 = pulse_chars();
        c3.dominant_waveform = 0x20; // sawtooth
        let chars = vec![pulse_chars(), c2, c3];
        let (patches, assignments) = extract_patches(&notes, &chars);
        assert!(patches.is_empty());
        assert!(assignments.iter().all(|a| a.is_none()));
    }

    #[test]
    fn permutation_invariance_modulo_id_remap() {
        // Two clusters, three notes total. Permuting the input order
        // shouldn't change the *grouping*; only the patch IDs may
        // re-assign because IDs are stable-by-start-frame.
        let a = pulse_chars();
        let b = noise_chars();
        let chars_1 = vec![a.clone(), a.clone(), b.clone()];
        let notes_1 = vec![note_at(0), note_at(10), note_at(20)];
        let (p1, asn1) = extract_patches(&notes_1, &chars_1);

        let chars_2 = vec![b.clone(), a.clone(), a.clone()];
        let notes_2 = vec![note_at(20), note_at(0), note_at(10)];
        let (p2, asn2) = extract_patches(&notes_2, &chars_2);

        // Same number of patches, same grouping cardinalities.
        assert_eq!(p1.len(), p2.len());
        let counts_1: Vec<u16> = p1.iter().map(|p| p.member_count).collect();
        let counts_2: Vec<u16> = p2.iter().map(|p| p.member_count).collect();
        assert_eq!(counts_1, counts_2);

        // The "a"-cluster note at start_frame=0 should get id=0 in both.
        // In run 1 it's at index 0; in run 2 it's at index 1.
        assert_eq!(asn1[0], Some(PatchId(0)));
        assert_eq!(asn2[1], Some(PatchId(0)));
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let (patches, assignments) = extract_patches(&[], &[]);
        assert!(patches.is_empty());
        assert!(assignments.is_empty());
    }

    #[test]
    fn patch_id_assignment_is_stable_by_start_frame() {
        // Two clusters: noise (3 members starting at frame 100) and
        // pulse (2 members starting at frame 5). Pulse appears first
        // → id 0; noise → id 1.
        let chars = vec![
            noise_chars(),
            pulse_chars(),
            noise_chars(),
            pulse_chars(),
            noise_chars(),
        ];
        let notes = vec![
            note_at(100),
            note_at(5),
            note_at(120),
            note_at(60),
            note_at(140),
        ];
        let (patches, assignments) = extract_patches(&notes, &chars);
        assert_eq!(patches.len(), 2);
        assert_eq!(assignments[1], Some(PatchId(0))); // pulse first
        assert_eq!(assignments[0], Some(PatchId(1))); // noise second
    }

    #[test]
    fn patch_id_tie_break_is_deterministic_on_equal_start_frame() {
        // Two clusters whose earliest notes share start_frame 0: a pulse
        // cluster (note indices 0, 1) and a noise cluster (indices 2, 3).
        // The frame key alone leaves the order to HashMap iteration; the
        // `indices[0]` tie-break must make it total — pulse (index 0) < noise
        // (index 2) → pulse is always id 0. Each call builds a fresh HashMap
        // with a random seed, so repeating exercises differing iteration order.
        let notes = vec![
            note_at_voice(0, 1),
            note_at_voice(10, 1),
            note_at_voice(0, 2),
            note_at_voice(20, 2),
        ];
        let chars = vec![pulse_chars(), pulse_chars(), noise_chars(), noise_chars()];
        for _ in 0..64 {
            let (patches, assignments) = extract_patches(&notes, &chars);
            assert_eq!(patches.len(), 2);
            assert_eq!(assignments[0], Some(PatchId(0)), "pulse → id 0");
            assert_eq!(assignments[2], Some(PatchId(1)), "noise → id 1");
        }
    }

    #[test]
    fn loop_body_pulled_from_example_waveform_sequence() {
        let mut c = pulse_chars();
        c.waveform_sequence = SeqOrLoop::Loop {
            body: vec![0x40, 0x80],
            offset: 0,
        };
        let chars = vec![c.clone(), c.clone()];
        let notes = vec![note_at(0), note_at(20)];
        let (patches, _) = extract_patches(&notes, &chars);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].voices[0].waveform_loop, Some(vec![0x40, 0x80]));
    }

    #[test]
    fn pw_envelope_kind_is_preserved_from_example() {
        let mut c = pulse_chars();
        c.pw_envelope = PwEnvelope {
            kind: ContourKind::Triangle,
            ..Default::default()
        };
        let chars = vec![c.clone(), c.clone()];
        let notes = vec![note_at(0), note_at(20)];
        let (patches, _) = extract_patches(&notes, &chars);
        let _ = PitchBehavior::Stable; // imports kept honest
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].voices[0].pw_envelope.kind, ContourKind::Triangle);
    }
}
