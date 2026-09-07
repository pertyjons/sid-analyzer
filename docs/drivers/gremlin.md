# Ben Daglish / Gremlin player

> **Status: native notes and exact order/pattern structure for compact and
> repeated-order-setup generations.** Instruments and effects remain
> trace-derived.

## Coverage

HVSC #84 contains 60 files identified as `Ben_Daglish/Gremlin`. The permanent
`sid-gremlin-qualify` command runs every start song for 400 play calls and uses
the same precision/recall/timing gate as production native export:

```text
identified=60 accepted=49 structured=49 locate_failed=10 unsupported_configuration=1
```

The repeated-order-setup locator added ten accepted variants: Basil the Great
Mouse Detective, Gauntlet II, Jack the Nipper II, Re-Bounder, Thing Bounces
Back, Action Fighter, Monty Python's Flying Circus, Rick Dangerous, Rick
Dangerous II, and Saint and Greavsie. These builds repeat a longer per-voice
init block instead of the compact three-store form. Some guard the third block
with `CPY #limit / BCS`; the locator reads that limit and prevents a two-voice
tune from acquiring a fabricated third native stream.

The zero-page-indexed generation uses the same table grammar with shorter
transpose, detune, and subtune-selector operands. Requiring its complete note
conversion and three consecutive orderlist-pointer pairs added another nine
accepted start songs across HVSC without relaxing native validation. In the
Daglish directory this recovers Blasteroids, Chubby Gristle, Flintstones,
Munsters, Pac-Mania, and Terramex: 23 newly accepted subtunes in the full-length
census. A stricter absolute-selector pointer-pair match also resolves Terramex's
otherwise ambiguous repeated setup while the established relaxed match remains
available for generations that copy the pointer through several destinations.

The former four located strict failures were:

- `Spy_Who_Loved_Me`: exact onset set, but 72.3% precision/recall under full
  duration-aware alignment;
- `Gary_Linekers_Hot_Shot`: 0.63 exact-MIDI onset agreement, dominated by one
  voice's effect-shifted gate pitch;
- `TechnoCop_V2`: 0.26 exact-MIDI onset agreement with the same systematic
  effect-pitch behavior;
- `Cyberball-Football_in_the_21st_Century`: 397 decoded rows versus 144 traced
  notes, indicating an unsupported loop/control variant.

`MASK_III_Venom_Strikes_Back` exposed an independent decoder bug: its init
seeds per-voice transpose to `$F4`. Reading transpose and detune from the
post-init player cells instead of assuming zero makes it pass production
validation with all 100 notes matched.

The three effect-shifted pitch cases now pass after gate-adjacent player-state
evidence was added. Cyberball remains the one typed unsupported configuration:
its post-init pattern pointers are all zero and require a first-play stream
control model. The remaining ten files are locator failures. Their reports
include counts for absolute and zero-page note decoders, pattern lookup,
compact order setup, strict pointer-pair setup, repeated order setup, and
zero-page order setup, so each unresolved generation is clustered without
losing its typed failure class.

## Located representation

The locator cross-checks three relocation-independent instruction shapes:

- note conversion: transpose plus `$14` bias into parallel frequency tables;
- pattern lookup: pattern number into split low/high pointer tables;
- subtune setup: per-voice orderlist pointers selected through a subtune table.

For the repeated setup generation, three longer blocks must reference
consecutive words in one orderlist-pointer table. This cross-check keeps the
relaxed setup match relocation-independent. Direct pointer-pair copies use a
stricter match first; zero-page-indexed builds require the same three-word
invariant before their shorter cells are accepted.

The player has up to three orderlists; guarded variants can leave the third
physical voice inactive. Bytes below `$80` select a pattern, `$FF` stops the
voice, `$E8` carries a filter value, and the remaining high bytes encode
transpose, repeat count, detune, or instrument-pointer changes.

Patterns retain their native byte grammar:

- `[note][duration]` for bytes below `$80`;
- `[A0][duration]` for rests;
- one-byte instrument/filter/timbre commands;
- four-byte `$C0..$FE` effect blocks;
- `$FF` pattern end, optionally repeating according to order state.

## Structure trust boundary

The frame-synchronous decoder and structure recorder are one pass over the same
state machine. `recovered_structure` retains pattern numbers, byte offsets,
durations, raw commands and operands, order offsets, transpose changes, repeat
counts, and every runtime instance. `structure` is the corresponding placement
timeline used for rendering. Repeated pattern numbers are source reuse, not
similarity matches.

Pitch and structure are authored-decoded. Gate articulation, instruments,
effects, and timbre characteristics remain trace-derived.

## Reproduction

```bash
cargo run --release -p sid-analyzer --bin sid-gremlin-qualify -- \
  --corpus /path/to/C64Music --frames 400 \
  --output /tmp/gremlin-qualification.json

SID_DBG_TUNE=/path/to/tune.sid SID_DBG_FRAMES=400 \
cargo test -p sid-analyzer --lib \
  gremlin::tests::dbg_gremlin_tune -- --ignored --nocapture

cargo run -p sid-analyzer --bin sid-re -- taint \
  assets/music/720_Degrees.sid --song 2 --frames 400
```
