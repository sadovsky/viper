//! viper — a vim-keybound chiptune step sequencer
//!
//! Stage-1: data model, modal input, phrase editor UI.
//! Stage-2: cpal audio thread producing sound from the edited phrase.

mod audio;

use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};

// ---------- Data model ----------

pub(crate) const STEPS_PER_PHRASE: usize = 16;
pub(crate) const CHANNELS: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cell {
    /// MIDI note number; None = empty ("---").
    pub note: Option<u8>,
    /// Instrument index.
    pub instr: u8,
    /// Volume 0..=15.
    pub vol: u8,
    /// Effect column: (cmd, param). None = no effect.
    pub fx: Option<(u8, u8)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Phrase {
    pub cells: [[Cell; CHANNELS]; STEPS_PER_PHRASE],
}

impl Default for Phrase {
    fn default() -> Self {
        Self { cells: [[Cell::default(); CHANNELS]; STEPS_PER_PHRASE] }
    }
}

pub(crate) const INSTRUMENTS: usize = 16;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct Instrument {
    /// Attack time in ms (0 = instant).
    pub attack_ms: u16,
    /// Decay time in ms (from peak down to sustain).
    pub decay_ms: u16,
    /// Sustain level 0..=1.
    pub sustain: f32,
    /// Release time in ms (from sustain down to 0).
    pub release_ms: u16,
    /// Pulse duty cycle 0..=1 (used by PU1/PU2).
    pub duty: f32,
    /// Instrument-level volume 0..=1.
    pub volume: f32,
}

impl Default for Instrument {
    fn default() -> Self {
        Self {
            attack_ms: 2,
            decay_ms: 60,
            sustain: 0.7,
            release_ms: 120,
            duty: 0.5,
            volume: 1.0,
        }
    }
}

pub(crate) const INSTR_PARAM_NAMES: [&str; 6] =
    ["attack", "decay", "sustain", "release", "duty", "volume"];

impl Instrument {
    fn adjust(&mut self, param: usize, delta: i32) {
        let d = delta as f32;
        match param {
            0 => self.attack_ms  = (self.attack_ms  as i32 + delta * 2).clamp(0, 5000) as u16,
            1 => self.decay_ms   = (self.decay_ms   as i32 + delta * 5).clamp(0, 5000) as u16,
            2 => self.sustain    = (self.sustain + d * 0.05).clamp(0.0, 1.0),
            3 => self.release_ms = (self.release_ms as i32 + delta * 10).clamp(0, 10000) as u16,
            4 => self.duty       = (self.duty + d * 0.05).clamp(0.05, 0.95),
            5 => self.volume     = (self.volume + d * 0.05).clamp(0.0, 1.0),
            _ => {}
        }
    }

    fn display(&self, param: usize) -> String {
        match param {
            0 => format!("{} ms", self.attack_ms),
            1 => format!("{} ms", self.decay_ms),
            2 => format!("{:.2}", self.sustain),
            3 => format!("{} ms", self.release_ms),
            4 => format!("{:.2}", self.duty),
            5 => format!("{:.2}", self.volume),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Song {
    bpm: u16,
    /// How far to advance the cursor after inserting a note in insert mode.
    edit_step: usize,
    phrases: Vec<Phrase>,
    /// One phrase loaded at a time for now.
    current_phrase: usize,
    pub instruments: [Instrument; INSTRUMENTS],
}

impl Default for Song {
    fn default() -> Self {
        Self {
            bpm: 140,
            edit_step: 1,
            phrases: vec![Phrase::default()],
            current_phrase: 0,
            instruments: [Instrument::default(); INSTRUMENTS],
        }
    }
}

impl Song {
    /// Default startup song: a 16-step Am–F–G–Am loop ("i–VI–VII–i"), a
    /// progression you'll recognize from plenty of NES-era soundtracks. 140 BPM,
    /// one chord per bar, with a lead on PU1, arp on PU2, bass on TRI, and a
    /// simple kick/snare/hat on NOI.
    fn demo() -> Self {
        let mut song = Song::default();
        song.bpm = 140;
        song.edit_step = 1;

        // Instrument 00 — lead pulse: medium attack, punchy.
        song.instruments[0] = Instrument {
            attack_ms: 2, decay_ms: 80, sustain: 0.6,
            release_ms: 150, duty: 0.5, volume: 0.7,
        };
        // Instrument 01 — thinner arp pulse, narrower duty.
        song.instruments[1] = Instrument {
            attack_ms: 2, decay_ms: 60, sustain: 0.3,
            release_ms: 80, duty: 0.25, volume: 0.5,
        };
        // Instrument 02 — round triangle bass, long sustain.
        song.instruments[2] = Instrument {
            attack_ms: 2, decay_ms: 40, sustain: 0.9,
            release_ms: 200, duty: 0.5, volume: 0.9,
        };
        // Instrument 03 — percussive click for the noise channel.
        song.instruments[3] = Instrument {
            attack_ms: 0, decay_ms: 60, sustain: 0.0,
            release_ms: 20, duty: 0.5, volume: 0.7,
        };

        // Helper: write (note, instrument) into (step, channel).
        let put = |song: &mut Song, step: usize, ch: usize, note: u8, instr: u8| {
            let cell = &mut song.phrases[0].cells[step][ch];
            cell.note = Some(note);
            cell.instr = instr;
            cell.vol = 15;
        };

        // Lead melody (PU1, ch0) — ascending then descending over the turnaround.
        const PU1: usize = 0;
        const PU2: usize = 1;
        const TRI: usize = 2;
        const NOI: usize = 3;
        //       step note
        put(&mut song,  0, PU1, 81, 0); // A5
        put(&mut song,  2, PU1, 72, 0); // C5
        put(&mut song,  3, PU1, 76, 0); // E5
        put(&mut song,  4, PU1, 77, 0); // F5
        put(&mut song,  6, PU1, 69, 0); // A4
        put(&mut song,  7, PU1, 72, 0); // C5
        put(&mut song,  8, PU1, 67, 0); // G4
        put(&mut song, 10, PU1, 71, 0); // B4
        put(&mut song, 11, PU1, 74, 0); // D5
        put(&mut song, 12, PU1, 81, 0); // A5
        put(&mut song, 13, PU1, 79, 0); // G5
        put(&mut song, 14, PU1, 76, 0); // E5
        put(&mut song, 15, PU1, 72, 0); // C5

        // Arpeggio (PU2, ch1) — every step outlines the current chord.
        let arp = [
            57, 64, 69, 72, // Am: A3 E4 A4 C5
            53, 60, 65, 69, // F:  F3 C4 F4 A4
            55, 62, 67, 71, // G:  G3 D4 G4 B4
            57, 64, 69, 64, // Am: A3 E4 A4 E4
        ];
        for (s, n) in arp.iter().enumerate() {
            put(&mut song, s, PU2, *n, 1);
        }

        // Triangle bass (TRI, ch2) — root note on beats 1 and 3 of each bar.
        let bass = [
            (0, 45), (2, 45),   // Am
            (4, 41), (6, 41),   // F
            (8, 43), (10, 43),  // G
            (12, 45), (14, 45), // Am
        ];
        for (s, n) in bass {
            put(&mut song, s, TRI, n, 2);
        }

        // Drums (NOI, ch3) — kick-hat-snare-hat per bar. The noise generator
        // ignores pitch, so these numbers just need to be non-None to retrigger.
        for bar in 0..4 {
            let base = bar * 4;
            put(&mut song, base,     NOI, 36, 3); // kick
            put(&mut song, base + 1, NOI, 60, 3); // hat
            put(&mut song, base + 2, NOI, 50, 3); // snare
            put(&mut song, base + 3, NOI, 60, 3); // hat
        }

        song
    }

    fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .context("serializing song")?;
        std::fs::write(path, json)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let song: Song = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(song)
    }
}

// ---------- Modal input ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
    Visual,
    Command,
    Help,
    Instrument,
}

/// Pending multi-key sequence in normal mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    None,
    Z,                        // pressed `Z`, waiting for second `Z` to quit
    Op(char),                 // pressed `d` or `y`, waiting for motion / object prefix
    OpScope(char, char),      // pressed `da`, `di`, `ya`, or `yi`, waiting for object letter
}

impl Pending {
    fn display(&self) -> String {
        match self {
            Pending::None => String::new(),
            Pending::Z => "Z".into(),
            Pending::Op(c) => c.to_string(),
            Pending::OpScope(a, b) => format!("{}{}", a, b),
        }
    }
}

/// Clipboard from yank / delete. `rows[i][j]` is the j-th channel of row i.
#[derive(Clone, Debug, Default)]
struct Register {
    rows: Vec<Vec<Cell>>,
    /// True if the register holds a column-sized slice (width 1); false = full row width.
    is_column: bool,
}

/// Recorded destructive action, replayable via `.`.
#[derive(Clone, Copy, Debug)]
enum LastAction {
    DeleteRow,
    DeleteBar,
    DeletePhrase,
    DeleteChannel,
}

struct App {
    song: Song,
    mode: Mode,
    cursor_step: usize,
    cursor_ch: usize,
    /// Pending multi-key sequence (Z chord, operator, operator+scope).
    pending: Pending,
    /// Pending count prefix (e.g. `4j`).
    count: u32,
    command_buf: String,
    status: String,
    playing: bool,
    /// Current playhead step, mirrored from the audio engine.
    play_step: usize,
    /// Which instrument new notes are tagged with and the editor is viewing.
    selected_instr: u8,
    /// Cursor row in the instrument editor (0..NUM_INSTR_PARAMS).
    instr_param: usize,
    /// True until the user presses a key on the splash screen.
    show_splash: bool,
    /// Unnamed register holding the last yank/delete contents.
    register: Register,
    /// Last destructive action, replayable via `.`.
    last_action: Option<LastAction>,
    /// Path of the currently-loaded song file, if any.
    current_file: Option<PathBuf>,
    quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            song: Song::demo(),
            mode: Mode::Normal,
            cursor_step: 0,
            cursor_ch: 0,
            pending: Pending::None,
            count: 0,
            command_buf: String::new(),
            status: "welcome — demo loaded; press space to play, ? for help".into(),
            playing: false,
            play_step: 0,
            selected_instr: 0,
            instr_param: 0,
            show_splash: true,
            register: Register::default(),
            last_action: None,
            current_file: None,
            quit: false,
        }
    }

    fn take_count(&mut self) -> u32 {
        let n = if self.count == 0 { 1 } else { self.count };
        self.count = 0;
        n
    }

    fn phrase_mut(&mut self) -> &mut Phrase {
        &mut self.song.phrases[self.song.current_phrase]
    }

    fn phrase(&self) -> &Phrase {
        &self.song.phrases[self.song.current_phrase]
    }

    // ---------- Motions ----------

    fn motion_j(&mut self, n: u32) {
        self.cursor_step = (self.cursor_step + n as usize).min(STEPS_PER_PHRASE - 1);
    }
    fn motion_k(&mut self, n: u32) {
        self.cursor_step = self.cursor_step.saturating_sub(n as usize);
    }
    fn motion_h(&mut self, n: u32) {
        self.cursor_ch = self.cursor_ch.saturating_sub(n as usize);
    }
    fn motion_l(&mut self, n: u32) {
        self.cursor_ch = (self.cursor_ch + n as usize).min(CHANNELS - 1);
    }

    // ---------- Operators ----------

    fn op_delete_cell(&mut self) {
        let (s, c) = (self.cursor_step, self.cursor_ch);
        self.phrase_mut().cells[s][c] = Cell::default();
        self.status = format!("deleted [{:02X},ch{}]", s, c + 1);
    }

    // ---------- Yank / delete / paste ----------

    fn yank_range(&mut self, steps: Range<usize>, chs: Range<usize>, is_column: bool) {
        let mut rows = Vec::with_capacity(steps.len());
        for s in steps.clone() {
            let mut row = Vec::with_capacity(chs.len());
            for c in chs.clone() {
                row.push(self.phrase().cells[s][c]);
            }
            rows.push(row);
        }
        self.register = Register { rows, is_column };
    }

    fn clear_range(&mut self, steps: Range<usize>, chs: Range<usize>) {
        for s in steps {
            for c in chs.clone() {
                self.phrase_mut().cells[s][c] = Cell::default();
            }
        }
    }

    fn op_row(&mut self, op: char) {
        let steps = self.cursor_step..self.cursor_step + 1;
        let chs = 0..CHANNELS;
        self.yank_range(steps.clone(), chs.clone(), false);
        if op == 'd' {
            self.clear_range(steps, chs);
            self.last_action = Some(LastAction::DeleteRow);
            self.status = "deleted row".into();
        } else {
            self.status = "yanked row".into();
        }
    }

    fn op_object(&mut self, op: char, scope: char, obj: char) {
        let (steps, chs, is_column, action) = match obj {
            'b' => {
                let bar = self.cursor_step / 4;
                (bar * 4..bar * 4 + 4, 0..CHANNELS, false, LastAction::DeleteBar)
            }
            'p' => (0..STEPS_PER_PHRASE, 0..CHANNELS, false, LastAction::DeletePhrase),
            'v' => (
                0..STEPS_PER_PHRASE,
                self.cursor_ch..self.cursor_ch + 1,
                true,
                LastAction::DeleteChannel,
            ),
            _ => {
                self.status = format!("unknown text object: {}{}", scope, obj);
                return;
            }
        };
        self.yank_range(steps.clone(), chs.clone(), is_column);
        if op == 'd' {
            self.clear_range(steps, chs);
            self.last_action = Some(action);
            self.status = format!("deleted {}{}", scope, obj);
        } else {
            self.status = format!("yanked {}{}", scope, obj);
        }
    }

    fn paste(&mut self, after: bool) {
        if self.register.rows.is_empty() {
            self.status = "register empty".into();
            return;
        }
        let start_step = if after {
            (self.cursor_step + 1).min(STEPS_PER_PHRASE)
        } else {
            self.cursor_step
        };
        let start_ch = if self.register.is_column {
            self.cursor_ch
        } else {
            0
        };
        let rows = self.register.rows.clone();
        let n_rows = rows.len();
        for (i, row) in rows.iter().enumerate() {
            let s = start_step + i;
            if s >= STEPS_PER_PHRASE {
                break;
            }
            for (j, cell) in row.iter().enumerate() {
                let c = start_ch + j;
                if c >= CHANNELS {
                    break;
                }
                self.phrase_mut().cells[s][c] = *cell;
            }
        }
        self.status = format!("pasted {} row(s)", n_rows);
    }

    fn replay_last_action(&mut self) {
        let Some(action) = self.last_action else {
            self.status = "nothing to repeat".into();
            return;
        };
        match action {
            LastAction::DeleteRow => self.op_row('d'),
            LastAction::DeleteBar => self.op_object('d', 'a', 'b'),
            LastAction::DeletePhrase => self.op_object('d', 'i', 'p'),
            LastAction::DeleteChannel => self.op_object('d', 'i', 'v'),
        }
    }

    // ---------- Insert-mode piano row ----------

    /// Map the bottom keyboard row to a chromatic octave (z = C, s = C#, x = D, ...).
    fn piano_row_note(key: char, octave: u8) -> Option<u8> {
        // MIDI note = 12 * (octave + 1) + semitone.
        let semi = match key {
            'z' => 0, 's' => 1, 'x' => 2, 'd' => 3, 'c' => 4,
            'v' => 5, 'g' => 6, 'b' => 7, 'h' => 8, 'n' => 9,
            'j' => 10, 'm' => 11,
            ',' => 12, 'l' => 13, '.' => 14, ';' => 15, '/' => 16,
            _ => return None,
        };
        Some(12 * (octave + 1) + semi)
    }

    fn insert_note(&mut self, note: u8) {
        let (s, c) = (self.cursor_step, self.cursor_ch);
        let instr = self.selected_instr;
        let cell = &mut self.phrase_mut().cells[s][c];
        cell.note = Some(note);
        cell.instr = instr;
        // Auto-advance by edit_step (1 = classic tracker, 4 = one note per beat).
        let step = self.song.edit_step.max(1);
        self.cursor_step = (self.cursor_step + step).min(STEPS_PER_PHRASE - 1);
    }
}

