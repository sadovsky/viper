//! Stage 22: the style plug-in interface and the form → phrase → note
//! generator that consumes it.
//!
//! A *style* is a directory holding `style.vps`, a line-oriented file in
//! the same `@directive key=value` grammar as `.vip`. It supplies scales,
//! chord progressions, instruments, riff templates (rhythm skeleton +
//! contour + harmonization + bass + drum vocabulary), section recipes, a
//! song-form grammar, and optional title words and a shared motif. viper
//! ships one neutral style in `styles/neutral/`; genre styles live in
//! their own repos.
//!
//! Generation is deterministic per (style, seed, key, bpm). The output is
//! a complete [`Song`] with a phrase per bar (identical bars share a
//! phrase), an order list, and a loop point after the intro.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::gen::{euclid_mask, parse_key, Mode, Rng};
use crate::{Cell, Instrument, Phrase, Song, INSTRUMENTS, STEPS_PER_PHRASE};

// ---------------------------------------------------------------- data

#[derive(Clone, Debug)]
pub struct Riff {
    pub name: String,
    /// 16 chars: `x` = hit, `.` = rest.
    pub rhythm: Vec<bool>,
    pub contour: String,
    pub harmony: String,
    pub bass: String,
    pub lead_instr: u8,
    pub harm_instr: u8,
    pub bass_instr: u8,
    /// Repeat each pitch this many hits (tremolo pairs).
    pub pair: usize,
    /// Effect applied to every lead hit (e.g. `V52`).
    pub fx: Option<(u8, u8)>,
    /// Octave for the lead line.
    pub octave: i32,
    /// Volume accents: (on-beat volume, off-beat volume), from `accent=FA`.
    pub accent: Option<(u8, u8)>,
    /// Probability that a lead hit (not the first of a bar) slides in.
    pub slide: f32,
}

#[derive(Clone, Debug)]
pub struct Drums {
    pub name: String,
    pub noi: Vec<char>,
    pub dpcm: Vec<char>,
}

#[derive(Clone, Debug)]
pub struct Section {
    pub name: String,
    pub bars: Vec<usize>,
    pub riffs: Vec<String>,
    pub drums: Vec<String>,
    pub repeat: usize,
    /// Probability that this section carries the style's motif.
    pub motif: f32,
    pub crash_end: bool,
    /// Volume swell from quiet to full across the section's bars.
    pub swell: bool,
}

#[derive(Clone, Debug)]
pub struct Progression {
    pub name: String,
    pub chords: Vec<ChordSym>,
}

/// A roman-numeral chord: scale degree (0-based) with an optional
/// chromatic offset (`bII` = degree 1 flattened), quality from case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChordSym {
    pub degree: usize,
    pub accidental: i32,
    pub minor: bool,
}

/// A `@dpcm NN name= path= rate= token=` line: a bank slot the generated
/// songs reference, and the `@drums` token that triggers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleSample {
    pub name: String,
    pub path: PathBuf,
    pub rate: u8,
    pub token: Option<char>,
}

#[derive(Clone, Debug, Default)]
pub struct Style {
    pub name: String,
    /// DPCM bank, indexed by slot (note C-4 + slot). Empty = built-in bank.
    pub samples: Vec<StyleSample>,
    pub version: String,
    pub tempo_min: u16,
    pub tempo_max: u16,
    pub keys: Vec<u8>,
    pub scales: Vec<(String, Vec<u8>)>,
    pub progressions: Vec<Progression>,
    pub instruments: Vec<(usize, Instrument)>,
    pub instr_names: HashMap<String, u8>,
    pub riffs: Vec<Riff>,
    pub drums: Vec<Drums>,
    pub sections: Vec<Section>,
    pub forms: Vec<Vec<String>>,
    pub motif: Vec<i32>,
    pub title_adj: Vec<String>,
    pub title_noun: Vec<String>,
    /// Hat / crash notes on NOI and their instruments.
    pub hat: (u8, u8),
    pub crash: (u8, u8),
    pub open_hat: (u8, u8),
}

// ---------------------------------------------------------------- parsing

pub(crate) fn kv(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().to_string();
        rest = &rest[eq + 1..];
        let value;
        if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').unwrap_or(r.len());
            value = r[..end].to_string();
            rest = r.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            value = rest[..end].to_string();
            rest = &rest[end..];
        }
        rest = rest.trim_start();
        out.push((key, value));
    }
    out
}

fn list(v: &str) -> Vec<String> {
    v.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()
}

fn strip_comment(s: &str) -> &str {
    let mut in_token = false;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            in_token = false;
        } else if c == '#' && !in_token {
            return &s[..i];
        } else {
            in_token = true;
        }
    }
    s
}

fn parse_fx(s: &str) -> Option<(u8, u8)> {
    let b = s.as_bytes();
    if b.len() != 3 {
        return None;
    }
    Some((b[0].to_ascii_uppercase(), u8::from_str_radix(&s[1..], 16).ok()?))
}

pub fn parse_chord(tok: &str) -> Option<ChordSym> {
    let (acc, rest) = if let Some(r) = tok.strip_prefix('b') {
        (-1, r)
    } else if let Some(r) = tok.strip_prefix('#') {
        (1, r)
    } else {
        (0, tok)
    };
    let minor = rest.chars().next()?.is_ascii_lowercase();
    let degree = match rest.to_ascii_uppercase().as_str() {
        "I" => 0, "II" => 1, "III" => 2, "IV" => 3, "V" => 4, "VI" => 5, "VII" => 6,
        _ => return None,
    };
    Some(ChordSym { degree, accidental: acc, minor })
}

