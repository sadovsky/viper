//! .vip file format — plain-text tracker format for viper songs.
//!
//! Line-oriented, whitespace-tolerant, diffable. A composer with a text editor
//! should be able to author a valid song by hand.
//!
//! Directives begin with `@` (`@song`, `@phrase`, `@instr`). Data rows start
//! with a two-digit hex step index followed by four cells. A cell is either
//! `---` (empty) or `NOTE[:INSTR[:VOL]]` where NOTE is three chars (letter,
//! accidental `-` or `#`, octave digit), INSTR and VOL are two hex digits.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::{Cell, Chain, Phrase, Song, CHANNELS, INSTRUMENTS, STEPS_PER_PHRASE};

const NOTE_NAMES: [&str; 12] = [
    "C-", "C#", "D-", "D#", "E-", "F-", "F#", "G-", "G#", "A-", "A#", "B-",
];

/// Pitch-class name for headers/reports (sharps).
pub fn key_name(pc: u8) -> &'static str {
    ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"][(pc % 12) as usize]
}

fn encode_note(n: u8) -> String {
    let pc = (n % 12) as usize;
    let oct = ((n as i32) / 12 - 1).clamp(0, 9);
    format!("{}{}", NOTE_NAMES[pc], oct)
}

pub fn decode_note_pub(s: &str) -> Option<u8> {
    decode_note(s)
}

fn decode_note(s: &str) -> Option<u8> {
    let b = s.as_bytes();
    if b.len() != 3 {
        return None;
    }
    let pc = match b[0].to_ascii_uppercase() {
        b'C' => 0, b'D' => 2, b'E' => 4, b'F' => 5,
        b'G' => 7, b'A' => 9, b'B' => 11,
        _ => return None,
    };
    let acc = match b[1] {
        b'-' => 0,
        b'#' => 1,
        _ => return None,
    };
    let oct = (b[2] as char).to_digit(10)? as i32;
    let midi = 12 * (oct + 1) + pc + acc;
    if !(0..=127).contains(&midi) {
        return None;
    }
    Some(midi as u8)
}

fn encode_cell(c: Cell) -> String {
    if c.hold {
        return "===".to_string();
    }
    match c.note {
        None => "---".to_string(),
        Some(n) => {
            let mut s = format!("{}:{:02X}:{:02X}", encode_note(n), c.instr, c.vol);
            if let Some((cmd, param)) = c.fx {
                s.push(':');
                s.push(cmd as char);
                s.push_str(&format!("{:02X}", param));
            }
            s
        }
    }
}

fn decode_cell(s: &str) -> Result<Cell> {
    if s == "---" || s.is_empty() {
        return Ok(Cell::default());
    }
    if s == "===" || s.starts_with("===:") {
        return Ok(Cell::hold());
    }
    let mut parts = s.split(':');
    let note_s = parts.next().ok_or_else(|| anyhow!("empty cell token"))?;
    let note = decode_note(note_s)
        .ok_or_else(|| anyhow!("bad note {:?}", note_s))?;
    let instr = parts
        .next()
        .map(|t| u8::from_str_radix(t, 16))
        .transpose()
        .context("instr field")?
        .unwrap_or(0);
    let vol = parts
        .next()
        .map(|t| u8::from_str_radix(t, 16))
        .transpose()
        .context("vol field")?
        .unwrap_or(0);
    let fx = parts
        .next()
        .map(decode_fx)
        .transpose()?
        .flatten();
    Ok(Cell { note: Some(note), instr, vol, fx, hold: false })
}

/// FORMAT.md effect column is `CPP`: a single-char command (letter or digit)
/// followed by two hex-digit parameters. `---` (or an empty field) = no fx.
fn decode_fx(s: &str) -> Result<Option<(u8, u8)>> {
    if s == "---" || s.is_empty() {
        return Ok(None);
    }
    if s.len() != 3 {
        bail!("fx field must be 3 chars, got {:?}", s);
    }
    let b = s.as_bytes();
    let cmd = b[0];
    if !(cmd.is_ascii_alphanumeric()) {
        bail!("fx command must be A-Z or 0-9, got {:?}", cmd as char);
    }
    let param = u8::from_str_radix(&s[1..], 16).context("fx param")?;
    Ok(Some((cmd, param)))
}

