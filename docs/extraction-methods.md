# Extraction methods — driver-agnostic RE accelerators

> **Status: methodology and proposal reference.** Active extractor priorities
> are consolidated in the corpus-ranked follow-ups in
> [`PLAN.md`](../plans/PLAN.md#4-corpus-ranked-follow-ups).

Method proposals for extracting notes/instruments beyond the per-driver native
extractors. **Verdict (built/rejected):**

- **Method 1 — Dynamic taint analysis → BUILT as `sid-re taint`.** Data-flow
  provenance of SID-register sources. Combined with `sid-re probe`
  (mutate→replay→diff role classification) it drove the **Gremlin extractor
  end-to-end with zero hardcoded addresses**. The reusable RE substrate.
- **Method 2 — Symbolic execution → REJECTED.** These players are worst-case for
  it (self-modifying dispatch, indirect jumps, small code size).
- **Further proposals (A–F), 2026-06-15 review** — ranked at the end. Highest
  leverage: **F (LLM-proposed decoders behind the existing PASS gate)** and
  **D (held-gate plateau segmentation)**.

The original method write-ups and the review conclusions follow.

Currently, `sid-analyzer` uses two distinct strategies for extracting notes:

1. **Heuristic/Register-based synthesis**
   ([synth.rs](../crates/analyzer/src/export/synth.rs)), which observes register
   writes to `$D400..$D41C` and aggregates them into note/timbre shapes using
   threshold-based heuristics.
2. **Static/Driver-specific simulation**
   ([hubbard](../crates/analyzer/src/export/native/hubbard/mod.rs),
   [galway.rs](../crates/analyzer/src/export/native/galway.rs)), which uses
   static signatures to locate sequencer structures in C64 RAM and dynamically
   simulates the driver's state machine.

While the native simulation approach is note-perfect, it requires painstaking reverse-engineering for every new player
engine. To solve this, we propose two alternative automated extraction methods.

---

## Method 1: Dynamic Data-Flow / Taint Analysis

### Theory of Operation

Dynamic taint analysis (or origin-tracking) instruments the 6502 emulation to trace how values written to the SID
frequency and control registers originate from C64 RAM. Instead of treating RAM as a flat array of concrete bytes, we
attach a metadata layer (a "taint" or "provenance" tag) to every byte in RAM and to CPU registers.

```
                  +-----------------------------------+
                  |        Provenance Tracker         |
                  +-----------------------------------+
                                    |
                                    v (Instruction Stepping)
 [RAM Read] ------------> [CPU Registers (A,X,Y)] ------------> [SID Write]
  (Taint source:           (Tainted intermediate state)         (Taint sink: trace
  e.g., ($10),Y or                                               origin address and
  PitchTable,X)                                                  transposition offset)
```

By tracing the provenance of values written to the SID frequency registers (`$D400-$D401`, `$D407-$D408`, `$D40E-$D40F`)
and gate registers (`$D404`, `$D40B`, `$D412`), we can automatically discover:

- **Track Pointers:** The zero-page or RAM locations holding the current pattern address (sourced via indirect-indexed
  addressing `($zp),Y`).
- **Pitch/Note Tables:** The base arrays containing the 16-bit values corresponding to note frequencies (sourced via
  indexed addressing `Table,X` or `Table,Y`).
- **Notes/Opcodes:** The actual raw sequence bytes in RAM that dictate note changes.

### Proposed Implementation Sketch

We can extend the existing [Bus](../crates/analyzer/src/emu/bus.rs) and
[Cpu](../crates/analyzer/src/emu/runner.rs) to maintain an origin map.

1. **Provenance Metadata Struct:**
   ```rust
   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub enum Provenance {
       Concrete(u8),
       Sourced {
           source_addr: u16,
           /// Allows tracking simple arithmetic modifications (like transpositions)
           offset: i8,
       },
   }
   ```

2. **Instrumented Bus/Registers:**
   Instead of raw `u8` arrays, we track the shadow provenance for all 64 KiB RAM and CPU registers:
   ```rust
   pub struct TaintBus {
       pub ram: Box<[u8; 65536]>,
       pub provenance: Box<[Provenance; 65536]>,
   }
   ```

3. **Instruction-Level Trace Rules:**
   For every executed instruction, the CPU emulator propagates the provenance:
    - `LDA addr` -> `RegA.provenance = Bus.provenance[addr]`
    - `LDA (zp),Y` -> `RegA.provenance = Sourced { source_addr: read_addr, offset: Y }`
    - `ADC #val` / `SBC #val` -> Update the offset in `Sourced` provenance if the source is tainted.
    - `STA addr` -> `Bus.provenance[addr] = RegA.provenance`

4. **Register Write Auditing:**
   When a write to a SID register occurs (the "sink"), we inspect the provenance of the value written:
   ```rust
   fn set_byte(&mut self, address: u16, value: u8) {
       if is_sid_frequency_register(address) {
           if let Provenance::Sourced { source_addr, offset } = self.provenance_for_write {
               // We have dynamically located the pitch table entry and its transposition/modifier!
               log_frequency_origin(address, source_addr, offset);
           }
       }
       // ... standard write logic ...
   }
   ```

### Feasibility and Trade-offs

* **Pros:**
    * **Highly General:** Works across almost all C64 player engines since they all must read sequence data from RAM and
      write it to `$D400..`.
    * **Low implementation footprint:** Can be built within the CPU runner loop, without writing custom disassemblers or
      symbolic path finders.
* **Cons:**
    * **Performance Overhead:** Taint propagation requires keeping tracking shadow state for every CPU instruction,
      which can slow down emulation by 3x–5x (though still well within real-time bounds on modern hardware).
    * **Taint Loss:** If a routine uses complex mathematical operations (like looking up frequency indexes via a
      secondary table, or passing data through logic gates), the taint connection might be lost (requires fine-grained
      propagation rules).

---

## Method 2: Symbolic Execution of the Playroutine

### Theory of Operation

Symbolic execution treats the player engine as a program acting on *symbolic* data rather than concrete bytes. Instead
of running a vblank loop frame-by-frame, the playroutine's memory reads are treated as symbolic variables (representing
note streams, durations, and commands).