impl Style {
    pub fn parse(text: &str) -> Result<Self> {
        let mut st = Style { tempo_min: 120, tempo_max: 140, hat: (84, 3), crash: (72, 4), open_hat: (79, 3), ..Default::default() };
        let mut instr_idx = 0usize;
        for (ln, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let Some(rest) = line.strip_prefix('@') else {
                bail!("line {}: expected an @directive", ln + 1);
            };
            let (dir, args) = match rest.find(char::is_whitespace) {
                Some(i) => (&rest[..i], rest[i..].trim()),
                None => (rest, ""),
            };
            let ctx = || format!("line {}: @{}", ln + 1, dir);
            match dir {
                "style" => {
                    for (k, v) in kv(args) {
                        match k.as_str() {
                            "name" => st.name = v,
                            "version" => st.version = v,
                            _ => {}
                        }
                    }
                }
                "tempo" => {
                    for (k, v) in kv(args) {
                        match k.as_str() {
                            "min" => st.tempo_min = v.parse().with_context(ctx)?,
                            "max" => st.tempo_max = v.parse().with_context(ctx)?,
                            _ => {}
                        }
                    }
                }
                "keys" => {
                    for t in args.split_whitespace() {
                        st.keys.push(parse_key(t).ok_or_else(|| anyhow!("{}: bad key {:?}", ctx(), t))?);
                    }
                }
                "scales" => {
                    for t in args.split_whitespace() {
                        if let Some(m) = Mode::parse(t) {
                            st.scales.push((t.to_string(), m.intervals().to_vec()));
                        } else if !st.scales.iter().any(|(n, _)| n == t) {
                            bail!("{}: unknown scale {:?} (define it with @scale first)", ctx(), t);
                        }
                    }
                }
                "scale" => {
                    let mut it = args.split_whitespace();
                    let name = it.next().ok_or_else(|| anyhow!("{}: missing name", ctx()))?.to_string();
                    let iv: Vec<u8> = it.map(|t| t.parse::<u8>()).collect::<Result<_, _>>().with_context(ctx)?;
                    if iv.is_empty() || iv[0] != 0 {
                        bail!("{}: scale must start at 0", ctx());
                    }
                    st.scales.retain(|(n, _)| *n != name);
                    st.scales.push((name, iv));
                }
                "progression" => {
                    let mut name = String::new();
                    let mut chords = Vec::new();
                    for t in args.split_whitespace() {
                        if let Some(n) = t.strip_prefix("name=") {
                            name = n.to_string();
                        } else {
                            chords.push(parse_chord(t).ok_or_else(|| anyhow!("{}: bad chord {:?}", ctx(), t))?);
                        }
                    }
                    if chords.is_empty() {
                        bail!("{}: empty progression", ctx());
                    }
                    if name.is_empty() {
                        name = format!("prog{}", st.progressions.len());
                    }
                    st.progressions.push(Progression { name, chords });
                }
                "instr" => {
                    let (idx_tok, rest) = match args.find(char::is_whitespace) {
                        Some(i) => (&args[..i], args[i..].trim()),
                        None => (args, ""),
                    };
                    let idx = usize::from_str_radix(idx_tok, 16).with_context(ctx)?;
                    if idx >= INSTRUMENTS {
                        bail!("{}: instrument {:X} out of range", ctx(), idx);
                    }
                    let mut inst = Instrument::default();
                    for (k, v) in kv(rest) {
                        match k.as_str() {
                            "a" => inst.attack_ms = v.parse().with_context(ctx)?,
                            "d" => inst.decay_ms = v.parse().with_context(ctx)?,
                            "s" => inst.sustain = v.parse().with_context(ctx)?,
                            "r" => inst.release_ms = v.parse().with_context(ctx)?,
                            "duty" => inst.duty = v.parse().with_context(ctx)?,
                            "vol" => inst.volume = v.parse().with_context(ctx)?,
                            "name" => { st.instr_names.insert(v, idx as u8); }
                            _ => {}
                        }
                    }
                    st.instruments.push((idx, inst));
                    instr_idx = instr_idx.max(idx + 1);
                }
                "riff" => {
                    let (name, rest) = match args.find(char::is_whitespace) {
                        Some(i) => (args[..i].to_string(), args[i..].trim()),
                        None => bail!("{}: missing name", ctx()),
                    };
                    let mut r = Riff { name, rhythm: vec![true; 16], contour: "walk".into(), harmony: "third".into(), bass: "roots".into(), lead_instr: 0, harm_instr: 1, bass_instr: 2, pair: 1, fx: None, octave: 5, accent: None, slide: 0.0 };
                    let instr_of = |v: &str, st: &Style| -> Result<u8> {
                        if let Some(&i) = st.instr_names.get(v) { return Ok(i); }
                        u8::from_str_radix(v, 16).map_err(|_| anyhow!("unknown instrument {:?}", v))
                    };
                    for (k, v) in kv(rest) {
                        match k.as_str() {
                            "rhythm" => {
                                if v.len() != STEPS_PER_PHRASE {
                                    bail!("{}: rhythm must be 16 chars", ctx());
                                }
                                r.rhythm = v.chars().map(|c| c == 'x' || c == 'X').collect();
                            }
                            "contour" => r.contour = v,
                            "harmony" => r.harmony = v,
                            "bass" => r.bass = v,
                            "lead" => r.lead_instr = instr_of(&v, &st).with_context(ctx)?,
                            "harm" => r.harm_instr = instr_of(&v, &st).with_context(ctx)?,
                            "bassi" => r.bass_instr = instr_of(&v, &st).with_context(ctx)?,
                            "pair" => r.pair = v.parse::<usize>().with_context(ctx)?.max(1),
                            "fx" => r.fx = parse_fx(&v),
                            "octave" => r.octave = v.parse().with_context(ctx)?,
                            "accent" => {
                                let b = v.as_bytes();
                                if b.len() != 2 { bail!("{}: accent wants two hex digits (on-beat, off-beat)", ctx()); }
                                let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
                                r.accent = Some((hex(b[0]).ok_or_else(|| anyhow!("{}: bad accent", ctx()))?, hex(b[1]).ok_or_else(|| anyhow!("{}: bad accent", ctx()))?));
                            }
                            "slide" => r.slide = v.parse().with_context(ctx)?,
                            _ => {}
                        }
                    }
                    st.riffs.push(r);
                }
                "drums" => {
                    let (name, rest) = match args.find(char::is_whitespace) {
                        Some(i) => (args[..i].to_string(), args[i..].trim()),
                        None => bail!("{}: missing name", ctx()),
                    };
                    let mut d = Drums { name, noi: vec!['.'; 16], dpcm: vec!['.'; 16] };
                    for (k, v) in kv(rest) {
                        let pat: Vec<char> = v.split_whitespace().map(|t| t.chars().next().unwrap_or('.')).collect();
                        if pat.len() != STEPS_PER_PHRASE {
                            bail!("{}: {} needs 16 tokens, got {}", ctx(), k, pat.len());
                        }
                        match k.as_str() {
                            "noi" => d.noi = pat,
                            "dpcm" => d.dpcm = pat,
                            _ => {}
                        }
                    }
                    st.drums.push(d);
                }
                "section" => {
                    let (name, rest) = match args.find(char::is_whitespace) {
                        Some(i) => (args[..i].to_string(), args[i..].trim()),
                        None => bail!("{}: missing name", ctx()),
                    };
                    let mut s = Section { name, bars: vec![2], riffs: Vec::new(), drums: Vec::new(), repeat: 1, motif: 0.0, crash_end: false, swell: false };
                    for (k, v) in kv(rest) {
                        match k.as_str() {
                            "bars" => s.bars = list(&v).iter().map(|t| t.parse::<usize>()).collect::<Result<_, _>>().with_context(ctx)?,
                            "riffs" => s.riffs = list(&v),
                            "drums" => s.drums = list(&v),
                            "repeat" => s.repeat = v.parse::<usize>().with_context(ctx)?.max(1),
                            "motif" => s.motif = v.parse().with_context(ctx)?,
                            "end" => s.crash_end = v == "crash",
                            "swell" => s.swell = v == "1" || v == "on",
                            _ => {}
                        }
                    }
                    if s.riffs.is_empty() {
                        bail!("{}: section needs riffs=", ctx());
                    }
                    st.sections.push(s);
                }
                "dpcm" => {
                    let (idx_tok, rest) = match args.find(char::is_whitespace) {
                        Some(i) => (&args[..i], args[i..].trim()),
                        None => bail!("{}: want NN name= path= [rate=] [token=]", ctx()),
                    };
                    let idx = usize::from_str_radix(idx_tok, 16).with_context(ctx)?;
                    if idx > 63 {
                        bail!("{}: slot {:X} out of range (max 3F)", ctx(), idx);
                    }
                    let mut smp = StyleSample { name: format!("sample{:02X}", idx), path: PathBuf::new(), rate: 15, token: None };
                    for (k, v) in kv(rest) {
                        match k.as_str() {
                            "name" => smp.name = v,
                            "path" | "file" => smp.path = PathBuf::from(v),
                            "rate" => {
                                smp.rate = v.parse().with_context(ctx)?;
                                if smp.rate > 15 { bail!("{}: rate out of range 0..=15", ctx()); }
                            }
                            "token" => smp.token = v.chars().next(),
                            _ => {}
                        }
                    }
                    if smp.path.as_os_str().is_empty() {
                        bail!("{}: needs path=", ctx());
                    }
                    while st.samples.len() <= idx {
                        st.samples.push(StyleSample { name: String::new(), path: PathBuf::new(), rate: 15, token: None });
                    }
                    st.samples[idx] = smp;
                }
                "form" => st.forms.push(args.split_whitespace().map(String::from).collect()),
                "motif" => st.motif = args.split_whitespace().map(|t| t.parse::<i32>()).collect::<Result<_, _>>().with_context(ctx)?,
                "title" => {
                    for (k, v) in kv(args) {
                        match k.as_str() {
                            "adj" => st.title_adj = list(&v),
                            "noun" => st.title_noun = list(&v),
                            _ => {}
                        }
                    }
                }
                "noise" => {
                    for (k, v) in kv(args) {
                        let parse = |v: &str| -> Result<(u8, u8)> {
                            let (n, i) = v.split_once('/').ok_or_else(|| anyhow!("{}: want NOTE/INSTR", ctx()))?;
                            let note = crate::vip::decode_note_pub(n).ok_or_else(|| anyhow!("{}: bad note {:?}", ctx(), n))?;
                            Ok((note, u8::from_str_radix(i, 16).with_context(ctx)?))
                        };
                        match k.as_str() {
                            "hat" => st.hat = parse(&v)?,
                            "open" => st.open_hat = parse(&v)?,
                            "crash" => st.crash = parse(&v)?,
                            _ => {}
                        }
                    }
                }
                _ => bail!("{}: unknown directive", ctx()),
            }
        }
        st.validate()?;
        Ok(st)
    }

