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
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use crate::{Cell, Phrase, Song, CHANNELS, INSTRUMENTS, STEPS_PER_PHRASE};

const NOTE_NAMES: [&str; 12] = [
    "C-", "C#", "D-", "D#", "E-", "F-", "F#", "G-", "G#", "A-", "A#", "B-",
];

fn encode_note(n: u8) -> String {
    let pc = (n % 12) as usize;
    let oct = ((n as i32) / 12 - 1).clamp(0, 9);
    format!("{}{}", NOTE_NAMES[pc], oct)
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
    match c.note {
        None => "---".to_string(),
        Some(n) => format!("{}:{:02X}:{:02X}", encode_note(n), c.instr, c.vol),
    }
}

fn decode_cell(s: &str) -> Result<Cell> {
    if s == "---" || s.is_empty() {
        return Ok(Cell::default());
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
    Ok(Cell { note: Some(note), instr, vol, fx: None })
}

// ---------- Writer ----------

pub fn to_vip(song: &Song) -> String {
    let mut out = String::new();
    out.push_str("# viper song file\n");
    writeln!(
        out,
        "@song  bpm={}  edit_step={}  current={:02X}",
        song.bpm, song.edit_step, song.current_phrase
    )
    .unwrap();
    out.push('\n');

    for (pi, phrase) in song.phrases.iter().enumerate() {
        writeln!(out, "@phrase {:02X}", pi).unwrap();
        writeln!(out, "  # step   PU1        PU2        TRI        NOI").unwrap();
        for (s, row) in phrase.cells.iter().enumerate() {
            let cells: Vec<String> = row.iter().map(|c| encode_cell(*c)).collect();
            writeln!(
                out,
                "  {:02X}       {:<9}  {:<9}  {:<9}  {:<9}",
                s, cells[0], cells[1], cells[2], cells[3]
            )
            .unwrap();
        }
        out.push('\n');
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

pub fn from_vip(text: &str) -> Result<Song> {
    let mut song = Song::default();
    song.phrases.clear();
    let mut current_phrase: Option<usize> = None;

    for (line_no, raw) in text.lines().enumerate() {
        let line_num = line_no + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix('@') {
            let (dir, args) = split_once_ws(rest);
            match dir {
                "song" => parse_song(&mut song, args)
                    .with_context(|| format!("line {}: @song", line_num))?,
                "phrase" => {
                    let idx = parse_phrase_idx(args)
                        .with_context(|| format!("line {}: @phrase", line_num))?;
                    while song.phrases.len() <= idx {
                        song.phrases.push(Phrase::default());
                    }
                    current_phrase = Some(idx);
                }
                "instr" => parse_instr(&mut song, args)
                    .with_context(|| format!("line {}: @instr", line_num))?,
                _ => {} // unknown directive: skip silently
            }
        } else {
            let pi = current_phrase.ok_or_else(|| {
                anyhow!("line {}: data row appears before any @phrase directive", line_num)
            })?;
            parse_data_row(&mut song.phrases[pi], line)
                .with_context(|| format!("line {}", line_num))?;
        }
    }

    if song.phrases.is_empty() {
        song.phrases.push(Phrase::default());
    }
    if song.current_phrase >= song.phrases.len() {
        song.current_phrase = 0;
    }
    Ok(song)
}

fn strip_comment(s: &str) -> &str {
    match s.find('#') {
        Some(i) => &s[..i],
        None => s,
    }
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
            "steps" | "order" => {} // reserved / informational
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

pub fn load(path: &Path) -> Result<Song> {
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
        let back = from_vip(&text).unwrap();
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
        let back = from_vip(&text).unwrap();
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
        let song = from_vip(text).unwrap();
        assert_eq!(song.bpm, 123);
        assert_eq!(song.phrases[0].cells[0][0].note, Some(69));
    }
}
