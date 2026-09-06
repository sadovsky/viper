# NSF pipeline

How a `.vip` becomes a Nintendo Sound Format file, how viper plays that
file back through an emulated APU instead of its own synth, and how the
result is rendered to stems. This is the viper half of the
[nintendo-metal](https://github.com/sadovsky/nintendo-metal) project
plan; the album-specific half (driver source, generator style, reamp
session) lives in that repo. Nothing in this document is specific to
one album.

Stage numbers refer to [`STAGES.md`](STAGES.md).

## Why

viper's synth is a convenience. It is not a 2A03. If the point of a
chiptune record is that it runs on the hardware, the audio has to come
from real 6502 code driving a cycle-accurate APU, and viper's job is to
be the front end and the compiler for that — not the instrument.

Three properties fall out of that:

1. **Every song is an NSF.** Real code and data, playable on a flash
   cart, in NSFPlay, in Mesen.
2. **Playback is emulation.** `space` compiles and runs the NSF through
   an APU core. The internal synth stays available for quick editing,
   but it is never the render path.
3. **Renders are receipts.** A register-write log per render lets any
   third-party emulator confirm the audio is what the NSF actually does.

## Architecture

```
.vip ──parse──▶ Song ──lower──▶ IR ──emit──▶ NSF ──6502 host──▶ APU core ──▶ PCM
                                  │                    │
                                  │                    └──▶ (frame, addr, value) log
                                  └──▶ driver bytecode  ◀── driver.bin + driver.sym
```

Three crates, in dependency order:

| crate | stage | owns |
|---|---|---|
| `viper-nsf` | 18 | IR, lowering from `Song`, bytecode emitter, NSF header/bank layout, driver linkage |
| `viper-apu` | 19–20 | 6502 host loop, FFI to the APU core, register-write log, TUI playback path |
| `viper` (bin) | 21–22 | `compile` / `render` / `gen` subcommands, stem + trigger export, style interface |

## IR (Stage 18)

A channel-indexed event stream, one event list per row per channel
(`viper_nsf::Pattern`). Rows are the timestamps: the driver converts
rows to 60 Hz frames with an 8.8 fixed-point accumulator (`900 / BPM`
frames per row at 16th-note steps), so 220 BPM (4.09 frames/row) lands
rows on alternating 4- and 5-frame boundaries and stays exact over a
phrase. `Song::frames_for_rows` reproduces that clock so the compiler
knows a song's length to the frame.

Primitives, chosen for what fast minor-key music needs:

| event | payload | meaning |
|---|---|---|
| `note` | pitch, instr | key-on; period from the NTSC table |
| `off` | — | key-off (TRI via linear counter, others via volume 0) |
| `vol` | 0–15 | set channel volume (ignored on TRI) |
| `duty` | 0–3 | pulse duty select (12.5 / 25 / 50 / 75%) |
| `retrig` | rate (frames) | re-key the current note every N frames |
| `slide` | target pitch, frames | linear period slide |
| `vibrato` | depth, rate | period LFO |
| `arp` | up to 3 offsets | per-frame semitone cycle |
| `env_reset` | — | restart the instrument envelope without a new key-on |
| `dpcm` | sample_id | trigger a DPCM sample |
| `loop` | count | pattern-local repeat |
| `jump` | pattern index | song-order jump |

Channels are an enum of exactly the 2A03 five: `PU1 PU2 TRI NOI DPCM`.
This section used to claim they were a table that an expansion flag
extended with VRC6 `VP1 VP2 SAW` rows, and that the emitter never
special-cases expansion. Both were false, and not merely unimplemented:
the order-entry stride and the driver's zero-page channel arrays are
sized by the channel count, so expansion audio is an ABI change, not a
flag. Authoring VRC6 needs an ABI v2 and a driver that implements it.

What *is* true as of Stage 33: viper can **render** VRC6, and the
expansion byte can no longer lie. See below.

## Emitter and driver linkage (Stage 18)

viper does **not** own a sound driver. The emitter takes:

- `driver.bin` — assembled driver code, position-fixed at its load
  address.
- `driver.sym` — a symbol map naming the entry points (`init`, `play`),
  the data-table anchors (song header, pattern table, instrument table,
  period table, DPCM table), and the driver's ABI version.

and produces one NSF: 128-byte header (load / init / play addresses,
NTSC flag, expansion bits, song count, track names) followed by PRG
banks holding the driver, pattern bytecode, tables, and DPCM samples.
DPCM samples are placed at `$C000+` on 64-byte boundaries, as the
hardware requires.

The bytecode format is whatever the driver reads; viper learns it from
the symbol map's ABI version, and refuses to link a driver whose ABI it
doesn't know. This keeps viper generic across drivers and keeps the
driver free to be as small as it wants (the nintendo-metal target is
≤1 KB).

`.vip` gains a `@driver` directive (see [`FORMAT.md`](FORMAT.md)):

```
@driver  path=driver/build/driver.bin  sym=driver/build/driver.sym  expansion=none
```

and effect columns for `retrig`, `duty`, and `env_reset` where they
aren't already spec'd.

## APU-backed playback (Stages 19–20)

Core: a pure-Rust 2A03 in `viper-apu` (pulses with sweep muting,
triangle with linear counter, 15-bit LFSR noise, DMC with memory fetch,
4/5-step frame counter, the nesdev non-linear mixer as lookup tables,
90 Hz high-pass + 14 kHz low-pass at the output). The plan's default
was blargg's Nes_Snd_Emu over FFI; the Rust core was chosen to avoid a
C++ build dependency and keep renders bit-exact across machines.
Reference-grade accuracy is enforced downstream by the register-log
diff against NSFPlay/Mesen, not by the core's pedigree.

Playback in the TUI:

1. On `space`, compile the current song to an NSF in memory.
2. Run `init` once, then `play` at 60 Hz in a minimal 6502 host (no
   PPU, no mapper beyond what NSF banking needs).
3. Every write to `$4000–$4017` (and expansion ranges) goes to the APU
   core, and to the register-write log.
4. The audio thread pulls PCM from the core. The existing atomic beat
   counter on `Transport` remains the clock; the `VizFrame` bus is fed
   from APU-side state (channel enable, period, volume) so the
   visualizer keeps working unchanged.

**What a log diff does and does not prove.** It validates the CPU, the
memory map, the bankswitching and the frame clock — the half of the
system that decides *which registers get written when*. It says nothing
about the sound cores, because no APU or VRC6 register is readable, so
nothing a core computes can ever feed back into the log. Two emulators
with wildly different mixers produce identical logs. Validating a core
needs audio comparison, not a log diff.

**Register-write log.** Every render emits `(frame, addr, value)`
triples as `frame addr value` text lines (decimal frame, hex address and
value; INIT's writes are frame 0). A verifier normalizes another
emulator's dump into the same shape and diffs. Any divergence between
viper-apu and NSFPlay/Mesen on the same NSF is a viper bug until proven
otherwise.

## Stem rendering (Stage 21)

```
viper render song.nsf --stems out/ [--triggers out/drums.mid]
```

Offline, deterministic: same NSF → bit-identical WAVs. One WAV per
channel, rendered by running the full NSF once per channel with the
other channels' output muted at the mixer (not by dropping their
register writes — DPCM DMA and frame-counter timing stay identical).
DPCM is further split by sample ID, so a two-sample driver yields
`kick.wav` and `snare.wav`.

`--triggers` emits a Standard MIDI File with one note per drum hit
(kick / snare / hat, mapped to GM 36 / 38 / 42 like the Stage 15b
exporter) so a DAW sampler can layer acoustic drums under the chip
ones.

## Style interface (Stage 22)

[`GENERATION.md`](GENERATION.md)'s hierarchical generators (form →
phrase → note; Euclidean rhythms; scale walks) grow a plug-in boundary.
A **style** is a directory that supplies:

- scales and preferred progressions,
- riff templates (rhythmic skeleton + contour rules over a scale),
- harmonization rules (how PU2 follows PU1, when it drops to a pedal),
- drum vocabulary (named patterns, fill rules),
- a song-form grammar with weights,
- album-level constraints (key distribution, tempo curve, shared motif).

viper ships the interface and one neutral style. Genre styles live in
their own repos.

```
viper gen --style <dir> --seed N [-o songs/]
```

is deterministic per seed and style version; the output `.vip` records
both in `@meta` so a track can always be regenerated.

## Expansion audio (Stage 33)

`viper-apu` implements the VRC6 — two pulses and a sawtooth — so viper
renders any VRC6 NSF deterministically: full mix, per-channel stems
(`vp1`, `vp2`, `saw`), register log, `viper verify`. The chip is a
sibling of the 2A03, not a member of it, because that is the hardware:
it sits on the cartridge with its own linear DAC and is summed onto the
audio pin externally, bypassing the 2A03's non-linear tables.

Interception of `$9000-$9003`, `$A000-$A002` and `$B000-$B002` is gated
on the NSF header's expansion bit, so a plain 2A03 file cannot gain a
single line in its register log. That is what keeps the Stage 24 golden
log byte-identical.

**The header can no longer lie.** `@driver expansion=vrc6` used to set
header byte `0x7B` and emit no VRC6 data whatsoever — a file claiming a
chip nothing in it ever writes to, which nothing downstream could catch
because the header was the only claim and it was self-consistent. A
driver now declares what it drives through an optional `DRIVER_EXPANSION`
symbol, the header byte is written from *that*, and a song asking for
more than its driver provides fails to compile.