    /// DPCM note for a `@drums` token: with a declared bank, the slot whose
    /// `token=` matches; without one, the built-in layout k/s/t → 0/1/2.
    pub fn dpcm_note(&self, token: char) -> Option<u8> {
        if self.samples.is_empty() {
            return match token { 'k' => Some(60), 's' => Some(61), 't' => Some(62), _ => None };
        }
        self.samples.iter().position(|s| s.token == Some(token)).map(|i| 60 + i as u8)
    }

    fn validate(&self) -> Result<()> {
        if self.keys.is_empty() { bail!("style has no @keys"); }
        for (i, s) in self.samples.iter().enumerate() {
            if s.path.as_os_str().is_empty() {
                bail!("@dpcm slot {:02X} is missing (slots must be contiguous)", i);
            }
        }
        for d in &self.drums {
            for &c in &d.dpcm {
                if c != '.' && self.dpcm_note(c).is_none() {
                    bail!("@drums {} uses DPCM token {:?} with no @dpcm token= for it", d.name, c);
                }
            }
        }
        if self.scales.is_empty() { bail!("style has no @scales"); }
        if self.progressions.is_empty() { bail!("style has no @progression"); }
        if self.riffs.is_empty() { bail!("style has no @riff"); }
        if self.drums.is_empty() { bail!("style has no @drums"); }
        if self.sections.is_empty() { bail!("style has no @section"); }
        if self.forms.is_empty() { bail!("style has no @form"); }
        if self.tempo_min == 0 || self.tempo_max < self.tempo_min { bail!("bad @tempo range"); }
        for f in &self.forms {
            for s in f {
                let (name, _) = split_transpose(s);
                if !self.sections.iter().any(|x| x.name == name) {
                    bail!("@form references unknown section {:?}", s);
                }
            }
        }
        for s in &self.sections {
            for r in &s.riffs {
                if !self.riffs.iter().any(|x| &x.name == r) {
                    bail!("section {:?} references unknown riff {:?}", s.name, r);
                }
            }
            for d in &s.drums {
                if !self.drums.iter().any(|x| &x.name == d) {
                    bail!("section {:?} references unknown drums {:?}", s.name, d);
                }
            }
        }
        Ok(())
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let file = if dir.is_dir() { dir.join("style.vps") } else { dir.to_path_buf() };
        let text = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", file.display()))
    }
}

