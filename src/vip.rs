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
    Ok(Cell { note: Some(note), instr, vol, fx })
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
    writeln!(
        out,
        "@song  bpm={}  edit_step={}  current={:02X}",
        song.bpm, song.edit_step, song.current_phrase
    )
    .unwrap();
    out.push('\n');

    for (pi, phrase) in song.phrases.iter().enumerate() {
        writeln!(out, "@phrase {:02X}", pi).unwrap();
        writeln!(out, "  # step   PU1             PU2             TRI             NOI").unwrap();
        for (s, row) in phrase.cells.iter().enumerate() {
            let cells: Vec<String> = row.iter().map(|c| encode_cell(*c)).collect();
            writeln!(
                out,
                "  {:02X}       {:<14}  {:<14}  {:<14}  {:<14}",
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

pub fn from_vip(text: &str) -> Result<(Song, Vec<String>)> {
    let mut song = Song::default();
    song.phrases.clear();
    let mut current_phrase: Option<usize> = None;
    let mut warnings: Vec<String> = Vec::new();

    // Directives reserved in FORMAT.md but not yet implemented; parsing them
    // shouldn't error, but silently dropping them has burned us in hand-edited
    // files, so warn.
    const RESERVED: &[&str] = &[
        "chain", "scene", "bind", "sprite", "groove", "meta",
    ];

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
    Ok((song, warnings))
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
        let cell = Cell { note: Some(60), instr: 1, vol: 0x0F, fx: Some((b'A', 0x42)) };
        let s = encode_cell(cell);
        assert_eq!(s, "C-4:01:0F:A42");
        let back = decode_cell(&s).unwrap();
        assert_eq!(back.fx, Some((b'A', 0x42)));
        // No fx field → round-trip without trailing :
        let bare = Cell { note: Some(60), instr: 0, vol: 0, fx: None };
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
        let text = "@song bpm=120\n@phrase 00\n@meta author=alex\n@bogus foo=bar\n";
        let (_, warns) = from_vip(text).unwrap();
        assert_eq!(warns.len(), 2);
        assert!(warns[0].contains("@meta"));
        assert!(warns[1].contains("@bogus"));
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