// ---------- Rendering ----------

fn note_name(n: Option<u8>) -> String {
    match n {
        None => "---".into(),
        Some(midi) => {
            const NAMES: [&str; 12] = ["C-", "C#", "D-", "D#", "E-", "F-",
                                       "F#", "G-", "G#", "A-", "A#", "B-"];
            let pc = (midi % 12) as usize;
            let oct = (midi / 12) as i32 - 1;
            format!("{}{}", NAMES[pc], oct)
        }
    }
}

fn render_phrase(f: &mut Frame, area: Rect, app: &App) {
    let p = app.phrase();
    let mut lines: Vec<Line> = Vec::with_capacity(STEPS_PER_PHRASE + 2);

    // Header
    let mut header = vec![Span::raw("     ")];
    for ch in 0..CHANNELS {
        let label = match ch { 0 => "PU1", 1 => "PU2", 2 => "TRI", 3 => "NOI", _ => "???" };
        header.push(Span::styled(format!(" {}  ", label),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    }
    lines.push(Line::from(header));
    lines.push(Line::from(""));

    for (i, row) in p.cells.iter().enumerate() {
        let is_playhead = app.playing && i == app.play_step;
        let row_bg = if is_playhead { Some(Color::Rgb(60, 20, 20)) } else { None };
        let label_style = if is_playhead {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut spans = vec![Span::styled(format!(" {:02X}  ", i), label_style)];
        for (c, cell) in row.iter().enumerate() {
            let text = note_name(cell.note);
            let mut style = Style::default();
            if cell.note.is_some() {
                style = style.fg(Color::Green);
            } else {
                style = style.fg(Color::DarkGray);
            }
            if let Some(bg) = row_bg {
                style = style.bg(bg);
            }
            if i == app.cursor_step && c == app.cursor_ch {
                style = style.bg(Color::Rgb(40, 40, 80)).add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(format!(" {} ", text), style));
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    }

    let title = format!(" PHRASE {:02X}   {} BPM   {} ",
        app.song.current_phrase,
        app.song.bpm,
        if app.playing { "● PLAY" } else { "■ STOP" });
    let block = Block::default().title(title).borders(Borders::ALL);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_help(f: &mut Frame, area: Rect) {
    let section = |title: &str| Line::from(Span::styled(
        title.to_string(),
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    ));
    let row = |keys: &str, desc: &str| Line::from(vec![
        Span::styled(format!("  {:<14}", keys), Style::default().fg(Color::Cyan)),
        Span::raw(desc.to_string()),
    ]);

    let lines = vec![
        section("Normal mode"),
        row("h j k l / ←↓↑→", "move cursor (prefix count, e.g. 4j)"),
        row("w / b",          "next / prev bar (4 steps)"),
        row("0 / $",           "start / end of phrase"),
        row("g / G",           "top / bottom of phrase"),
        row("x",               "clear cell"),
        row("dd / yy",         "delete / yank current step row"),
        row("dab / yab",       "delete / yank current bar (4 steps)"),
        row("dip / yip",       "delete / yank whole phrase"),
        row("div / yiv",       "delete / yank current channel column"),
        row("p / P",           "paste after / at cursor"),
        row(".",               "repeat last delete"),
        row("i",               "insert mode"),
        row("a",               "append (move down, then insert)"),
        row(":",               "command mode"),
        row("space",           "toggle play"),
        row("? / F1",          "toggle this help"),
        row("F2 / :inst",      "instrument editor"),
        row("ZZ / Ctrl-q",     "quit"),
        Line::from(""),
        section("Insert mode — bottom row = chromatic octave 4"),
        row("z s x d c v",     "C  C# D  D# E  F"),
        row("g b h n j m",     "F# G  G# A  A# B"),
        row(", l . ; /",       "continue into next octave"),
        row("Backspace",       "clear cell and move up"),
        row("Esc",             "back to normal"),
        Line::from(""),
        section("Command mode"),
        row(":q / :q!",        "quit"),
        row(":help",           "open this help"),
        row(":inst [NN]",      "instrument editor (hex index 00-0F)"),
        row(":set bpm=140",    "set tempo"),
        row(":set step=4",     "auto-advance N steps per inserted note"),
        row(":play / :stop",   "transport"),
        row(":w [path]",       "save song to JSON (path required first time)"),
        row(":e <path>",       "load song from JSON"),
        row(":wq [path]",      "save and quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  press q, Esc, or ? to close help",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )),
    ];

    let block = Block::default()
        .title(" HELP ")
        .borders(Borders::ALL);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_splash(f: &mut Frame, area: Rect) {
    // Keep each line exactly 80 cells wide so the right border lines up.
    const ART: &[&str] = &[
        "╔══════════════════════════════════════════════════════════════════════════════╗",
        "║                                                                              ║",
        "║    ██╗   ██╗██╗██████╗ ███████╗██████╗                                       ║",
        "║    ██║   ██║██║██╔══██╗██╔════╝██╔══██╗       .~\"~.~\"~.                      ║",
        "║    ██║   ██║██║██████╔╝█████╗  ██████╔╝      /  ___   \\                      ║",
        "║    ╚██╗ ██╔╝██║██╔═══╝ ██╔══╝  ██╔══██╗     | (o o)   |                      ║",
        "║     ╚████╔╝ ██║██║     ███████╗██║  ██║      \\  >    /                       ║",
        "║      ╚═══╝  ╚═╝╚═╝     ╚══════╝╚═╝  ╚═╝       `-._.-'~\\_                     ║",
        "║                                                  \\__    \\_                   ║",
        "║     ┌──┐    ┌──┐    ┌──┐    ┌──┐    ┌──┐    ┌──┐   \\__   \\                   ║",
        "║     │  │    │  │    │  │    │  │    │  │    │  │      \\   )                  ║",
        "║  ───┘  └────┘  └────┘  └────┘  └────┘  └────┘  └──────_/__/                  ║",
        "║                                                                              ║",
        "║                    ── vi keybinding audio stepper ──                         ║",
        "║                                                                              ║",
        "╚══════════════════════════════════════════════════════════════════════════════╝",
    ];

    let cyan = Style::default().fg(Color::Cyan);
    let cyan_bold = cyan.add_modifier(Modifier::BOLD);
    let green = Style::default().fg(Color::Green);
    let blue = Style::default().fg(Color::LightBlue);
    let dim = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);

    let styled: Vec<Line> = ART
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let style = match i {
                0 | 15 => cyan_bold,               // top / bottom border
                2..=7 => cyan_bold,                // VIPER logo rows (with snake accents)
                8 => green,                        // snake-only row
                9..=11 => blue,                    // keyboard row
                13 => dim,                         // tagline
                _ => cyan,                         // blank border rows
            };
            Line::from(Span::styled((*s).to_string(), style))
        })
        .collect();

    let mut lines = styled;
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "                            press any key to begin",
        dim,
    )));

    let text_h = lines.len() as u16;
    let text_w = 80u16;
    let vpad = area.height.saturating_sub(text_h) / 2;
    let hpad = area.width.saturating_sub(text_w) / 2;
    let inner = Rect {
        x: area.x + hpad,
        y: area.y + vpad,
        width: text_w.min(area.width),
        height: text_h.min(area.height.saturating_sub(vpad)),
    };
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_instrument(f: &mut Frame, area: Rect, app: &App) {
    let idx = app.selected_instr as usize;
    let inst = app.song.instruments[idx];

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        format!("  INSTRUMENT {:02X}", idx),
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(""));

    for (i, name) in INSTR_PARAM_NAMES.iter().enumerate() {
        let sel = i == app.instr_param;
        let marker = if sel { ">" } else { " " };
        let style = if sel {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::raw(format!("  {} ", marker)),
            Span::styled(format!("{:<8}", name), style),
            Span::raw("  "),
            Span::styled(inst.display(i), Style::default().fg(Color::Green)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  j/k select · h/l or -/+ adjust · [ ] prev/next instr · Esc back",
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
    )));

    let block = Block::default()
        .title(format!(" INSTRUMENT EDITOR  (current: {:02X}) ", idx))
        .borders(Borders::ALL);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let mode_str = match app.mode {
        Mode::Normal => "NORMAL",
        Mode::Insert => "INSERT",
        Mode::Visual => "VISUAL",
        Mode::Command => "COMMAND",
        Mode::Help => "HELP",
        Mode::Instrument => "INSTR",
    };
    let mode_color = match app.mode {
        Mode::Normal => Color::Cyan,
        Mode::Insert => Color::Green,
        Mode::Visual => Color::Magenta,
        Mode::Command => Color::Yellow,
        Mode::Help => Color::Blue,
        Mode::Instrument => Color::LightRed,
    };

    let left = Line::from(vec![
        Span::styled(format!(" {} ", mode_str),
            Style::default().bg(mode_color).fg(Color::Black).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::raw(&app.status),
    ]);
    let right = if app.mode == Mode::Command {
        format!(":{}", app.command_buf)
    } else if app.count > 0 || app.pending != Pending::None {
        format!("{}{}",
            if app.count > 0 { app.count.to_string() } else { String::new() },
            app.pending.display())
    } else {
        String::new()
    };
    let content = vec![left, Line::from(right)];
    f.render_widget(Paragraph::new(content), area);
}