/// `chorus+3` → ("chorus", 3); `verse` → ("verse", 0).
pub fn split_transpose(tok: &str) -> (&str, i32) {
    if let Some(i) = tok.find(['+', '-']) {
        let (name, n) = tok.split_at(i);
        (name, n.parse().unwrap_or(0))
    } else {
        (tok, 0)
    }
}

// ---------------------------------------------------------------- generation

#[derive(Clone, Debug)]
pub struct GenParams {
    pub seed: u64,
    pub key: Option<u8>,
    pub bpm: Option<u16>,
    pub scale: Option<String>,
    /// Force the motif on (Some(true)) or off (Some(false)).
    pub motif: Option<bool>,
    pub form: Option<usize>,
    pub driver: Option<(PathBuf, PathBuf)>,
    pub artist: String,
}

impl Default for GenParams {
    fn default() -> Self {
        Self { seed: 1, key: None, bpm: None, scale: None, motif: None, form: None, driver: None, artist: String::new() }
    }
}

#[derive(Clone, Copy, Debug)]
struct Chord {
    /// Semitone root relative to the key tonic (may be chromatic).
    root: i32,
    third: i32,
    fifth: i32,
    /// Scale degree of the root if diatonic.
    #[allow(dead_code)]
    degree: Option<usize>,
}

struct Ctx<'a> {
    style: &'a Style,
    rng: Rng,
    key: u8,
    scale: Vec<u8>,
    prog: Vec<ChordSym>,
    motif_on: bool,
}

impl Ctx<'_> {
    fn pick<'b, T>(&mut self, v: &'b [T]) -> &'b T {
        &v[self.rng.range(0, v.len() as u32) as usize]
    }

    fn chord(&self, sym: ChordSym) -> Chord {
        let n = self.scale.len();
        let deg = sym.degree.min(n - 1);
        let root = self.scale[deg] as i32 + sym.accidental;
        if sym.accidental == 0 {
            let third = self.scale[(deg + 2) % n] as i32 + if deg + 2 >= n { 12 } else { 0 } - root;
            let fifth = self.scale[(deg + 4) % n] as i32 + if deg + 4 >= n { 12 } else { 0 } - root;
            Chord { root, third, fifth, degree: Some(deg) }
        } else {
            Chord { root, third: if sym.minor { 3 } else { 4 }, fifth: 7, degree: None }
        }
    }

    /// MIDI note for a scale degree index (any integer; wraps octaves).
    fn degree_note(&self, deg: i32, octave: i32) -> u8 {
        let n = self.scale.len() as i32;
        let oct = octave + deg.div_euclid(n);
        let iv = self.scale[deg.rem_euclid(n) as usize] as i32;
        ((oct + 1) * 12 + self.key as i32 + iv).clamp(24, 119) as u8
    }

    /// Nearest scale degree index for a semitone offset from the tonic.
    fn nearest_degree(&self, semis: i32) -> i32 {
        let n = self.scale.len() as i32;
        let oct = semis.div_euclid(12);
        let pc = semis.rem_euclid(12) as u8;
        let mut best = 0i32;
        let mut bestd = 99;
        for (i, &iv) in self.scale.iter().enumerate() {
            let d = (iv as i32 - pc as i32).abs();
            if d < bestd {
                bestd = d;
                best = i as i32;
            }
        }
        oct * n + best
    }
}

