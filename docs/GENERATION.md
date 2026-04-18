# Generation

Programmatic song generation in viper. Two layers: algorithmic generators
that run in-process (deterministic, fast, testable) and an LLM backend for
natural-language prompting (nondeterministic, slow, vibes).

## Why this exists

1. **Test fixtures for the audio engine.** A known-good generated song is
   the ground truth for sample-accuracy regressions.
2. **Default content.** First-run experience is `cargo run` → `space` →
   something listenable, not an empty grid.
3. **Composition assist.** A seed to edit from is often more useful than
   a blank canvas, especially for drum patterns.
4. **Demo material.** Procedurally generated viz-reactive songs are
   shareable social content.
5. **The vim philosophy applied to music.** Text in, text out, composable
   with Unix tools. `viper --gen euclid pu1 5 16 | viper`.

## Architecture

Generators are pure functions: `fn(&Params) -> Song`. No I/O, no
randomness except from a seed passed in `Params`. This gives you:

- Deterministic tests (seed = 42 always produces the same song).
- Trivial CLI integration (`viper --gen <algo> <args...>` → stdout).
- Trivial TUI integration (`:gen <algo> <args...>` → replaces current
  phrase or appends a new one).
- Composability (pipe generators through each other via `.vip` text).

```rust
pub trait Generator {
    fn generate(&self, params: &Params, rng: &mut impl Rng) -> Song;
}
```

Every generator emits a complete `Song`. The caller decides whether to
merge it into the current song, replace the current phrase, or write to
disk. Generation never mutates global state.

## Algorithmic generators

### `four_on_floor`

Kick on every 4th step, snare on 2 and 4 of each bar, hat on offbeats.
Tunable via `channels` param (which voice gets what). Baseline sanity check.

```
:gen four
:gen four bpm=128
```

### `euclid`

Bjorklund's algorithm. `k` hits distributed as evenly as possible across
`n` steps. The mathematical basis for most world-music grooves and an
absurdly high payoff-per-line-of-code.

```
:gen euclid pu1 5 16           # 5-in-16 on pulse 1
:gen euclid noi 7 16 offset=2  # 7-in-16 on noise, rotated
```

| (k, n)  | feels like                            |
|---------|---------------------------------------|
| (3, 8)  | Cuban tresillo                        |
| (5, 8)  | Cuban cinquillo                       |
| (3, 7)  | Bulgarian ruchenitza                  |
| (5, 16) | Bossa nova clave                      |
| (7, 12) | West African bell pattern             |
| (7, 16) | Samba                                 |

Bundled preset table in `gen::euclid::presets` for `:gen euclid preset bossa`.

### `random_in_scale`

Uniform random notes in a given mode. Not musical on its own but the
substrate for better generators.

```
:gen scale key=A mode=minor density=0.4
```

Params: `key`, `mode` (major, minor, dorian, phrygian, lydian, mixolydian,
locrian, harmonic_minor, pentatonic_major, pentatonic_minor, blues),
`density` (0.0–1.0, probability of a note per step), `octave_range`.

### `markov`

Order-2 Markov chain over pitch intervals. Seed with a corpus of existing
`.vip` phrases; emits phrases in the same statistical style.

```
:gen markov corpus=classics/*.vip order=2 length=16
```

The useful knob: `temperature`. Low = faithful, boring. High = surprising,
often broken. Ship with `temperature=0.7` as default.

### `chord_prog`

Roman-numeral chord progression → voiced arrangement. Pu1 = top voice,
Pu2 = middle, Tri = bass, Noi = rhythm.

```
:gen chord_prog "i iv V i" key=Am bpm=90
:gen chord_prog "I vi IV V" key=C bpm=120  # 50s doo-wop
```

Bundled progressions: `12bar`, `doowop`, `canon`, `andalusian`,
`royal_road`, `four_chords`.

### `bassline`

Voice-leads a bassline under a chord progression. Follows standard rules
(root on beat 1, walk or arpeggio to next root, chromatic approach
allowed). Output goes to Tri channel.

```
:gen bassline "Am Dm E Am" style=walking
:gen bassline "Am Dm E Am" style=arpeggio
:gen bassline "Am Dm E Am" style=root_fifth
```

### `arp`

Arpeggiator. Standard tracker macro but as pattern data. Input: chord
symbol + pattern.

```
:gen arp Am up 16
:gen arp Cmaj7 updown 16 rate=2
:gen arp F#dim7 random 32
```

### `drums`

Style-library drum patterns. Hardcoded presets, tunable.

```
:gen drums breakbeat
:gen drums trap
:gen drums amen
:gen drums gameboy fills=2
```

### `lsystem`

Lindenmayer system. Axiom + production rules → step pattern. Produces
fractal, self-similar rhythms that sound weird and cool.

```
:gen lsystem axiom=A rules="A=ABA,B=.A." iterations=4 map="A=C4,B=G3,.=-"
```

Good demo material. Medium effort, high novelty payoff.

### `cellular`

