# viper stages

Development roadmap and the current binding surface. User-facing
install / intro docs live in [`../README.md`](../README.md); format and
generation specs live alongside in [`FORMAT.md`](FORMAT.md) and
[`GENERATION.md`](GENERATION.md).

Stages progress incrementally — each one is shippable. ✅ means done,
no mark means planned.

## Implemented surface

Normal mode:
- `h j k l` / arrows — move cursor (with counts, e.g. `4j`)
- `w` / `b` — next / prev bar (4 steps)
- `0` / `$` — first / last channel (PU1 ↔ NOI)
- `g` / `G` — top / bottom of phrase
- `{` / `}` — previous / next phrase
- `x` — clear cell (`Nx` clears N cells down the column)
- `dd` / `yy` — delete / yank current step row (count prefix: `3dd`)
- `dab` / `yab` — delete / yank current bar (count prefix: `2dab`)
- `dip` / `yip` — delete / yank whole phrase
- `div` / `yiv` — delete / yank current channel column
- `p` / `P` — paste after / at cursor (overwrite)
- `.` — repeat last destructive action (delete, paste, `x`)
- `u` / `Ctrl-r` — undo / redo (snapshot history, up to 200 steps)
- `v` — visual block (rectangular) selection; `d` / `y` / `c` / `x` operate on it
- `V` — visual linewise selection (full-width rows across all channels)
- `c<obj>` — change: delete object and enter insert mode (`cc`, `cip`, `cab`, `civ`)
- `r<key>` — replace cell's note with next piano-row keystroke
- `i` — insert mode
- `a` — append (move down one, then insert)
- `:` — command mode
- `space` — toggle play
- `Esc` — cancel pending count / operator
- `?` / `F1` — toggle help screen
- `F2` — instrument editor
- `K` — live keyboard monitor (piano row plays through audio, no pattern write)
- `R` — toggle record-arm on current channel (`● REC` badge shows armed channels; Esc in normal disarms all)
- `M` — mute / unmute current channel (muted header renders dim; audio silences within one buffer)
- `q<letter>` — record performance macro into register `<letter>` (press `q` again to stop)
- `@<letter>` / `@@` — play back macro / replay last
- `ZZ` — save and quit (errors out if no filename is set)
- `ZQ` / `Ctrl-q` — quit without saving

Insert mode (bottom keyboard row = chromatic octave 4):
- `z s x d c v g b h n j m` — C through B
- `, l . ; /` — continue up into next octave
- `Backspace` — clear and move up
- `Esc` — back to normal