fn ui(f: &mut Frame, app: &App) {
    if app.show_splash {
        render_splash(f, f.area());
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(2)])
        .split(f.area());
    match app.mode {
        Mode::Help => render_help(f, chunks[0]),
        Mode::Instrument => render_instrument(f, chunks[0], app),
        _ => render_phrase(f, chunks[0], app),
    }
    render_status(f, chunks[1], app);
}

// ---------- Input handling ----------

fn handle_key(app: &mut App, key: KeyEvent) {
    if app.show_splash {
        app.show_splash = false;
        return;
    }
    match app.mode {
        Mode::Normal => handle_normal(app, key),
        Mode::Insert => handle_insert(app, key),
        Mode::Command => handle_command(app, key),
        Mode::Visual => handle_normal(app, key), // TODO: real visual mode
        Mode::Help => handle_help(app, key),
        Mode::Instrument => handle_instrument(app, key),
    }
}

fn handle_instrument(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
            app.status = "".into();
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.instr_param = (app.instr_param + 1) % INSTR_PARAM_NAMES.len();
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.instr_param = (app.instr_param + INSTR_PARAM_NAMES.len() - 1)
                % INSTR_PARAM_NAMES.len();
        }
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('-') => {
            let p = app.instr_param;
            app.song.instruments[app.selected_instr as usize].adjust(p, -1);
        }
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Char('+')
            | KeyCode::Char('=') => {
            let p = app.instr_param;
            app.song.instruments[app.selected_instr as usize].adjust(p, 1);
        }
        KeyCode::Char('[') => {
            app.selected_instr = app.selected_instr.saturating_sub(1);
        }
        KeyCode::Char(']') => {
            app.selected_instr = (app.selected_instr + 1).min((INSTRUMENTS - 1) as u8);
        }
        _ => {}
    }
}