// ---------- Writer ----------

pub fn to_vip(song: &Song) -> String {
    let mut out = String::new();
    out.push_str("# viper song file\n");
    write!(
        out,
        "@song  bpm={}  edit_step={}  current={:02X}",
        song.bpm, song.edit_step, song.current_phrase
    )
    .unwrap();
    // With an arrangement the order is derived (Song::flat_order), so only
    // the explicit list is written — never both.
    if song.arrangement.is_empty() && !song.order.is_empty() {
        let list: Vec<String> = song.order.iter().map(|i| format!("{:02X}", i)).collect();
        write!(out, "  order=[{}]  loop={:02X}", list.join(","), song.loop_pos).unwrap();
    }
    out.push('\n');
    if !song.title.is_empty() || !song.artist.is_empty() || !song.copyright.is_empty() || !song.key_name.is_empty() {
        write!(out, "@meta ").unwrap();
        if !song.title.is_empty() { write!(out, " title={:?}", song.title).unwrap(); }
        if !song.artist.is_empty() { write!(out, " artist={:?}", song.artist).unwrap(); }
        if !song.copyright.is_empty() { write!(out, " license={:?}", song.copyright).unwrap(); }
        if !song.key_name.is_empty() { write!(out, " key={:?}", song.key_name).unwrap(); }
        out.push('\n');
    }
    if let Some((bin, sym)) = &song.driver {
        writeln!(out, "@driver  bin={}  sym={}  expansion={}", bin.display(), sym.display(),
            if song.expansion { "vrc6" } else { "none" }).unwrap();
    }
    for (i, r) in song.samples.iter().enumerate() {
        write!(out, "@dpcm {:02X}  name={}  path={}", i, r.name, r.path.display()).unwrap();
        if r.rate != 15 {
            write!(out, "  rate={}", r.rate).unwrap();
        }
        out.push('\n');
    }
    out.push('\n');

    for (pi, phrase) in song.phrases.iter().enumerate() {
        writeln!(out, "@phrase {:02X}", pi).unwrap();
        writeln!(out, "  # step   PU1             PU2             TRI             NOI             DPCM").unwrap();
        for (s, row) in phrase.cells.iter().enumerate() {
            let cells: Vec<String> = row.iter().map(|c| encode_cell(*c)).collect();
            writeln!(
                out,
                "  {:02X}       {:<14}  {:<14}  {:<14}  {:<14}  {:<14}",
                s, cells[0], cells[1], cells[2], cells[3], cells[4]
            )
            .unwrap();
        }
        out.push('\n');
    }

    // Stage 23: chains + arrangement. Every chain is written, empty ones
    // included, so arrangement entries keep pointing at the same ids.
    if !song.chains.is_empty() || !song.arrangement.is_empty() {
        for (ci, chain) in song.chains.iter().enumerate() {
            match &chain.name {
                Some(n) if !n.is_empty() => writeln!(out, "@chain {:02X}  name={:?}", ci, n).unwrap(),
                _ => writeln!(out, "@chain {:02X}", ci).unwrap(),
            }
            let list: Vec<String> = chain.phrases.iter().map(|p| format!("{:02X}", p)).collect();
            if !list.is_empty() {
                writeln!(out, "  {}", list.join(" ")).unwrap();
            }
        }
        writeln!(out, "@arrangement  loop={:02X}", song.arr_loop).unwrap();
        let arr: Vec<String> = song.arrangement.iter().map(|c| format!("{:02X}", c)).collect();
        if !arr.is_empty() {
            writeln!(out, "  {}", arr.join(" ")).unwrap();
        }
        out.push('\n');
    }
    // Groove and per-channel lengths are only written when they deviate
    // from straight time / full-length, keeping simple files tidy.
    if song.has_groove() {
        writeln!(out, "@groove").unwrap();
        let tokens: Vec<String> = song.groove.iter().map(|g| g.to_string()).collect();
        writeln!(out, "  {}\n", tokens.join(" ")).unwrap();
    }
    if song.channel_length != [STEPS_PER_PHRASE as u8; CHANNELS] {
        let l = song.channel_length;
        writeln!(out, "@length  pu1={}  pu2={}  tri={}  noi={}  dpcm={}\n", l[0], l[1], l[2], l[3], l[4]).unwrap();
    }

    for (i, inst) in song.instruments.iter().enumerate() {
        writeln!(
            out,
            "@instr {:02X}  a={:<4}  d={:<4}  s={:.3}  r={:<5}  duty={:.3}  vol={:.3}",
            i, inst.attack_ms, inst.decay_ms, inst.sustain, inst.release_ms, inst.duty, inst.volume
        )
        .unwrap();
    }
    out
}