Command mode:
- `:q` / `:q!` — quit
- `:help` — open help screen
- `:inst [NN]` — instrument editor (optional hex index)
- `:set bpm=140`
- `:set step=4` — auto-advance N steps per inserted note (edit step)
- `:set octave=4` — base octave for insert-mode piano row (0–8)
- `:set theme=nes` / `:set theme=phosphor` — switch color theme
- `:set still=on|off|toggle` — freeze the tempo-locked breathing animations
- `:diff [phrase] <A> [B]` — compare two phrases in the grid; `:diff off` (or Esc) dismisses
- `:set scroll=jumpy|smooth|toggle` — phrase scroll style (see Stage 30)
- `:transpose ±N` / `:tr ±N` — shift all pitched notes by N semitones (skips NOI)
- `:viz` / `:viz <kind>` — toggle visualizer pane (kinds: `bars`, `scope`, `grid`, `orbit`, `sprites`, `register`); `:viz off` hides it
- `:sprite load <path> [WxH]` — load a PNG sprite sheet (≤4 opaque colors; cell size defaults to the whole image)
- `:sprite place <sheet> <idx> <x> <y>` — paint a tile into the viz pane (pane coords are half-block pixels)
- `:sprite palette <name> <c0> <c1> <c2> <c3>` — define a 4-color palette (hex `#rrggbb` or `transparent`)
- `:sprite repalette <sheet> <palette>` — swap a sheet's palette
- `:sprite list` / `:sprite clear` — inspect loaded sheets / drop all placements
- `:play` / `:stop`
- `:rec` / `:rec off` — toggle record-arm on cursor channel / disarm all
- `:mute [N]` / `:unmute [N]` — toggle / clear mute (N = 1-4 or pu1/pu2/tri/noi); `:mute off` unmutes all
- `:scene N save` — bind current phrase to scene slot N (1–9)
- `:scene N` — queue/launch scene N (clear with `:scene N clear`, cancel queue with `:scene off`)
- `:w [path]` — save song as `.vip` (path required the first time)
- `:e <path>` — load `.vip`, or start a new song at `<path>` if it doesn't exist
- `:new` — start a new empty song (unsets the current filename)
- `:wq [path]` — save and quit
- `:phrase [NN]` — show / switch to phrase by hex index
- `:phrase new` — append a new empty phrase and switch to it
- `:phrase del` — delete the current phrase (clears if it's the last one)
- `:gen four` — four-on-floor drums on NOI
- `:gen euclid <ch> <k> <n> [off]` — Euclidean rhythm on channel
- `:gen scale <ch> <key> [mode] [density]` — random notes in a mode
- `:gen chord_prog <preset|chords…> [key=Am] [steps=4]` — voiced progression on PU1/PU2/TRI with hats on NOI
- `:gen bassline <preset|chords…> [style=walking|arpeggio|root_fifth|octaves|roots] [key=Am] [steps=4]` — TRI bassline
- `:gen arp <chord> [up|down|updown|random] [len] [rate=1] [ch=pu2] [octaves=2]` — arpeggiator
- `:gen drums <preset> [fills=N] [dpcm=off]` — kick/snare on DPCM, hats on NOI (`:gen drums` lists presets)
- `:gen lsystem axiom=A rules=A=ABA,B=.A. [iterations=4] [map=A=C4,B=G3,.=-] [ch=pu1]` — L-system
- `:gen cellular [rule=30] [ch=pu1] [key=Am] [seed=center|random]` — elementary cellular automaton
- `:gen style <dir> [seed]` — compose a whole song from a style directory (Stage 22)
- `:bounce <path> [loops]` / `:midi <path> [loops]` — offline WAV render / SMF export of the playback sequence
- `:bind <sheet>[.N|*] <target> = <expr>` / `:bind list|clear|del N` — sprite modulation bindings
- `:order [A,B,..]` / `:order off` / `:order loop N` — flat song order (hex phrase indices)
- `:song on|off` — song mode: play through the order, or loop the current phrase
- `:song` / `:song show` — toggle the song pane (arrangement + chains) / print a summary
- `:chain new|del [NN]|sel NN|add NN|pop|name TEXT` — edit chains (`>` marks the selected chain)
- `:arr add NN|del [pos]|loop pos|clear` — edit the arrangement; adding a slot turns song mode on
- `:len <ch> N` / `:len all N` — per-channel polymeter length (1–16)
- `:groove swing N` / `:groove straight` / `:groove <16 ints>` — per-16th sample offsets (synth engine)
- `:driver BIN SYM` / `:compile PATH` — set the NSF driver / compile the song to an NSF
- `:engine apu|synth` — play through the compiled NSF on the 2A03 core, or the internal synth

Instrument editor mode:
- `j` / `k` (arrows) — select parameter
- `h` / `l` (arrows) or `-` / `+` — adjust value
- `[` / `]` — prev / next instrument
- `Esc` / `q` — back to normal

Parameters: attack (ms), decay (ms), sustain (0–1), release (ms), duty (0.05–0.95), volume (0–1).

## Roadmap

### Core engine

- **Stage 1** ✅ — data model, modal input, phrase editor UI
- **Stage 2** ✅ — cpal audio thread, pulse oscillator, sample-accurate step playback
- **Stage 3** ✅ — 4 voices (PU1/PU2/TRI/NOI), ADSR, instrument editor mode
- **Stage 4** ✅ — operators (`d y p`), text objects (`ip ab iv`), unnamed register, `.` repeat
- **Stage 3.5** ✅ — `.vip` text file format + generators (`four_on_floor`, `euclid`, `random_in_scale`)

### Live play

- **Stage 5** ✅ — Live keyboard monitor. `K` enters `LIVE` mode; piano-row keys trigger notes in realtime on the current channel while transport is stopped or playing. Each keypress hits the audio engine directly (via a `live_events` queue on `Transport`), no pattern write. Tab / arrows switch channel, `</>` shift octave, `Backspace` releases, `Esc` all-notes-off.
- **Stage 6** ✅ — Live overdub mode. `R` (or `:rec`) toggles record-arm on the cursor channel. While armed, piano-row keys in Live mode write the played note to the cell under the playhead (while playing) or the cursor (while stopped), in addition to triggering the audio pluck. Mode-line grows a red `● REC <channels>` badge. `Esc` in Normal disarms all armed channels. No sub-step quantize yet — always snaps to the current 16th.
- **Stage 7** ✅ — Scene launching. Scene slots `1`–`9` bind to phrase indices (`:scene N save` captures the current phrase). In Live mode, tapping a digit queues that scene for launch on the next bar boundary while playing, or launches immediately when stopped. Modeline shows a `▸ N → PP (Y)` badge with a per-step countdown while queued. `:scene`, `:scene N`, `:scene N clear`, `:scene off`. Per-channel mutes and drain-animation bar are deferred; launch preserves song step position (Ableton-style continuity).
- **Stage 8** ✅ — Performance macros + channel mutes. `M` toggles per-channel mute (pattern steps skipped, live gates suppressed, audio voice killed on the next callback; muted header renders dim with a `MUTE` tag). `:mute [ch]` / `:unmute [ch]` / `:mute off` cover the same from command mode. On top of that, vim's macro machinery: `q<letter>` records a sequence of performance ops (scene launch, mute toggle, transpose, play toggle) captured at the hotkey layer via a `perform()` indirection; a second `q` saves the buffer. `@<letter>` replays, `@@` re-runs the last one. Scene launches inside macros still respect the bar-boundary queue so replays stay groove-locked. Macro recording shows a `◉ q<letter> (count)` badge in the modeline next to `● REC`; Esc in Normal cancels an in-progress recording (falls through to rec-disarm when neither is active).

### Visualizer

- **Stage 9** ✅ — VizFrame bus. Audio thread writes a `VizFrame { playing, step, step_phase, voices: [VoiceFrame {gate, env_level, freq, vel}; 4] }` slot on `Transport` at the end of every audio callback. UI reads it inside the existing `sync_audio` lock — one slot, newest-wins (we're not accumulating, 60Hz UI never catches up to kHz audio anyway). First consumer: channel-header LEDs now flash off real ADSR level, so live-mode notes light them up too and release decays the glow. Deferred a real lock-free SPSC queue (`rtrb`/`ringbuf`) until Stage 10+ actually needs history.
- **Stage 10** ✅ — Built-in viz (ASCII/Unicode). `:viz` toggles a right-side viz pane; `:viz <kind>` picks a renderer and shows it. Four renderers all use `▀`/`▄`/`█` half-blocks for 2× vertical resolution + 24-bit color, reading the Stage-9 `VizFrame` on every UI tick:
  - **bars** — per-voice envelope bars (env×vel), labelled by channel name
  - **scope** — synthesized waveform trace summed across voices; tint follows the loudest voice so you can tell which channel is singing
  - **grid** — 4×4 step grid with the playhead diamond pulsing on `step_phase`
  - **orbit** — one body per voice on a shared ring; pitch class → angle, velocity → radius, env → brightness
  Viz is a side pane (≈26 cols) alongside the phrase editor, hidden when Help or Instrument take over the screen or when the terminal is too narrow (<40 cols of phrase).
- **Stage 11** ✅ — Sprite engine. PNG sprite sheets load via `:sprite load <path> [WxH]` using the `image` crate (PNG-only feature). Each sheet is decoded to indexed 4-color pixels (slot 0 = transparent, 1–3 = opaque); sheets that use more than 4 opaque colors are rejected rather than quantized so the NES-palette discipline is explicit. Relative paths resolve from the current `.vip` file's directory so songs and assets ship together. `:sprite place <sheet> <idx> <x> <y>` pushes a placement onto an ordered list (later placements win pixel conflicts); `:sprite palette <n> <c0> <c1> <c2> <c3>` defines a named palette and `:sprite repalette <sheet> <n>` swaps a sheet's colors at runtime. A new `:viz sprites` renderer draws placements into the same half-block pixel grid as the other viz kinds (2× vertical via `▀`/`▄`/`█`), with transparent pixels leaving the underlying buffer intact so sheets overlap cleanly. Modulation bindings (tie sprite position / palette / frame to voice env, pitch, gate, scene index) land in Stage 12.
- **Stage 12** ✅ — Modulation bindings. A small expression language ties audio
  sources to sprite-placement properties, parsed from `:bind` commands and
  evaluated every UI tick into a derived `EffectivePlacement` list the viz
  renderer consumes. Example:
  ```
  :bind mario.0 scale = tri.env * 0.5 + 1.0
  :bind mario.0 flipx = noi.gate
  :bind mario.0 frame = floor(step / 4) % 2
  :bind background.* y = sin(time * 2) * 4
  ```
  Address form is `<sheet>.<N|*>`: `N` is the Nth placement of that sheet in
  the placements list; `*` matches every placement of the sheet.
  Sources: `pu1.env | pu2.env | tri.env | noi.env`, same with `.pitch`,
  `.gate`, `.vel`; `master.rms`; transport counters `step`, `step_phase`,
  `beat`, `bar`, `scene.index` (current phrase), `phrase`, `tempo`, `time`
  (seconds since boot), `playing`. Operators: `+ - * / %`, unary `-`,
  parens. Functions: `abs sin cos floor min max clamp`.
  Targets: `x`, `y` (additive pixel offsets on top of the base
  placement), `scale` (nearest-neighbor stretch), `flipx`, `flipy`, `frame`
  (overrides the tile index, wrapped mod cell count), `visible` (0 hides).
  Address form: bare `<sheet>` (= `<sheet>.*`), `<sheet>.N`, `<sheet>.*`.

- **Stage 12.1** ✅ — Color-domain + rotation targets. `rotate` (degrees,
  inverse-mapped around the sprite center with nearest-neighbor sampling),
  `hue` (HSV hue shift in degrees), `saturation` / `value` (HSV
  multipliers, 1.0 = identity), and `palette` (integer index into the
  alphabetically-sorted named palettes — naming convention is the ring
  order). Identity color transforms short-circuit the HSV round-trip on
  the fast axis-aligned path; rotated placements use a center-based
  bounding-box loop. Example:
  ```
  :bind bub hue   = pu1.pitch % 360       # cycle hue with pitch
  :bind bub value = pu1.env                # dim with envelope
  :bind bub rotate = time * 30             # spin at 30°/sec
  :bind bub palette = scene.index          # swap palette per scene
  ```
- **Stage 13** ✅ — Event-triggered animations via `.age`. Every UI tick the
  app diffs per-voice gate state and stamps the timestamp of each rising
  edge; bindings get a `<ch>.age` source = seconds since that channel's
  last note-on. Composed with the Stage 12 expression language, this
  gives state-machine-style one-shot animations without a separate DSL:
  ```
  :bind mario.0 frame = clamp(floor(noi.age * 16), 0, 3) + 4
  :bind mario.0 visible = 1 - floor(clamp(pu1.age, 0, 1))
  ```
  The first plays a 4-frame animation (frames 4–7) over the first 250ms
  after NOI hits, then holds on frame 7. The second hides the sprite
  once PU1 has been idle for ≥1 second. `<ch>.age` is huge on startup so
  thresholds-from-scratch behave correctly. A real `:anim` / `:trigger`
  DSL (named states, sequenced transitions) is deferred — the expression
  form covers the musically-triggered case cleanly.
### Export & polish

- **Stage 15a** ✅ — Offline WAV bounce. `:bounce <path> [loops]` renders
  the current phrase to 16-bit mono 44.1kHz PCM WAV without touching any
  audio driver — a `bounce_to_wav` fn reuses the live `Voice`/`EnvPhase`
  synth and the same `spb` step scheduler, then keeps rendering after the
  last step until every voice is Idle (capped at 2s) so release tails
  finish cleanly. Hand-rolled WAV writer (RIFF + fmt + data chunks); no
  hound dep. Resolves `~` and anchors relative paths to the current `.vip`
  file's directory, same as `:sprite load`.
- **Stage 15b** ✅ — MIDI export. `:midi <path> [loops]` writes a format-1
  Standard MIDI File with a conductor track (tempo) plus one track per
  channel. PU1/PU2/TRI → MIDI channels 0/1/2; NOI → channel 10 (GM drums)
  with a small pitch→slot remap (low=kick, mid=snare, high=hat) so the
  demo's 36/50/60 land on GM 36/38/42 and sound right in any DAW. Hand-
  rolled SMF writer, no midly dep — VLQ, MThd/MTrk chunks, note-offs
  ordered before note-ons at the same tick to survive same-tick retriggers.
- Possible later: `:render out.mp4` recording the viz synced to the bounce.
- **Stage 16** ✅ — Song mode (lite). Global order list: `@song order=[..] loop=NN`, `:order`, `:song on|off`; the grid follows the playing phrase; `:bounce` / `:midi` render the order. Chains, groove and polymeter landed as Stage 23.
- **Stage 17 — Plugin voices.** Load external SID/VRC6/FDS emulator cores as additional voice types for that extended-chip flavor.

### NSF pipeline

Design doc: [`NSF.md`](NSF.md). These stages turn viper into the front
end and compiler for real 2A03 output: songs compile to NSF, play back
through an emulated APU, and render to per-channel stems. The first
consumer is [nintendo-metal](https://github.com/sadovsky/nintendo-metal),
which owns the 6502 driver and the genre style; nothing album-specific
lives in viper.

- **Stage 18** ✅ — **IR + NSF emitter (`viper-nsf`).** Lower `Song` to a
  channel-indexed, frame-timestamped event IR (`note off vol duty retrig
  slide vibrato arp env_reset dpcm loop jump`), serialize it to a
  driver's bytecode, lay out period tables and `$C000`-aligned DPCM
  samples, write the 128-byte NSF header and PRG banks. viper links
  against an external `driver.bin` + `driver.sym` and pins a driver ABI
  version. `@driver` directive and retrig/duty/env_reset effect columns
  in `FORMAT.md`. Channel table carries an `expansion` flag so VRC6 rows
  are just more channels. Exit: `projects/stress_melodeath.vip` compiles
  to an NSF that plays in Mesen.
- **Stage 19** ✅ — **APU-backed playback (`viper-apu`).** Pure-Rust 2A03 core (decided over the Nes_Snd_Emu FFI: no C++ toolchain, deterministic, easy to test).
  `space` compiles on play, runs the driver in a minimal 6502 host, and
  feeds register writes to the APU core; the audio thread pulls PCM from
  it. The atomic beat counter stays the clock and `VizFrame` reads from
  APU-side state so the visualizer is untouched. Exit: the Stage 18 NSF
  plays in the TUI with the playhead locked to it.
- **Stage 20** ✅ — **Register-write log.** Every render dumps `(frame, addr,
  value)` as text (`frame addr value` per line, hex). Golden-log test for the stress song.
  Exit: log diffs clean against NSFPlay headless.
- **Stage 21** ✅ — **Stems + triggers.** `viper render song.nsf --stems out/
  --triggers out/drums.mid`: one deterministic WAV per channel (DPCM
  split per sample ID), plus a MIDI file with one note per drum hit for
  DAW sampler layering. Bit-identical output for the same NSF.
- **Stage 22** ✅ — **Style interface + `viper gen`.** Plug-in boundary in the
  generation layer: a style directory supplies scales, riff templates,
  harmonization rules, drum vocabulary, form grammar, and album-level
  constraints. viper ships the interface and a neutral style; genre
  styles live downstream. `viper gen --style <dir> --seed N` is
  deterministic and records seed + style version in `@meta`.

### Song structure

- **Stage 23** ✅ — **Full song mode: chains, arrangement, groove,
  polymeter.** `Chain { phrases, name }` and `Song::arrangement` (a list
  of chain indices with a loop slot) sit on top of the Stage 16 order
  list: `Song::flat_order()` expands arrangement → chains → phrases, and
  the transport, `:bounce`, `:midi`, and the NSF compiler all consume that
  flat order, so nothing downstream learned a new shape. `.vip` gains
  `@chain NN [name=".."]`, `@arrangement [loop=NN]`, `@length`, and
  `@groove` (see `FORMAT.md`); a file with an arrangement never writes
  `order=`. Commands: `:song` (pane with the live slot highlighted),
  `:chain`, `:arr`, `:len`, `:groove`. Polymeter (`channel_length`) wraps a
  channel inside each phrase and is unrolled by the compiler, so it plays
  on hardware; groove shifts the synth step clock per 16th and is
  synth-only (the compiler warns). Modulation sources `arr` and
  `chain.pos` expose the live position. MIDI export ignores both.
  Salvaged from an unmerged Stage 16 prototype, along with five pastiche
  songs in `projects/` (espresso, ff_prelude, mario, metroid, zelda).

### Verification

- **Stage 24** ✅ — **Verification receipts.** `tests/golden/` holds
  viper-apu's register-write log for the stress song (a render must
  reproduce it byte for byte) and an FCEUX 2.6.6 dump of the same NSF
  captured with `tools/fceux_apu_log.lua`; all 196 PLAY frames match.
  `viper verify song.nsf --against other.log` (`viper_apu::verify`) reads
  any `frame addr value`-shaped dump, lines the frame numbering up
  (including FCEUX's merged INIT + PLAY 1 frame), compares INIT writes as
  a set and PLAY frames exactly, and exits 1 at the first divergence. See
  `NSF.md` § Verification.

### Interface

- **Stage 26** ✅ — **Breath + the playhead as a character.** The first
  two items from [`DESIGN.md`](DESIGN.md). `Breath` is one tempo-locked
  oscillator that every animated element reads, so the interface breathes
  together rather than each widget keeping its own timer: phase comes from
  the audio thread's step counter plus its sub-step phase (free-running at
  the song's tempo off the UI tick while stopped), `wave` swells and
  `pulse` strikes-and-decays, and the named accessors are `pane` (bar
  downbeat), `mode` (half bar), `cursor` (half beat) and `rec` (beat).
  Consumers: the pane border brightens on the downbeat, the mode chip
  pulses on beats 1 and 3, `● REC` breathes instead of blinking, and the
  cursor breathes at half-beat rate. `:set still=on` collapses every
  accessor to zero for anyone who wants the screen to hold still; signal-
  driven feedback (channel LEDs, the playhead) stays live.

  The playhead is now a character: a `◆` travels down a six-column gutter
  leaving a `◇` `·` trail that fades into the theme's ground, its row
  strikes bright at the top of each step and settles across it (off the
  same sub-step phase, so the strike lands with the note), and a row that
  actually gates something flashes brighter still. Channel-header LEDs ride
  the envelope instead of switching, so release tails decay. Colors move
  through one `mix` primitive that blends RGB numerically but switches
  named ANSI colors at the halfway point, so a user's terminal palette
  still decides what "yellow" is.

- **Stage 27** ✅ — **Ghost preview and diff mode.** DESIGN.md's "because
  it's text, because of course" pair, both built on one per-cell `Overlay`
  the phrase grid composites over the authored cells.

  Typing `:gen euclid pu1 5 16` or `:transpose +5` shows what the command
  would do, dimmed and italic, before it commits; Esc discards it. The
  guarantee that the ghost is exactly what commits comes from sharing the
  transformation, not from mirroring it: preview runs `gen::dispatch` on a
  throwaway clone of the song at the *current* `gen_seed`, and the commit
  runs it at the same value — `App::take_gen_seed` is now the only place
  that seed advances. `:transpose` likewise funnels through a new
  `transpose_phrase_cells`, so the commit still records into macros while
  the preview does not. Only cheap in-memory generators preview;
  `:gen style` reads a directory off disk and `lsystem` expands
  exponentially, so both are left out.

  `:diff [phrase] <A> [B]` compares two phrases in the grid, `git`-style:
  `+` added, `-` removed, `~` changed, in the pad the note field already
  had, so the diff costs zero columns. A change marks only the fields that
  differ, so a volume-only edit tints the volume column rather than
  painting the cell. DESIGN.md asked for two phrases stacked vertically;
  that needs about 40 rows and the grid is already 83 columns wide, so a
  unified diff was built instead — it fits an 80x24 terminal, adds no
  layout, and is closer to what `git diff` is. A standing diff is dropped
  by any edit (one line in `snapshot`, which every mutation goes through),
  by undo/redo, by leaving the phrase, and by Esc.

  Signal is carried on backgrounds and the margin sigil rather than
  foreground colour: every background in both themes is `Color::Rgb` and
  blends, while most `nes` foregrounds are named ANSI colours that `mix`
  deliberately snaps rather than resolving — a derived ghost would have
  been invisible there.

  Also fixed here: the channel headers emitted 16 columns against a
  15-column data column, so every label sat one column right of its own
  data, four by the DPCM column. The Stage 26 alignment test only checked
  PU1, which is why it shipped.
- **Stage 28** ✅ — **The panels come alive.** The instrument editor was a
  flat parameter list; it now draws its ADSR envelope and the waveform that
  envelope shapes, in half-blocks, beside the list. The segment belonging to
  the selected parameter is drawn in the accent colour, so `h`/`l` on
  `attack` visibly moves that ramp — DESIGN.md's "the envelope lights up in
  sequence" as a note plays was *not* built, because an instrument is not
  bound to a channel anywhere in the model and there is no honest way to
  know which voice is sounding it. Below ~54 columns the graphs step aside
  and the list takes the pane.

  `viz::HalfBlock` is the pixel canvas behind it: five viz renderers had
  each hand-rolled the same "build a bitmap, collapse row pairs into
  `▀`/`▄`/`█`" loop, so the sixth copy became a shared type instead. The
  existing five can migrate onto it as they are touched.

  The queued-scene badge grew DESIGN.md's **queue-drain**: an eight-cell bar
  that empties toward the launch, counting the sub-step phase as well as
  whole steps so it drains smoothly rather than in four jerks.

  `:viz register` is the **register panel**: one glyph per cell, note letter
  or a faint dot, so you read the *shape* of what you are about to paste.
  DESIGN.md describes "a grid of yanked phrases", which presumes named
  registers; viper has exactly one unnamed register, so that is what it
  shows.

- **Stage 29** ✅ — **Atmosphere.** Four small pieces, all reading state
  that already existed. Entering INSERT dims the channels you are not
  typing into, so attention narrows to the column under the cursor. LIVE
  outlines the phrase pane in red, putting the mode at the edge of vision
  rather than only in the modeline. The border also flashes on a NOI hit —
  DESIGN.md's "the viz bleeds into the UI"; its other half, the row tinting
  toward red when a note is dissonant against the root, needs a notion of
  consonance the app does not have and is not built. `phosphor` gains CRT
  scanlines as a faint background on alternate rows, at the very bottom of
  the background precedence chain so it never competes with the cursor,
  the playhead or the column tint; `nes` declares none.

  Row 2 of the modeline, previously blank unless you were typing a command
  or holding a count, now carries a rolling event log. It is derived by
  watching `status` once per UI tick rather than instrumenting every
  action: everything notable already announces itself there, so one watcher
  catches all of it, and the newest entry is skipped because it is already
  the status line above.

- **Stage 30** ✅ — **Stacked phrases and context-aware help.** The grid is
  now a window onto a virtual *tape* of 48 rows indexed from the active
  phrase's step 0: −16..−1 is the previous phrase, 0..15 the active one,
  16..31 the next. Every behaviour — graceful degradation, centring, scroll
  style, song mode — is a choice of where that window starts, decided by one
  pure function with a priority ladder: the cursor is always on screen; the
  playing row joins it whenever both fit; then the requested anchor; and
  never scroll past the neighbours into blank space, except on a pane taller
  than the whole tape, where the tape is centred instead.

  Spare rows become context, split evenly with the odd row below. At 24
  terminal rows that is about two rows each side; at 34, seven; at 54, the
  full three-phrase view DESIGN.md described. Below 16 rows of content the
  window narrows to the active phrase alone and scrolls to follow the
  cursor, so the active phrase is never silently truncated. `phrase_view`
  returns plain data with no `Frame` or `Theme`, so all of this is unit
  tested rather than screenshotted.

  Neighbour rows get three signals that do not depend on a brightness ramp,
  because under `nes` most foregrounds are named ANSI colours and there is
  no computable "30% of `Color::Green`": a single declared `Theme::ghost`
  ink for every field, blanks instead of `--- -- -- ---` so you read a
  sparse silhouette, and the *phrase* index in the gutter instead of the
  step index, which makes the boundary self-evident without spending a
  separator row. In song mode neighbours come from the flattened order —
  what actually plays next, including the wrap to the loop point — and an
  intro before the loop point correctly has no predecessor.

  `:set scroll=` picks the style. A terminal cell is the smallest unit of
  vertical position, so "smooth" cannot mean sub-row motion; what it means
  here is *what is pinned*. `jumpy` (the default) anchors the grid so step
  07 is always on the same screen row, which is the LSDJ feel and today's
  behaviour. `smooth` pins the playing row and flows the tape past it, so a
  phrase boundary is a non-event. It only differs while playing, and
  `:set still=on` forces `jumpy`.

  **Context-aware help** landed in two halves. The modeline now says what a
  half-finished command will accept next — after `d`, the motions and
  objects; after `da`, the three object letters — which is vim's which-key
  where your eyes already are. And the help screen scrolls (`j`/`k`, `g`/`G`,
  PageUp/PageDown) with its position in the title, fixing a real defect:
  it had no scrolling and no wrapping, so on a 24-row terminal most of the
  reference was unreachable with no sign anything had been cut.

### Format

- **Stage 31** ✅ — **`.vip` round-trip losslessness.** `@scene`, `@sprite`
  and `@bind` were parsed only to warn and discard, and were never written, so
  saving a song silently destroyed every scene slot, sprite sheet, placement,
  palette and modulation binding it had. The whole Stage 11–13 visualizer
  could not be persisted at all, and FORMAT.md's own canonical example warned
  twice on load — against a doc whose first principle calls round-tripping
  "non-negotiable".

  `Song` now carries those lines verbatim in `extras`, so `viper fmt` and
  `viper check` are lossless without knowing their grammar and `Song` stays
  cheap to deep-clone on every undo snapshot. The TUI regenerates them from
  live state on save and replays them through `execute_command` on load, so
  the file grammar and the `:` grammar are the same thing by construction.

  Two fields were missing for a faithful round trip: `SpriteSheet` did not
  record `quantize` (so a quantized sheet failed to reload, its source PNG
  still having too many colours) or which named palette `:sprite repalette`
  had applied. Both are stored now. Palette colours are written without a
  leading `#`, which would otherwise be eaten as a comment.

  Two bugs surfaced while testing. `:sprite load` drops a convenience
  placement at (0,0), so placements bred on every save/load cycle until the
  replay learned to discard them before applying the file's own. And
  `edit_file` never cleared this state, so opening a second song inherited the
  first one's visualizer and scene slots.

### Generators

- **Stage 25** ✅ — **Pattern generators from `GENERATION.md`.** `:gen
  chord_prog`, `bassline`, `arp`, `drums`, `lsystem`, `cellular`, sharing
  one chord-symbol parser (roman numerals relative to `key=`, or absolute
  names) and the bundled progression and drum presets. Deterministic per
  seed; each writes the current phrase (progressions flow into following
  phrases). Deferred: `markov` (needs a corpus loader) and the LLM
  backend. Details in `GENERATION.md` § Staging.
