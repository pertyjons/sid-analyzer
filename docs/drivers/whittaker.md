# David Whittaker bytecode player

> **Status: native pitch and musical-stream structure for two verified player
> generations.** Authored instruments are not recovered yet.

## Coverage

HVSC #84 contains 117 files identified by SIDId as `David_Whittaker`. A
deterministic full-corpus qualification accepts the Defcom/Glider Rider
voice-state family in nine files:

- Defcom
- Elevator Action
- Glider Rider
- Grange Hill
- Hyperbowl
- Leviathan
- Panther
- Storm
- Terra Cognita

It also accepts the older nibble-note generation used by BMX Simulator and Red
Max. At 400 play calls, strict native validation accepts all eleven with exact
recovered source structure. Agreement for the original nine ranges
from 93.0% for Defcom to 100% for Elevator Action, Grange Hill, Leviathan,
Panther, and Terra Cognita. The remaining typed classes are 97 locator
failures, six inexact-timing rejections, two init-emulation failures, and one
extractor-setup emulation failure.

Defcom passes for its complete 5,400-call diagnostic window. All three Glider
Rider subtunes pass their complete 5,100-call diagnostic windows. Elevator
Action passes at 3,609 calls, Grange Hill at 6,165, and Hyperbowl at 5,965.
BMX Simulator and Red Max pass strict native export over their complete
2,150- and 2,350-call start-song windows.

## Located representation

The player has three contiguous, relocated voice-state records, each `$24`
bytes long. Init clears all records with this code shape:

```text
LDY #$23
LDA #$00
STA voice1,Y
STA voice2,Y
STA voice3,Y
DEY
BPL ...
```

The three operands must differ by exactly `$24`. The current authored note
index is at record offset `$12`.

The family uses an interleaved little-endian SID-frequency table with the
canonical values `$0116, $0126, $0138, $014B, $0160`. Some generations
reference a table view 24 bytes after that canonical prefix, so the locator
requires a matching `LDA table,X` low/high pair and uses its operand rather than
the prefix address.

The driver transforms the note index with tune data and self-modifying
transpose operands before the lookup. The extractor therefore observes X at
the located lookup instruction during replay and assigns that effective byte
offset to the active voice-state pointer. This preserves dynamic transposition
without treating the resulting SID register value as authored pitch.

Locator success requires exactly one coherent voice-state candidate and one
code-referenced frequency-table candidate; ambiguity is a rejection.

The older generation has three repeated stream readers. Each reader compares
`$7F` as its order transition, calls one shared note converter, and then writes
one physical SID voice. Notes encode octave in the high nibble and semitone in
the low nibble. The converter indexes an interleaved 12-note base table and
shifts the result by the authored octave. The locator requires the converter,
all three code references, the corresponding SID-frequency stores, and the
three stream fetches. Replay observes the accumulator at each converter call,
so inline player transposition remains authored evidence. This generation's
duration is maintained in procedural player state rather than a proven source
field; recovered pattern events therefore use zero for unknown source duration
instead of fabricating a native value. Rendered note duration remains the
measured gate articulation described below.

## Trust boundary

Pitch is decoded from the player's effective native frequency-table lookup.
Note articulation and duration currently use the gate behavior produced by the
driver. Noise-program notes use trace pitch only for validation because their
oscillator frequency is a timbre control rather than a musical pitch; exported
pitch remains the native table value. Census provenance therefore reports
`note.pitch` as `authored_decoded` and `note.articulation` as `trace_measured`.

The located musical-byte fetch is observed alongside the frequency lookup.
Maximal forward source runs become patterns; pointer jumps create placements,
and jumps back into an already observed run reuse its pattern number. Raw note
indices, durations, driver commands, source offsets, placement order, and repeat
ordinals are retained in `recovered_structure`. This is the player's procedural
stream graph, not similarity-based phrase detection.

Instruments and their command parameters remain trace-derived.

## Reproduction

```bash
cargo run --release -p sid-analyzer --bin sid-whittaker-qualify -- \
  --corpus /path/to/C64Music --frames 400 \
  --output /tmp/whittaker-qualification.json

cargo run -p sid-analyzer --bin sid-re -- taint assets/music/Defcom.sid --frames 400
cargo run -p sid-analyzer --bin sid-re -- probe assets/music/Defcom.sid --frames 240 --targets 24
cargo run -p sid-analyzer --bin sid-re -- taint assets/music/Glider_Rider.sid --frames 400
cargo run -p sid-analyzer --bin sid-re -- probe assets/music/Glider_Rider.sid --frames 240 --targets 24
```