fn handle_help(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::F(1) => {
            app.mode = Mode::Normal;
            app.status = "".into();
        }
        _ => {}
    }
}

fn handle_normal(app: &mut App, key: KeyEvent) {
    // Resolve any in-progress multi-key sequence first.
    match app.pending {
        Pending::Op(op) => {
            handle_pending_op(app, op, key);
            return;
        }
        Pending::OpScope(op, scope) => {
            handle_pending_op_scope(app, op, scope, key);
            return;
        }
        Pending::Z => {
            // Only a second `Z` completes the chord; anything else cancels and
            // re-interprets the key normally.
            if matches!(key.code, KeyCode::Char('Z')) {
                app.pending = Pending::None;
                app.quit = true;
                return;
            }
            app.pending = Pending::None;
        }
        Pending::None => {}
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) if c.is_ascii_digit() && !(c == '0' && app.count == 0) => {
            app.count = app.count * 10 + c.to_digit(10).unwrap();
        }
        KeyCode::Char('j') | KeyCode::Down  => { let n = app.take_count(); app.motion_j(n); }
        KeyCode::Char('k') | KeyCode::Up    => { let n = app.take_count(); app.motion_k(n); }
        KeyCode::Char('h') | KeyCode::Left  => { let n = app.take_count(); app.motion_h(n); }
        KeyCode::Char('l') | KeyCode::Right => { let n = app.take_count(); app.motion_l(n); }
        KeyCode::Char('0') => { app.cursor_step = 0; app.count = 0; }
        KeyCode::Char('$') => { app.cursor_step = STEPS_PER_PHRASE - 1; }
        KeyCode::Char('g') => { app.cursor_step = 0; }
        KeyCode::Char('G') => { app.cursor_step = STEPS_PER_PHRASE - 1; }
        KeyCode::Char('w') => { app.motion_j(4); }
        KeyCode::Char('b') => { app.motion_k(4); }

        KeyCode::Char('x') => { app.op_delete_cell(); }
        KeyCode::Char('d') => { app.pending = Pending::Op('d'); }
        KeyCode::Char('y') => { app.pending = Pending::Op('y'); }
        KeyCode::Char('p') => { app.paste(true); }
        KeyCode::Char('P') => { app.paste(false); }
        KeyCode::Char('.') => { app.replay_last_action(); }

        KeyCode::Char('i') => { app.mode = Mode::Insert; app.status = "-- INSERT --".into(); }
        KeyCode::Char('a') => {
            app.motion_j(1);
            app.mode = Mode::Insert;
            app.status = "-- INSERT (append) --".into();
        }
        KeyCode::Char('v') => { app.mode = Mode::Visual; }
        KeyCode::Char(':') => {
            app.mode = Mode::Command;
            app.command_buf.clear();
        }
        KeyCode::Char(' ') => {
            app.playing = !app.playing;
            app.status = if app.playing { "playing...".into() } else { "stopped".into() };
        }
        KeyCode::Char('q') if ctrl => { app.quit = true; }
        KeyCode::Char('?') | KeyCode::F(1) => {
            app.mode = Mode::Help;
            app.status = "help — q/Esc/? to close".into();
        }
        KeyCode::F(2) => {
            app.mode = Mode::Instrument;
            app.status = format!("instrument {:02X} — Esc to return", app.selected_instr);
        }
        KeyCode::Char('Z') => { app.pending = Pending::Z; }
        _ => {}
    }
}