1D cellular automaton (Rule 30, Rule 110, etc) → rhythmic pattern. Wolfram
meets Squarepusher.

```
:gen cellular rule=30 width=16 rows=16
:gen cellular rule=110 width=16 rows=16 seed=center
```

## Composition

Generators can be chained through `.vip` text. Each generator reads a
`Song` on stdin (or empty) and writes one on stdout.

```bash
viper --gen chord_prog "i iv V i" key=Am \
  | viper --gen bassline style=walking \
  | viper --gen drums breakbeat \
  | viper --gen arp Am up 16 channel=pu2 \
  > song.vip
```

In-TUI: `:gen` always operates on the current song state. Repeated calls
layer voices.

```
:gen chord_prog "i iv V i" key=Am
:gen bassline style=walking
:gen drums breakbeat
```

## LLM generation (stage 13.5)

Wrap the Anthropic API. System prompt contains the format spec from
`FORMAT.md` plus 3 canonical example songs. User prompt is the natural-
language description.

### Command surface

```
:gen "dark arpeggiated bassline in F# minor, 90 bpm"
:gen "driving chiptune chase theme, lots of pulse 1 lead, noise snare"
:gen "make phrase 03 feel more urgent"
```

The last form is contextual — viper includes the current song as context
so the LLM can extend or edit coherently.

### Prompt structure

```
System:
  You are a chiptune composition assistant. You emit only valid .vip
  format content as specified below.

  <format spec from FORMAT.md>

  <3 example .vip songs with varied styles>

  Rules:
  - Always emit a complete @song directive.
  - Keep phrases exactly the user-requested length.
  - Only use voices PU1, PU2, TRI, NOI.
  - Comments are encouraged; explain musical choices.
  - If editing an existing song (provided below), preserve @phrase indices
    the user didn't ask to change.

User:
  Current song:
  <optional current .vip content>

  Request: <natural language>
```

### Validation

Every LLM response goes through the parser before being accepted. On
parse failure:

1. Show the parse error and the offending line.
2. Feed the error back to the model with "fix this" as a follow-up.
3. After 2 retries, give up and show the raw output to the user.

This is the same self-correction pattern that works well for SQL
generation, structured data extraction, etc.

### Config

```
:set llm.model=claude-opus-4-7
:set llm.max_tokens=4096
:set llm.api_key_env=ANTHROPIC_API_KEY
```

Never store API keys in `.vip` files or viper config. Env var only.

### Cost control

- Show token count + estimated cost before sending (`--dry-run` flag).
- Cache identical prompts locally (`~/.cache/viper/gen/<hash>.vip`).
- Default to the cheapest capable model; let users opt into larger ones.

## Testing strategy

```rust
#[test]
fn four_on_floor_is_deterministic() {
    let a = gen::four_on_floor(&Params::default(), &mut seeded_rng(42));
    let b = gen::four_on_floor(&Params::default(), &mut seeded_rng(42));
    assert_eq!(a, b);
}

#[test]
fn euclid_has_correct_hit_count() {
    let s = gen::euclid(5, 16, 0, &mut seeded_rng(0));
    assert_eq!(s.phrases[0].count_hits(Channel::Pu1), 5);
}

#[test]
fn round_trip_through_text() {
    let original = gen::four_on_floor(&Params::default(), &mut seeded_rng(0));
    let text = original.to_vip();
    let parsed = Song::from_vip(&text).unwrap();
    assert_eq!(original, parsed);
}

#[test]
fn audio_output_matches_golden_wav() {
    let song = gen::golden_test_song();
    let audio = render_to_wav(&song, 44100, 4.0);
    assert_audio_matches("tests/golden/four_on_floor.wav", &audio, tolerance=0.001);
}
```

The last test is the payoff. Programmatic songs + deterministic rendering
= real regression tests for audio correctness.

## Staging

Split across the main roadmap:

- **Stage 3.5** — `.vip` read/write, `gen::four_on_floor`,
  `gen::euclid`, `gen::random_in_scale`, `:gen` ex command.
- **Stage 6.5** — `gen::markov`, `gen::chord_prog`, `gen::bassline`,
  `gen::arp`, `gen::drums` (preset library).
- **Stage 13.5** — LLM backend, `:gen "<natural language>"`.
- **Stage 14.5** — `gen::lsystem`, `gen::cellular` (viz-reactive demos).

## Open design questions

1. **Merge vs replace semantics.** When `:gen` runs on a non-empty song,
   does it replace the current phrase, append a new one, or overlay on
   top of existing notes? Current lean: overlay by default, `:gen!` to
   replace. Matches vim's `:w` vs `:w!`.
2. **Seed stability across versions.** If we tweak `gen::markov`'s
   algorithm, do we break `seed=42` reproducibility? Current lean: yes,
   document it, major version bumps only.
3. **User-defined generators.** Should composers be able to write their
   own generators? Probably — a `~/.config/viper/gen/` directory with
   shell scripts or Lua/Rhai would be great but is scope creep for v1.