/// Realize one bar of a riff for the lead channel: (note, fx) per step.
fn lead_bar(ctx: &mut Ctx, riff: &Riff, chord: Chord, bar_in_section: usize, use_motif: bool) -> Vec<Option<(u8, Option<(u8, u8)>)>> {
    let hits: Vec<usize> = (0..STEPS_PER_PHRASE).filter(|&s| riff.rhythm[s]).collect();
    let mut out = vec![None; STEPS_PER_PHRASE];
    let root_deg = ctx.nearest_degree(chord.root);
    let third_deg = ctx.nearest_degree(chord.root + chord.third);
    let fifth_deg = ctx.nearest_degree(chord.root + chord.fifth);
    let chord_degs = [root_deg, third_deg, fifth_deg];
    let oct = riff.octave;
    let contour = if use_motif && !ctx.style.motif.is_empty() { "motif" } else { riff.contour.as_str() };
    let mut pitches: Vec<i32> = Vec::with_capacity(hits.len());
    match contour {
        "pedal" | "pedal_jumps" | "pedal_then_run" => {
            let n = hits.len();
            let jab_from = if contour == "pedal_then_run" { n.saturating_sub(4) } else { n };
            let jab = ctx.rng.chance(0.55);
            let tritone = ctx.rng.chance(0.25);
            let mut cur = root_deg;
            for (i, _) in hits.iter().enumerate() {
                let p = if i >= jab_from && jab {
                    // walk up or down toward the b2 / tritone
                    if tritone && i == n - 1 { cur = root_deg; root_deg * 0 + ctx.nearest_degree(chord.root + 6) } else { cur += if ctx.rng.chance(0.5) { 1 } else { 2 }; cur }
                } else if contour == "pedal_jumps" && i % 4 == 3 && ctx.rng.chance(0.5) {
                    *ctx.pick(&chord_degs) + ctx.scale.len() as i32 * if ctx.rng.chance(0.5) { 1 } else { 0 }
                } else if contour == "pedal" && i == n - 1 && ctx.rng.chance(0.35) {
                    ctx.nearest_degree(chord.root + if tritone { 6 } else { 1 })
                } else {
                    root_deg
                };
                pitches.push(p);
            }
        }
        "walk" => {
            let mut cur = *ctx.pick(&chord_degs);
            let mut dir: i32 = if ctx.rng.chance(0.5) { 1 } else { -1 };
            let mut i = 0;
            while i < hits.len() {
                for _ in 0..riff.pair {
                    if i < hits.len() {
                        pitches.push(cur);
                        i += 1;
                    }
                }
                // step motion, occasionally a leap to a chord tone, turn at range edges
                if ctx.rng.chance(0.2) {
                    cur = *ctx.pick(&chord_degs);
                } else {
                    if ctx.rng.chance(0.25) { dir = -dir; }
                    cur += dir;
                    if cur > root_deg + 7 { dir = -1; cur -= 2; }
                    if cur < root_deg - 3 { dir = 1; cur += 2; }
                }
            }
            // land on a chord tone at the end of the bar for cadence
            if let Some(last) = pitches.last_mut() {
                if bar_in_section % 2 == 1 { *last = root_deg; }
            }
        }
        "run" => {
            let up = ctx.rng.chance(0.5);
            let mut cur = if up { root_deg - 2 } else { root_deg + 7 };
            let dir = if up { 1 } else { -1 };
            for i in 0..hits.len() {
                pitches.push(cur);
                cur += dir;
                if i % 4 == 3 && ctx.rng.chance(0.3) { cur -= dir * 2; }
            }
        }
        "chord" => {
            for (i, _) in hits.iter().enumerate() {
                pitches.push(chord_degs[(i + bar_in_section) % 3]);
            }
        }
        "melody" => {
            let mut cur = *ctx.pick(&chord_degs);
            for (i, &s) in hits.iter().enumerate() {
                if s % 4 == 0 && ctx.rng.chance(0.7) {
                    cur = *ctx.pick(&chord_degs);
                } else {
                    cur += if ctx.rng.chance(0.5) { 1 } else { -1 };
                }
                if i == hits.len() - 1 && bar_in_section % 2 == 1 { cur = root_deg; }
                pitches.push(cur.clamp(root_deg - 3, root_deg + 9));
            }
        }
        "motif" => {
            let m = &ctx.style.motif;
            for i in 0..hits.len() {
                pitches.push(root_deg + m[i % m.len()]);
            }
        }
        _ => {
            for _ in &hits { pitches.push(root_deg); }
        }
    }
    for (i, &s) in hits.iter().enumerate() {
        let n = ctx.degree_note(pitches[i], oct);
        let mut fx = riff.fx;
        if i > 0 && riff.slide > 0.0 && fx.is_none() && ctx.rng.chance(riff.slide) {
            fx = Some((b'S', 0x04));
        }
        out[s] = Some((n, fx));
    }
    out
}

fn harmony_bar(ctx: &mut Ctx, riff: &Riff, chord: Chord, lead: &[Option<(u8, Option<(u8, u8)>)>]) -> Vec<Option<(u8, Option<(u8, u8)>)>> {
    let mut out = vec![None; STEPS_PER_PHRASE];
    let n = ctx.scale.len() as i32;
    for s in 0..STEPS_PER_PHRASE {
        let Some((ln, fx)) = lead[s] else { continue };
        let note = match riff.harmony.as_str() {
            "third" | "sixth" => {
                // diatonic interval below the lead
                let rel = ln as i32 - (ctx.key as i32 + 12);
                let deg = ctx.nearest_degree(rel);
                let below = if riff.harmony == "third" { 2 } else { 5 };
                let d = deg - below;
                let oct = d.div_euclid(n);
                let iv = ctx.scale[d.rem_euclid(n) as usize] as i32;
                (ctx.key as i32 + 12 + oct * 12 + iv).clamp(24, 119) as u8
            }
            "fifth" => {
                // power-chord voicing: the chord's fifth a fourth below the lead's root
                let root = ctx.key as i32 + 12 * (riff.octave + 1) + chord.root;
                (root + chord.fifth - 12).clamp(24, 119) as u8
            }
            "root" => (ctx.key as i32 + 12 * (riff.octave + 1) + chord.root - 12).clamp(24, 119) as u8,
            "unison" => ln,
            _ => continue,
        };
        let fx = if riff.harmony == "unison" { Some((b'S', 0x03)) } else { fx };
        out[s] = Some((note, fx));
    }
    out
}