```
                      +-----------------------------+
                      |  Symbolic Execution Engine  |
                      +-----------------------------+
                                     |
                +--------------------+--------------------+
                |                                         |
                v (Branch condition: A < $80)             v (Branch condition: A >= $80)
      [Path A: Note Decode]                     [Path B: Command/Effect]
      - Constraint: Input < $80                 - Constraint: Input >= $80
      - Action: Write pitch to SID              - Action: Update envelope/tempo
```

When a conditional branch instruction (e.g. `BNE`, `BEQ`) is encountered:

1. The engine forks the execution path.
2. It adds path constraints (e.g., "Input Byte is less than $80" on one branch, "Input Byte is greater/equal to $80" on
   the other).
3. It solves these constraints to map out the entire control flow graph (CFG) of the playroutine.

### Proposed Implementation Sketch

We would replace or wrap the CPU interpreter to run symbolically:

1. **Symbolic Memory & Registers:**
   Memory reads from the track sequence return a symbol `S_n` representing the $n$-th byte of the track.
   ```rust
   pub enum SymValue {
       Concrete(u8),
       Symbolic(SymbolId),
       Expr(Box<SymExpr>),
   }
   ```

2. **Path Forking:**
   When the CPU encounters a conditional branch (like `BNE`) based on a symbolic comparison, we clone the emulator state
   and explore both paths:
   ```rust
   match instruction {
       BNE(offset) => {
           if let SymValue::Symbolic(_) = cpu.status_flag_zero() {
               // Path 1: Zero flag is true (Branch not taken)
               let mut path_true = self.clone();
               path_true.add_constraint(expr_equals_zero);
               path_true.execute_from(pc + 2);
               
               // Path 2: Zero flag is false (Branch taken)
               let mut path_false = self.clone();
               path_false.add_constraint(expr_not_equals_zero);
               path_false.execute_from(pc + offset);
           }
       }
       // ...
   }
   ```

3. **Opcode Mapping:**
   By collecting constraints on paths that eventually write to `$D400..$D41C`, we can statically solve the playroutine's
   complete command set. For instance, if writing a pitch requires `S_0 < $80`, then any byte `< $80` is classified as a
   note. If modifying instrument parameters requires `S_0 == $FE`, then `$FE` is identified as an escape/instrument
   command.

### Feasibility and Trade-offs

* **Pros:**
    * **Static Sound/Structure Mapping:** Discovers the entire sequence format and command map of the tune without
      needing to play the tune frame-by-frame.
    * **Completeness:** Can find hidden tracks, unreachable sound effects, or conditional subroutines that might not
      play during a standard 10,000-frame concrete run.