fn handle_pending_op(app: &mut App, op: char, key: KeyEvent) {
    match key.code {
        KeyCode::Char(c) if c == op => {
            app.pending = Pending::None;
            app.op_row(op);
        }
        KeyCode::Char('a') => { app.pending = Pending::OpScope(op, 'a'); }
        KeyCode::Char('i') => { app.pending = Pending::OpScope(op, 'i'); }
        KeyCode::Esc => { app.pending = Pending::None; }
        _ => {
            app.pending = Pending::None;
            app.status = "cancelled".into();
        }
    }
}

fn handle_pending_op_scope(app: &mut App, op: char, scope: char, key: KeyEvent) {
    let obj = match key.code {
        KeyCode::Char('b') => Some('b'),
        KeyCode::Char('p') => Some('p'),
        KeyCode::Char('v') => Some('v'),
        _ => None,
    };
    app.pending = Pending::None;
    if let Some(o) = obj {
        app.op_object(op, scope, o);
    } else {
        app.status = "unknown object".into();
    }
}

fn handle_insert(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.status = "".into();
        }
        KeyCode::Down  => { app.motion_j(1); }
        KeyCode::Up    => { app.motion_k(1); }
        KeyCode::Left  => { app.motion_h(1); }
        KeyCode::Right => { app.motion_l(1); }
        KeyCode::Char(c) => {
            // Default octave 4 for now; real impl tracks current octave.
            if let Some(note) = App::piano_row_note(c, 4) {
                app.insert_note(note);
            }
        }
        KeyCode::Backspace => {
            if app.cursor_step > 0 {
                app.cursor_step -= 1;
            }
            let (s, c) = (app.cursor_step, app.cursor_ch);
            app.phrase_mut().cells[s][c] = Cell::default();
        }
        _ => {}
    }
}