fn bass_bar(ctx: &mut Ctx, riff: &Riff, chord: Chord, lead: &[Option<(u8, Option<(u8, u8)>)>]) -> Vec<Option<u8>> {
    let root = (ctx.key as i32 + 36 + chord.root) as u8; // octave 2
    let up = root + 12;
    let mut out = vec![None; STEPS_PER_PHRASE];
    match riff.bass.as_str() {
        "octaves" => for s in 0..16 { out[s] = Some(if s % 2 == 0 { root } else { up }); },
        "gallop" => for s in 0..16 { if s % 4 != 1 { out[s] = Some(root); } },
        "roots" => for s in (0..16).step_by(2) { out[s] = Some(root); },
        "half" => { out[0] = Some(root); out[8] = Some(root); }
        "follow" => for s in 0..16 { if lead[s].is_some() { out[s] = Some(root); } },
        "walk" => {
            // root, fifth, octave, approach
            let fifth = (root as i32 + chord.fifth) as u8;
            let seq = [root, root, fifth, root, up, root, fifth, (root as i32 - 1).max(24) as u8];
            for (i, s) in (0..16).step_by(2).enumerate() { out[s] = Some(seq[i]); }
        }
        _ => { out[0] = Some(root); }
    }
    let _ = &mut ctx.rng;
    out
}

fn drum_cells(style: &Style, d: &Drums, fill: bool, crash_start: bool, rng: &mut Rng) -> (Vec<Option<(u8, u8)>>, Vec<Option<u8>>) {
    let mut noi = vec![None; STEPS_PER_PHRASE];
    let mut dpcm = vec![None; STEPS_PER_PHRASE];
    for s in 0..STEPS_PER_PHRASE {
        noi[s] = match d.noi[s] {
            'h' => Some(style.hat),
            'o' => Some(style.open_hat),
            'c' => Some(style.crash),
            _ => None,
        };
        dpcm[s] = if d.dpcm[s] == '.' { None } else { style.dpcm_note(d.dpcm[s]) };
    }
    if fill {
        // Euclidean snare fill over the last 8 steps.
        let k = 3 + rng.range(0, 3) as usize;
        let mask = euclid_mask(k, 8, rng.range(0, 8) as usize);
        let snare = style.dpcm_note('s').unwrap_or(61);
        for (i, hit) in mask.iter().enumerate() {
            if *hit {
                dpcm[8 + i] = Some(snare);
            }
        }
        dpcm[15] = Some(snare);
    }
    if crash_start {
        noi[0] = Some(style.crash);
    }
    (noi, dpcm)
}

fn cell(note: u8, instr: u8, fx: Option<(u8, u8)>) -> Cell {
    Cell { note: Some(note), instr, vol: 0, fx, hold: false }
}

/// What `generate` chose, for headers and manifests.
#[derive(Clone, Debug)]
pub struct GenInfo {
    pub key: u8,
    pub scale: String,
    pub progression: String,
    pub form: usize,
    pub motif: bool,
}

/// Generate a song from a style.
pub fn generate(style: &Style, p: &GenParams) -> Result<Song> {
    generate_with_info(style, p).map(|(s, _)| s)
}