// ---------- Parser ----------

/// Which directive the following data rows belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    None,
    Phrase(usize),
    Chain(usize),
    Arrangement,
    Groove,
}

pub fn from_vip(text: &str) -> Result<(Song, Vec<String>)> {
    let mut song = Song::default();
    song.phrases.clear();
    let mut section = Section::None;
    let mut groove_pos = 0usize;
    let mut warnings: Vec<String> = Vec::new();

    // Directives reserved in FORMAT.md but not yet implemented; parsing them
    // shouldn't error, but silently dropping them has burned us in hand-edited
    // files, so warn.
    const RESERVED: &[&str] = &["scene", "bind", "sprite"];

    for (line_no, raw) in text.lines().enumerate() {
        let line_num = line_no + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix('@') {
            let (dir, args) = split_once_ws(rest);
            match dir {
                "song" => {
                    parse_song(&mut song, args)
                        .with_context(|| format!("line {}: @song", line_num))?;
                    section = Section::None;
                }
                "phrase" => {
                    let idx = parse_phrase_idx(args)
                        .with_context(|| format!("line {}: @phrase", line_num))?;
                    while song.phrases.len() <= idx {
                        song.phrases.push(Phrase::default());
                    }
                    section = Section::Phrase(idx);
                }
                "chain" => {
                    let (idx, name) = parse_chain_header(args)
                        .with_context(|| format!("line {}: @chain", line_num))?;
                    while song.chains.len() <= idx {
                        song.chains.push(Chain::default());
                    }
                    song.chains[idx].name = name;
                    section = Section::Chain(idx);
                }
                "arrangement" | "arr" => {
                    for (k, v) in kv_iter(args) {
                        if k == "loop" {
                            song.arr_loop = usize::from_str_radix(v, 16)
                                .with_context(|| format!("line {}: @arrangement loop", line_num))?;
                        }
                    }
                    section = Section::Arrangement;
                }
                "groove" => {
                    // Inline form: `@groove swing=N` (±N samples on alternate 16ths).
                    let mut inline = false;
                    for (k, v) in kv_iter(args) {
                        if k == "swing" {
                            let n: i16 = v.parse().with_context(|| format!("line {}: @groove swing", line_num))?;
                            song.groove = swing_groove(n);
                            inline = true;
                        }
                    }
                    groove_pos = 0;
                    section = if inline { Section::None } else { Section::Groove };
                }
                "length" | "len" => {
                    parse_length(&mut song, args)
                        .with_context(|| format!("line {}: @length", line_num))?;
                    section = Section::None;
                }
                "instr" => {
                    parse_instr(&mut song, args)
                        .with_context(|| format!("line {}: @instr", line_num))?;
                    section = Section::None;
                }
                "meta" => { parse_meta(&mut song, args); section = Section::None; }
                "driver" => { parse_driver(&mut song, args); section = Section::None; }
                "dpcm" => {
                    parse_dpcm(&mut song, args)
                        .with_context(|| format!("line {}: @dpcm", line_num))?;
                    section = Section::None;
                }
                d if RESERVED.contains(&d) => {
                    warnings.push(format!(
                        "line {}: @{} reserved but not implemented — ignored",
                        line_num, d
                    ));
                }
                _ => {
                    warnings.push(format!(
                        "line {}: unknown @{} directive — ignored",
                        line_num, dir
                    ));
                }
            }
        } else {
            match section {
                Section::Phrase(pi) => parse_data_row(&mut song.phrases[pi], line)
                    .with_context(|| format!("line {}", line_num))?,
                Section::Chain(ci) => {
                    for tok in line.split_whitespace() {
                        let p = u8::from_str_radix(tok, 16)
                            .with_context(|| format!("line {}: chain phrase {:?}", line_num, tok))?;
                        song.chains[ci].phrases.push(p);
                    }
                }
                Section::Arrangement => {
                    for tok in line.split_whitespace() {
                        let c = u8::from_str_radix(tok, 16)
                            .with_context(|| format!("line {}: arrangement chain {:?}", line_num, tok))?;
                        song.arrangement.push(c);
                    }
                }
                Section::Groove => {
                    for tok in line.split_whitespace() {
                        if groove_pos >= 16 {
                            bail!("line {}: @groove has more than 16 values", line_num);
                        }
                        song.groove[groove_pos] = tok.parse::<i16>()
                            .with_context(|| format!("line {}: groove value {:?}", line_num, tok))?;
                        groove_pos += 1;
                    }
                }
                Section::None => bail!(
                    "line {}: data row appears before any @phrase / @chain / @arrangement / @groove directive",
                    line_num
                ),
            }
        }
    }

    // A hold with no note above it in the same phrase (rows > 0) sustains
    // nothing; keep it (the synth idles, the compiler emits nothing) but say so.
    let mut orphans = 0;
    for p in &song.phrases {
        for ch in 0..CHANNELS {
            let mut sounding = false;
            for s in 0..STEPS_PER_PHRASE {
                let c = p.cells[s][ch];
                if c.note.is_some() { sounding = true; }
                else if c.hold {
                    // a hold on row 0 continues the previous phrase: assume it sounds
                    if s == 0 { sounding = true; } else if !sounding { orphans += 1; }
                }
                else { sounding = false; }
            }
        }
    }
    if orphans > 0 {
        warnings.push(format!("{} hold cell(s) (===) have no note above them", orphans));
    }
    if song.phrases.is_empty() {
        song.phrases.push(Phrase::default());
    }
    if song.current_phrase >= song.phrases.len() {
        song.current_phrase = 0;
    }
    let n = song.phrases.len();
    let before = song.order.len();
    song.order.retain(|&i| i < n);
    if song.order.len() != before {
        warnings.push("@song order references phrases that do not exist — dropped".into());
    }
    if song.loop_pos >= song.order.len() {
        song.loop_pos = 0;
    }
    // Stage 23: chains may only reference existing phrases, the arrangement
    // only existing chains.
    for (ci, chain) in song.chains.iter_mut().enumerate() {
        let before = chain.phrases.len();
        chain.phrases.retain(|&p| (p as usize) < n);
        if chain.phrases.len() != before {
            warnings.push(format!("@chain {:02X} references phrases that do not exist — dropped", ci));
        }
    }
    let nc = song.chains.len();
    let before = song.arrangement.len();
    song.arrangement.retain(|&c| (c as usize) < nc);
    if song.arrangement.len() != before {
        warnings.push("@arrangement references chains that do not exist — dropped".into());
    }
    if song.arr_loop >= song.arrangement.len() {
        song.arr_loop = 0;
    }
    if !song.arrangement.is_empty() && !song.order.is_empty() {
        warnings.push("@song order= ignored: the @arrangement decides playback".into());
        song.order.clear();
        song.loop_pos = 0;
    }
    Ok((song, warnings))
}

