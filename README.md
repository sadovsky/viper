# viper

A vim-keybound chiptune step sequencer for the terminal.

```
╔══════════════════════════════════════════════════════════════════════════════╗
║                                                                              ║
║    ██╗   ██╗██╗██████╗ ███████╗██████╗                                       ║
║    ██║   ██║██║██╔══██╗██╔════╝██╔══██╗                                      ║
║    ██║   ██║██║██████╔╝█████╗  ██████╔╝                                      ║
║    ╚██╗ ██╔╝██║██╔═══╝ ██╔══╝  ██╔══██╗                                      ║
║     ╚████╔╝ ██║██║     ███████╗██║  ██║                                      ║
║      ╚═══╝  ╚═╝╚═╝     ╚══════╝╚═╝  ╚═╝                                      ║
║           ___                                                                ║
║      ___ /   \___       ┌───┐   ┌───┐   ┌───┐   ┌───┐   ┌───┐                ║
║    >(o o)     ( )──────┐│   │   │   │   │   │   │   │   │   │                ║
║      \_/ \___/ /       ││   │   │   │   │   │   │   │   │   │                ║
║                        └┘   └───┘   └───┘   └───┘   └───┘   └───┘            ║
║                    ── vi keybinding audio stepper ──                         ║
╚══════════════════════════════════════════════════════════════════════════════╝
```

Five-voice (two pulse, triangle, noise, DPCM) step sequencer in a
single Rust binary, running inside your terminal, controlled entirely
by vim-style modal keys. Write songs in a tracker grid, yank/paste
patterns, script up drum patterns with `:gen`, save to a plain-text
`.vip` file you can `grep` and diff — then compile it to a real NSF and
hear it through a cycle-stepped 2A03 (see [`docs/NSF.md`](docs/NSF.md)).

```sh
viper song.vip                                   # open in the tracker
viper check song.vip                             # parse + lowering report
viper compile song.vip --driver tests/fixtures/driver.bin -o song.nsf
viper render song.nsf -o mix.wav --stems stems/ --triggers drums.mid \
             --log writes.txt --vip song.vip
viper verify song.nsf --against fceux.log --vip song.vip  # diff vs another emulator
viper rip song.nsf -o ripped.vip                 # read the music back out
viper gen --style styles/neutral --seed 7 -o songs/    # compose a song
viper dpcm encode kick.wav -o kick.dmc                  # hand-crafted drum samples
```

## Install & run

Requires Rust **1.80+**. On Linux, `cpal` needs the ALSA development
headers (`libasound2-dev` on Debian/Ubuntu, `alsa-lib-devel` on Fedora).

```sh
git clone https://github.com/sadovsky/viper.git
cd viper
cargo run --release
```

### Compiling to NSF needs a driver

viper is the front end and the compiler; it deliberately does not own a 6502
sound driver. `viper compile` links your song's data against one you supply as
a `driver.bin` plus a `driver.sym` symbol map.

A working driver ships in this repo at `tests/fixtures/driver.bin`, so the
`viper compile` line above runs as written. It is ABI v3: 1685 bytes, strict
2A03 (no expansion audio), 16 rows per pattern, and it implements every effect
the IR emits — note, off, volume, duty, instrument, retrigger, portamento,
vibrato, arpeggio and envelope reset. That is enough for the whole toolchain,
including `viper render` and `viper verify`.

viper links against ABI v1 through v3, picking the header layout from the
driver's own declared version, so an older driver keeps working. A second
fixture, `driver-fceux.bin`, is the exact v1 build that FCEUX played when the
comparison log in `tests/golden/` was captured; it is pinned there so that
receipt stays evidence about the driver it actually ran.

Its source, and the ABI it implements, live in
[nintendo-metal](https://github.com/sadovsky/nintendo-metal) under `driver/`.
Point `--driver` at your own build once you want to change the driver itself;
a song's `@driver` directive can name one so you never have to pass the flag.

Viper boots with a demo song loaded — an Am–F–G–Am (i–VI–VII–i)
progression with a lead pulse, an arpeggiated pulse, a triangle bass,
and kick/snare/hat on the noise channel. Press any key to dismiss the
splash, then <kbd>space</kbd> to play.

## The 30-second tour

| action                              | keys                                |
|-------------------------------------|-------------------------------------|
| move                                | `h j k l` or arrows (`4j` = down 4) |
| jump by bar / phrase / column       | `w b`, `{ }`, `0 $ g G`             |
| insert a note                       | `i`, then bottom keyboard row       |
| delete a row / bar / phrase         | `dd`, `dab`, `dip`                  |
| delete a channel column             | `div`                               |
| yank / paste                        | `y{...}`, `p` or `P`                |
| visual block selection              | `v`, then move, then `d` / `y` / `x`|
| repeat last destructive action      | `.`                                 |
| undo / redo                         | `u` / `Ctrl-r`                      |
| play / stop                         | `space` or `:play` / `:stop`        |
| live keyboard monitor / record arm  | `K`, `R`                            |
| mute channel / launch scene         | `M`, digit key in Live mode         |
| record / replay macro               | `q<letter>` ... `q`, `@<letter>`    |
| toggle visualizer                   | `:viz` (bars/scope/grid/orbit/sprites) |
| preview a command before running it | type `:gen …` or `:transpose …`     |
| compare two phrases                 | `:diff 03`, `:diff off`             |
| phrase context above and below      | automatic; `:set scroll=smooth`     |
| save / load `.vip`                  | `:w path`, `:e path`                |
| edit instrument                     | `F2` or `:inst`                     |
| help screen                         | `?` or `F1`                         |
| quit                                | `ZZ`, `Ctrl-q`, or `:q`             |

Insert mode uses the bottom keyboard row as a chromatic piano:

```
 z  s  x  d  c  v  g  b  h  n  j  m  ,  l  .  ;  /
 C  C# D  D# E  F  F# G  G# A  A# B  C  C# D  D# E
```

The full binding reference lives in [`docs/STAGES.md`](docs/STAGES.md).

## Pattern generators

Viper ships with a small library of algorithmic pattern generators
you can invoke from command mode. They're deterministic (same seed =
same song), composable, and fast enough to run on every keypress.