pub fn generate_with_info(style: &Style, p: &GenParams) -> Result<(Song, GenInfo)> {
    let mut rng = Rng::new(p.seed);
    let key = match p.key {
        Some(k) => k,
        None => style.keys[rng.range(0, style.keys.len() as u32) as usize],
    };
    let (scale_name, scale) = match &p.scale {
        Some(name) => style.scales.iter().find(|(n, _)| n == name).cloned()
            .ok_or_else(|| anyhow!("style has no scale {:?}", name))?,
        None => style.scales[rng.range(0, style.scales.len() as u32) as usize].clone(),
    };
    let bpm = match p.bpm {
        Some(b) => b,
        None => {
            let span = (style.tempo_max - style.tempo_min) as u32 / 5 + 1;
            style.tempo_min + 5 * rng.range(0, span) as u16
        }
    };
    let prog_ref = &style.progressions[rng.range(0, style.progressions.len() as u32) as usize];
    let prog_name = prog_ref.name.clone();
    let prog = prog_ref.chords.clone();
    let form_idx = p.form.unwrap_or_else(|| rng.range(0, style.forms.len() as u32) as usize).min(style.forms.len() - 1);
    let form = style.forms[form_idx].clone();
    let motif_on = p.motif.unwrap_or(true) && !style.motif.is_empty();
    let mut ctx = Ctx { style, rng, key, scale, prog, motif_on };

    let mut song = Song::default();
    song.bpm = bpm;
    for (idx, inst) in &style.instruments {
        song.instruments[*idx] = *inst;
    }
    song.phrases.clear();
    let mut phrase_index: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut order: Vec<usize> = Vec::new();

    // Section realizations are cached by name so a recurring section
    // (chorus, riff A) comes back identical — the hook has to be a hook.
    let mut realized: HashMap<String, Vec<Phrase>> = HashMap::new();
    let mut loop_pos: Option<usize> = None;

    let motif_section: Option<String> = if motif_on {
        // Pick the section with the highest motif probability that wins its roll.
        let mut cands: Vec<&Section> = style.sections.iter().filter(|s| s.motif > 0.0).collect();
        cands.sort_by(|a, b| b.motif.partial_cmp(&a.motif).unwrap());
        cands.into_iter().find(|s| ctx.rng.chance(s.motif)).map(|s| s.name.clone())
    } else {
        None
    };

    for (si, stok) in form.iter().enumerate() {
        let (sname_str, transpose) = split_transpose(stok);
        let sname = &sname_str.to_string();
        let section = style.sections.iter().find(|s| &s.name == sname).unwrap().clone();
        let mut phrases: Vec<Phrase> = if let Some(p) = realized.get(sname) {
            p.clone()
        } else {
            let bars = *ctx.pick(&section.bars);
            let riff = ctx.pick(&section.riffs).clone();
            let riff = style.riffs.iter().find(|r| r.name == riff).unwrap().clone();
            let drums_name = if section.drums.is_empty() { style.drums[0].name.clone() } else { ctx.pick(&section.drums).clone() };
            let drums = style.drums.iter().find(|d| d.name == drums_name).unwrap().clone();
            let use_motif = motif_section.as_deref() == Some(sname.as_str());
            let mut out = Vec::new();
            for b in 0..bars {
                let sym = ctx.prog[b % ctx.prog.len()];
                let chord = ctx.chord(sym);
                let lead = lead_bar(&mut ctx, &riff, chord, b, use_motif);
                let harm = harmony_bar(&mut ctx, &riff, chord, &lead);
                let bass = bass_bar(&mut ctx, &riff, chord, &lead);
                let last = b == bars - 1;
                let (noi, dpcm) = drum_cells(style, &drums, last && bars > 1, b == 0 && si > 0, &mut ctx.rng);
                let mut ph = Phrase::default();
                let vol_at = |s: usize| -> u8 {
                    let base = match riff.accent { Some((on, off)) => if s % 4 == 0 { on } else { off }, None => 0 };
                    if section.swell && bars > 1 {
                        // 6..15 across the bars; accents scale within it
                        let top = 6 + (9 * b / (bars - 1).max(1)) as u8;
                        if base == 0 { top } else { (base as u32 * top as u32 / 15).max(1) as u8 }
                    } else {
                        base
                    }
                };
                for s in 0..STEPS_PER_PHRASE {
                    if let Some((n, fx)) = lead[s] { let mut c = cell(n, riff.lead_instr, fx); c.vol = vol_at(s); ph.cells[s][0] = c; }
                    if let Some((n, fx)) = harm[s] { let mut c = cell(n, riff.harm_instr, fx); c.vol = vol_at(s); ph.cells[s][1] = c; }
                    if let Some(n) = bass[s] { ph.cells[s][2] = cell(n, riff.bass_instr, None); }
                    if let Some((n, i)) = noi[s] { ph.cells[s][3] = cell(n, i, None); }
                    if let Some(n) = dpcm[s] { ph.cells[s][4] = cell(n, 0, None); }
                }
                if section.crash_end && last {
                    // ring out: retrig the final hit, crash, drop the drums after beat 1
                    for c in 0..2 {
                        if let Some(last_hit) = (0..STEPS_PER_PHRASE).rev().find(|&s| ph.cells[s][c].note.is_some()) {
                            ph.cells[last_hit][c].fx = Some((b'R', 0x03));
                        }
                    }
                    ph.cells[0][3] = cell(style.crash.0, style.crash.1, None);
                }
                out.push(ph);
            }
            realized.insert(sname.clone(), out.clone());
            out
        };
        if transpose != 0 {
            for ph in phrases.iter_mut() {
                for row in ph.cells.iter_mut() {
                    for c in row.iter_mut().take(3) {
                        if let Some(n) = c.note {
                            c.note = Some((n as i32 + transpose).clamp(24, 119) as u8);
                        }
                    }
                }
            }
        }
        if si == 1 && loop_pos.is_none() {
            loop_pos = Some(order.len());
        }
        for _ in 0..section.repeat {
            for ph in &phrases {
                let key: Vec<u8> = ph.cells.iter().flatten().flat_map(|c| [c.note.unwrap_or(if c.hold { 0xFE } else { 0xFF }), c.instr, c.fx.map(|f| f.0).unwrap_or(0), c.fx.map(|f| f.1).unwrap_or(0)]).collect();
                let idx = *phrase_index.entry(key).or_insert_with(|| {
                    song.phrases.push(ph.clone());
                    song.phrases.len() - 1
                });
                order.push(idx);
            }
        }
    }
    song.order = order;
    song.loop_pos = loop_pos.unwrap_or(0).min(song.order.len().saturating_sub(1));
    song.current_phrase = song.order.first().copied().unwrap_or(0);
    song.title = title(style, &mut ctx.rng);
    song.artist = p.artist.clone();
    song.driver = p.driver.clone();
    song.key_name = format!("{} {}", crate::vip::key_name(key), scale_name);
    song.samples = style.samples.iter().map(|s| crate::DpcmRef { name: s.name.clone(), path: s.path.clone(), rate: s.rate }).collect();
    let info = GenInfo { key, scale: scale_name, progression: prog_name, form: form_idx, motif: motif_section.is_some() };
    let _ = ctx.motif_on;
    Ok((song, info))
}

fn title(style: &Style, rng: &mut Rng) -> String {
    if style.title_adj.is_empty() || style.title_noun.is_empty() {
        return String::new();
    }
    let a = &style.title_adj[rng.range(0, style.title_adj.len() as u32) as usize];
    let n = &style.title_noun[rng.range(0, style.title_noun.len() as u32) as usize];
    format!("{} {}", a, n)
}

/// A small musical sanity report for a generated song, used by curation.
#[derive(Debug, Clone)]
pub struct Report {
    pub bars: usize,
    pub unique_phrases: usize,
    pub lead_density: f32,
    pub lead_range: u8,
    pub drum_hits: usize,
}

