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
- `:play` / `:stop`
- `:rec` / `:rec off` — toggle record-arm on cursor channel / disarm all
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
- **Stage 7 — Scene launching.** A `scene` is a saved chain state. `:scene 01 save`, then bind scenes to number keys in live mode. Tap `1`…`9` to queue the next scene to launch on the next bar boundary — Ableton Session-view in a TUI.
- **Stage 8 — Performance macros.** Reuse the `q`/`@` machinery but for live: record a sequence of transport commands (mute ch2, launch scene 3, transpose +5, unmute ch2) and fire the whole thing with one key.

### Visualizer

- **Stage 9 — VizFrame bus.** Audio thread writes per-voice state (gate, env level, pitch, current step, beat phase) into a lock-free SPSC queue at ~60Hz. UI thread drains for rendering. This is the foundation for everything visual.
- **Stage 10 — Built-in viz (ASCII/Unicode).** `:viz` toggles a visualizer pane. Uses Unicode half-blocks (`▀`) for 2x vertical resolution with 24-bit color. Default visualizations:
  - **bars** — per-voice envelope levels as vertical bars
  - **scope** — synthesized waveform trace
  - **grid** — 16-step playhead pulsing in sync
  - **orbit** — pitch as radius, envelope as brightness, one body per voice
- **Stage 11 — Sprite engine.** Load PNG sprite sheets via `:sprites load mario.png 16x16`. Sheets are grids of cells addressed by index. Viz pane can render any sprite at any position. Core primitives:
  - `sprite place <sheet> <idx> <x> <y>` — static placement
  - `sprite palette <n> <4 hex colors>` — define NES-style 4-color palettes
  - `sprite repalette <sheet> <src_palette> <dst_palette>` — recolor at runtime
- **Stage 12 — Modulation bindings.** The fun part. A small declarative binding language:
  ```
  bind sprite mario.0 scale = tri.env * 0.5 + 1.0
  bind sprite mario.0 hue   = pu1.pitch % 360
  bind sprite mario.0 flipx = noi.gate
  bind sprite mario.0 palette = scene.index
  bind sprite background.* shake = master.rms
  ```
  Sources: per-voice `env`, `pitch`, `gate`, `rms`; global `beat`, `bar`, `scene.index`, `tempo`. Targets: `x`, `y`, `scale`, `rotate`, `hue`, `saturation`, `value`, `flipx`, `flipy`, `palette`, `frame` (for spritesheet animation).
- **Stage 13 — Animation states.** Multi-frame sprite animations with state machines: `idle`, `hit`, `transition`. State transitions triggered by musical events (e.g. "play `hit` anim for 4 steps when NOI channel gates").
- **Stage 14 — Optional native window backend** (feature flag `window`). Same viz, rendered to a real GPU-accelerated window via `pixels` or `macroquad`. Same bindings, more pixels, optional shaders.

### Export & polish

- **Stage 15 — Export.** `:bounce out.wav` renders to WAV. `:midi out.mid` exports MIDI. `:render out.mp4` records the viz synced to the bounce.
- **Stage 16 — Song mode.** Phrases → chains → song, groove/swing, per-channel track length (polymeter).
- **Stage 17 — Plugin voices.** Load external SID/VRC6/FDS emulator cores as additional voice types for that extended-chip flavor.
