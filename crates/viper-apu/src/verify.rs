//! Stage 24: the "true emulation" receipt. Compare viper's register-write
//! log for an NSF against a dump from another emulator, frame by frame.
//!
//! viper's log is `frame addr value` per line: decimal frame (INIT = 0,
//! first PLAY = 1), 4-hex-digit address, 2-hex-digit value. Foreign dumps
//! are read loosely — any line carrying a frame number, a `$4000`–`$4017`
//! address and a byte, in that order, in decimal or hex with optional
//! `$` / `0x` prefixes — so NSFPlay, Mesen and FCEUX Lua dumps all fit
//! without a format flag.
//!
//! Two allowances keep the comparison about the *driver*, not the player
//! shell around it:
//!
//! - Frame numbering may differ by a constant; the offset is taken from
//!   the first PLAY frame on each side and reported.
//! - Writes on the INIT frame are compared as a set, not a sequence, and
//!   only the writes viper made must be present: NSF players clear the
//!   APU themselves before calling INIT, and that housekeeping is theirs.
//!
//! PLAY frames must match exactly, in order.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::host::RegWrite;

/// Parse viper's own `frame addr value` format, or any loose dump that
/// carries the same three fields in order. Lines that don't fit are
/// skipped so headers and comments don't need stripping.
pub fn parse_log(text: &str) -> Vec<RegWrite> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(w) = parse_line(line) {
            out.push(w);
        }
    }
    out
}

fn parse_line(line: &str) -> Option<RegWrite> {
    // Tokenize on anything that is not alphanumeric, keeping `$`/`0x`
    // prefixes attached so we can tell hex from decimal.
    let toks: Vec<&str> = line
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '$'))
        .filter(|t| !t.is_empty())
        .collect();
    let mut i = 0;
    // Frame: first decimal token (an optional leading word like `frame`
    // is skipped).
    let frame = loop {
        let t = *toks.get(i)?;
        i += 1;
        if let Ok(f) = t.parse::<u32>() {
            break f;
        }
        if !t.chars().all(|c| c.is_ascii_alphabetic()) {
            return None;
        }
    };
    // Address: a 4-hex-digit token in $4000..=$4017, optionally prefixed.
    let addr = loop {
        let t = *toks.get(i)?;
        i += 1;
        let h = strip_hex_prefix(t);
        if h.len() == 4 {
            if let Ok(a) = u16::from_str_radix(h, 16) {
                if (0x4000..=0x4017).contains(&a) {
                    break a;
                }
            }
        }
    };
    // Value: the next 1–2 hex-digit token.
    let t = *toks.get(i)?;
    let h = strip_hex_prefix(t);
    if h.is_empty() || h.len() > 2 {
        return None;
    }
    let value = u8::from_str_radix(h, 16).ok()?;
    Some(RegWrite { frame, addr, value })
}

fn strip_hex_prefix(t: &str) -> &str {
    t.strip_prefix('$')
        .or_else(|| t.strip_prefix("0x"))
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t)
}

/// Format a log in viper's canonical shape.
pub fn format_log(log: &[RegWrite]) -> String {
    let mut s = String::with_capacity(log.len() * 14);
    for w in log {
        writeln!(s, "{} {:04X} {:02X}", w.frame, w.addr, w.value).unwrap();
    }
    s
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// Frame in viper's numbering.
    pub frame: u32,
    pub expected: Vec<(u16, u8)>,
    pub got: Vec<(u16, u8)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Frames compared (PLAY frames present on both sides).
    pub frames_compared: u32,
    /// Constant added to the other emulator's frame numbers to line them
    /// up with viper's.
    pub frame_offset: i64,
    /// INIT-frame writes viper made that the other log lacks.
    pub init_missing: Vec<(u16, u8)>,
    /// First mismatching PLAY frame, if any.
    pub first_mismatch: Option<Mismatch>,
    /// Frames the shorter log stops before the longer one ends.
    pub truncated_at: Option<u32>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.first_mismatch.is_none() && self.init_missing.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut s = String::new();
        writeln!(s, "frame offset: {:+}", self.frame_offset).unwrap();
        writeln!(s, "frames compared: {}", self.frames_compared).unwrap();
        if !self.init_missing.is_empty() {
            writeln!(s, "INIT writes missing from the other log:").unwrap();
            for (a, v) in &self.init_missing {
                writeln!(s, "  {:04X} {:02X}", a, v).unwrap();
            }
        }
        match &self.first_mismatch {
            Some(m) => {
                writeln!(s, "first mismatch at frame {}:", m.frame).unwrap();
                writeln!(s, "  viper:  {}", fmt_writes(&m.expected)).unwrap();
                writeln!(s, "  other:  {}", fmt_writes(&m.got)).unwrap();
            }
            None => writeln!(s, "PLAY frames match").unwrap(),
        }
        if let Some(t) = self.truncated_at {
            writeln!(s, "(one log ends at frame {}; later frames not compared)", t).unwrap();
        }
        s
    }
}