fn handle_command(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.command_buf.clear();
        }
        KeyCode::Enter => {
            let cmd = app.command_buf.trim().to_string();
            execute_command(app, &cmd);
            app.command_buf.clear();
            // Only fall back to Normal if the command didn't switch modes itself.
            if app.mode == Mode::Command {
                app.mode = Mode::Normal;
            }
        }
        KeyCode::Backspace => { app.command_buf.pop(); }
        KeyCode::Char(c) => { app.command_buf.push(c); }
        _ => {}
    }
}

fn execute_command(app: &mut App, cmd: &str) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts.as_slice() {
        ["q"] | ["q!"] | ["quit"] | ["quit!"] => { app.quit = true; }
        ["help"] | ["h"] => {
            app.mode = Mode::Help;
            app.status = "help — q/Esc/? to close".into();
        }
        ["inst"] | ["instrument"] => {
            app.mode = Mode::Instrument;
            app.status = format!("instrument {:02X} — Esc to return", app.selected_instr);
        }
        ["inst", n] | ["instrument", n] => {
            if let Ok(i) = u8::from_str_radix(n, 16) {
                app.selected_instr = i.min((INSTRUMENTS - 1) as u8);
            }
            app.mode = Mode::Instrument;
            app.status = format!("instrument {:02X} — Esc to return", app.selected_instr);
        }
        ["w"] => { write_current(app); }
        ["w", path] => { write_to(app, Path::new(path)); }
        ["wq"] => { write_current(app); if !app.status.starts_with("error") { app.quit = true; } }
        ["wq", path] => {
            write_to(app, Path::new(path));
            if !app.status.starts_with("error") { app.quit = true; }
        }
        ["e", path] => { edit_file(app, Path::new(path)); }
        ["e!"] | ["edit!"] => {
            if let Some(p) = app.current_file.clone() {
                edit_file(app, &p);
            } else {
                app.status = "error: no file loaded".into();
            }
        }
        ["play"] => { app.playing = true; app.status = "playing".into(); }
        ["stop"] => { app.playing = false; app.status = "stopped".into(); }
        ["set", kv] => {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "bpm" => {
                        if let Ok(n) = v.parse::<u16>() { app.song.bpm = n; }
                        app.status = format!("bpm = {}", app.song.bpm);
                    }
                    "step" => {
                        if let Ok(n) = v.parse::<usize>() {
                            app.song.edit_step = n.clamp(1, STEPS_PER_PHRASE);
                        }
                        app.status = format!("edit step = {}", app.song.edit_step);
                    }
                    _ => { app.status = format!("unknown setting: {}", k); }
                }
            }
        }
        _ => { app.status = format!("unknown command: {}", cmd); }
    }
}

