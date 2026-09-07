#!/usr/bin/env python3
# Build minimal single-note PSID fixtures: one voice, steady combined/simple
# waveform, held forever. Renders deterministically through reSID (sidplayfp).
# Chip model: encoded in the header (flags bits 4-5); sidplayfp honors it and
# defaults to MOS 6581 when unknown — pinned empirically 2026-07-02
# (Pertylizer `sid` module vs combined_ref.wav: 6581 10.2 dB, 8580 16.2 dB).
#
# The Rust tests build the small fixtures they need in memory. This script is
# retained for reproducible local listening experiments; its SID/WAV outputs
# are disposable and are not checked in.
#
# Run with no args to generate the widened PoC matrix in this directory:
#   controls {0x21,0x31,0x51,0x61,0x71} x pitches {A1,A2,A4} x {6581,8580}
#   + triangle+saw pulse-width controls {0x0200,0x0E00}; 0x0800 is the base file
#   + noise (0x81 @ A2), ring-mod (tri @ B5, osc3 = 165 Hz) and hard-sync
#   (saw @ C#5, osc3 = 110 Hz) per model.
# Render references:  for f in *.sid; do sidplayfp -w"${f%.sid}.wav" -t0:02 -vp "$f"; done
# The legacy two-arg form (saw.sid combined.sid paths) is kept for the original
# 2026-07-01 PoC fixtures.
import struct
import sys
from pathlib import Path

CLOCK_PAL = 985_248
FLAG_PAL = 0x0004
MODEL_FLAG = {"unknown": 0x0000, "6581": 0x0010, "8580": 0x0020}


def freq_reg(hz):
    return round(hz * (1 << 24) / CLOCK_PAL)


# Filter modes as the $D418 mode nibble (bits 4-6): LP / BP / HP.
FILTER_MODE = {"lp": 0x10, "bp": 0x20, "hp": 0x40}


def build(control, path, freq=0x0751, pw=0x0800, ad=0x00, sr=0xF0, vol=0x0F,
          model="unknown", v3_freq=None, filt=None):
    # `filt`, when set, is (cutoff_11bit, resonance_4bit, mode) — routes voice 1
    # through the SID filter: $D415/$D416 cutoff, $D417 resonance + voice-1
    # enable, $D418 mode nibble | volume.
    LOAD, INIT = 0x1000, 0x1000
    def st(imm, addr):  # LDA #imm ; STA addr
        return bytes([0xA9, imm & 0xFF, 0x8D, addr & 0xFF, (addr >> 8) & 0xFF])
    code = b""
    code += st(freq & 0xFF, 0xD400)
    code += st(freq >> 8,   0xD401)
    code += st(pw & 0xFF,   0xD402)
    code += st(pw >> 8,     0xD403)
    code += st(ad,          0xD405)
    code += st(sr,          0xD406)
    if v3_freq is not None:  # voice 3 oscillator = ring/sync source for voice 1
        code += st(v3_freq & 0xFF, 0xD40E)
        code += st(v3_freq >> 8,   0xD40F)
    if filt is not None:
        cutoff, res, mode = filt
        code += st(cutoff & 0x07,        0xD415)  # FC low 3 bits
        code += st((cutoff >> 3) & 0xFF, 0xD416)  # FC high 8 bits
        code += st((res << 4) | 0x01,    0xD417)  # resonance + filter voice 1
        vol = FILTER_MODE[mode] | (vol & 0x0F)    # mode nibble | volume
    code += st(vol,         0xD418)
    code += st(control,     0xD404)  # control last: gate + waveform bits
    code += bytes([0x60])            # RTS  (end of init)
    play = INIT + len(code)          # play = RTS, note sustains
    code += bytes([0x60])

    hdr = bytearray(0x7C)
    hdr[0x00:0x04] = b"PSID"
    struct.pack_into(">H", hdr, 0x04, 0x0002)   # version 2
    struct.pack_into(">H", hdr, 0x06, 0x007C)   # dataOffset
    struct.pack_into(">H", hdr, 0x08, LOAD)     # loadAddress
    struct.pack_into(">H", hdr, 0x0A, INIT)     # initAddress
    struct.pack_into(">H", hdr, 0x0C, play)     # playAddress
    struct.pack_into(">H", hdr, 0x0E, 0x0001)   # songs
    struct.pack_into(">H", hdr, 0x10, 0x0001)   # startSong
    struct.pack_into(">I", hdr, 0x12, 0x00000000)  # speed (vblank)
    name = b"SID PoC fixture"
    hdr[0x16:0x16 + len(name)] = name
    struct.pack_into(">H", hdr, 0x76, FLAG_PAL | MODEL_FLAG[model])
    with open(path, "wb") as f:
        f.write(bytes(hdr) + code)
    print(f"wrote {path}: control=0x{control:02X} freq={freq} model={model}"
          + (f" v3_freq={v3_freq}" if v3_freq is not None else ""))


PITCHES = {"a1": 55.0, "a2": 110.0, "a4": 440.0}   # MIDI 33 / 45 / 69
TRI_SAW_PULSE_WIDTH_CONTROLS = {"pw0200": 0x0200, "pw0e00": 0x0E00}
CONTROLS = {0x21: "saw", 0x31: "tri+saw", 0x51: "pulse+tri",
            0x61: "pulse+saw", 0x71: "saw+tri+pulse"}


def gen_matrix(out_dir):
    out = Path(out_dir)
    for model in ("6581", "8580"):
        for ctrl in CONTROLS:
            for pname, hz in PITCHES.items():
                build(ctrl, out / f"c{ctrl:02x}_{pname}_{model}.sid",
                      freq=freq_reg(hz), model=model)
                if ctrl == 0x31:
                    for pwname, pulse_width in TRI_SAW_PULSE_WIDTH_CONTROLS.items():
                        build(ctrl, out / f"c31_{pname}_{pwname}_{model}.sid",
                              freq=freq_reg(hz), pw=pulse_width, model=model)
        build(0x81, out / f"c81_a2_{model}.sid",
              freq=freq_reg(110.0), model=model)                      # noise
        build(0x15, out / f"ring_b5_{model}.sid",
              freq=freq_reg(987.767), v3_freq=freq_reg(165.0), model=model)
        build(0x23, out / f"sync_cs5_{model}.sid",
              freq=freq_reg(554.365), v3_freq=freq_reg(110.0), model=model)


if __name__ == "__main__":
    if len(sys.argv) > 1:
        build(0x21, sys.argv[1])                                      # gate+saw
        build(0x51, sys.argv[2] if len(sys.argv) > 2 else "/tmp/combined.sid")
    else:
        gen_matrix(Path(__file__).parent)