* **Cons:**
    * **Path Explosion:** 6502 playroutines often feature busy loops, timer waits, and self-modifying code (SMC) that
      cause symbolic execution paths to balloon exponentially.
    * **Implementation Cost:** Writing a symbolic executor for the NMOS 6502 is highly complex and requires integration
      with an SMT solver (like `z3`).

---

## Comparison Matrix

| Aspect                              | Static Signature Drivers (Current)           | Method 1: Dynamic Taint Analysis                     | Method 2: Symbolic Execution                    |
|:------------------------------------|:---------------------------------------------|:-----------------------------------------------------|:------------------------------------------------|
| **Driver Coverage**                 | Low (requires custom code per driver family) | **High** (driver-agnostic)                           | **High** (driver-agnostic)                      |
| **Accuracy**                        | Note-Perfect (for supported drivers)         | Heuristic/Perfect (highly accurate source addresses) | Structural (maps the entire protocol)           |
| **Implementation Complexity**       | Medium (per-driver signature work)           | **Low-Medium** (one-time emulator instrumentation)   | Very High (requires SMT solver & path explorer) |
| **Runtime Performance**             | Extremely Fast                               | Fast (3x–5x emulator speed)                          | Slow (due to path solving)                      |
| **Handling of Self-Modifying Code** | Good (modeled manually)                      | **Excellent** (monitors writes dynamically)          | Poor (hard to model symbolically)               |

---

## Recommended Next Steps

We recommend proceeding with **Method 1 (Dynamic Taint Analysis)** as an incremental, low-risk prototype:

1. **Lightweight Tracker:** Implement a simple shadow provenance tracker for the accumulator `A` and index registers
   `X`/`Y` inside [Cpu](../crates/analyzer/src/emu/runner.rs).
2. **Table Origin Logging:** When writing to `$D400`, trace if the value came from `LDA Table,X`. Record the address of
   `Table` and index `X`.
3. **Corpus Validation:** Test the prototype against known files (e.g., Martin Galway and Rob Hubbard tunes) to verify
   if the dynamically discovered table and pointer locations match our reverse-engineered parameters.

---

## Review & Conclusions (2026-06-10)

Assessed against what the three shipped extractors actually required. Hubbard's
historical 198/289 one-sided onset figure is no longer an acceptance claim; the
current checked result is 8/11 start songs under bidirectional alignment. Galway
(5/5), and Crowther's two generations
plus revision spectrum (50/88, built in one day *with* measurement tooling).
The work splits cleanly into two halves, and the proposals land very
differently on each:

### What taint automates well — the `locate` half

Every per-driver `locate` is a hand-built set of instruction-shape anchors
whose only job is to recover cell addresses. Taint provenance recovers the
same cells driver-agnostically:

- A `$D400/$D401` write sourced from `Table,Y` → **freq table found**
  (today: the octave-fold anchor, hand-generalized per revision).