**Authoring VRC6 is not built.** Nothing emits VRC6 events, because the
channel count is baked into the wire format (see above) and the only
driver in existence is strict 2A03.

## Verification (Stage 24)

Two receipts, both in `tests/golden/` and both enforced by
`tests/pipeline.rs`:

1. **Golden log.** `stress_melodeath.log` is viper-apu's own
   register-write log for `projects/stress_melodeath.vip` compiled
   against the vendored driver. A render must reproduce it byte for
   byte. Regenerate it deliberately when the driver fixture or the
   compiler changes:
   ```
   viper compile projects/stress_melodeath.vip --driver tests/fixtures/driver.bin -o stress.nsf
   viper render stress.nsf --vip projects/stress_melodeath.vip --log tests/golden/stress_melodeath.log
   ```
2. **External emulator.** `stress_melodeath.fceux.log` is FCEUX 2.6.6
   playing the same NSF, captured by `tools/fceux_apu_log.lua`. All 196
   PLAY frames match viper-apu's log write for write (first run
   2026-09-05).

`viper verify` is the comparator:

```
viper verify song.nsf --against other.log [--vip song.vip] [-o normalized.log]
viper verify writes.log --against other.log
```

It reads the other dump loosely — any line with a frame number, a
`$4000`–`$4017` address and a byte, in that order, decimal or hex, with
or without `$`/`0x` — so NSFPlay, Mesen and FCEUX dumps need no
conversion. Two allowances keep the diff about the driver rather than
the player shell: frame numbering may differ by a constant (taken from
the first PLAY frame; FCEUX also runs INIT and PLAY 1 in one frame, which
is detected and split), and INIT-frame writes are compared as a set with
extra player housekeeping allowed. PLAY frames must match exactly and in
order; the first divergence is printed with both sides and the exit code
is 1.

Capturing an FCEUX dump:

```
# Linux, with the distro FCEUX and xvfb:
FCEUX_LOG=out.log FCEUX_FRAMES=300 xvfb-run -a fceux --loadlua tools/fceux_apu_log.lua song.nsf
# WSL, with the Windows build (paths must be Windows paths inside the script):
./fceux64.exe -lua 'C:\path\apu_log.lua' 'C:\path\song.nsf'
```

Downstream repos are expected to CI the same thing: compile each `.vip`,
render with viper-apu, dump with an external emulator, `viper verify`,
fail on any mismatch. nintendo-metal's `session/verify.sh` is the
template.

## Milestones

1. **Stage 18** — IR + emitter produce a valid NSF from
   `projects/stress_melodeath.vip` that plays in Mesen.
2. **Stage 19** — `viper-apu` plays that NSF inside the TUI with the
   playhead locked to it.
3. **Stage 20** — register-write log diffs clean against NSFPlay.
4. **Stage 21** — stem + trigger export.
5. **Stage 22** — style interface, neutral style, `viper gen`.
6. **Stage 24** — golden log + `viper verify`; the stress song diffs
   clean against FCEUX.

Stage 18 needs a driver binary to link against; that is Phase 0/1 of the
nintendo-metal plan and lands there first.
