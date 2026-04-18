//! viper — a vim-keybound chiptune step sequencer
//!
//! Stage-1: data model, modal input, phrase editor UI.
//! Stage-2: cpal audio thread producing sound from the edited phrase.

mod audio;

use std::io;
use std::time::Duration;

use anyhow::Result;
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

#[derive(Clone, Debug)]
pub(crate) struct Phrase {
    pub cells: [[Cell; CHANNELS]; STEPS_PER_PHRASE],
}

impl Default for Phrase {
    fn default() -> Self {
        Self { cells: [[Cell::default(); CHANNELS]; STEPS_PER_PHRASE] }
    }
}

pub(crate) const INSTRUMENTS: usize = 16;

#[derive(Clone, Copy, Debug)]
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

#[derive(Debug)]
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

struct App {
    song: Song,
    mode: Mode,
    cursor_step: usize,
    cursor_ch: usize,
    /// Pending operator (e.g. user pressed `d`, awaiting motion).
    pending_op: Option<char>,
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
    quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            song: Song::default(),
            mode: Mode::Normal,
            cursor_step: 0,
            cursor_ch: 0,
            pending_op: None,
            count: 0,
            command_buf: String::new(),
            status: "welcome to viper — press : for commands, i to insert".into(),
            playing: false,
            play_step: 0,
            selected_instr: 0,
            instr_param: 0,
            show_splash: true,
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
        self.status = format!("deleted [{:02x},ch{}]", s, c + 1);
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
        row("i",               "insert mode"),
        row("a",               "append (move down, then insert)"),
        row(":",               "command mode"),
        row("space",           "toggle play (stub)"),
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
    } else if app.count > 0 || app.pending_op.is_some() {
        format!("{}{}",
            if app.count > 0 { app.count.to_string() } else { String::new() },
            app.pending_op.map(|c| c.to_string()).unwrap_or_default())
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
        KeyCode::Char('g') => {
            // Simplified: treat `g` alone as `gg` for the skeleton.
            app.cursor_step = 0;
        }
        KeyCode::Char('G') => { app.cursor_step = STEPS_PER_PHRASE - 1; }
        KeyCode::Char('w') => { app.motion_j(4); } // next bar
        KeyCode::Char('b') => { app.motion_k(4); } // prev bar

        KeyCode::Char('x') => { app.op_delete_cell(); }
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
        KeyCode::Char('Z') => {
            if app.pending_op == Some('Z') {
                app.pending_op = None;
                app.quit = true;
            } else {
                app.pending_op = Some('Z');
            }
        }
        _ => { app.pending_op = None; }
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
        ["w", path] => { app.status = format!("TODO: write to {}", path); }
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
