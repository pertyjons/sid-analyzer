# Chip-state emulation — implementation code review (2026-07-19)

Expert review of the digital SID emulator landed 2026-07-19 (`54be803..4d63bc2`)
and its surrounding code, from three perspectives: SID/C64 emulation
correctness (reference: reSID 0.16 / residfp semantics and the locked spec in
[`chip-state-emulation.md`](chip-state-emulation.md)), professional Rust, and
DSP/audio-export fidelity.

**Files reviewed in full:** `emu/sid.rs`, `emu/mod.rs`, `emu/bus.rs`,
`emu/runner.rs`, `trace.rs`, `analysis/osc3.rs`, `analysis/mod.rs` (replay
path), plus the chip-state consumers in `export/synth.rs`
(`chip_articulation`, census), `analysis/note.rs` (`sound_end_frame`), and
`export/forward.rs`.

**Verification experiments run during this review** (scratch tests, since
deleted):

1. *Synthetic DS-zero repro* — gate on, AD=`$00`, SR=`$00`, decay to zero,
   then raise sustain: the envelope wrapped `0 → 0xFF` (finding F1, confirmed).
2. *Corpus digest A/B* — full envelope+oscillator state digest over 3 000
   frames of Monty on the Run, Auf Wiedersehen Monty, Commando, and Nemesis
   the Warlock, with and without the F1 fix: **digests identical**, so F1 is
   latent in the four control assets (no gate regression risk from fixing it).
3. *Delay-bug census* — the same four traces contain real voice-frames whose
   attack is delayed past a frame boundary by the rate-counter wrap
   (Nemesis st1: 21, AWM: 4, Monty/Commando: 0). The emulator resolves these
   correctly; the export does not yet use the measured onset (F16).

---

## Verified correct (checked against reSID semantics, no action needed)

- `RATE_PERIOD` table, exponential thresholds (`FF/5D/36/1A/0E/06/00` →
  `1/2/4/8/16/30/1`), `sustain_level = (s<<4)|s`.
- Delay bug preserved: gate/AD/SR writes reload only the active phase's
  `rate_period` and never reset `rate_counter` (`sid.rs:238-269`).
- Attack resumes from the current level on gate-on during release; decay never
  seeks upward toward a raised sustain (for nonzero levels — see F1).
- Exponential-counter handling is equivalent to reSID's short-circuit form,
  including the 8-bit overshoot wrap when `exp_period` drops mid-count.
- Noise LFSR: seed `0x7FFFF8`, bit-19 rising-edge clock, feedback
  `bit22 ^ bit17`, output taps `22,20,16,13,11,7,4,2` — all match reSID.
- Sync routing V1←V3/V2←V1/V3←V2 and the same-cycle source-reset suppression
  corner (`sid.rs:442-449`) match reSID's `synchronize()`.
- Ring mod = triangle inversion by `own MSB ^ source MSB`; pulse compare
  `acc>>12 >= pw` with TEST forcing high; saw = `acc>>16`; OSC3 returns the
  *selected waveform output*, not the bare accumulator (`sid.rs:500-535`).
- The analytic fast-forward (`clock_uncoupled_oscillators`) edge-counting
  arithmetic is correct (half-open `(start, end]` crossing count), and the
  envelope jump-clock is property-tested against a per-cycle reference
  (`jump_clock_matches_slow_reference_for_random_event_streams`).
- The one-core/two-instances design (live in `Bus`, replayed in `analyze`)
  reproduces identical timelines; write-before-clock ordering and same-cycle
  insertion order are contract-tested.
- Rational call-boundary scheduler is drift-free (u128 arithmetic, tested);
  PAL 19 656 / NTSC 17 095 boundaries are correct for the selected host models.
- Export consumers: `sound_end_frame`'s `active_cycles == 0` predicate,
  per-frame `sync_resets`/`source_msb_edges` deltas, and the census's
  sync/ring activity checks use the digital state correctly.

---

## Findings

