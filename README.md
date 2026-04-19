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

Four-voice (two pulse, one triangle, one noise) step sequencer in a
single Rust binary, running inside your terminal, controlled entirely
by vim-style modal keys. Write songs in a tracker grid, yank/paste
patterns, script up drum patterns with `:gen`, save to a plain-text
`.vip` file you can `grep` and diff.

## Install & run

Requires Rust **1.80+**. On Linux, `cpal` needs the ALSA development
headers (`libasound2-dev` on Debian/Ubuntu, `alsa-lib-devel` on Fedora).

```sh
git clone https://github.com/sadovsky/viper.git
cd viper
cargo run --release
```

Viper boots with a demo song loaded — an Am–F–G–Am (i–VI–VII–i)
progression with a lead pulse, an arpeggiated pulse, a triangle bass,
and kick/snare/hat on the noise channel. Press any key to dismiss the
splash, then <kbd>space</kbd> to play.

## The 30-second tour

| action                           | keys                                |
|----------------------------------|-------------------------------------|
| move                             | `h j k l` or arrows (`4j` = down 4) |
| jump by bar / phrase / column    | `w b`, `{ }`, `0 $ g G`             |
| insert a note                    | `i`, then bottom keyboard row       |
| delete a row / bar / phrase      | `dd`, `dab`, `dip`                  |
| delete a channel column          | `div`                               |
| yank / paste                     | `y{...}`, `p` or `P`                |
| visual block selection           | `v`, then move, then `d` / `y` / `x`|
| repeat last destructive action   | `.`                                 |
| play / stop                      | `space` or `:play` / `:stop`        |
| save / load `.vip`               | `:w path`, `:e path`                |
| edit instrument                  | `F2` or `:inst`                     |
| help screen                      | `?` or `F1`                         |
| quit                             | `ZZ`, `Ctrl-q`, or `:q`             |

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
```

More generators (Markov chains, chord-progression voicing, basslines,
arpeggiator, L-systems, cellular automata, LLM prompting) are planned —
see [`docs/GENERATION.md`](docs/GENERATION.md) for the full design.

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
proper `viper --check path.vip` is on the roadmap.

## Why?

Because modal editing is the right answer for pattern data. Because
chiptunes sound great and a terminal is a perfectly good place to make
them. Because `h j k l` in a tracker grid feels correct in a way that
mouse-driven DAWs never quite do.

## Status

**Stages 1–4 and 3.5 are shipped.** You can:

- edit a 16-step × 4-channel phrase with full vim motions, operators,
  text objects, visual block selection, counts, and `.` repeat;
- play it back with sample-accurate ADSR-driven pulse/triangle/noise
  synthesis via `cpal`;
- edit instruments with a dedicated modal editor;
- save and load `.vip` files;
- generate drum patterns, Euclidean rhythms, and random-in-scale melodies.

Upcoming: live keyboard monitoring, overdub recording, scene launching,
a real-time visualizer, sprite bindings, WAV/MIDI export. See
[`docs/STAGES.md`](docs/STAGES.md) for the full roadmap.

## Contributing

Issues and pull requests welcome. Keep changes scoped to a single
stage if possible; lean toward `cargo test` coverage for anything in
the audio engine or `.vip` parser.

## License

MIT.
