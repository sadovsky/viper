# UI Design

The viper interface. A CRT hacker's dream workstation running a fantasy
console that's also a tracker that's also vim. Not retro as aesthetic —
retro as honest expression of what the program is doing.

## Design thesis

Every pixel explicable by the underlying model. No chrome. No fake
gradients. No drop shadows unless they represent a real layer. The
interface is a direct view into song state; ornament is banned.

**Reference points**, in order of influence:

- **LSDJ** — the pillar. Tiny resolution, ruthless information density,
  every glyph meaningful.
- **Renoise** — for how the playhead feels. The moving highlight bar
  across the pattern is almost hypnotic.
- **vim itself** — the modeline is sacred. `:` commands feel like
  casting spells.
- **PICO-8** — fantasy console self-awareness. One window, one font,
  one attitude.
- **Grafx2 / Aseprite** — how pros lay out modal tools.
- **Bitwig's modulation visualization** — for when modulation bindings
  need to visibly flow through the interface.

## Layout

The canvas is a 120×40 terminal in full-screen mode. Everything is
monospace. No scrollbars ever. Keyboard only.

### Top bar (1 row)

Left: `viper · nocturne.vip [+]` — program name, filename, unsaved-star
indicator. Tabs are first-class but collapsed when only one song is open.

Right: `●REC 140bpm 4/4 ch2 i00 v0F phrase 04` — transport state, time
signature, current channel, current instrument, current volume, phrase
index. When recording, the `●REC` glyph pulses on the beat with a
brightness shift rather than a hard blink.

### Phrase editor (left, ~60% width)

The star of the interface. Shows **three phrases stacked vertically**,
with the active one centered and brightest. Phrase above and below are
dimmed to roughly 30% brightness. Peripheral context the way vim's
relative line numbers provide it.

As the playhead crosses a phrase boundary, phrases scroll. Two scroll
styles, configurable: `smooth` (Renoise-like) or `jumpy` (LSDJ-like,
better for sync cues).

The active step row has a **full-width horizontal highlight bar** that
sweeps downward at the pattern rate when playing. Not a pulse — a
sweep. When transport stops, it freezes on the current step.

Each cell is 11 characters: `C#4 01 0F A04` — note, instrument, volume,
effect. Empty cells are `--- -- -- ---` in the darkest possible grey.
Active cells color-code by field: note by channel color, instrument in
yellow, volume in cyan, effect in magenta. The channel under the cursor
has a subtle tinted background column — "you are here" without shouting.

### Side panels (right, ~40% width, stacked)

**Instrument editor** (top third). Live ADSR curve drawn in half-blocks.
Waveform preview updating on every parameter change. When a note plays,
the envelope lights up in sequence — attack glows, decay fades, sustain
holds, release dims. The pulse width displays as a literal square wave
morphing in real time. Noise instruments show a fluttering dithered
pattern that actually scans through noise seeds.

**Song / chain view** (middle third). Tiny matrix; columns are
channels, rows are chain positions. Current position marked with a `►`
glyph. Scenes show as colored letter tags (`A`, `B`, `C`...) in the
margin. Launching a scene shows a countdown bar — "queued, 3 beats
until launch" — that visibly drains.

**Visualizer / registers** (bottom third), toggleable. When viz is on,
this is where sprites live. When registers are showing, it's a grid of
yanked phrases with preview thumbnails — mini ASCII renderings of the
phrase shapes.

### Modeline (2 rows, bottom)

Row 1: mode indicator (colored block), pending keystrokes (e.g. `4d`),
status message, right-aligned clock and CPU indicator.

Row 2: the command line when `:` is active; otherwise a rolling event
log ("▸ scene B queued · step 0c · ch2 muted").

## The stuff that makes it feel alive

### The playhead is a character, not a cursor

A glyph (`◆`) travels down the step column when playing. It leaves a
2-step trail that fades. When it hits a step with notes, it flashes
brighter. When a channel triggers, the channel's header letter lights
up for ~200ms. This is how LSDJ composers visually parse what's
happening in real time.

### Breath and pulse, on-beat

Every static element has a tiny animation tied to the master tempo
clock. The border of the active pane brightens 5% on the downbeat. The
mode-indicator character pulses on beats 1 and 3. The cursor pulses at
half-beat rate. Everything breathes together.

This is the single detail that turns a TUI from "impressive" to "feels
like an instrument."

### The viz bleeds into the UI

In stage 12+, sprite bindings aren't confined to the viz panel. The
entire border of the phrase editor can flash on a snare hit. The row
highlight tints toward red when the last note was dissonant versus the
root. The instrument panel's waveform background goes chromatic-
aberrationy when volume is driven past `0F`.

### Mode transitions are real transitions

**Entering INSERT**: phrase editor subtly shifts 1 column to center on
the cursor, other channels dim slightly, a `▸` ready-glyph appears.

**Entering LIVE**: whole screen darkens 10%, red recording indicator
fades in from the top-right, phrase editor gets a thin red outer
border. Returning to NORMAL feels like the lights coming back on.

### Command palette with preview

Typing `:gen euclid pu1 5 16` ghost-previews the generated notes in
the target phrase — dim "proposed" style that won't commit until Enter.
Miss-typing doesn't destroy anything. Same for `:transpose +5`: ghost
the result, then commit.

### Diff mode

`:diff phrase 00 03` splits the phrase editor into two stacked phrases
with differences highlighted like `git diff`. Added notes in green,
removed in red, changed in yellow. Because it's text, because of
course.