```
:gen four                          # four-on-the-floor drums on NOI
:gen euclid pu1 5 16               # 5-hits-in-16 Euclidean rhythm on PU1
:gen euclid noi 7 16 offset=2      # rotated Euclidean on NOI
:gen scale pu2 A minor density=0.4 # random notes in A minor, 40% hit rate
:gen chord_prog i iv V i key=Am    # voiced progression: PU1 / PU2 / TRI + hats
:gen chord_prog doowop key=C       # bundled presets: 12bar doowop canon andalusian …
:gen bassline Am Dm E Am style=walking
:gen arp Cmaj7 updown 16 rate=2    # arpeggiator on PU2
:gen drums breakbeat fills=2       # kick/snare on DPCM, hats on NOI
:gen lsystem axiom=A rules=A=ABA,B=.A. iterations=4 map=A=C4,B=G3,.=-
:gen cellular rule=30              # Wolfram meets Squarepusher
:gen style styles/neutral 7        # a whole song from a style directory
```

Markov chains over a corpus and LLM prompting are still on the list —
see [`docs/GENERATION.md`](docs/GENERATION.md) for the full design.

## Live performance

Beyond the grid editor, viper is playable as an instrument:

- `K` drops you into **Live mode** — the piano row triggers notes in
  realtime on the current channel without writing to the pattern.
- `R` arms the cursor channel for **overdub recording**; live-mode
  notes snap to the nearest 16th under the playhead while the transport
  rolls.
- Digits `1`–`9` in Live mode launch **scenes** (phrases bound via
  `:scene N save`) at the next bar boundary — Ableton-style continuity.
- `M` mutes the current channel; muted voices drop cleanly within one
  audio buffer and re-enter on the next note.
- `q<letter>` records a **performance macro** (scene launches, mutes,
  transposition, play/stop). `@<letter>` replays. Scene launches inside
  macros still respect the bar-boundary queue.

## Visualizer & sprites

Toggle the viz pane with `:viz`. Six renderers, all using half-blocks
and 24-bit color for 2× vertical terminal resolution:

- **bars** — per-voice envelope levels
- **scope** — synthesized waveform, tinted by loudest voice
- **grid** — 4×4 step grid with a pulsing playhead
- **orbit** — per-voice bodies orbiting a shared ring, pitch → angle
- **sprites** — load 4-color sprite sheets and animate them
- **sheet** — a tile atlas of one sheet, with indices, for finding a tile

### Sprites out of a NES ROM

A sheet can come from a game rather than a PNG:

```
:sprite load ~/roms/megaman3.nes bank=4   # 256 tiles from one pattern table
:sprite show megaman3                     # the atlas — see what you have
:sprite page +1                           # page through it
:sprite place megaman3 0x2A 12 8          # place the tile you found
```

This needs no emulation and loses nothing. NES character data is 2bpp
planar — sixteen bytes per 8×8 tile, one bitplane for each bit of a 0–3
index — which is *exactly* what a viper sprite sheet already is, so the
graphics arrive as the artist drew them rather than quantized down to
four colours the way a PNG has to be.

What it cannot do is invent colour: which palette a tile is drawn with is
chosen by the game's code, per frame, per attribute block. Sheets load with
a legible grey ramp; `:sprite repalette` sets a real one.

Some games have nothing to read. Roughly half the ROMs I tried build their
tiles into CHR-RAM as they run — Metroid, Zelda, Contra, Final Fantasy —
and viper says so rather than reporting an empty sheet.

Sprites can be bound to any audio-reactive source. The binding language
is a small expression DSL — operators, parentheses, a handful of
functions, and sources like `pu1.env`, `noi.gate`, `tri.age` (seconds
since last note-on), `master.rms`, `beat`, `time`:

```
:sprite load ~/mario.png 16x16 q
:sprite place mario 0 10 10
:bind mario y = sin(time * 4) * 6                    # bob on sine
:bind mario scale = pu1.env * 1.5 + 1                # pulse with PU1
:bind mario flipx = tri.gate                         # turn on TRI notes
:bind mario frame = clamp(floor(noi.age * 16), 0, 3) # 4-frame hit anim
```

A bare `<sheet>` address targets every placement of that sheet. Use
`<sheet>.N` for the Nth placement or `<sheet>.*` to be explicit.

Sheets are strict NES-style (≤3 opaque colors + transparent); append
`q` on load to auto-quantize richer PNGs to their top 3 colors.

## The `.vip` file format

Songs save as plain text. Human-writable, LLM-friendly, round-trip
lossless:

```
# viper song file
@song  bpm=140  edit_step=1  current=00

@phrase 00
  # step   PU1        PU2        TRI        NOI
  00       A-5:00:0F  A-3:01:0F  A-2:02:0F  C-4:03:0F
  01       ---        E-4:01:0F  ---        ---
  02       C-5:00:0F  A-4:01:0F  A-2:02:0F  C-3:03:0F
  ...

@instr 00  attack=2  decay=80  sustain=0.60  release=150  duty=0.50  vol=0.70
```

Full grammar lives in [`docs/FORMAT.md`](docs/FORMAT.md). Validate a
file without opening the TUI by writing the parser test yourself —
`viper check path.vip` does the same from the shell.

## Why?

Because modal editing is the right answer for pattern data. Because
chiptunes sound great and a terminal is a perfectly good place to make
them. Because `h j k l` in a tracker grid feels correct in a way that
mouse-driven DAWs never quite do.

## Status

**Stages 1–34 are shipped.** The tracker, the NSF pipeline and the generation
layer are all complete against the roadmap in
[`docs/STAGES.md`](docs/STAGES.md).

**Editing.** A 16-step × 5-channel grid with vim motions, operators, text
objects, visual block selection, counts, undo/redo and `.` repeat; hold cells
(`===`) that sustain a note across rows; an instrument editor drawing its own
ADSR envelope and waveform; plain-text `.vip` files that round-trip losslessly.

**Playing.** Sample-accurate ADSR synthesis through `cpal`, or the real thing:
`:engine apu` compiles the song and plays it through an emulated 2A03. Live
keyboard mode, overdub recording, scene launching on bar boundaries, mutes and
performance macros.

**Arranging.** Phrases into chains into an arrangement, with per-channel
polymeter (`:len`) and swing (`:groove`), or a flat `:order` list.

**Seeing.** A visualizer pane with five renderers, 4-colour PNG sprite sheets,
and a small expression language binding sprite position, scale, rotation,
palette and frame to any audio source. Ghost previews of `:gen` and
`:transpose` before they commit, and `:diff` between two phrases.

**Shipping.** `viper compile` to NSF or NSFe, including multi-song album
bundles; `viper render` for deterministic mixes, per-channel stems, trigger
MIDI and a register-write log; `viper verify` to diff that log against another
emulator (the bundled stress song matches FCEUX frame for frame); `viper dpcm`
to encode DPCM samples with a trellis encoder that returns the DAC to its start
level; `viper import` to turn a MIDI file into a `.vip`; and `viper gen` to
compose whole songs from a style directory.

**Rendering VRC6.** `viper render` and `viper verify` drive a real VRC6 core —
two pulses and the sawtooth — so any existing VRC6 NSF renders deterministically
to a mix, per-channel stems and a register log. The header can no longer claim a
chip the driver does not drive. *Authoring* for VRC6 is a separate job: the
channel count is baked into the wire format, so it needs a driver that
implements the extra channels.

**Ripping.** `viper rip song.nsf -o song.vip` reads music back out of a
compiled NSF — or out of any `frame addr value` register dump another
emulator produced, which is how game music gets in without emulating the
game. It runs the file, folds the register traffic into per-frame channel
state, recovers the row grid by simulating the driver's fixed-point row
clock at candidate tempos, and writes notes, volumes, holds, phrases and an
order list. Ripping the bundled stress song recovers its tempo, its 48 rows
and its 3 phrases exactly.

Instruments come back too, read off the envelopes the notes played rather
than guessed: the stress song's lead is written `s=0.90 r=60 duty=0.25
vol=0.70` and rips as `s=0.909 r=67 duty=0.250 vol=0.733`, from the register
log alone.

Effect columns come back too. Vibrato, portamento and arpeggio all recover
their parameters exactly, and a portamento target is found even though
sliding never retriggers the channel — the case a ripper that followed
key-ons alone would lose outright.

An NSF records none of this, so all of it is inferred, and the report says
which numbers were read and which were guessed — including when two tempos
fit the evidence equally well. DPCM sample data is not extracted yet, so a
ripped drum track plays the built-in bank.

## Contributing

Issues and pull requests welcome. Keep changes scoped to a single
stage if possible; lean toward `cargo test` coverage for anything in
the audio engine or `.vip` parser.

## License

MIT — see [`LICENSE`](LICENSE).