pub fn report(song: &Song) -> Report {
    let mut hits = 0usize;
    let mut lo = 127u8;
    let mut hi = 0u8;
    let mut drums = 0usize;
    for &pi in &song.order {
        let p = &song.phrases[pi];
        for row in &p.cells {
            if let Some(n) = row[0].note { hits += 1; lo = lo.min(n); hi = hi.max(n); }
            if row[3].note.is_some() { drums += 1; }
            if row[4].note.is_some() { drums += 1; }
        }
    }
    let bars = song.order.len();
    Report {
        bars,
        unique_phrases: song.phrases.len(),
        lead_density: if bars == 0 { 0.0 } else { hits as f32 / (bars * STEPS_PER_PHRASE) as f32 },
        lead_range: hi.saturating_sub(lo),
        drum_hits: drums,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEUTRAL: &str = include_str!("../styles/neutral/style.vps");

    #[test]
    fn neutral_style_parses_and_generates_deterministically() {
        let st = Style::parse(NEUTRAL).unwrap();
        let a = generate(&st, &GenParams { seed: 7, ..Default::default() }).unwrap();
        let b = generate(&st, &GenParams { seed: 7, ..Default::default() }).unwrap();
        assert_eq!(crate::vip::to_vip(&a), crate::vip::to_vip(&b));
        assert!(!a.order.is_empty());
        assert!(a.bpm >= st.tempo_min && a.bpm <= st.tempo_max);
        let r = report(&a);
        assert!(r.lead_density > 0.1, "{:?}", r);
        assert!(r.drum_hits > 0);
        let c = generate(&st, &GenParams { seed: 8, ..Default::default() }).unwrap();
        assert_ne!(crate::vip::to_vip(&a), crate::vip::to_vip(&c));
    }

    #[test]
    fn transposed_form_sections_shift_pitched_channels_only() {
        let text = NEUTRAL.replace("@form intro verse chorus verse chorus bridge chorus outro", "@form intro verse chorus+3 outro");
        let st = Style::parse(&text).unwrap();
        let a = generate(&st, &GenParams { seed: 5, form: Some(0), ..Default::default() }).unwrap();
        let base = generate(&Style::parse(&NEUTRAL.replace("@form intro verse chorus verse chorus bridge chorus outro", "@form intro verse chorus outro")).unwrap(), &GenParams { seed: 5, form: Some(0), ..Default::default() }).unwrap();
        // same bar count; chorus bars differ by +3 on PU1/PU2/TRI, drums identical
        assert_eq!(a.order.len(), base.order.len());
        let mut shifted = 0;
        for (x, y) in a.order.iter().zip(base.order.iter()) {
            let (pa, pb) = (&a.phrases[*x], &base.phrases[*y]);
            for s in 0..STEPS_PER_PHRASE {
                assert_eq!(pa.cells[s][3].note, pb.cells[s][3].note);
                assert_eq!(pa.cells[s][4].note, pb.cells[s][4].note);
                if let (Some(n1), Some(n2)) = (pa.cells[s][0].note, pb.cells[s][0].note) {
                    if n1 != n2 { assert_eq!(n1, n2 + 3); shifted += 1; }
                }
            }
        }
        assert!(shifted > 0);
        assert_eq!(split_transpose("chorus-2"), ("chorus", -2));
    }

    #[test]
    fn style_dpcm_bank_maps_tokens_and_reaches_generated_songs() {
        let text = NEUTRAL.replace("@noise   hat=C-6/03  open=G-5/03  crash=C-5/04",
            "@noise   hat=C-6/03  open=G-5/03  crash=C-5/04\n@dpcm 00 name=kick path=../samples/kick.dmc token=k\n@dpcm 01 name=snare path=../samples/snare.dmc rate=13 token=s\n@dpcm 02 name=tom path=../samples/tom.dmc token=t");
        let st = Style::parse(&text).unwrap();
        assert_eq!(st.dpcm_note('s'), Some(61));
        assert_eq!(st.dpcm_note('t'), Some(62));
        assert_eq!(st.dpcm_note('x'), None);
        let song = generate(&st, &GenParams { seed: 2, ..Default::default() }).unwrap();
        assert_eq!(song.samples.len(), 3);
        assert_eq!(song.samples[1].rate, 13);
        let out = crate::vip::to_vip(&song);
        assert!(out.contains("@dpcm 01  name=snare  path=../samples/snare.dmc  rate=13"), "{}", out);
        // legacy: no @dpcm keeps k/s/t
        let plain = Style::parse(NEUTRAL).unwrap();
        assert_eq!(plain.dpcm_note('k'), Some(60));
        // a drums line with an unmapped token is rejected
        let bad = text.replace("@drums half   noi=\"h . . . h . . . h . . . h . . .\"  dpcm=\"k . . . . . . . s . . . . . . .\"",
            "@drums half   noi=\"h . . . h . . . h . . . h . . .\"  dpcm=\"k . . . . . . . c . . . . . . .\"");
        assert!(Style::parse(&bad).unwrap_err().to_string().contains("token"));
    }

    #[test]
    fn chord_symbols() {
        assert_eq!(parse_chord("bII"), Some(ChordSym { degree: 1, accidental: -1, minor: false }));
        assert_eq!(parse_chord("vi"), Some(ChordSym { degree: 5, accidental: 0, minor: true }));
        assert_eq!(parse_chord("VIII"), None);
    }

    #[test]
    fn style_validation_catches_dangling_names() {
        let bad = "@keys E\n@scales minor\n@progression i VI\n@riff a rhythm=xxxxxxxxxxxxxxxx\n@drums d noi=\"h h h h h h h h h h h h h h h h\"\n@section s riffs=nope\n@form s\n";
        let e = Style::parse(bad).unwrap_err().to_string();
        assert!(e.contains("unknown riff"), "{}", e);
    }
}