### Inline help

Pressing `?` drops a translucent overlay showing the keys valid *in
the current mode and context*. Operator-pending after `d`? Only motion
keys light up, everything else dims. vim's `which-key` plugin, native.

## Aesthetic

Two themes ship. One canonical, one alt.

### `theme=nes` (default)

The NES master palette, curated. Dark bluish-purple background, deep
and cool. Green pulse 1, amber pulse 2, blue-violet triangle, pink
noise. Dominant colors, sharp accents, no gradients except where they
represent real value (envelope fills, playhead glow).

The palette isn't cosplay. It's the same design-constraint discipline
the project is built around — a bounded vocabulary that forces every
color choice to mean something. If PU1 is always green, the user
reads the screen with peripheral vision after a week.

### `theme=phosphor` (alt)

Amber on black. Dead monochrome. Faint scanlines via Unicode
overlays. Characters have a subtle bloom via adjacent-cell dimmer
duplicates. No color information at all — channel differentiation
comes from position and glyph.

Thematically perfect for demo videos, live streams, and moments where
"hacker mystical" outweighs "composer practical." Muscle memory
transfers cleanly from the NES theme because the layout is identical.

### Why these two

Any third theme is scope creep. Neutral-dark "studio monitor" themes
are available in every DAW; the whole point of viper is to pick a
voice and commit. NES palette for daily use, phosphor for the vibe.
Users who want custom colors can edit the TOML — but the defaults
stand for something.

## Typography

Body / grid: **JetBrains Mono**. Distinctive but legible, tabular
numerals, good at small sizes, matches the "modern terminal tool"
register.

Display / headers: **VT323**. The CRT terminal font. Used sparingly —
program title, pane titles, splash screen. Signals the project's
relationship to its ancestors without drowning the functional text.

No third font. No body sans-serif. One family for the working
surface, one for the frame.

## Micro-interactions worth naming

- **Pane-breath**: active pane border brightens ~8% on the downbeat,
  4-beat cycle.
- **Mode-glow**: the mode chip has a subtle `box-shadow` that pulses
  in sync with the tempo.
- **Rec-pulse**: `●REC` opacity cycles 0.45 → 1.0 → 0.45 once per
  beat. Not a hard blink; a breath.
- **Cursor-blink**: standard terminal cursor, half-beat cycle, tempo-
  locked.
- **Playhead-sweep**: the amber row bar steps through 16 positions
  over 4 beats. `steps(16, end)` timing function — not smooth, not
  jittery; *locked*.
- **Channel-LED flash**: per-channel dot in the column header flashes
  on trigger. NOI channel flashes every step during a blast-beat
  pattern and becomes ambient rhythm on its own.
- **Queue-drain**: scene-launch countdown bar drains linearly. User
  sees the commit point coming.
- **Wave-shift**: instrument panel waveform scrolls at the instrument's
  pitch, giving a live "this is the sound" signal even when silent.
- **Sprite-bop**: the demo pixel fox in the viz panel bounces at half-
  beat. In production, this is driven by an actual modulation binding.

## The unifying image

A 1987 composer at a workstation. Amber CRT light. A tracker grid
scrolling in tight syncopation. Every keypress produces both a sound
and a tiny visual ripple across the screen. The playhead glides. A
sprite of a fox, palette-swapped by the bass channel's pitch, pulses
in the corner to the snare. `:gen euclid noi 7 16` ghosts a shimmer
of proposed notes and the composer presses Enter and the groove locks
in.

It's vim. It's a tracker. It's alive.

## Implementation notes for the Rust side

- **ratatui styling**: everything theme-able through a single
  `Theme` struct read from TOML. Swap via `:set theme=phosphor`.
- **Tempo-locked animations**: single atomic beat counter shared
  between audio and UI threads. UI interpolates visual phase from
  audio position, never from wall clock.
- **Half-block Unicode for curves**: ADSR and waveform previews
  render via `▀` characters with fg/bg color pairs, doubling
  vertical resolution.
- **Breathing is a first-class thing**: define a `Breath` trait or
  helper that emits a 0.0–1.0 value per frame based on beat position
  and cycle length. Every animated element reads from it. Consistent,
  cheap, trivially switched off for accessibility (`:set still`).
- **Sprite system reuses the theme palette** by default. `:sprites
  load fox.png 12x12` with no palette argument uses NES palette
  colors 0-3 mapped to the sheet's indexed colors. Custom palettes
  override.
- **Scanlines in phosphor mode**: rendered as alternating-line
  background color tweak, not a texture overlay. The CRT-ness
  should be *of* the terminal, not painted on top.

## What the mockup demonstrates

See `mockup/ui.html` for a working static render showing:

- Three-phrase stacked layout (03 dimmed, 04 active, 05 dimmed)
- Playhead sweeping through 16 steps at 140 BPM
- Cursor highlight on step 07 of PU2
- Instrument editor with live ADSR curve and pulse-25% waveform
- Song/chain view with scene B queued, draining countdown bar
- Viz panel with 4-channel envelope bars and pixel-fox sprite
- LIVE mode active, red modeline, pending `4d` count visible
- Command line ghost-previewing `:gen euclid noi 5 16`
- Mode gallery showing all five modes with their chips
- Theme comparison: NES palette next to phosphor amber

Open it in a browser and everything breathes. The mockup is the
design spec — if the real TUI doesn't feel as alive as the HTML,
something's wrong.