// ---------- File I/O helpers ----------

fn write_current(app: &mut App) {
    let Some(path) = app.current_file.clone() else {
        app.status = "error: no filename (use :w <path>)".into();
        return;
    };
    write_to(app, &path);
}

fn write_to(app: &mut App, path: &Path) {
    match app.song.save(path) {
        Ok(()) => {
            app.current_file = Some(path.to_path_buf());
            app.status = format!("wrote {}", path.display());
        }
        Err(e) => { app.status = format!("error: {}", e); }
    }
}

fn edit_file(app: &mut App, path: &Path) {
    match Song::load(path) {
        Ok(song) => {
            app.song = song;
            app.current_file = Some(path.to_path_buf());
            app.cursor_step = 0;
            app.cursor_ch = 0;
            app.play_step = 0;
            app.status = format!("loaded {}", path.display());
        }
        Err(e) => { app.status = format!("error: {}", e); }
    }
}

// ---------- Main loop ----------

fn sync_audio(app: &mut App, engine: Option<&audio::AudioEngine>) {
    let Some(engine) = engine else { return };
    if let Ok(mut tr) = engine.transport.lock() {
        tr.bpm = app.song.bpm;
        tr.playing = app.playing;
        tr.phrase = app.phrase().clone();
        tr.instruments = app.song.instruments;
        app.play_step = tr.step;
    }
}

fn run<B: Backend>(terminal: &mut Terminal<B>, audio: Option<&audio::AudioEngine>) -> Result<()> {
    let mut app = App::new();
    if audio.is_none() {
        app.status = "audio disabled (no output device)".into();
    }
    loop {
        sync_audio(&mut app, audio);
        terminal.draw(|f| ui(f, &app))?;
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                handle_key(&mut app, key);
            }
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    // Initialise audio before entering raw mode so init errors print cleanly.
    let audio = match audio::AudioEngine::new() {
        Ok(a) => Some(a),
        Err(e) => {
            eprintln!("viper: audio init failed, continuing without sound: {}", e);
            None
        }
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run(&mut terminal, audio.as_ref());

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    res
}
