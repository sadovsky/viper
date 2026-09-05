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

Channels are a table, not an enum. The 2A03 set is `PU1 PU2 TRI NOI
DPCM`; an `expansion` flag on the channel table adds VRC6 `VP1 VP2 SAW`
as more rows. The emitter never special-cases expansion beyond the
header bits and the driver build variant it links against.

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

## Verification

Downstream repos are expected to CI this: compile each `.vip`, render
with viper-apu and with NSFPlay headless, diff the register logs per
frame, fail on any mismatch. viper's own test suite carries the same
check for `projects/stress_melodeath.vip` against a checked-in golden
log.

## Milestones

1. **Stage 18** — IR + emitter produce a valid NSF from
   `projects/stress_melodeath.vip` that plays in Mesen.
2. **Stage 19** — `viper-apu` plays that NSF inside the TUI with the
   playhead locked to it.
3. **Stage 20** — register-write log diffs clean against NSFPlay.
4. **Stage 21** — stem + trigger export.
5. **Stage 22** — style interface, neutral style, `viper gen`.

Stage 18 needs a driver binary to link against; that is Phase 0/1 of the
nintendo-metal plan and lands there first.