Ordered by severity. Each has evidence and a concrete fix.

### F1 — envelope wraps `0 → 0xFF` when sustain is raised at zero in DecaySustain — **HIGH, confirmed bug**

`sid.rs:204-208`: the DecaySustain arm decrements with `wrapping_sub` while
`level != sustain_level`, but `hold_zero` is only ever set in the Release arm
(`sid.rs:209-222`). In reSID the envelope counter landing on `0x00` sets
`hold_zero` in **any** phase (the bottom `switch (envelope_counter)` in
`EnvelopeGenerator::clock`). Consequence here: a voice gated on with sustain 0
(the classic percussive envelope) decays to zero and sits in DecaySustain
*without* `hold_zero`; when the driver pre-writes the next note's SR while the
old gate is still on — a completely ordinary driver pattern — the very next
rate match executes `0u8.wrapping_sub(1)` and the envelope jumps to `0xFF` and
then decays from full level. A gate-off in that window releases from `0xFF`
instead of from silence.

*Verified:* synthetic repro wraps to 255 on current code and reads 0 with the
fix; the four control assets are digest-identical with and without the fix, so
it is latent there — but it will fire somewhere in an HVSC-wide scan, and it
poisons `peak_level` (→ export velocity 1.0), `active_cycles`, `sound_end`,
and the envelope census when it does.

*Fix:* treat "level reached zero" uniformly: after any level change in
DecaySustain (and Attack, for completeness — unreachable there), set
`hold_zero` when `level == 0`, emit `ReachedZero`, and record
`observation.reached_zero`. This also fixes the related observability gap:
today a DS-decay-to-zero (sustain 0, gate held) never emits `ReachedZero` and
never sets `EnvelopeFrameActivity::reached_zero`, so sub-frame sound-end
offsets are silently missing for the most common percussion envelope
(frame-level `active_cycles == 0` masks this downstream, but the event log
lies by omission).

*Also update the spec:* `chip-state-emulation.md` §envelope says only
"**Release** decrements to 0; at 0 set hold_zero" — the locked spec itself
under-specifies reSID here. Add unit tests: sustain raised at DS-zero; gate-off
after sustain-raise-at-zero; DS-decay-to-zero emits `ReachedZero`.

### F2 — unstable illegal opcodes hang the runner without a deadline — **HIGH, robustness**