/// `@groove swing=N` and `:groove swing N`: every off-16th lands N samples
/// late and every on-16th N samples early, keeping the bar length exact.
pub fn swing_groove(n: i16) -> [i16; 16] {
    let mut g = [0i16; 16];
    for (i, v) in g.iter_mut().enumerate() {
        *v = if i % 2 == 0 { -n } else { n };
    }
    g
}

/// `@chain NN [name="..."]`
fn parse_chain_header(args: &str) -> Result<(usize, Option<String>)> {
    let (idx_tok, rest) = split_once_ws(args);
    let idx = usize::from_str_radix(idx_tok, 16).context("chain index")?;
    let mut name = None;
    for (k, v) in kv_quoted(rest) {
        if k == "name" && !v.is_empty() {
            name = Some(v);
        }
    }
    Ok((idx, name))
}

/// `@length pu1=16 pu2=16 tri=12 noi=8 dpcm=16` — any subset.
fn parse_length(song: &mut Song, args: &str) -> Result<()> {
    for (k, v) in kv_iter(args) {
        let n: u8 = v.parse().with_context(|| format!("length {}={}", k, v))?;
        if n == 0 || n as usize > STEPS_PER_PHRASE {
            bail!("length {}={} out of range (1..={})", k, n, STEPS_PER_PHRASE);
        }
        let ch = match k {
            "pu1" => 0,
            "pu2" => 1,
            "tri" => 2,
            "noi" => 3,
            "dpcm" => 4,
            _ => bail!("length: unknown channel {:?}", k),
        };
        song.channel_length[ch] = n;
    }
    Ok(())
}