- Sequence bytes read via `($zp),Y` → **stream pointers found** (today:
  the drain-head anchors — five shape variants and counting across
  Crowther's revision spectrum alone).
- `$D405/$D406` write provenance → **instrument table found** (today:
  hubbard's packed/columnar confirms).

The Crowther revision grind is the strongest evidence: most of that day
went into generalizing byte-shape matchers across hand-evolved builds of
*the same engine*. Taint output would have collapsed that to reading a
report.

**A win the proposal undersells:** the *address stream* itself. Logging
which sequence addresses are read per frame gives pattern reuse for free —
a re-read address range *is* a loop/pattern repeat. That is exactly what
the Galway structure spike measured manually (`EVENT_REC` over
`step_voice`), generalized to any driver. It feeds the structured-export
path (placements), not just cell discovery.

### What taint does not give — the `decode_song` half

Grammar semantics are invisible to data-flow: Crowther's fetch-at-one
countdown (dur `$01` = 256 frames), porta-prefix markers, loop counts'
`DEC $00 -> $FF` wrap, additive vs. replacing transpose, stop commands vs.
positional song end. Every one of these was found by *reading the code* (or
diffing decode against the trace), and every one was needed for the gate to
hit 1.000. Note-perfect, structure-preserving export still requires the
human RE pass — taint shortens it, it does not replace it.

### Design correction: track source chains, not value+offset

The proposed `Sourced { source_addr, offset: i8 }` model breaks on all
three known engines:

- **Index arithmetic, not value arithmetic:** Crowther adds transpose to
  the *note byte before the table lookup* — the interesting offset applies
  to the index, not the written value.
- **Value transforms:** the octave fold halves the table value through
  `LSR/ROR` chains (Galway shifts too); a scalar offset cannot survive.
- **Staged multi-hop writes:** Crowther's conversion result lands in a
  global cell (`$A5DB/$A5DC` / `$FF5F/$FF60`) and reaches the SID frames
  *later*, sometimes on another voice (tie rows re-gate the cell).

The robust model is a **provenance chain**: propagate `source_addr` hops
through RAM stores/loads and record the chain at the sink, without trying
to capture the arithmetic exactly. Value correlation can be re-derived
offline; the addresses are the gold.

### Method 2: rejected

The doc's own risk assessment is right and can be sharpened: these players
are *worst-case* inputs for symbolic execution. All three engines
self-modify — Crowther patches tempo/dispatch immediates and Cobra's
dispatch rewrites its own `JSR` operand every command; Galway block-copies
register images at runtime. Meanwhile the payoff is small: a 6502 player is
1–2 KB of code, and with good measurement tooling a generation falls in
hours. An SMT-backed executor is the wrong tool for this problem size.

### Revised plan

Build Method 1 as **`sid-re taint`** — a subcommand of the existing RE
toolkit, not (initially) an export path:

1. Shadow provenance for A/X/Y + 64 KiB RAM in the existing emulator
   (chain model above). Run N play frames, then report:
   (a) freq/ctrl/ADSR source tables and their index cells,
   (b) sequence stream pointers (zp pairs + their backing cells),
   (c) an address-stream repetition map per voice (the structure signal).
2. **Validate against the known corpus** — this project is unusually well
   positioned: ~253 tunes with gate-verified `HubbardLayout` /
   `GalwayLayout` / `CrowtherLayout` cell addresses as ground truth.
3. First real targets: the **Apex-era 31** (the fold-less Crowther group)
   and **Ben_Daglish/Gremlin (60 tunes)** — the next two engines on the
   roadmap, where the accelerator pays for itself immediately.

Longer term, if the corpus validation shows high precision, a
taint-derived middle export tier (correct note indices/onsets/instrument
identities without grammar-level structure) may be worth revisiting — but
that is an evidence-gated follow-up, not the first deliverable.

---

## Further Proposals (2026-06-15 external review)

A second review proposed six complementary methods (A–F). Each is assessed
below **against the codebase as it actually stands today** — several premises
are already wholly or partly addressed, which changes their cost/value. Methods
are ranked by leverage; the recommended ordering is at the end. Two of the six
(C, D) live better in [PLAN.md](../plans/PLAN.md) and are only summarized here.

### A — Driver-agnostic structure via grammar induction / loop detection

**Concept.** Treat the emulated note+effect stream as a "document" and run
grammar induction (Sequitur) or build a suffix tree to recover patterns,
sub-patterns, and transpositions driver-agnostically; detect a *global* loop on
note level to export a finite loop instead of a fixed frame cut.

**What already exists.** Native decoders already recover orderlist structure as
`PatternPlacement` (one per orderlist step, `synth.rs:1851`) — the flat
monolithic export is only the **fallback (non-native) path**. And the taint
track's *address-stream repetition map* (see "A win the proposal undersells"
above) already targets loop/pattern reuse driver-agnostically, from the RE side.

**Verdict — split it.** *Loop detection* is a cheap, high-value win for the
fallback path; do it. *Full grammar induction* has a canonicalization trap: the
SID note stream is noisy (vibrato, micro-timing, arpeggio expansion), so
exact-match grammar induction will under-merge unless the stream is
**canonicalized first** (strip vibrato, fold arpeggios back, quantize timing to
rows). It also only produces *fallback-quality* structure — native decoders stay
better where they exist. **Rank: medium** (loop-detection now; grammar induction
later, and only with canonicalization).

### B — Periodicity analysis on RAM update timestamps

**Concept.** Log per-address write timestamps during emulation and run an FFT /
autocorrelation on each address's update intervals. Tempo tickers and step
counters update on extremely regular periods, so an address whose change
frequency correlates with the tune tempo is very likely a control/tempo cell —
recovering variables taint misses (they live in branch conditions, never reach a
SID register).

**What already exists.** `sid-re probe` already classifies byte *roles*
(pitch/timing/envelope) causally via mutate→replay→diff. So role-classification
overlaps; B's unique contribution is **discovery without per-byte mutation
cost** — it complements taint's blind spot directly.

**Verdict — good idea, wrong DSP.** These are discrete event trains with a
*fixed integer period*, not continuous signals. Autocorrelation on the
inter-update-interval series — or, simpler, a per-address write-**period
histogram** flagging low-variance periodic writers — finds tempo/counter cells
without FFT windowing/leakage/bin-resolution overhead. **Rank: medium**, as an RE
accelerator alongside taint/probe; replace FFT with autocorrelation/histogram.

### E — Control-flow-graph signature matching for driver ID

**Concept.** Build a CFG over the driver's `play` routine and match graph
topology against known templates; survives relocation and light patching where
byte signatures break.

**What already exists.** Relocation-independent signatures already ship —
Gremlin's three reloc-independent sigs, Crowther's dispatch-tail matching, the
SIDId-port driver ID. *Structural* byte-sigs (dispatch tails, indirect-jump
tables) already survive relocation.

**Verdict — right instinct, cheaper variant already shipping.** Full CFG
recovery from 6502 is hard precisely here: every engine documented in this file
self-modifies (Crowther/Cobra dispatch rewrites, Galway block-copies) and uses
indirect jumps — both wreck static CFG recovery. High cost, unclear marginal gain
over the existing reloc-independent sigs. Pursue **only** if driver-ID
false-negatives become a *measured* bottleneck (e.g. Gremlin's 29 locate-fails),
and then as a targeted experiment, not a framework. **Rank: low.**

### F — LLM-in-the-loop driver RE

**Concept.** Feed the combined `sid-re dis` + `sid-re taint` + `sid-re probe`
reports for a small (1–2 KB) 6502 player to an LLM agent that drafts the Rust
decoder (`Layout` struct + `decode_song`) for a new driver family, dramatically
speeding HVSC scale-up.

**What already exists — the decisive point.** The **verification gate already
exists**: every native extractor is accepted only by an X/N PASS gate
(byte-identical / faithful-frame-sim onset agreement vs. the real emulated
trace). That makes a generated decoder a **hypothesis the gate accepts or
rejects**, not trusted output.

**Verdict — highest scaling leverage.** It automates exactly the manual
taint+probe+dis loop these RE sessions already run, and the PASS gate de-risks
correctness. It attacks the real bottleneck of the whole native-decoder strategy
— per-driver RE cost (Gremlin 27/60, Crowther 52/88, many locate-fails). Frame
it strictly as *"LLM proposes → existing gate verifies"*, never *"LLM emits
trusted code"*. **Rank: high.**

### C — Spectral / MIR complement → historical proposal

Two sub-ideas: (1) **chip-revision detection** (6581 vs 8580) from filter-sweep
spectra; (2) **drum classification** via spectral centroid / zero-crossing rate
on rendered drum frames. Both belong with PLAN Track C (M7 Slice 4 spectral
fidelity), which already plans in-process rendering via `resid-rs`/`sidera`.
Grounding: the PSID header already carries a chip-model flag (already used by the
synth filter curve, `export/synth.rs`), so revision inference only helps the
*undeclared/either* minority and is hard (filter behaviour is program-dependent)
— low ROI; M7 already does drum subclassing, and reSID A/B spectral comparison
already runs **out-of-process via the Pertylizer MCP** (`analyze_mix_bus`). So do
**not** link reSID in-process just for this — add spectral drum features only if
M7 subclassing needs them, reusing whatever Track C lands. **Rank: low / folded.**

### D — Held-gate legato segmentation → historical proposal

Already documented as a known bug (held-gate players collapse the timbre
pipeline to raw pulse). Refinement from this review: the hard part
(`pitch_plateaus`) **already exists and ships in the export legato split**, so
the fix is *wiring it earlier* (into the `detect_notes` / pre-timbre feed), not a
new algorithm — and `detect_notes` also drives MIDI export and note-on/off, so
add a plateau-aware segmentation **mode feeding M7** rather than destructively
changing `detect_notes`. **Rank: high, low-risk.**

### Recommended ordering (this review)

1. **D** — wiring `pitch_plateaus` into the pre-timbre feed; unblocks M7 timbre
   for the whole held-gate class (Whittaker/Galway). Lowest risk, additive.
2. **F** — LLM-proposed decoders behind the existing PASS gate; the scaling lever
   for HVSC driver coverage.
3. **Loop detection** (the safe subset of A) for the driver-agnostic fallback
   export.

B is an RE accelerator to reach for during the next manual RE pass (as
autocorrelation, not FFT); C folds into Track C; E waits for a measured driver-ID
bottleneck.