`mos6502` 0.9 implements the *stable* undocumented opcodes (LAX, SAX, DCP,
ISC, ANC, ALR, ARR, SLO, …) but decodes the *unstable* ones (SHA, TAS, SHY,
SHX, LXA, …) to `None`; `single_step()` then returns `false` **without
advancing PC or the cycle counter**. `runner.rs:112` ignores that return
value, and since `cpu.cycles` never advances, `CYCLE_GUARD` can never trip.
The comment at `runner.rs:57-59` ("step loops where `cpu.cycles` doesn't
advance") documents the symptom without the cause.

- `call()` / `run()` without a deadline: **infinite loop** on such a byte.
- `call_stepwise` (taint, probe, `sid-re`): always deadline-free → same hang.
- With a deadline: silent, misclassified `WallDeadlineExceeded`.

*Fix:* check `single_step()`'s return; on `false`, read the offending opcode
at PC and return a distinct `RunError::UnimplementedOpcode { pc, opcode }`.
Count occurrences in the corpus scanner — this also tells you exactly which
HVSC files execute unstable illegals (today they burn 30 s of wall clock each
and produce garbage traces). Longer term: implement the unstable illegals with
their dominant stable behavior (SHY/SHX etc. have well-documented
"usually" semantics) so those tunes trace at all.

### F3 — `play_address == 0` (init-installed IRQ player) unhandled — **MEDIUM**

PSID files with `playAddress = 0` drive playback from an interrupt handler
installed during `init`. Nothing checks for this (`header.rs` parses it;
`emu/mod.rs` JSRs to it): the emulator calls `$0000`, executes BRK through a
zeroed IRQ vector, and burns `CYCLE_GUARD` (1 M cycles) per frame before
failing with a misleading `CycleGuardTripped` — repeated for every requested
frame.

*Fix (staged):* (1) detect at `run_inner` entry and return a clear
`EmuError::InterruptDrivenPlayerUnsupported`; (2) later, a pragmatic policy:
after `init`, read the vector the tune installed (`$0314/$0315`, or
`$FFFE/$FFFF` image) and schedule that address as the play routine — this
recovers a substantial HVSC cohort without full CIA/VIC IRQ emulation.

### F4 — rate-counter wrap is one cycle long vs reSID *and* the locked spec — **LOW, confirmed spec mismatch**

reSID: `if (++rate_counter & 0x8000) rate_counter = ++rate_counter & 0x7fff;`
— the counter goes `0x7FFF → 1` in one cycle (an LFSR with 2^15−1 states; the
spec doc pins the same: "`counter = (counter + 1) & 0x7FFF`"). The
implementation wraps `0x7FFF → 0` (2^15 states): the jump-clock formula
`(0x7FFF - c) + 1 + p` (`sid.rs:166`) is one cycle longer than reSID's
`(0x7FFF - c) + p`, the partial-advance mask (`sid.rs:171-172`) lands one
short, and both the test reference (`sid.rs:597-609`) and
`rate_counter_wrap_reproduces_delay_bug` (`sid.rs:710-721`) pin the deviant
behavior, so the property test cannot catch it.

One cycle in ~32 768 is inaudible, but it breaks the "bit-exact for a supplied
timeline" contract against any future reSID oracle (F14), and the fix is
mechanical: adjust the formula, the partial-advance arithmetic, the reference
stepper, and the pinned test together.

### F5 — noise LFSR clocked from the post-sync accumulator — **LOW-MEDIUM**

`sid.rs:442-453`: sync resets are applied to `next` *before*
`clock_noise(old, next)`. reSID clocks the shift register inside
`WaveformGenerator::clock()` (from the natural accumulator increment) and
applies `synchronize()` afterwards — so when a sync reset coincides with a
bit-19 rising edge, reSID shifts the LFSR and this model does not. Only
audible for noise-waveform voices that are also hard-sync targets (a known
percussion trick), where the LFSR phase then drifts from hardware. *Fix:*
compute `clock_noise(old, natural_next)` before overwriting `next` with the
sync reset (destination accumulator still ends at 0, matching reSID).

### F6 — `chip_articulation` retrigger count eats one attack for legato-started notes — **MEDIUM (export)**

`synth.rs:693-698` counts `EnteredAttack` events over the note's frames and
applies `saturating_sub(1)` on the assumption that the note's own onset
contributed one attack. A note created by a legato pitch change (no gate rise,
no attack event) that then receives a genuine mid-note retrigger reports
`retriggers = 0` — the hard-restart is silently dropped from the census and
any downstream articulation decision. *Fix:* subtract the onset attack only
when the note actually started with one (the note detector knows; or check
for an `EnteredAttack` in the start frame at/after the note's onset offset).

### F7 — TEST-bit LFSR micro-model is uncited and partial — **LOW**

`sid.rs:465-480` applies a specific transform on TEST rise
(`bit1 ← !bit19`, other bits kept) and one feedback shift on TEST fall. This
resembles residfp's researched two-phase shift behavior, but: (a) no comment
cites the model, so it is unreviewable against its source; (b) the *hold*
behavior is missing — on real chips the shift register progressively fills
with ones while TEST stays set (residfp: `shift_register_reset` ≈ tens of ms),
so drivers that park TEST for many frames get a deterministic all-ones-ish
state on release, which this model does not reproduce. *Fix:* cite the exact
reference implementation for both edge transforms; add the timed fill (a
`test_set_at: ChipCycle` field suffices); pin all three behaviors with oracle
vectors (F14).

### F8 — combined-waveform LFSR writeback not modeled — **LOW**

Combined waveforms returning 0 from `oscillator_output` with
`combined_waveform_exact = false` is an honest, documented policy (good). But
on real hardware, selecting noise *together with* another waveform actively
pulls shift-register bits low (the destructive "noise writeback"); after such
an episode the LFSR content differs from this model permanently, so
`noise_shift_register` snapshots and subsequent pure-noise OSC3 reads are
silently wrong for the rest of the tune. *Fix (cheap):* track a sticky
`noise_state_poisoned` flag per voice, set when noise+other is selected while
clocking; surface it next to `combined_waveform_exact` so the census and the
deterministic-noise-seed export (`waveform_programs.deterministic_noise_seeded`)
can refuse to claim exactness. Modeling the actual bit-zeroing (residfp does)
can come later.

### F9 — reads of write-only registers return the RAM mirror — **LOW**

`bus.rs:61-73` routes only `$D41B/$D41C` to the core; reads of `$D400-$D41A`
fall through to `ram[addr]`, which `set_byte` keeps in sync per register. Real
hardware returns the shared data-bus latch (the last value written to *any*
SID register, decaying over ~ms). Practical consequences: read-modify-write
digis (`inc $d418`) *accidentally work* here (same-register readback), while
cross-register readback diverges from hardware — both silently. POT
`$D419/$D41A` read as 0; open paddle lines on real hardware typically float
high (`$FF`), and a few drivers use POT reads as entropy. *Fix:* document the
policy in `Bus`; optionally model the latch as a single `last_write: u8`
(exact enough without decay), and return `0xFF` for POT.

### F10 — SID mirror writes (`$D41D-$D7FF`) silently land in RAM — **LOW**

The SID is incompletely decoded on a real C64: `$D400-$D7FF` all address the
chip. The trap window is `$D400-$D41C` (`bus.rs:7-8`), so a tune writing via a
mirror (rare, but HVSC contains oddballs) mutates plain RAM and the trace
shows a silent, wrong-looking driver. *Fix:* count writes to
`$D41D-$D7FF` and emit a loud diagnostic (mirroring them into the core is then
a one-line policy decision).

### F11 — `OscillatorSnapshot` has two meanings — **INFO (API)**

From `DigitalSid::oscillator_snapshots()`, `sync_resets`/`source_msb_edges`
are *cumulative* counters; after `analysis/mod.rs:227-235` rewrites them they
are *per-frame deltas* stored in the same type on `FrameState`. Same struct,
two semantics — a future consumer reading the emu-side snapshot with
delta expectations (or vice versa) will be wrong by construction, which is
exactly the confusion the project's newtype rule exists to prevent. Also
`source_msb_edges` counts the voice's *own* MSB edges (its activity *as* a
source) — the name reads as "edges of my source". *Fix:* split into
`OscillatorState` (absolute: accumulator, LFSR) and `OscillatorFrameActivity`
(deltas), or at minimum document both fields on both sites;
`combined_waveform_exact` is a register-file property and sits oddly on an
oscillator snapshot.

### F12 — misplaced timing-contract doc paste — **INFO (docs)**

The five-line exactness disclaimer is pasted verbatim on
`Trace::total_writes` (`trace.rs:169-177`) — a write counter, neither a replay
nor a snapshot API — and on `DigitalSid::envelope_snapshots` (`sid.rs:568-573`)
where the struct-level copy (`sid.rs:350-357`) already covers it. The spec
asked for it on "the replay and snapshot APIs"; `analyze()` has it correctly.
Remove the stray copies; keep `DigitalSid` + `analyze`.

### F13 — per-cycle sync path cost — **INFO (perf, measure first)**

`DigitalSid::clock_to` falls back to per-cycle stepping of *all three*
oscillators whenever *any* voice has its sync bit set for the segment
(`sid.rs:382-394`) — ~19 656 iterations per frame, for entire tunes that park
a sync bit. Fine for single-tune analysis; measurable on HVSC-wide scans. If
profiling justifies it: only the actual sync destination(s) and their sources
need coupling; uninvolved voices can keep the analytic path, and segments
where no *destination* has sync selected (source MSB edges still countable
analytically) never need the loop. `EmulationClock::first_boundary_at_or_after`
(`mod.rs:124-129`) is a linear scan — harmless today, O(1) by division if init
ever runs very long.

### F14 — no reSID/residfp oracle vectors yet — **IMPROVEMENT, highest leverage**

The validation pyramid's step 4 (cycle-event-stream oracle) is the one
unimplemented layer — and it is precisely the layer that would have caught F1,
F4, F5, and F7 mechanically. Concrete shape: a tiny generator (residfp via a C
shim, or VICE's SID test programs) producing committed golden vectors —
`(cycle, write)*` in, `(cycle, env_level, acc, lfsr)*` out — replayed against
`DigitalSid` in CI with the generator's version recorded. Prioritize vectors
for: DS-zero sustain-raise, delay-bug wrap distances, TEST hold/fall LFSR
state, sync+noise coincidence, and combined-waveform OSC3 bytes (documenting
the intentional divergence).

### F15 — CIA timer capture — **IMPROVEMENT, high export value**

CIA-timed subtunes currently run on a knowingly wrong 50 Hz timeline with a
loud warning (`mod.rs:402-406`) and are excluded from ground truth — the
single biggest timing gap for the export (double-speed and faster tunes are a
large HVSC cohort, and tempo is the most audible property there). The full fix
does not require CIA emulation: snoop writes to `$DC04/$DC05` (and `$DD04/05`)
during `init`, take the latched timer period as `CallRate` (phi2-rational, the
scheduler already supports it), and only fall back to vblank when no timer was
programmed. Multi-speed detection (timer ≠ n·frame) falls out for free.

### F16 — export the measured attack onset (`first_nonzero`) — **IMPROVEMENT (export)**

The review's corpus experiment found real, correctly-emulated delay-bug hits
(Nemesis st1: 21 voice-frames, AWM: 4, over 3 000 frames) where the audible
attack starts up to ~1.7 frames after the gate rise because the rate counter
had to wrap. The export currently anchors note-on at the gate edge;
`EnvelopeFrameActivity::first_nonzero` already carries the true audible onset
at sub-frame precision. Using it (a) fixes onset timing for delay-bug notes,
(b) is the principled anchor for *all* slow-attack instruments. Verify the 21
AWM/Nemesis candidates by ear/render first — this is measurable with the
existing render-abtest harness.

### F17 — velocity mapping ignores duration — **CONSIDERATION (export)**

`export_velocity` (`synth.rs:713-720`) maps `peak_level/255` linearly. A
2 ms percussive blip that touches 255 exports at full velocity even though its
perceived loudness is far lower; `active_cycles` (and the event log) are
already available to weight peak by duration/energy for short envelopes.
Ear-test before changing — this interacts with §A9 master-bus coloring.

### F18 — taint/stepwise timelines are idle-compressed — **INFO (document)**

`call_stepwise` (`runner.rs:128-154`) starts each call at
`digital_sid.cycle()` — the previous call's last write — with no idle
advancement to the next boundary, so under `run_taint`/probe the OSC3/ENV3
values a driver reads differ from the production trace (envelope decays less,
oscillators advance less between calls). Differential probing stays
self-consistent (both runs compressed identically), but absolute comparisons
against `run()` traces are not valid. Worth one doc sentence on `run_taint`
and `call_stepwise`; alternatively have stepwise advance to scheduler
boundaries like `run_play_frame` does.

### F19 — smaller notes

- **Waveform-0 output latch:** OSC3 with no waveform selected returns 0; real
  chips hold the previous DAC value briefly (exploited by a few digi players).
  Rare for voice 3 reads; document as out of scope.
- **Envelope/write pipelines:** residfp models 1-2-cycle pipelines on register
  writes and envelope state changes; combined with instruction-start write
  stamping (documented bound), absolute sub-frame offsets carry a few cycles
  of systematic skew. If a future oracle (F14) diffs at that resolution, stamp
  writes at `instruction_start + len - 1` (the 6502 store is the last cycle);
  relative STA-to-STA spacing is already nearly exact.
- **`emu::sid` → `analysis::voice::Adsr` import** (`sid.rs:1`): the emulator
  layer depending on the analysis layer inverts the crate's conceptual
  layering (analysis consumes emu everywhere else). Move `Adsr` to a neutral
  module (`trace.rs` or a small `sid_types` module).
- **OSC3 attribution window is intra-frame only** (`osc3.rs:47-50`): a read
  late in frame *n* consumed by a write early in frame *n+1* is invisible.
  Track reads within `CAUSAL_WINDOW` of frame end into the next frame's write
  scan if `RandomOnly`/`Unknown` rates look inflated on real OSC3 tunes.
- **`sid-re`/probe reuse:** `Emulator::run_play_frame_stepwise` doesn't clock
  `digital_sid` to call boundaries (same root as F18) — fine for RE, worth the
  same doc sentence.

---

## Priority order

| # | Finding | Kind | Effort |
|---|---------|------|--------|
| 1 | F1 DS-zero `hold_zero` wrap | correctness bug | S |
| 2 | F2 unimplemented-opcode hang | robustness bug | S |
| 3 | F14 reSID oracle vectors | validation | M |
| 4 | F15 CIA timer capture | export fidelity | M |
| 5 | F6 legato retrigger undercount | export correctness | S |
| 6 | F3 `play == 0` detection | robustness | S |
| 7 | F4 wrap off-by-one | spec conformance | S |
| 8 | F16 `first_nonzero` onsets | export fidelity | M |
| 9 | F5 noise-vs-sync ordering | correctness nit | S |
| 10 | F7 TEST-hold LFSR fill + citations | fidelity | M |
| 11 | F8-F12, F17-F19 | hygiene / polish | S each |

F1+F4 should land together with the new unit tests and a re-run of the
chip-state export gates (the digest experiment predicts zero diffs on the four
control assets; any diff that *does* appear is itself a finding). F14 then
locks both in permanently.

---

## Implementation status (2026-07-19, branch `fix/chip-state-review-findings`)

Everything except **F14** (reSID oracle vectors — deliberately deferred) and
**F17** (velocity weighting — needs an ear-test first) is implemented on this
branch, one commit per finding. A fresh-eyes adversarial review of the full
branch diff (with A/B census measurement on the control assets) found four
issues in the first implementation pass; all four are fixed in the final
commit:

1. The first F6 rule looked for the onset attack only in the start frame,
   inflating retriggers for row-leads-gate drivers — now searched over the
   `GATE_ON_LEAD_MAX` window (AWM synth-native: 273 → 93 with identical
   attack/note counts).
2. The F7 fill countdown was edge-triggered; reSID 1.0's `if (test)` branch
   is level-triggered — every control write with TEST set now restarts it
   (AWM holds TEST across frames while rewriting control 796×/2000 frames).
3. F15's adopted CIA rate now reaches the export: `Trace::call_rate`
   → `Export::timing` → the §A6 time base, so tempo is wall-clock
   correct; absolute-rate recipes (arp/LFO/PWM millihz) still assume vblank
   and emit a loud warning on CIA-rate subtunes — migrating them is a
   follow-up.
4. `measured_onset_start` could land on `authored_end` (the successor's
   frame) when a ≤2-frame note was fully swallowed — now guarded.

Open follow-ups: F14 oracle vectors; F17 velocity weighting (ear-test);
migrate absolute-rate recipes to `PlaybackTiming`; render-abtest pass on
Ark_Pandora/AWM for the F16 onset shift (blast radius beyond the delay-bug
cohort: late-in-frame gate writes also shift, arguably correctly, but
unreviewed by ear); a distinct JAM-vs-unimplemented opcode error message;
plumb `SidModel` into `DigitalSid` for the 8580 fill constant.