/// `key=value` pairs where a value may be double-quoted to contain spaces.
fn kv_quoted(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().to_string();
        rest = &rest[eq + 1..];
        let value;
        if let Some(r) = rest.strip_prefix('"') {
            match r.find('"') {
                Some(end) => { value = r[..end].to_string(); rest = &r[end + 1..]; }
                None => { value = r.to_string(); rest = ""; }
            }
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

fn parse_meta(song: &mut Song, args: &str) {
    for (k, v) in kv_quoted(args) {
        match k.as_str() {
            "title" => song.title = v,
            "artist" | "composer" | "author" => song.artist = v,
            "license" | "copyright" => song.copyright = v,
            "key" => song.key_name = v,
            _ => {}
        }
    }
}

fn parse_driver(song: &mut Song, args: &str) {
    let (mut bin, mut sym) = (None, None);
    for (k, v) in kv_quoted(args) {
        match k.as_str() {
            "bin" | "path" => bin = Some(PathBuf::from(v)),
            "sym" => sym = Some(PathBuf::from(v)),
            "expansion" => song.expansion = v.eq_ignore_ascii_case("vrc6"),
            _ => {}
        }
    }
    if let Some(b) = bin {
        let s = sym.unwrap_or_else(|| b.with_extension("sym"));
        song.driver = Some((b, s));
    }
}

fn parse_dpcm(song: &mut Song, args: &str) -> Result<()> {
    let (idx_tok, rest) = split_once_ws(args);
    let idx = usize::from_str_radix(idx_tok, 16).context("sample index")?;
    if idx > 63 {
        bail!("sample index {:X} out of range (max 3F)", idx);
    }
    let mut name = format!("sample{:02X}", idx);
    let mut path = None;
    let mut rate = 15u8;
    for (k, v) in kv_quoted(rest) {
        match k.as_str() {
            "name" => name = v,
            "path" | "file" => path = Some(PathBuf::from(v)),
            "rate" => {
                rate = v.parse().context("rate")?;
                if rate > 15 {
                    bail!("rate {} out of range 0..=15", rate);
                }
            }
            _ => {}
        }
    }
    let path = path.ok_or_else(|| anyhow!("@dpcm needs path="))?;
    while song.samples.len() <= idx {
        song.samples.push(crate::DpcmRef { name: String::new(), path: PathBuf::new(), rate: 15 });
    }
    song.samples[idx] = crate::DpcmRef { name, path, rate };
    Ok(())
}

/// A `#` starts a comment only when it begins a whitespace-separated token.
/// A `#` inside a token (e.g. the sharp in `F#5`) is literal.
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

fn split_once_ws(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

fn kv_iter(s: &str) -> impl Iterator<Item = (&str, &str)> + '_ {
    s.split_whitespace().filter_map(|tok| tok.split_once('='))
}

fn parse_song(song: &mut Song, args: &str) -> Result<()> {
    for (k, v) in kv_iter(args) {
        match k {
            "bpm" => song.bpm = v.parse().context("bpm")?,
            "edit_step" => song.edit_step = v.parse().context("edit_step")?,
            "current" => {
                song.current_phrase = usize::from_str_radix(v, 16).context("current")?;
            }
            "order" => {
                let inner = v.trim_start_matches('[').trim_end_matches(']');
                song.order = inner
                    .split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(|t| usize::from_str_radix(t, 16).context("order entry"))
                    .collect::<Result<Vec<_>>>()?;
            }
            "loop" => song.loop_pos = usize::from_str_radix(v, 16).context("loop")?,
            "steps" => {} // informational
            _ => {}
        }
    }
    Ok(())
}

fn parse_phrase_idx(args: &str) -> Result<usize> {
    let tok = args
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("missing phrase index"))?;
    let idx = usize::from_str_radix(tok, 16).context("phrase index")?;
    Ok(idx)
}

fn parse_instr(song: &mut Song, args: &str) -> Result<()> {
    let (idx_tok, rest) = split_once_ws(args);
    let idx = usize::from_str_radix(idx_tok, 16).context("instrument index")?;
    if idx >= INSTRUMENTS {
        bail!("instrument {:X} out of range (max {:X})", idx, INSTRUMENTS - 1);
    }
    let mut inst = song.instruments[idx];
    for (k, v) in kv_iter(rest) {
        match k {
            "a"    => inst.attack_ms  = v.parse().context("a")?,
            "d"    => inst.decay_ms   = v.parse().context("d")?,
            "s"    => inst.sustain    = v.parse().context("s")?,
            "r"    => inst.release_ms = v.parse().context("r")?,
            "duty" => inst.duty       = v.parse().context("duty")?,
            "vol"  => inst.volume     = v.parse().context("vol")?,
            _ => {}
        }
    }
    song.instruments[idx] = inst;
    Ok(())
}

fn parse_data_row(phrase: &mut Phrase, line: &str) -> Result<()> {
    let mut iter = line.split_whitespace();
    let step_tok = iter.next().ok_or_else(|| anyhow!("empty row"))?;
    let step = usize::from_str_radix(step_tok, 16).context("step")?;
    if step >= STEPS_PER_PHRASE {
        bail!("step {:X} out of range", step);
    }
    for ch in 0..CHANNELS {
        let tok = iter.next().unwrap_or("---");
        phrase.cells[step][ch] =
            decode_cell(tok).with_context(|| format!("ch{}", ch + 1))?;
    }
    Ok(())
}

// ---------- File I/O ----------

pub fn load(path: &Path) -> Result<(Song, Vec<String>)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    from_vip(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save(song: &Song, path: &Path) -> Result<()> {
    std::fs::write(path, to_vip(song))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_round_trip() {
        for midi in 12..=127u8 {
            let s = encode_note(midi);
            let back = decode_note(&s).unwrap_or_else(|| panic!("decode {:?}", s));
            assert_eq!(midi, back, "round-trip {:?}", s);
        }
    }

    #[test]
    fn round_trip_default_song() {
        let song = Song::default();
        let text = to_vip(&song);
        let (back, warns) = from_vip(&text).unwrap();
        assert!(warns.is_empty());
        assert_eq!(song.bpm, back.bpm);
        assert_eq!(song.edit_step, back.edit_step);
        assert_eq!(song.phrases.len(), back.phrases.len());
        for i in 0..INSTRUMENTS {
            assert_eq!(song.instruments[i].attack_ms, back.instruments[i].attack_ms);
            assert_eq!(song.instruments[i].decay_ms, back.instruments[i].decay_ms);
        }
    }

    #[test]
    fn round_trip_demo_song() {
        let song = Song::demo();
        let text = to_vip(&song);
        let (back, _) = from_vip(&text).unwrap();
        assert_eq!(song.bpm, back.bpm);
        for (a, b) in song.phrases[0].cells.iter().zip(back.phrases[0].cells.iter()) {
            for (ca, cb) in a.iter().zip(b.iter()) {
                assert_eq!(ca.note, cb.note);
                assert_eq!(ca.instr, cb.instr);
                assert_eq!(ca.vol, cb.vol);
            }
        }
    }

    #[test]
    fn reject_garbage_step() {
        let bad = "@phrase 00\n  GZ  ---  ---  ---  ---\n";
        assert!(from_vip(bad).is_err());
    }

    #[test]
    fn comment_lines_ignored() {
        let text = "# comment only\n@song bpm=123\n@phrase 00\n  00 A-4 --- --- ---\n";
        let (song, _) = from_vip(text).unwrap();
        assert_eq!(song.bpm, 123);
        assert_eq!(song.phrases[0].cells[0][0].note, Some(69));
    }

    #[test]
    fn fx_round_trip() {
        let cell = Cell { note: Some(60), instr: 1, vol: 0x0F, fx: Some((b'A', 0x42)), hold: false };
        let s = encode_cell(cell);
        assert_eq!(s, "C-4:01:0F:A42");
        let back = decode_cell(&s).unwrap();
        assert_eq!(back.fx, Some((b'A', 0x42)));
        // No fx field → round-trip without trailing :
        let bare = Cell { note: Some(60), instr: 0, vol: 0, fx: None, hold: false };
        assert_eq!(encode_cell(bare), "C-4:00:00");
    }

    #[test]
    fn fx_rejects_bad_form() {
        assert!(decode_cell("C-4:00:00:AB").is_err());       // too short
        assert!(decode_cell("C-4:00:00:ABCD").is_err());     // too long
        assert!(decode_cell("C-4:00:00:!42").is_err());      // bad cmd char
    }

    #[test]
    fn reserved_directive_warns() {
        let text = "@song bpm=120\n@phrase 00\n@scene 1 phrase=00\n@bogus foo=bar\n";
        let (_, warns) = from_vip(text).unwrap();
        assert_eq!(warns.len(), 2);
        assert!(warns[0].contains("@scene"));
        assert!(warns[1].contains("@bogus"));
    }

    #[test]
    fn order_meta_driver_round_trip() {
        let mut song = Song::default();
        song.phrases.push(Phrase::default());
        song.order = vec![0, 1, 0];
        song.loop_pos = 1;
        song.title = "Blast Furnace".into();
        song.artist = "viper".into();
        song.driver = Some(("driver/build/driver.bin".into(), "driver/build/driver.sym".into()));
        song.samples.push(crate::DpcmRef { name: "kick".into(), path: "samples/kick.dmc".into(), rate: 13 });
        song.phrases[1].cells[3][4] = Cell { note: Some(61), instr: 0, vol: 0, fx: None, hold: false };
        let text = to_vip(&song);
        let (back, warns) = from_vip(&text).unwrap();
        assert!(warns.is_empty(), "{:?}", warns);
        assert_eq!(back.order, vec![0, 1, 0]);
        assert_eq!(back.loop_pos, 1);
        assert_eq!(back.title, "Blast Furnace");
        assert_eq!(back.artist, "viper");
        assert_eq!(back.driver.as_ref().unwrap().1, Path::new("driver/build/driver.sym"));
        assert_eq!(back.samples[0].name, "kick");
        assert_eq!(back.samples[0].rate, 13);
        let (again, _) = from_vip("@dpcm 00 name=k path=k.dmc\n").unwrap();
        assert_eq!(again.samples[0].rate, 15);
        assert_eq!(back.phrases[1].cells[3][4].note, Some(61));
    }

    #[test]
    fn hold_cells_round_trip_and_orphans_warn() {
        let mut song = Song::default();
        song.phrases[0].cells[0][0] = Cell { note: Some(60), instr: 1, vol: 0, fx: None, hold: false };
        song.phrases[0].cells[1][0] = Cell::hold();
        song.phrases[0].cells[2][0] = Cell::hold();
        let text = to_vip(&song);
        assert!(text.contains("===             ---"), "{}", text);
        let (back, warns) = from_vip(&text).unwrap();
        assert!(warns.is_empty(), "{:?}", warns);
        assert!(back.phrases[0].cells[1][0].hold && back.phrases[0].cells[2][0].hold);
        assert_eq!(back.phrases[0].cells[1][0].note, None);
        let (_, warns) = from_vip("@phrase 00\n  00 --- --- --- --- ---\n  01 === --- --- --- ---\n").unwrap();
        assert_eq!(warns.len(), 1, "{:?}", warns);
        assert!(decode_cell("===:00").unwrap().hold);
    }

    #[test]
    fn four_column_files_still_load() {
        let text = "@phrase 00\n  00 C-4 --- E-2 G-2\n";
        let (song, _) = from_vip(text).unwrap();
        assert_eq!(song.phrases[0].cells[0][3].note, Some(43));
        assert_eq!(song.phrases[0].cells[0][4].note, None);
    }

    #[test]
    fn chains_and_arrangement_round_trip_and_flatten() {
        let mut song = Song::default();
        song.phrases.push(Phrase::default());
        song.phrases.push(Phrase::default());
        song.chains = vec![
            Chain { phrases: vec![0, 1, 0], name: Some("intro riff".into()) },
            Chain { phrases: vec![2, 1], name: None },
        ];
        song.arrangement = vec![0, 1, 0];
        song.arr_loop = 1;
        let text = to_vip(&song);
        assert!(!text.contains("order="), "arrangement songs must not also write order=\n{}", text);
        let (back, warns) = from_vip(&text).unwrap();
        assert!(warns.is_empty(), "unexpected warnings: {:?}", warns);
        assert_eq!(back.chains, song.chains);
        assert_eq!(back.arrangement, vec![0, 1, 0]);
        assert_eq!(back.arr_loop, 1);
        assert_eq!(back.flat_order(), (vec![0, 1, 0, 2, 1, 0, 1, 0], 3));
        assert_eq!(back.arrangement_map()[3], (1, 0));
        // Second pass is byte-stable.
        assert_eq!(to_vip(&back), text);
    }

    #[test]
    fn groove_and_length_round_trip_and_defaults_are_omitted() {
        let plain = to_vip(&Song::default());
        assert!(!plain.contains("@groove") && !plain.contains("@length") && !plain.contains("@chain"));

        let mut song = Song::default();
        song.groove = swing_groove(120);
        song.channel_length = [16, 16, 12, 8, 16];
        let text = to_vip(&song);
        let (back, warns) = from_vip(&text).unwrap();
        assert!(warns.is_empty(), "{:?}", warns);
        assert_eq!(back.groove, song.groove);
        assert_eq!(back.channel_length, song.channel_length);

        let (inline, _) = from_vip("@song bpm=120\n@groove swing=7\n@phrase 00\n").unwrap();
        assert_eq!(inline.groove, swing_groove(7));
    }

    #[test]
    fn bad_chain_and_arrangement_references_are_dropped_with_warnings() {
        let text = "@song bpm=120 order=[00]\n@phrase 00\n@chain 00\n  00 05\n@chain 01\n  00\n@arrangement loop=03\n  00 07 01\n";
        let (song, warns) = from_vip(text).unwrap();
        assert_eq!(song.chains[0].phrases, vec![0]);
        assert_eq!(song.arrangement, vec![0, 1]);
        assert_eq!(song.arr_loop, 0);
        assert!(song.order.is_empty(), "order= gives way to the arrangement");
        assert!(warns.iter().any(|w| w.contains("@chain 00")));
        assert!(warns.iter().any(|w| w.contains("@arrangement")));
        assert!(warns.iter().any(|w| w.contains("order= ignored")));
    }

    #[test]
    fn projects_stress_melodeath_parses() {
        // Round-trip the bundled stress test so format drift fails CI early.
        let text = include_str!("../projects/stress_melodeath.vip");
        let (song, _) = from_vip(text).expect("stress_melodeath parses");
        assert_eq!(song.bpm, 220);
        assert_eq!(song.phrases.len(), 3);
        // Phrase 00 row 00 should have all four voices active with the
        // designated instruments.
        let row = &song.phrases[0].cells[0];
        assert!(row[0].note.is_some(), "PU1 should have a note");
        assert_eq!(row[1].instr, 0x01, "PU2 should use instr 01 (harmony)");
        assert_eq!(row[2].instr, 0x02, "TRI should use instr 02 (bass)");
        assert_eq!(row[3].instr, 0x03, "NOI should use instr 03 (blast)");
        // Instrument sustain values parsed as floats, not hex slots.
        assert!((song.instruments[0].sustain - 0.90).abs() < 0.01);
    }
}
