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
- `:transpose ±N` / `:tr ±N` — shift all pitched notes by N semitones (skips NOI)
- `:viz` / `:viz <kind>` — toggle visualizer pane (kinds: `bars`, `scope`, `grid`, `orbit`, `sprites`); `:viz off` hides it
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
- **Stage 14 — Optional native window backend** (feature flag `window`). Same viz, rendered to a real GPU-accelerated window via `pixels` or `macroquad`. Same bindings, more pixels, optional shaders.

### Export & polish

- **Stage 15 — Export.** `:bounce out.wav` renders to WAV. `:midi out.mid` exports MIDI. `:render out.mp4` records the viz synced to the bounce.
- **Stage 16 — Song mode.** Phrases → chains → song, groove/swing, per-channel track length (polymeter).
- **Stage 17 — Plugin voices.** Load external SID/VRC6/FDS emulator cores as additional voice types for that extended-chip flavor.