fn fmt_writes(ws: &[(u16, u8)]) -> String {
    if ws.is_empty() {
        return "(no writes)".into();
    }
    ws.iter().map(|(a, v)| format!("{:04X}={:02X}", a, v)).collect::<Vec<_>>().join(" ")
}

fn by_frame(log: &[RegWrite]) -> BTreeMap<u32, Vec<(u16, u8)>> {
    let mut m: BTreeMap<u32, Vec<(u16, u8)>> = BTreeMap::new();
    for w in log {
        m.entry(w.frame).or_default().push((w.addr, w.value));
    }
    m
}

/// Compare viper's log (`ours`) against another emulator's (`theirs`).
pub fn compare(ours: &[RegWrite], theirs: &[RegWrite]) -> Report {
    let a = by_frame(ours);
    let b = by_frame(theirs);
    let empty = Vec::new();

    // INIT is the first frame on each side.
    let our_init = a.keys().next().copied().unwrap_or(0);
    let their_init = b.keys().next().copied().unwrap_or(0);
    let ours_init = a.get(&our_init).unwrap_or(&empty);
    let p1 = a.get(&(our_init + 1)).unwrap_or(&empty);
    let theirs_init_all = b.get(&their_init).unwrap_or(&empty);

    // Line the two numberings up. Prefer a frame after their INIT whose
    // writes equal our first PLAY frame; failing that, detect a player
    // that runs INIT and the first PLAY inside one frame (FCEUX does),
    // where our PLAY 1 is the tail of their INIT frame.
    // Only frames close to their INIT are candidates for PLAY 1: a song's
    // loop restarts with the same writes as PLAY 1, and a divergence on
    // PLAY 1 itself must be reported there, not hidden by re-aligning to
    // the loop point.
    let their_p1_frame = b
        .range(their_init + 1..)
        .take(8)
        .find(|(_, ws)| !p1.is_empty() && ws.as_slice() == p1.as_slice())
        .map(|(&f, _)| f);
    let merged_init = their_p1_frame.is_none()
        && !p1.is_empty()
        && theirs_init_all.len() > p1.len()
        && theirs_init_all.ends_with(p1);
    let (frame_offset, theirs_init, mut remapped): (i64, Vec<(u16, u8)>, BTreeMap<i64, &[(u16, u8)]>) = if merged_init {
        let split = theirs_init_all.len() - p1.len();
        let mut m: BTreeMap<i64, &[(u16, u8)]> = BTreeMap::new();
        m.insert(our_init as i64 + 1, &theirs_init_all[split..]);
        let offset = (our_init as i64 + 1) - their_init as i64;
        for (&f, ws) in b.range(their_init + 1..) {
            m.insert(f as i64 + offset, ws.as_slice());
        }
        (offset, theirs_init_all[..split].to_vec(), m)
    } else {
        let their_play = their_p1_frame.or_else(|| b.keys().find(|&&f| f > their_init).copied());
        let offset = match their_play {
            Some(t) if !p1.is_empty() => (our_init as i64 + 1) - t as i64,
            _ => our_init as i64 - their_init as i64,
        };
        let mut m: BTreeMap<i64, &[(u16, u8)]> = BTreeMap::new();
        for (&f, ws) in b.range(their_init + 1..) {
            m.insert(f as i64 + offset, ws.as_slice());
        }
        (offset, theirs_init_all.clone(), m)
    };

    // INIT: every write viper made must appear; the player's own APU
    // housekeeping around them is allowed.
    let mut init_missing = Vec::new();
    let mut pool = theirs_init;
    for w in ours_init {
        match pool.iter().position(|x| x == w) {
            Some(i) => { pool.remove(i); }
            None => init_missing.push(*w),
        }
    }

    let mut frames_compared = 0;
    let mut first_mismatch = None;
    let our_last = a.keys().next_back().copied().unwrap_or(0) as i64;
    let their_last = remapped.keys().next_back().copied().unwrap_or(our_init as i64);
    let last = our_last.min(their_last);
    for f in (our_init as i64 + 1)..=last {
        let ours_f = a.get(&(f as u32)).unwrap_or(&empty);
        let theirs_f = remapped.remove(&f).unwrap_or(&[]);
        frames_compared += 1;
        if ours_f.as_slice() != theirs_f {
            first_mismatch = Some(Mismatch { frame: f as u32, expected: ours_f.clone(), got: theirs_f.to_vec() });
            break;
        }
    }
    let truncated_at = if our_last != their_last { Some(last.max(0) as u32) } else { None };
    Report { frames_compared, frame_offset, init_missing, first_mismatch, truncated_at }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(frame: u32, addr: u16, value: u8) -> RegWrite {
        RegWrite { frame, addr, value }
    }

    #[test]
    fn parses_viper_and_loose_formats() {
        let text = "0 4015 0F\n1 4003 A8\n# comment\nframe 2: $4000 <- 0x3F\n[3] write 0x4002 = 7\nnot a write\n";
        let log = parse_log(text);
        assert_eq!(log, vec![w(0, 0x4015, 0x0F), w(1, 0x4003, 0xA8), w(2, 0x4000, 0x3F), w(3, 0x4002, 0x07)]);
        assert_eq!(format_log(&log[..2]), "0 4015 0F\n1 4003 A8\n");
    }

    #[test]
    fn identical_logs_match() {
        let ours = vec![w(0, 0x4015, 0x0F), w(1, 0x4000, 0x3F), w(1, 0x4002, 0x10), w(2, 0x4003, 0x01)];
        let r = compare(&ours, &ours);
        assert!(r.ok(), "{}", r.summary());
        assert_eq!(r.frames_compared, 2);
        assert_eq!(r.frame_offset, 0);
    }

    #[test]
    fn frame_offset_and_player_init_housekeeping_are_tolerated() {
        let ours = vec![w(0, 0x4015, 0x0F), w(1, 0x4000, 0x3F), w(2, 0x4003, 0x01)];
        // The other player numbers frames from 5, clears the APU itself
        // before INIT, and reorders INIT's writes.
        let theirs = vec![
            w(5, 0x4017, 0x40), w(5, 0x4015, 0x00), w(5, 0x4015, 0x0F),
            w(6, 0x4000, 0x3F), w(7, 0x4003, 0x01),
        ];
        let r = compare(&ours, &theirs);
        assert!(r.ok(), "{}", r.summary());
        assert_eq!(r.frame_offset, -5);
    }

    #[test]
    fn a_divergent_play_frame_is_reported_with_both_sides() {
        let ours = vec![w(0, 0x4015, 0x0F), w(1, 0x4000, 0x3F), w(2, 0x4003, 0x01), w(3, 0x4002, 0x22)];
        let theirs = vec![w(0, 0x4015, 0x0F), w(1, 0x4000, 0x3F), w(2, 0x4003, 0x02), w(3, 0x4002, 0x22)];
        let r = compare(&ours, &theirs);
        let m = r.first_mismatch.clone().expect("mismatch");
        assert_eq!(m.frame, 2);
        assert_eq!(m.expected, vec![(0x4003, 0x01)]);
        assert_eq!(m.got, vec![(0x4003, 0x02)]);
        assert!(r.summary().contains("first mismatch at frame 2"));
    }

    #[test]
    fn init_and_first_play_sharing_a_frame_are_split() {
        // FCEUX runs INIT and the first PLAY in the same frame.
        let ours = vec![w(0, 0x4015, 0x0F), w(0, 0x4001, 0x08), w(1, 0x4000, 0x3F), w(1, 0x4002, 0x10), w(2, 0x4003, 0x01)];
        let theirs = vec![
            w(3, 0x4015, 0x0F), w(3, 0x4001, 0x08), w(3, 0x4017, 0x40), w(3, 0x4000, 0x3F), w(3, 0x4002, 0x10),
            w(4, 0x4003, 0x01),
        ];
        let r = compare(&ours, &theirs);
        assert!(r.ok(), "{}", r.summary());
        assert_eq!(r.frames_compared, 2);
        assert_eq!(r.frame_offset, -2);
    }

    #[test]
    fn missing_init_write_fails() {
        let ours = vec![w(0, 0x4015, 0x0F), w(0, 0x4001, 0x08), w(1, 0x4000, 0x3F)];
        let theirs = vec![w(0, 0x4015, 0x0F), w(1, 0x4000, 0x3F)];
        let r = compare(&ours, &theirs);
        assert_eq!(r.init_missing, vec![(0x4001, 0x08)]);
        assert!(!r.ok());
    }

    #[test]
    fn shorter_log_is_compared_up_to_its_end() {
        let ours = vec![w(0, 0x4015, 0x0F), w(1, 0x4000, 0x3F), w(2, 0x4003, 0x01), w(9, 0x4002, 0x22)];
        let theirs = vec![w(0, 0x4015, 0x0F), w(1, 0x4000, 0x3F), w(2, 0x4003, 0x01)];
        let r = compare(&ours, &theirs);
        assert!(r.first_mismatch.is_none());
        assert_eq!(r.truncated_at, Some(2));
    }
}
