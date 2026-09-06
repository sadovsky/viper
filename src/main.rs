//! viper — a vim-keybound chiptune step sequencer
//!
//! Stage-1: data model, modal input, phrase editor UI.
//! Stage-2: cpal audio thread producing sound from the edited phrase.

mod audio;
mod compile;
mod dpcm;
mod gen;
mod midi;
mod modulation;
mod sprite;
mod style;
mod vip;
mod viz;
mod cli;

use std::collections::{HashMap, VecDeque};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
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
pub(crate) const CHANNELS: usize = 5;
/// Channel names in grid order.
pub(crate) const CH_NAMES: [&str; CHANNELS] = ["PU1", "PU2", "TRI", "NOI", "DPCM"];

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Phrase {
    pub cells: [[Cell; CHANNELS]; STEPS_PER_PHRASE],
}

impl Default for Phrase {
    fn default() -> Self {
        Self { cells: [[Cell::default(); CHANNELS]; STEPS_PER_PHRASE] }
    }
}

// ---------- Stage 27: the overlay layer ----------

/// Which fields of a cell actually differ. A volume-only change should
/// tint the volume column, not paint the whole cell — a diff you can't
/// read precisely is a diff you stop trusting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FieldMask {
    pub note: bool,
    pub instr: bool,
    pub vol: bool,
    pub fx: bool,
}

impl FieldMask {
    fn between(a: Cell, b: Cell) -> Self {
        // instr and vol render as `--` on a cell with no note, so a
        // difference there is invisible and marking it would put a sigil
        // beside two cells that look identical.
        let both_empty = a.note.is_none() && b.note.is_none();
        Self {
            note: a.note != b.note,
            instr: !both_empty && a.instr != b.instr,
            vol: !both_empty && a.vol != b.vol,
            fx: a.fx != b.fx,
        }
    }
    fn any(self) -> bool {
        self.note || self.instr || self.vol || self.fx
    }
}

/// What an overlaid cell is doing relative to the authored grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    /// Ghost preview: the pending command would write this cell.
    Proposed,
    /// Ghost preview: the pending command would erase this cell.
    Vanishing,
    /// Diff: absent on the left, present on the right.
    Added,
    /// Diff: present on the left, gone on the right.
    Removed,
    /// Diff: present on both, some fields differ.
    Changed(FieldMask),
}

impl Mark {
    /// The margin glyph, in the pad the note field already had. Colors can
    /// be themed away or lost on a monochrome terminal; the sigil cannot.
    fn sigil(self) -> char {
        match self {
            Mark::Proposed | Mark::Vanishing => '·',
            Mark::Added => '+',
            Mark::Removed => '-',
            Mark::Changed(_) => '~',
        }
    }
    /// Which fields this mark recolors. Adds, removes and ghosts concern
    /// the whole cell; a change concerns only what changed.
    fn fields(self) -> FieldMask {
        match self {
            Mark::Changed(m) => m,
            _ => FieldMask { note: true, instr: true, vol: true, fx: true },
        }
    }
}

/// One overlaid cell. The content travels with the mark because it is not
/// in the phrase the renderer is reading: a proposed cell shows the note
/// the command *would* write, a removed cell shows the note that is going
/// away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MarkedCell {
    pub cell: Cell,
    pub mark: Mark,
}

/// A sparse per-cell layer drawn over the phrase grid: the ghost of a
/// command that has not committed, or a diff against another phrase.
/// `None` entries fall straight through to the authored cell.
#[derive(Clone, Debug)]
pub(crate) struct Overlay {
    grid: [[Option<MarkedCell>; CHANNELS]; STEPS_PER_PHRASE],
    /// Appended to the pane title. The only place the user is told how to
    /// read the colors, so it earns its space.
    title: String,
}

impl Overlay {
    /// Ghost: compare the authored phrase against what the command would
    /// leave in the same slot. Returns `None` when nothing would change,
    /// so a no-op command draws nothing rather than an empty overlay.
    fn ghost(base: &Phrase, proposed: &Phrase, title: String) -> Option<Self> {
        let mut grid = [[None; CHANNELS]; STEPS_PER_PHRASE];
        let mut any = false;
        for s in 0..STEPS_PER_PHRASE {
            for c in 0..CHANNELS {
                let (a, b) = (base.cells[s][c], proposed.cells[s][c]);
                if a == b {
                    continue;
                }
                any = true;
                grid[s][c] = Some(if b.note.is_none() && a.note.is_some() {
                    // Show what is about to go away, not the blank.
                    MarkedCell { cell: a, mark: Mark::Vanishing }
                } else {
                    MarkedCell { cell: b, mark: Mark::Proposed }
                });
            }
        }
        any.then_some(Self { grid, title })
    }

    /// Diff: `left` is the phrase on screen, `right` the one compared
    /// against it.
    fn diff(left: &Phrase, right: &Phrase, title: String) -> Self {
        let mut grid = [[None; CHANNELS]; STEPS_PER_PHRASE];
        for s in 0..STEPS_PER_PHRASE {
            for c in 0..CHANNELS {
                let (a, b) = (left.cells[s][c], right.cells[s][c]);
                if a == b {
                    continue;
                }
                grid[s][c] = Some(match (a.note.is_none(), b.note.is_none()) {
                    (true, false) => MarkedCell { cell: b, mark: Mark::Added },
                    (false, true) => MarkedCell { cell: a, mark: Mark::Removed },
                    _ => {
                        let mask = FieldMask::between(a, b);
                        if !mask.any() {
                            continue;
                        }
                        MarkedCell { cell: b, mark: Mark::Changed(mask) }
                    }
                });
            }
        }
        Self { grid, title }
    }

    fn counts(&self) -> (usize, usize, usize) {
        let (mut add, mut del, mut chg) = (0, 0, 0);
        for row in &self.grid {
            for m in row.iter().flatten() {
                match m.mark {
                    Mark::Added | Mark::Proposed => add += 1,
                    Mark::Removed | Mark::Vanishing => del += 1,
                    Mark::Changed(_) => chg += 1,
                }
            }
        }
        (add, del, chg)
    }
}

pub(crate) const INSTRUMENTS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
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

/// Stage 23 (full song mode): a named list of phrase indices. The
/// arrangement sequences chains; playback flattens arrangement → chains →
/// phrases into the `order` list everything downstream already consumes.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Chain {
    pub phrases: Vec<u8>,
    pub name: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct Song {
    pub bpm: u16,
    /// How far to advance the cursor after inserting a note in insert mode.
    pub edit_step: usize,
    pub phrases: Vec<Phrase>,
    /// One phrase loaded at a time for now.
    pub current_phrase: usize,
    pub instruments: [Instrument; INSTRUMENTS],
    /// Song mode (Stage 16-lite): phrase indices in playback order. Empty
    /// means "loop the current phrase".
    pub order: Vec<usize>,
    /// Order position to jump back to after the last entry.
    pub loop_pos: usize,
    /// `@meta` fields carried into the NSF header.
    pub title: String,
    pub artist: String,
    pub copyright: String,
    /// `@meta key="E minor"` — informational, carried into manifests.
    pub key_name: String,
    /// `@driver bin=.. sym=..` — the NSF driver to compile against,
    /// relative to the `.vip` file.
    pub driver: Option<(PathBuf, PathBuf)>,
    /// `@dpcm NN name= path= rate=` — sample files for the DPCM column,
    /// indexed by note (C-4 = 00). Empty means the built-in kick/snare/hat bank.
    pub samples: Vec<DpcmRef>,
    /// `@driver expansion=vrc6` — request expansion audio in the NSF header.
    pub expansion: bool,
    /// Stage 23: chains and the arrangement that sequences them. When
    /// `arrangement` is non-empty it is the source of truth for playback
    /// order (see [`Song::flat_order`]) and `order` is not written to disk.
    pub chains: Vec<Chain>,
    pub arrangement: Vec<u8>,
    /// Arrangement slot to jump back to after the last one.
    pub arr_loop: usize,
    /// Per-16th signed sample offset added to the step clock (synth engine
    /// only). `[0; 16]` = straight time.
    pub groove: [i16; 16],
    /// Per-channel wrap length in steps (1..=16). A channel shorter than
    /// the phrase cycles inside it — polymeter.
    pub channel_length: [u8; CHANNELS],
}

impl Song {
    /// The flat playback order and loop position. Expands the arrangement
    /// when one exists, otherwise returns the explicit `order` list.
    /// Entries that point at missing chains or phrases are skipped.
    pub fn flat_order(&self) -> (Vec<usize>, usize) {
        if self.arrangement.is_empty() {
            return (self.order.clone(), self.loop_pos.min(self.order.len().saturating_sub(1)));
        }
        let mut order = Vec::new();
        let mut loop_pos = 0;
        for (slot, &ci) in self.arrangement.iter().enumerate() {
            if slot == self.arr_loop {
                loop_pos = order.len();
            }
            if let Some(chain) = self.chains.get(ci as usize) {
                order.extend(chain.phrases.iter().map(|&p| p as usize).filter(|&p| p < self.phrases.len()));
            }
        }
        let loop_pos = loop_pos.min(order.len().saturating_sub(1));
        (order, loop_pos)
    }

    /// For each flat order position, the `(arrangement slot, position in
    /// chain)` it came from. Empty when there is no arrangement.
    pub fn arrangement_map(&self) -> Vec<(usize, usize)> {
        let mut map = Vec::new();
        for (slot, &ci) in self.arrangement.iter().enumerate() {
            if let Some(chain) = self.chains.get(ci as usize) {
                for (pos, &p) in chain.phrases.iter().enumerate() {
                    if (p as usize) < self.phrases.len() {
                        map.push((slot, pos));
                    }
                }
            }
        }
        map
    }

    pub fn has_groove(&self) -> bool {
        self.groove.iter().any(|&g| g != 0)
    }
}

/// One `@dpcm` entry: a .dmc file and the DMC rate it was encoded for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DpcmRef {
    pub name: String,
    pub path: PathBuf,
    pub rate: u8,
}

impl Default for Song {
    fn default() -> Self {
        Self {
            bpm: 140,
            edit_step: 1,
            phrases: vec![Phrase::default()],
            current_phrase: 0,
            instruments: [Instrument::default(); INSTRUMENTS],
            order: Vec::new(),
            loop_pos: 0,
            title: String::new(),
            artist: String::new(),
            copyright: String::new(),
            key_name: String::new(),
            driver: None,
            samples: Vec::new(),
            chains: Vec::new(),
            arrangement: Vec::new(),
            arr_loop: 0,
            groove: [0; 16],
            channel_length: [STEPS_PER_PHRASE as u8; CHANNELS],
            expansion: false,
        }
    }
}

impl Song {
    /// Default startup song: a 16-step Am–F–G–Am loop ("i–VI–VII–i"), a
    /// progression you'll recognize from plenty of NES-era soundtracks. 80 BPM,
    /// one chord per bar, with a lead on PU1, arp on PU2, bass on TRI, and a
    /// simple kick/snare/hat on NOI.
    pub(crate) fn demo() -> Self {
        let mut song = Song::default();
        song.bpm = 80;
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
    /// Stage 5: live keyboard monitor. Piano row triggers notes on the current
    /// channel through the audio engine; no pattern writes.
    Live,
}

/// Pending multi-key sequence in normal mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    None,
    Z,                        // pressed `Z`, waiting for second `Z` (save-quit) or `Q` (force-quit)
    Op(char),                 // pressed `d` or `y`, waiting for motion / object prefix
    OpScope(char, char),      // pressed `da`, `di`, `ya`, or `yi`, waiting for object letter
    Replace,                  // pressed `r`, waiting for the replacement piano-row key
    MacroRecord,              // pressed `q`, waiting for register letter to record into
    MacroPlay,                // pressed `@`, waiting for register letter to play back
}

impl Pending {
    fn display(&self) -> String {
        match self {
            Pending::None => String::new(),
            Pending::Z => "Z".into(),
            Pending::Op(c) => c.to_string(),
            Pending::OpScope(a, b) => format!("{}{}", a, b),
            Pending::Replace => "r".into(),
            Pending::MacroRecord => "q".into(),
            Pending::MacroPlay => "@".into(),
        }
    }
}

/// Stage 8: a single atomic action recordable into a performance macro.
/// We capture transport/mix-level ops, not cell edits — macros are for
/// live play, not composition.
#[derive(Clone, Copy, Debug)]
enum MacroOp {
    /// Launch scene slot (0..9). Respects the same bar-boundary queue as a
    /// manual launch, so replaying a macro stays groove-locked.
    SceneLaunch(usize),
    /// Transpose the current phrase by N semitones (can go negative).
    Transpose(i32),
    /// Flip mute state on a channel.
    ToggleMute(usize),
    /// Flip transport play/stop.
    TogglePlay,
}

/// Clipboard from yank / delete. `rows[i][j]` is the j-th channel of row i.
///
/// Paste anchoring is derived from shape: a register exactly `CHANNELS` wide
/// is treated as row-wise (pastes from channel 0 regardless of cursor); any
/// narrower register is block-wise (pastes anchored at the cursor's channel).
#[derive(Clone, Debug, Default)]
struct Register {
    rows: Vec<Vec<Cell>>,
}

impl Register {
    /// True if the register spans all CHANNELS (came from a full-row yank like
    /// `yy`, `yab`, `yip`). Row-wise pastes ignore cursor_ch.
    fn is_full_row(&self) -> bool {
        self.rows.first().map_or(false, |r| r.len() == CHANNELS)
    }
}

/// Recorded destructive action, replayable via `.`.
#[derive(Clone, Copy, Debug)]
enum LastAction {
    DeleteCell,
    DeleteRow { count: u32 },
    DeleteBar { count: u32 },
    DeletePhrase,
    DeleteChannel,
    Paste { after: bool },
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
    /// Byte index into `command_buf`. Always on a char boundary.
    command_cursor: usize,
    status: String,
    playing: bool,
    /// Current playhead step, mirrored from the audio engine.
    play_step: usize,
    /// Which instrument new notes are tagged with and the editor is viewing.
    selected_instr: u8,
    /// Base octave for the insert-mode piano row. Shifted with `<` / `>`.
    insert_octave: u8,
    /// Cursor row in the instrument editor (0..NUM_INSTR_PARAMS).
    instr_param: usize,
    /// True until the user presses a key on the splash screen.
    show_splash: bool,
    /// Floating music notes animated on the splash screen.
    splash_particles: Vec<SplashParticle>,
    /// Dedicated RNG for splash particles so it doesn't perturb `:gen` seeding.
    splash_rng: gen::Rng,
    /// Unnamed register holding the last yank/delete contents.
    register: Register,
    /// Last destructive action, replayable via `.`.
    last_action: Option<LastAction>,
    /// Anchor (step, channel) of a rectangular visual selection. Live while `mode == Visual`.
    visual_anchor: Option<(usize, usize)>,
    /// `V` linewise visual: force the selection to span all channels regardless of cursor_ch.
    visual_linewise: bool,
    /// Path of the currently-loaded song file, if any.
    current_file: Option<PathBuf>,
    /// Monotonic counter used to seed `:gen` so repeated calls vary.
    gen_seed: u64,
    /// Snapshots for `u`. Each entry is the song state *before* a destructive op.
    undo_stack: Vec<Song>,
    /// Snapshots popped by `u`, used by `Ctrl-r` to redo.
    redo_stack: Vec<Song>,
    /// Active UI theme. Swap via `:set theme=<name>`.
    theme: Theme,
    /// True when the song has unsaved changes since the last write / load.
    /// Shown in the title bar as `[+]`.
    dirty: bool,
    /// Stage 5: pending gate events flushed to the audio engine each frame.
    live_events: VecDeque<audio::LiveEvent>,
    /// Last note played live, per channel. Displayed in the Live-mode status.
    live_last_note: [Option<u8>; CHANNELS],
    /// Stage 6: per-channel record arm. While armed, piano-row keys in Live
    /// mode write the played note into the cell under the playhead (when
    /// transport is playing) or under the cursor (when stopped).
    recording: [bool; CHANNELS],
    /// Stage 8: per-channel mutes. A muted channel's voice goes silent
    /// immediately — pattern steps skip it and live notes are suppressed.
    muted: [bool; CHANNELS],
    /// Stage 7: scene slots. `scenes[i] = Some(phrase_idx)` means number key
    /// `i+1` launches that phrase. `None` = unbound slot.
    scenes: [Option<usize>; 9],
    /// Slot index (0..9) queued to launch on the next bar boundary while
    /// playing. `None` = no pending launch.
    queued_scene: Option<usize>,
    /// Song mode follows `song.order`; off = loop the current phrase.
    song_mode: bool,
    /// Stage 23: chain that `:chain add` / `:chain pop` operate on, and
    /// whether the song pane (arrangement + chains) is shown.
    current_chain: usize,
    show_song: bool,
    /// Playback engine: internal synth or the APU (compiled NSF).
    engine: audio::Engine,
    /// Compiled NSF for APU playback, refreshed on play.
    nsf_cache: Option<std::sync::Arc<Vec<u8>>>,
    /// DPCM preview bank for the synth engine (see `reload_bank`).
    bank: std::sync::Arc<Vec<dpcm::Sample>>,
    bank_generation: u64,
    nsf_generation: u64,
    nsf_frames_per_row: f64,
    prev_playing: bool,
    /// `play_step` observed on the previous UI frame. We fire a queued
    /// launch only on the frame the audio thread actually crosses a bar,
    /// so scene changes land crisply on the downbeat.
    prev_play_step: usize,
    /// Stage 8: saved performance macros keyed by register letter. A macro
    /// is a short sequence of transport/mix ops recorded during live play.
    macros: HashMap<char, Vec<MacroOp>>,
    /// Register letter and buffer currently being recorded into, if any.
    recording_macro: Option<(char, Vec<MacroOp>)>,
    /// Last register letter played with `@` — `@@` replays this.
    last_macro: Option<char>,
    /// Stage 9: latest per-voice snapshot from the audio thread. Single slot;
    /// `sync_audio` copies it out of the Transport mutex each UI tick.
    viz_frame: audio::VizFrame,
    /// Stage 13: seconds since the last note-on edge per channel. Starts
    /// very negative so fresh `.age` reads are "long ago" before any note
    /// has played.
    voice_last_on: [f32; CHANNELS],
    /// Previous-tick gate state for edge detection.
    prev_gates: [bool; CHANNELS],
    /// Stage 10: visualizer pane toggle + selected viz kind. Hidden by
    /// default; `:viz` toggles, `:viz <kind>` picks and shows.
    show_viz: bool,
    viz_kind: viz::VizKind,
    /// Free-running counter bumped every UI tick. Scope uses it to animate
    /// phase so waveforms scroll instead of snapshot-freezing at rest.
    viz_tick: u32,
    /// Stage 27: ghost preview of the `:` command being typed. Lives only
    /// while `mode == Command`, recomputed on every keystroke, discarded on
    /// Esc and on Enter.
    ghost: Option<Overlay>,
    /// Stage 27: `:diff`. Sticky until dismissed or invalidated by an edit.
    diff: Option<Overlay>,
    /// Stage 29: the last few things the app announced, newest first, for
    /// row 2 of the modeline. Derived by watching `status` rather than
    /// instrumenting every action — everything notable already announces
    /// itself there, so one watcher catches all of it.
    event_log: VecDeque<String>,
    /// The status text already folded into `event_log`.
    last_logged: String,
    /// `:set still=on` — freeze the tempo-locked breathing animations.
    /// Signal-driven feedback (channel LEDs, the playhead) stays live;
    /// only the decorative pulses stop. See [`Breath`].
    still: bool,
    /// Stage 11: loaded sprite sheets, keyed by their `name` (file stem
    /// by default, overrideable at load time).
    sprite_sheets: HashMap<String, sprite::SpriteSheet>,
    /// Active sprite placements drawn in order — later placements win
    /// pixel conflicts. Stage 12 will make placements mutable.
    sprite_placements: Vec<sprite::Placement>,
    /// User-defined 4-color palettes keyed by name. Used to recolor
    /// sheets without reloading the source PNG.
    sprite_palettes: HashMap<String, [ratatui::style::Color; sprite::PALETTE_SIZE]>,
    /// Stage 12: modulation bindings tie audio sources to sprite props.
    bindings: Vec<modulation::Binding>,
    /// Derived each frame from placements + bindings; what the viz pane draws.
    effective_placements: Vec<modulation::EffectivePlacement>,
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
            command_cursor: 0,
            status: "demo loaded — space: play   ?: help   :new blank   :e <file.vip> open".into(),
            playing: true,
            play_step: 0,
            selected_instr: 0,
            insert_octave: 4,
            instr_param: 0,
            show_splash: true,
            splash_particles: Vec::new(),
            splash_rng: gen::Rng::new(0xD15C_0D1C_FACE_5EED),
            register: Register::default(),
            last_action: None,
            visual_anchor: None,
            visual_linewise: false,
            current_file: None,
            gen_seed: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            theme: Theme::NES,
            dirty: false,
            live_events: VecDeque::new(),
            live_last_note: [None; CHANNELS],
            recording: [false; CHANNELS],
            muted: [false; CHANNELS],
            scenes: [None; 9],
            queued_scene: None,
            song_mode: false,
            current_chain: 0,
            show_song: false,
            engine: audio::Engine::Synth,
            nsf_cache: None,
            bank: std::sync::Arc::new(dpcm::default_bank()),
            bank_generation: 0,
            nsf_generation: 0,
            nsf_frames_per_row: 4.0,
            prev_playing: false,
            prev_play_step: usize::MAX,
            macros: HashMap::new(),
            recording_macro: None,
            last_macro: None,
            viz_frame: audio::VizFrame::default(),
            voice_last_on: [-1.0e9; CHANNELS],
            prev_gates: [false; CHANNELS],
            show_viz: false,
            viz_kind: viz::VizKind::Bars,
            viz_tick: 0,
            ghost: None,
            diff: None,
            event_log: VecDeque::new(),
            last_logged: String::new(),
            still: false,
            sprite_sheets: HashMap::new(),
            sprite_placements: Vec::new(),
            sprite_palettes: HashMap::new(),
            bindings: Vec::new(),
            effective_placements: Vec::new(),
            quit: false,
        }
    }

    /// Advance splash-screen particles one frame. Called while `show_splash`
    /// is true, ~20 times per second (event-loop poll cadence).
    fn tick_splash(&mut self, area_w: u16, area_h: u16) {
        for p in &mut self.splash_particles {
            p.y -= p.vy;
            p.age += 1;
        }
        self.splash_particles
            .retain(|p| p.age < p.lifetime && p.y > -1.0);

        if area_w < 4 || area_h < 4 {
            return;
        }
        if self.splash_particles.len() < 40 && self.splash_rng.chance(0.22) {
            let glyph = SPLASH_GLYPHS[self.splash_rng.range(0, 4) as usize];
            let x = self.splash_rng.range(0, area_w as u32) as f32;
            let y = (area_h - 1) as f32;
            let vy = 0.15 + (self.splash_rng.range(0, 200) as f32) / 1000.0;
            let lifetime = 40 + self.splash_rng.range(0, 40);
            self.splash_particles.push(SplashParticle {
                x,
                y,
                vy,
                age: 0,
                lifetime,
                glyph,
            });
        }
    }

    /// Snapshot the current song into the undo stack and clear the redo stack.
    /// Call this *before* mutating. Cap the stack so edits over a long session
    /// don't grow the heap without bound.
    /// The layer the phrase grid draws over the authored cells. A live
    /// ghost outranks a standing diff: you are asking a question about
    /// what happens next, so the comparison steps aside until you are done.
    fn overlay(&self) -> Option<&Overlay> {
        self.ghost.as_ref().or(self.diff.as_ref())
    }

    /// The seed for the next `:gen`, consuming it. The **only** place
    /// `gen_seed` advances. The ghost preview reads the field directly
    /// without advancing it, so preview and commit run the same generator
    /// at the same seed; if anything else ever bumped the seed between the
    /// two, they would silently disagree and the user would get something
    /// other than what they saw.
    fn take_gen_seed(&mut self) -> u64 {
        let seed = self.gen_seed;
        self.gen_seed = self.gen_seed.wrapping_add(1);
        seed
    }

    fn snapshot(&mut self) {
        // A standing diff describes a comparison this edit is about to
        // falsify. Every mutation in the app snapshots first, so clearing
        // it here covers all of them from one line.
        self.diff = None;
        const UNDO_LIMIT: usize = 200;
        if self.undo_stack.len() == UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(self.song.clone());
        self.redo_stack.clear();
        self.dirty = true;
    }

    fn undo(&mut self) {
        let Some(prev) = self.undo_stack.pop() else {
            self.status = "already at oldest change".into();
            return;
        };
        let current = std::mem::replace(&mut self.song, prev);
        self.redo_stack.push(current);
        self.clamp_cursor_to_song();
        self.status = format!("undo ({} remaining)", self.undo_stack.len());
    }

    fn redo(&mut self) {
        let Some(next) = self.redo_stack.pop() else {
            self.status = "already at newest change".into();
            return;
        };
        let current = std::mem::replace(&mut self.song, next);
        self.undo_stack.push(current);
        self.clamp_cursor_to_song();
        self.status = format!("redo ({} remaining)", self.redo_stack.len());
    }

    /// After restoring a prior Song (from undo/redo or `:e`), make sure the
    /// editor cursor and phrase index still point at valid cells. Also drops
    /// a standing diff: undo and redo replace the song wholesale without
    /// snapshotting, so they bypass the clear in `snapshot`.
    fn clamp_cursor_to_song(&mut self) {
        self.diff = None;
        if self.song.current_phrase >= self.song.phrases.len() {
            self.song.current_phrase = self.song.phrases.len().saturating_sub(1);
        }
        if self.cursor_step >= STEPS_PER_PHRASE {
            self.cursor_step = STEPS_PER_PHRASE - 1;
        }
        if self.cursor_ch >= CHANNELS {
            self.cursor_ch = CHANNELS - 1;
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
        self.snapshot();
        let (s, c) = (self.cursor_step, self.cursor_ch);
        self.phrase_mut().cells[s][c] = Cell::default();
        self.last_action = Some(LastAction::DeleteCell);
        self.status = format!("deleted [{:02X},ch{}]", s, c + 1);
    }

    // ---------- Yank / delete / paste ----------

    fn yank_range(&mut self, steps: Range<usize>, chs: Range<usize>) {
        let mut rows = Vec::with_capacity(steps.len());
        for s in steps.clone() {
            let mut row = Vec::with_capacity(chs.len());
            for c in chs.clone() {
                row.push(self.phrase().cells[s][c]);
            }
            rows.push(row);
        }
        self.register = Register { rows };
    }

    fn clear_range(&mut self, steps: Range<usize>, chs: Range<usize>) {
        for s in steps {
            for c in chs.clone() {
                self.phrase_mut().cells[s][c] = Cell::default();
            }
        }
    }

    fn op_row(&mut self, op: char, count: u32) {
        let n = count.max(1) as usize;
        let start = self.cursor_step;
        let end = (start + n).min(STEPS_PER_PHRASE);
        let steps = start..end;
        let chs = 0..CHANNELS;
        let rows = steps.len();
        self.yank_range(steps.clone(), chs.clone());
        if op == 'd' || op == 'c' {
            self.snapshot();
            self.clear_range(steps, chs);
            self.last_action = Some(LastAction::DeleteRow { count: count.max(1) });
            let verb = if op == 'c' { "changed" } else { "deleted" };
            self.status = if rows == 1 { format!("{} row", verb) } else { format!("{} {} rows", verb, rows) };
            if op == 'c' {
                self.cursor_step = start;
                self.cursor_ch = 0;
                self.mode = Mode::Insert;
                self.status = "-- INSERT (change) --".into();
            }
        } else {
            self.status = if rows == 1 { "yanked row".into() } else { format!("yanked {} rows", rows) };
        }
    }

    fn op_object(&mut self, op: char, scope: char, obj: char, count: u32) {
        let n = count.max(1) as usize;
        let (steps, chs, action, label) = match obj {
            'b' => {
                let bar = self.cursor_step / 4;
                let start = bar * 4;
                let end = (start + 4 * n).min(STEPS_PER_PHRASE);
                (start..end, 0..CHANNELS, LastAction::DeleteBar { count: count.max(1) },
                 if n == 1 { "bar".to_string() } else { format!("{} bars", n) })
            }
            'p' => (0..STEPS_PER_PHRASE, 0..CHANNELS, LastAction::DeletePhrase, "phrase".into()),
            'v' => (
                0..STEPS_PER_PHRASE,
                self.cursor_ch..self.cursor_ch + 1,
                LastAction::DeleteChannel,
                "channel".into(),
            ),
            _ => {
                self.status = format!("unknown text object: {}{}", scope, obj);
                return;
            }
        };
        let start_step = steps.start;
        let start_ch = chs.start;
        self.yank_range(steps.clone(), chs.clone());
        if op == 'd' || op == 'c' {
            self.snapshot();
            self.clear_range(steps, chs);
            self.last_action = Some(action);
            let verb = if op == 'c' { "changed" } else { "deleted" };
            self.status = format!("{} {}", verb, label);
            if op == 'c' {
                self.cursor_step = start_step;
                self.cursor_ch = start_ch;
                self.mode = Mode::Insert;
                self.status = "-- INSERT (change) --".into();
            }
        } else {
            self.status = format!("yanked {}", label);
        }
    }

    /// Delete or yank the current rectangular Visual selection. Returns to Normal on completion.
    fn op_visual(&mut self, op: char) {
        let Some((as_step, as_ch)) = self.visual_anchor else {
            return;
        };
        let s0 = as_step.min(self.cursor_step);
        let s1 = as_step.max(self.cursor_step) + 1;
        let (c0, c1) = if self.visual_linewise {
            (0, CHANNELS)
        } else {
            (as_ch.min(self.cursor_ch), as_ch.max(self.cursor_ch) + 1)
        };
        self.yank_range(s0..s1, c0..c1);
        if op == 'd' || op == 'c' {
            self.snapshot();
            self.clear_range(s0..s1, c0..c1);
            let verb = if op == 'c' { "changed" } else { "deleted" };
            self.status = format!("{} {}×{} block", verb, s1 - s0, c1 - c0);
        } else {
            self.status = format!("yanked {}×{} block", s1 - s0, c1 - c0);
        }
        // Move cursor to top-left of the selection so paste targets the expected spot.
        self.cursor_step = s0;
        self.cursor_ch = c0;
        self.visual_anchor = None;
        self.visual_linewise = false;
        if op == 'c' {
            self.mode = Mode::Insert;
            self.status = "-- INSERT (change) --".into();
        } else {
            self.mode = Mode::Normal;
        }
    }

    fn paste(&mut self, after: bool) {
        if self.register.rows.is_empty() {
            self.status = "register empty".into();
            return;
        }
        self.snapshot();
        let start_step = if after {
            (self.cursor_step + 1).min(STEPS_PER_PHRASE)
        } else {
            self.cursor_step
        };
        let start_ch = if self.register.is_full_row() {
            0
        } else {
            self.cursor_ch
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
        self.last_action = Some(LastAction::Paste { after });
        self.status = format!("pasted {} row(s)", n_rows);
    }

    fn replay_last_action(&mut self) {
        let Some(action) = self.last_action else {
            self.status = "nothing to repeat".into();
            return;
        };
        match action {
            LastAction::DeleteCell => self.op_delete_cell(),
            LastAction::DeleteRow { count } => self.op_row('d', count),
            LastAction::DeleteBar { count } => self.op_object('d', 'a', 'b', count),
            LastAction::DeletePhrase => self.op_object('d', 'i', 'p', 1),
            LastAction::DeleteChannel => self.op_object('d', 'i', 'v', 1),
            LastAction::Paste { after } => self.paste(after),
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

    // ---------- Stage 6: overdub recording ----------

    fn any_recording(&self) -> bool {
        self.recording.iter().any(|&b| b)
    }

    fn disarm_all(&mut self) {
        self.recording = [false; CHANNELS];
    }

    /// Write a note cell at the current record target step on `ch`. Returns
    /// the step that was written so the caller can report it.
    fn record_note(&mut self, ch: usize, note: u8) -> usize {
        let step = if self.playing { self.play_step } else { self.cursor_step };
        let instr = self.selected_instr;
        self.snapshot();
        let cell = &mut self.phrase_mut().cells[step][ch];
        cell.note = Some(note);
        cell.instr = instr;
        step
    }
}

// ---------- Theme ----------

/// Named colors used across the UI. Swap via `:set theme=<name>`.
///
/// NES is the default: a curated fantasy-console palette where each channel
/// and each cell field gets its own hue, so the eye can parse the grid in
/// peripheral vision. PHOSPHOR is the alt: amber on black, near-monochrome;
/// channel differentiation comes from position and glyph, not color.
#[derive(Clone, Copy, Debug)]
struct Theme {
    name: &'static str,

    // Generic roles
    accent: Color,       // section headers, program title
    dim: Color,          // empty cells, faint hints
    label: Color,        // column headers, pane titles
    hint: Color,         // trailing italic hints

    // Cell field colors
    note: Color,
    instr: Color,
    vol: Color,
    fx: Color,

    // Cell highlights
    cursor_bg: Color,
    selection_bg: Color,
    playhead_bg: Color,
    playhead_label: Color,
    /// Faint tint applied to the entire column under the cursor channel.
    column_bg: Color,

    // Mode chip
    mode_fg: Color,
    mode_normal: Color,
    mode_insert: Color,
    mode_visual: Color,
    mode_command: Color,
    mode_help: Color,
    mode_instr: Color,
    mode_live: Color,

    // Splash
    splash_logo: Color,
    splash_snake: Color,
    splash_base: Color,

    // Instrument editor
    instr_title: Color,
    instr_row_bg: Color,
    instr_row_fg: Color,
    instr_value: Color,
    instr_label: Color,

    /// Phosphor's CRT scanline: a faint background on alternate grid rows.
    /// `None` in themes that do not want one — DESIGN.md is explicit that
    /// the effect should be *of* the terminal, an alternating-line colour
    /// tweak, not a texture painted on top.
    scanline_bg: Option<Color>,

    // Visualizer
    viz_bg: Color,

    // Stage 27 overlay: ghost preview and diff. Declared per theme rather
    // than derived with `mix`, because `mix` snaps *named* colors at the
    // halfway point and every field foreground under `nes` is named — a
    // derived ghost would be invisible there. Backgrounds are `Rgb` in both
    // themes, so they blend reliably and carry most of the signal.
    ghost_fg: Color,
    ghost_bg: Color,
    vanish_bg: Color,
    diff_add_fg: Color,
    diff_add_bg: Color,
    diff_del_fg: Color,
    diff_del_bg: Color,
    diff_chg_fg: Color,
    diff_chg_bg: Color,
}

impl Theme {
    /// Foreground and background for an overlay mark.
    fn mark_colors(&self, mark: Mark) -> (Color, Color) {
        match mark {
            Mark::Proposed => (self.ghost_fg, self.ghost_bg),
            Mark::Vanishing => (self.ghost_fg, self.vanish_bg),
            Mark::Added => (self.diff_add_fg, self.diff_add_bg),
            Mark::Removed => (self.diff_del_fg, self.diff_del_bg),
            Mark::Changed(_) => (self.diff_chg_fg, self.diff_chg_bg),
        }
    }
}

impl Theme {
    const NES: Self = Self {
        name: "nes",
        accent: Color::Yellow,
        dim: Color::DarkGray,
        label: Color::Yellow,
        hint: Color::DarkGray,
        note: Color::Green,
        instr: Color::Cyan,
        vol: Color::Magenta,
        fx: Color::LightYellow,
        cursor_bg: Color::Rgb(40, 40, 80),
        selection_bg: Color::Rgb(70, 40, 90),
        playhead_bg: Color::Rgb(60, 20, 20),
        playhead_label: Color::Red,
        column_bg: Color::Rgb(22, 22, 40),
        mode_fg: Color::Black,
        mode_normal: Color::Cyan,
        mode_insert: Color::Green,
        mode_visual: Color::Magenta,
        mode_command: Color::Yellow,
        mode_help: Color::Blue,
        mode_instr: Color::LightRed,
        mode_live: Color::Red,
        splash_logo: Color::Cyan,
        splash_snake: Color::Green,
        splash_base: Color::LightBlue,
        instr_title: Color::Yellow,
        instr_row_bg: Color::Cyan,
        instr_row_fg: Color::Black,
        instr_value: Color::Green,
        instr_label: Color::Gray,
        scanline_bg: None,
        viz_bg: Color::Rgb(12, 12, 24),
        // Colour does the talking here, matching git's vocabulary.
        ghost_fg: Color::DarkGray,
        ghost_bg: Color::Rgb(26, 30, 44),
        vanish_bg: Color::Rgb(40, 20, 20),
        diff_add_fg: Color::Green,
        diff_add_bg: Color::Rgb(12, 40, 16),
        diff_del_fg: Color::Red,
        diff_del_bg: Color::Rgb(48, 14, 14),
        diff_chg_fg: Color::Yellow,
        diff_chg_bg: Color::Rgb(46, 40, 10),
    };

    // Amber-on-black CRT. Three tiers of amber (bright/mid/dark) + black.
    const PHOSPHOR: Self = Self {
        name: "phosphor",
        accent: Color::Rgb(255, 176, 0),
        dim: Color::Rgb(90, 50, 0),
        label: Color::Rgb(255, 176, 0),
        hint: Color::Rgb(140, 80, 0),
        note: Color::Rgb(255, 176, 0),
        instr: Color::Rgb(200, 130, 0),
        vol: Color::Rgb(200, 130, 0),
        fx: Color::Rgb(255, 200, 60),
        cursor_bg: Color::Rgb(80, 45, 0),
        selection_bg: Color::Rgb(120, 70, 0),
        playhead_bg: Color::Rgb(50, 28, 0),
        playhead_label: Color::Rgb(255, 220, 120),
        column_bg: Color::Rgb(30, 18, 0),
        mode_fg: Color::Black,
        mode_normal: Color::Rgb(255, 176, 0),
        mode_insert: Color::Rgb(255, 220, 120),
        mode_visual: Color::Rgb(255, 140, 40),
        mode_command: Color::Rgb(255, 200, 60),
        mode_help: Color::Rgb(200, 130, 0),
        mode_instr: Color::Rgb(255, 100, 40),
        mode_live: Color::Rgb(255, 80, 20),
        splash_logo: Color::Rgb(255, 176, 0),
        splash_snake: Color::Rgb(200, 130, 0),
        splash_base: Color::Rgb(140, 80, 0),
        instr_title: Color::Rgb(255, 176, 0),
        instr_row_bg: Color::Rgb(255, 176, 0),
        instr_row_fg: Color::Black,
        instr_value: Color::Rgb(255, 220, 120),
        instr_label: Color::Rgb(200, 130, 0),
        scanline_bg: Some(Color::Rgb(18, 10, 0)),
        viz_bg: Color::Rgb(10, 5, 0),
        // Near-monochrome by design, so brightness and the margin sigil
        // carry the distinction instead of hue.
        ghost_fg: Color::Rgb(140, 80, 0),
        ghost_bg: Color::Rgb(30, 18, 0),
        vanish_bg: Color::Rgb(38, 14, 0),
        diff_add_fg: Color::Rgb(255, 220, 120),
        diff_add_bg: Color::Rgb(52, 34, 0),
        diff_del_fg: Color::Rgb(120, 66, 0),
        diff_del_bg: Color::Rgb(34, 12, 0),
        diff_chg_fg: Color::Rgb(255, 200, 60),
        diff_chg_bg: Color::Rgb(48, 36, 0),
    };

    fn by_name(n: &str) -> Option<Self> {
        match n {
            "nes" => Some(Self::NES),
            "phosphor" => Some(Self::PHOSPHOR),
            _ => None,
        }
    }
}

// ---------- Breath: tempo-locked animation ----------

/// Interpolate `from` → `to` by `t` (0..1).
///
/// Two RGB colors mix numerically. Anything else — named ANSI colors,
/// `Reset` — switches at the halfway point instead of being resolved to
/// fixed RGB, so the user's terminal palette keeps deciding what "yellow"
/// means. Every animated color in the UI goes through here.
fn mix(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let l = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u8;
            Color::Rgb(l(r1, r2), l(g1, g2), l(b1, b2))
        }
        _ if t >= 0.5 => to,
        _ => from,
    }
}

/// The brighter sibling of a color: an ANSI base color's `Light` variant
/// (so the terminal palette still picks the shade), or an RGB color moved
/// a quarter of the way to white.
fn brighter(c: Color) -> Color {
    use Color::*;
    match c {
        Black => DarkGray,
        Red => LightRed,
        Green => LightGreen,
        Yellow => LightYellow,
        Blue => LightBlue,
        Magenta => LightMagenta,
        Cyan => LightCyan,
        Gray => White,
        DarkGray => Gray,
        Rgb(r, g, b) => {
            let up = |v: u8| (v as f32 + (255.0 - v as f32) * 0.25).round() as u8;
            Rgb(up(r), up(g), up(b))
        }
        other => other,
    }
}

/// DESIGN.md's "breath and pulse": one oscillator that every animated
/// element reads, so the whole interface breathes together instead of
/// each widget inventing its own timer.
///
/// Phase comes from the audio thread's step counter and its sub-step
/// phase — never wall clock — so a brightening border lands on the same
/// sample as the note that caused it. Stopped, there is no audio position
/// to read, so the phase free-runs at the song's tempo off the UI tick;
/// the rate is what the eye reads, and it picks straight back up when the
/// transport rolls.
///
/// `:set still=on` collapses every accessor to a constant, for anyone who
/// would rather the screen held still.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Breath {
    /// Position in 16th-note steps, fractional.
    pos: f32,
    still: bool,
}

impl Breath {
    fn new(app: &App) -> Self {
        let pos = if app.playing {
            app.play_step as f32 + app.viz_frame.step_phase.clamp(0.0, 1.0)
        } else {
            // Steps per second = (bpm / 60) beats × 4 sixteenths.
            app.viz_tick as f32 / 60.0 * (app.song.bpm.max(1) as f32 / 15.0)
        };
        Self { pos, still: app.still }
    }

    /// Smooth 0 → 1 → 0 over a `steps`-long cycle. For things that swell.
    fn wave(&self, steps: f32) -> f32 {
        if self.still {
            return 0.0;
        }
        let frac = (self.pos / steps.max(0.001)).rem_euclid(1.0);
        (1.0 - (frac * std::f32::consts::TAU).cos()) * 0.5
    }

    /// 1.0 on the downbeat of a `steps`-long cycle, decaying to 0 across
    /// it. For things that land on a beat and fade.
    fn pulse(&self, steps: f32) -> f32 {
        if self.still {
            return 0.0;
        }
        let frac = (self.pos / steps.max(0.001)).rem_euclid(1.0);
        (1.0 - frac).powi(2)
    }

    /// Active pane border, brightening on the downbeat of every bar.
    fn pane(&self) -> f32 {
        self.pulse(16.0)
    }

    /// Mode chip, pulsing every half bar (beats 1 and 3).
    fn mode(&self) -> f32 {
        self.pulse(8.0)
    }

    /// Cursor, breathing at half-beat rate.
    fn cursor(&self) -> f32 {
        self.wave(2.0)
    }

    /// `● REC`, breathing once per beat — a breath, not a hard blink.
    fn rec(&self) -> f32 {
        self.wave(4.0)
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

/// Rows the song pane wants: border + arrangement row + chain rows (≤10)
/// + the length/groove summary.
fn song_pane_height(app: &App) -> u16 {
    (4 + app.song.chains.len().min(10)) as u16
}

/// Stage 23: arrangement + chain view with the live slot highlighted.
fn render_song_pane(f: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let live = app.playing && app.song_mode && !app.song.arrangement.is_empty();
    let hot = Style::default().fg(theme.playhead_label).bg(theme.playhead_bg);
    let plain = Style::default().fg(theme.label);
    let mut lines: Vec<Line> = Vec::new();

    let mut arr_spans = vec![Span::styled("ARR ", Style::default().fg(theme.accent))];
    if app.song.arrangement.is_empty() {
        arr_spans.push(Span::styled(" (empty — :arr add NN)", Style::default().fg(theme.dim)));
    }
    for (i, &ci) in app.song.arrangement.iter().enumerate() {
        let style = if live && i == app.viz_frame.arr_slot { hot } else { plain };
        let mark = if i == app.song.arr_loop { "↺" } else { " " };
        arr_spans.push(Span::styled(format!("{}{:02X}", mark, ci), style));
    }
    lines.push(Line::from(arr_spans));

    let play_chain = app.song.arrangement.get(app.viz_frame.arr_slot).copied().unwrap_or(u8::MAX) as usize;
    for (ci, chain) in app.song.chains.iter().enumerate().take(10) {
        let selected = ci == app.current_chain;
        let mut spans = vec![Span::styled(
            format!("{}{:02X} ", if selected { ">" } else { " " }, ci),
            Style::default().fg(if selected { theme.accent } else { theme.label }),
        )];
        for (pos, &pi) in chain.phrases.iter().enumerate() {
            let is_live = live && ci == play_chain && pos == app.viz_frame.chain_pos;
            spans.push(Span::styled(format!(" {:02X}", pi), if is_live { hot } else { plain }));
        }
        if chain.phrases.is_empty() {
            spans.push(Span::styled(" (empty)", Style::default().fg(theme.dim)));
        }
        if let Some(n) = &chain.name {
            spans.push(Span::styled(format!("  {}", n), Style::default().fg(theme.dim)));
        }
        lines.push(Line::from(spans));
    }
    if app.song.chains.len() > 10 {
        lines.push(Line::from(Span::styled(format!(" … {} more", app.song.chains.len() - 10), Style::default().fg(theme.dim))));
    }

    let lens: Vec<String> = (0..CHANNELS).map(|i| format!("{}={}", channel_name(i), app.song.channel_length[i])).collect();
    let groove_on = app.song.has_groove();
    lines.push(Line::from(vec![
        Span::styled("LEN ", Style::default().fg(theme.accent)),
        Span::styled(lens.join(" "), plain),
        Span::raw("   "),
        Span::styled(if groove_on { "GROOVE on" } else { "GROOVE straight" },
            Style::default().fg(if groove_on { theme.accent } else { theme.dim })),
    ]));

    let title = format!(" SONG  {} phrases · {} chains · {} slots ",
        app.song.phrases.len(), app.song.chains.len(), app.song.arrangement.len());
    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(theme.accent)))
        .borders(Borders::ALL);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_phrase(f: &mut Frame, area: Rect, app: &App) {
    let p = app.phrase();
    let theme = &app.theme;
    let breath = Breath::new(app);
    let overlay = app.overlay();
    let mut lines: Vec<Line> = Vec::with_capacity(STEPS_PER_PHRASE + 2);

    // Visual-mode rectangle (inclusive on both axes).
    let selection = if app.mode == Mode::Visual {
        app.visual_anchor.map(|(as_step, as_ch)| {
            let s0 = as_step.min(app.cursor_step);
            let s1 = as_step.max(app.cursor_step);
            let (c0, c1) = if app.visual_linewise {
                (0, CHANNELS - 1)
            } else {
                (as_ch.min(app.cursor_ch), as_ch.max(app.cursor_ch))
            };
            (s0, s1, c0, c1)
        })
    } else {
        None
    };

    // Header. Each column is `NOTE II VV FFF ` = 14 chars + trailing space.
    // LED flash: while playing, any channel that gates on the current step
    // renders its header as a lit chip — DESIGN.md's "channel header letter
    // lights up on trigger." At typical tempos a step is ~100ms, so presence-
    // based highlighting reads as a ~one-step LED blink per hit.
    let mut header = vec![Span::raw("      ")];
    for ch in 0..CHANNELS {
        let base = CH_NAMES[ch];
        let muted = app.muted[ch];
        let label = if muted { format!("{} MUTE", base) } else { base.to_string() };
        // Stage 9: LED flash now reads the actual ADSR level published by the
        // audio thread — lights up for pattern *and* live gates, fades with
        // release. The 0.05 floor kills flicker from deep-release voices.
        let lit = if muted { 0.0 } else { app.viz_frame.voices[ch].env_level.clamp(0.0, 1.0) };
        let triggered = lit > 0.05;
        let mut style = Style::default().add_modifier(Modifier::BOLD);
        if triggered {
            // The chip rides the envelope rather than switching on and off,
            // so a release tail visibly decays instead of cutting.
            style = style.fg(theme.mode_fg).bg(mix(theme.viz_bg, theme.label, 0.35 + 0.65 * lit));
        } else if muted {
            style = Style::default().fg(theme.dim).add_modifier(Modifier::DIM);
            if ch == app.cursor_ch {
                style = style.bg(theme.column_bg);
            }
        } else {
            style = style.fg(theme.label);
            if ch == app.cursor_ch {
                style = style.bg(theme.column_bg);
            }
        }
        // 15 columns, matching a data column exactly (note 5, instr 2, gap 1,
        // vol 2, gap 1, fx 3, trail 1). It read 16 until Stage 27, which drifted
        // every header one column right of its own data, four by the DPCM
        // column. The longest label is "DPCM MUTE" at 9, so 14 + the lead is
        // plenty.
        header.push(Span::styled(format!(" {:<14}", label), style));
    }
    lines.push(Line::from(header));
    lines.push(Line::from(""));

    // DESIGN.md: "the playhead is a character, not a cursor." A diamond
    // travels down the gutter leaving a two-step trail that fades into the
    // ground, the row strikes bright at the top of each step and settles
    // across it (read from the audio thread's sub-step phase, so the strike
    // lands with the note), and a row that actually gates something flashes
    // brighter still.
    let strike = 1.0 - app.viz_frame.step_phase.clamp(0.0, 1.0);
    for (i, row) in p.cells.iter().enumerate() {
        let behind = if app.playing {
            (app.play_step + STEPS_PER_PHRASE - i) % STEPS_PER_PHRASE
        } else {
            usize::MAX
        };
        let row_bg = match behind {
            0 => {
                let gates = row.iter().any(|c| c.note.is_some());
                let heat = 0.20 * strike + if gates { 0.22 } else { 0.0 };
                Some(mix(theme.playhead_bg, theme.playhead_label, heat))
            }
            1 => Some(mix(theme.playhead_bg, theme.viz_bg, 0.55)),
            2 => Some(mix(theme.playhead_bg, theme.viz_bg, 0.80)),
            _ => None,
        };
        let (glyph, label_style) = match behind {
            0 => ('◆', Style::default().fg(theme.playhead_label).add_modifier(Modifier::BOLD)),
            1 => ('◇', Style::default().fg(mix(theme.playhead_label, theme.viz_bg, 0.45))),
            2 => ('·', Style::default().fg(mix(theme.playhead_label, theme.viz_bg, 0.70))),
            _ => (' ', Style::default().fg(theme.dim)),
        };
        // Gutter is a fixed 6 columns (pad, glyph, pad, 2 hex, pad) so the
        // grid lines up with the header's 6-space lead whether or not the
        // playhead is in it.
        let mut spans = vec![Span::styled(format!(" {} {:02X} ", glyph, i), label_style)];
        for (c, authored) in row.iter().enumerate() {
            // A marked cell draws the overlay's content, not what is in the
            // phrase today: a proposed cell shows the note the command would
            // write, a removed one shows the note that is going away.
            let over = overlay.and_then(|o| o.grid[i][c]);
            let cell = &over.map_or(*authored, |m| m.cell);
            let mark = over.map(|m| m.mark);
            let has_note = cell.note.is_some();
            let note_text = note_name(cell.note);
            let instr_text = if has_note {
                format!("{:02X}", cell.instr)
            } else { "--".into() };
            let vol_text = if has_note && cell.vol > 0 {
                format!("{:02X}", cell.vol)
            } else { "--".into() };
            let fx_text = match cell.fx {
                Some((cmd, param)) => format!("{}{:02X}", cmd as char, param),
                None => "---".into(),
            };

            let mut note_color = if has_note { theme.note } else { theme.dim };
            let mut instr_color = if has_note { theme.instr } else { theme.dim };
            let mut vol_color = if has_note && cell.vol > 0 { theme.vol } else { theme.dim };
            let mut fx_color = if cell.fx.is_some() { theme.fx } else { theme.dim };
            // A mark recolors only the fields it concerns, so a volume-only
            // change tints the volume column and leaves the note reading
            // normally.
            if let Some(m) = mark {
                let (fg, _) = theme.mark_colors(m);
                let f = m.fields();
                if f.note { note_color = fg; }
                if f.instr { instr_color = fg; }
                if f.vol { vol_color = fg; }
                if f.fx { fx_color = fg; }
            }

            let in_selection = selection
                .map(|(s0, s1, c0, c1)| i >= s0 && i <= s1 && c >= c0 && c <= c1)
                .unwrap_or(false);
            let is_cursor = i == app.cursor_step && c == app.cursor_ch;
            let in_cursor_col = c == app.cursor_ch;

            // Background precedence: cursor > selection > playhead row > column tint.
            let bg = if is_cursor {
                // Breathes at half-beat rate, never far enough to lose it.
                Some(mix(theme.cursor_bg, brighter(theme.cursor_bg), breath.cursor()))
            } else if in_selection {
                Some(theme.selection_bg)
            } else if let Some(r) = row_bg {
                Some(r)
            } else if in_cursor_col {
                Some(theme.column_bg)
            } else {
                // Phosphor's scanline: alternate rows sit on a faintly
                // lifted ground. Lowest precedence, so it never competes
                // with anything that carries meaning.
                theme.scanline_bg.filter(|_| i % 2 == 1)
            };

            // The overlay composites on top of whatever that produced, so
            // the playhead still sweeps visibly under a ghost or a diff. The
            // cursor is the one thing it will not cover — you must never
            // lose track of where you are.
            let bg = match mark {
                Some(m) if !is_cursor => {
                    let (_, tint) = theme.mark_colors(m);
                    Some(mix(bg.unwrap_or(theme.viz_bg), tint, 0.75))
                }
                _ => bg,
            };

            let ghosting = matches!(mark, Some(Mark::Proposed) | Some(Mark::Vanishing));
            // Entering INSERT narrows attention to the column you are
            // typing into; the others stay legible but recede.
            let unfocused = app.mode == Mode::Insert && !in_cursor_col;
            let apply = |fg: Color| {
                let mut s = Style::default().fg(fg);
                if let Some(b) = bg { s = s.bg(b); }
                if is_cursor { s = s.add_modifier(Modifier::BOLD); }
                // Reinforcement only: DIM is unevenly supported, so the
                // tint and the sigil have to stand on their own.
                if ghosting { s = s.add_modifier(Modifier::DIM | Modifier::ITALIC); }
                if unfocused { s = s.add_modifier(Modifier::DIM); }
                s
            };

            // git puts +/- in the left margin, and so do we. It costs zero
            // columns — the note field already opened with a pad — and it is
            // the one signal that survives a monochrome terminal.
            let sigil = mark.map_or(' ', Mark::sigil);
            spans.push(Span::styled(format!("{}{} ", sigil, note_text), apply(note_color)));
            spans.push(Span::styled(instr_text, apply(instr_color)));
            spans.push(Span::styled(" ".to_string(), apply(theme.dim)));
            spans.push(Span::styled(vol_text, apply(vol_color)));
            spans.push(Span::styled(" ".to_string(), apply(theme.dim)));
            spans.push(Span::styled(fx_text, apply(fx_color)));
            // Trailing spacer between channel columns. Keep the cursor column's
            // tint continuous across it so the "you are here" bar is unbroken.
            let trail_style = if in_cursor_col && !is_cursor {
                let mut s = Style::default();
                if let Some(b) = bg { s = s.bg(b); }
                s
            } else {
                Style::default()
            };
            spans.push(Span::styled(" ".to_string(), trail_style));
        }
        lines.push(Line::from(spans));
    }

    let file_label = match &app.current_file {
        Some(p) => p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string()),
        None => "[no name]".into(),
    };
    let dirty = if app.dirty { " [+]" } else { "" };
    let ch_name = CH_NAMES[app.cursor_ch.min(CHANNELS - 1)];
    let cursor_cell = app.phrase().cells[app.cursor_step][app.cursor_ch];
    let (instr_txt, vol_txt) = match cursor_cell.note {
        Some(_) => (
            format!("i{:02X}", cursor_cell.instr),
            format!("v{:02X}", cursor_cell.vol),
        ),
        None => ("i--".to_string(), "v--".to_string()),
    };
    let title = format!(
        " {}{}   PHRASE {:02X}/{:02X}   {} {} {}   {} BPM   {} ",
        file_label,
        dirty,
        app.song.current_phrase,
        app.song.phrases.len().saturating_sub(1),
        ch_name, instr_txt, vol_txt,
        app.song.bpm,
        if app.playing { "● PLAY" } else { "■ STOP" },
    );
    // An overlay is a mode you can be in without having asked for it to
    // persist, so the pane says which one and, for a diff, between what.
    let title = match overlay {
        Some(o) => format!("{}{}  ", title, o.title),
        None => title,
    };
    // The frame brightens on the downbeat of every bar — the whole pane
    // taking a breath with the music — and Live mode outlines it in red,
    // so the mode is visible from the edge of vision rather than only in
    // the modeline.
    let base = if app.mode == Mode::Live { theme.mode_live } else { theme.hint };
    let lit = if app.mode == Mode::Live { theme.mode_live } else { theme.accent };
    // DESIGN.md's "the viz bleeds into the UI": the border flashes on a
    // snare. The other half of that idea — the row tinting toward red when
    // a note is dissonant against the root — needs a notion of consonance
    // the app does not have, so it is not built.
    let snare = app.viz_frame.voices[3].env_level.clamp(0.0, 1.0);
    let border = mix(mix(base, lit, breath.pane()), theme.playhead_label, 0.7 * snare);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_help(f: &mut Frame, area: Rect, theme: &Theme) {
    let section = |title: &str| Line::from(Span::styled(
        title.to_string(),
        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
    ));
    let row = |keys: &str, desc: &str| Line::from(vec![
        Span::styled(format!("  {:<14}", keys), Style::default().fg(theme.instr)),
        Span::raw(desc.to_string()),
    ]);

    let lines = vec![
        section("Normal mode"),
        row("h j k l / ←↓↑→", "move cursor (prefix count, e.g. 4j)"),
        row("w / b",          "next / prev bar (4 steps)"),
        row("0 / $",           "first / last channel (PU1 ↔ NOI)"),
        row("g / G",            "top / bottom of phrase"),
        row("x",                "clear cell (count: Nx clears N cells down column)"),
        row("dd / yy / cc",     "delete / yank / change current step row"),
        row("dab / yab / cab",  "delete / yank / change current bar (4 steps)"),
        row("dip / yip / cip",  "delete / yank / change whole phrase"),
        row("div / yiv / civ",  "delete / yank / change current channel column"),
        row("p / P",           "paste after / at cursor"),
        row(".",                "repeat last delete / paste / x"),
        row("u / Ctrl-r",       "undo / redo (200-step history)"),
        row("r<key>",           "replace cell's note with next piano-row key"),
        row("v / V",            "visual block / linewise selection (d/y/c/x apply)"),
        row("{ / }",           "previous / next phrase"),
        row("i",               "insert mode"),
        row("a",               "append (move down, then insert)"),
        row(":",               "command mode"),
        row("space",           "toggle play"),
        row("Esc",             "cancel pending count / operator"),
        row("? / F1",          "toggle this help"),
        row("F2 / :inst",      "instrument editor"),
        row("K",               "live keyboard monitor (piano row plays through audio)"),
        row("R",               "toggle record-arm on current channel (● REC badge appears)"),
        row("M",               "mute / unmute current channel"),
        row("q<letter>",       "record performance macro into register (q again to stop)"),
        row("@<letter> / @@",  "play back macro / replay last"),
        row("Esc (normal)",    "also disarms recording / cancels macro record"),
        row("ZZ",              "save and quit"),
        row("ZQ / Ctrl-q",     "quit without saving"),
        Line::from(""),
        section("Live mode (K) — play notes in realtime, optionally recording"),
        row("z s x d c v …",   "piano row triggers notes on current channel"),
        row("Tab / ← →",       "switch channel"),
        row("< / >",           "octave down / up"),
        row("R",               "arm / disarm recording on current channel"),
        row("M",               "mute / unmute current channel"),
        row("space",           "toggle transport playback"),
        row("Backspace",       "release current channel"),
        row("Esc",             "all notes off, back to normal"),
        row("(while armed)",   "piano keys write cell at playhead (playing) or cursor (stopped)"),
        row("1 … 9",           "launch scene N (queued at next bar while playing, immediate when stopped)"),
        row("q / @",           "record / replay performance macro (mutes, scenes, transpose, play)"),
        Line::from(""),
        section("Insert mode — bottom row = chromatic, base octave shiftable"),
        row("z s x d c v",     "C  C# D  D# E  F"),
        row("g b h n j m",     "F# G  G# A  A# B"),
        row(", l . ; /",       "continue into next octave"),
        row("< / >",           "octave down / up (0–8, default 4)"),
        row("Backspace",       "clear cell and move up"),
        row("Esc",             "back to normal"),
        Line::from(""),
        section("Command mode"),
        row(":q / :q!",        "quit"),
        row(":help",           "open this help"),
        row(":inst [NN]",      "instrument editor (hex index 00-0F)"),
        row(":set bpm=140",    "set tempo"),
        row(":set step=4",     "auto-advance N steps per inserted note"),
        row(":set octave=4",   "base octave for insert-mode piano row (0–8)"),
        row(":set theme=nes",  "color theme (nes / phosphor)"),
        row(":set still=on",   "freeze the tempo-locked breathing animations (off / toggle)"),
        row(":diff [A] B",     "compare phrase A (or the current one) against B; :diff off dismisses"),
        row(":play / :stop",   "transport"),
        row(":rec / :rec off",  "toggle record-arm / disarm all channels"),
        row(":mute [N]",        "toggle mute on cursor channel (or N: 1-5 / pu1..dpcm)"),
        row(":order [A,B,..]",  "show / set song order (hex phrase indices); :order off clears"),
        row(":song on|off",     "song mode: play through :order (on) or loop the phrase (off)"),
        row(":song",            "toggle the song pane (arrangement + chains)"),
        row(":chain new/del/sel NN", "create / delete / select a chain ('>' marks the selected one)"),
        row(":chain add NN / pop", "append phrase NN to the selected chain / drop its last phrase"),
        row(":arr add NN / del [pos]", "append chain NN to the arrangement / drop a slot"),
        row(":arr loop pos / clear", "set the loop slot / empty the arrangement"),
        row(":len <ch> N",      "polymeter: channel wraps every N steps (1–16); :len all N"),
        row(":groove swing N",  "±N-sample swing on alternate 16ths; :groove straight; :groove <16 ints>"),
        row(":engine apu|synth","play through the compiled NSF + 2A03 emulation, or the synth"),
        row(":driver BIN SYM",  "set the NSF driver (paths relative to the .vip)"),
        row(":compile PATH",    "compile the song to an NSF against the @driver"),
        row(":unmute [N|off]",  "unmute specific / all channels"),
        row(":viz [kind]",      "toggle viz pane (bars / scope / grid / orbit / sprites / register)"),
        row(":sprite load P WxH [q]", "load PNG sheet (≤3 opaque colors, or 'q' to quantize)"),
        row(":sprite place N I x y", "place sheet N's tile I at viz pixel (x,y)"),
        row(":sprite palette N c0 c1 c2 c3", "define named palette (hex or 'transparent')"),
        row(":sprite repalette N P", "apply palette P to sheet N"),
        row(":sprite list / clear", "list loaded sheets / remove placements"),
        row(":bind S[.N|*] T = EXPR", "modulate sprite S (bare=all placements): T = x/y/scale/flipx/flipy/frame/visible/rotate/hue/sat/value/palette"),
        row("  sources", "<ch>.env/.pitch/.gate/.vel/.age, master.rms, step/beat/bar/time/tempo/scene.index"),
        row(":bind list / clear / del N", "inspect / drop all / remove binding N"),
        row(":scene N save",    "bind current phrase to slot N (1-9)"),
        row(":scene N",         "queue / launch scene N (clears slot with :scene N clear)"),
        row(":scene off",       "cancel queued scene launch"),
        row(":w [path]",       "save song as .vip (path required first time)"),
        row(":e <path>",       "load .vip (or start new file at path if missing)"),
        row(":new",            "start a new empty song (unsets filename)"),
        row("Tab in :w / :e",   "complete file path (longest common prefix)"),
        row(":vol NN",          "set cursor cell velocity (hex 00–0F; 00 = default/full)"),
        row(":fx CPP",          "set cursor cell effect (e.g. :fx A04) / :fx off clears"),
        row(":transpose ±N",    "shift all pitched notes in phrase by N semitones (skips NOI)"),
        row(":wq [path]",      "save and quit"),
        row(":phrase [NN]",    "show / switch phrase (hex index)"),
        row(":phrase new",     "append a new empty phrase"),
        row(":phrase del",     "delete current phrase"),
        Line::from(""),
        section("Generators"),
        row(":gen four",       "kick/snare/hat on NOI"),
        row(":gen euclid …",   "<ch> <k> <n> [off] — Euclidean rhythm"),
        row(":gen scale …",    "<ch> <key> [mode] [density] — random in scale"),
        row(":gen chord_prog …", "<preset|i iv V i> [key=Am] [steps=4] — voiced on PU1/PU2/TRI + hats"),
        row(":gen bassline …", "<preset|chords> [style=walking|arpeggio|root_fifth|octaves] [key=Am]"),
        row(":gen arp …",      "<chord> [up|down|updown|random] [len] [rate=1] [ch=pu2] [octaves=2]"),
        row(":gen drums …",    "<four|breakbeat|amen|trap|gameboy|dnb|halftime|dbeat|blast> [fills=N] [dpcm=off]"),
        row(":gen lsystem …",  "axiom=A rules=A=ABA,B=.A. [iterations=4] [map=A=C4,B=G3,.=-] [ch=pu1]"),
        row(":gen cellular …", "[rule=30] [ch=pu1] [key=Am] [seed=center|random] — Wolfram automaton"),
        row(":gen style DIR [seed]", "compose a whole song from a style directory"),
        Line::from(""),
        Line::from(Span::styled(
            "  press q, Esc, or ? to close help",
            Style::default().fg(theme.hint).add_modifier(Modifier::ITALIC),
        )),
    ];

    let block = Block::default()
        .title(" HELP ")
        .borders(Borders::ALL);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

#[derive(Clone, Copy)]
struct SplashParticle {
    x: f32,
    y: f32,
    vy: f32,
    age: u32,
    lifetime: u32,
    glyph: char,
}

const SPLASH_GLYPHS: [char; 4] = ['♪', '♫', '♩', '♬'];

fn render_splash(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    particles: &[SplashParticle],
) {
    // Keep each line exactly 80 cells wide so the right border lines up.
    const ART: &[&str] = &[
        "╔══════════════════════════════════════════════════════════════════════════════╗",
        "║                                                                              ║",
        "║    ██╗   ██╗██╗██████╗ ███████╗██████╗                                       ║",
        "║    ██║   ██║██║██╔══██╗██╔════╝██╔══██╗                                      ║",
        "║    ██║   ██║██║██████╔╝█████╗  ██████╔╝                                      ║",
        "║    ╚██╗ ██╔╝██║██╔═══╝ ██╔══╝  ██╔══██╗                                      ║",
        "║     ╚████╔╝ ██║██║     ███████╗██║  ██║                                      ║",
        "║      ╚═══╝  ╚═╝╚═╝     ╚══════╝╚═╝  ╚═╝                                      ║",
        "║           ___                                                                ║",
        "║      ___ /   \\          ┌───┐   ┌───┐   ┌───┐   ┌───┐   ┌───┐                ║",
        "║     (o o)     \\--──────┐│   │   │   │   │   │   │   │   │   │                ║",
        "║      \\_/               ││   │   │   │   │   │   │   │   │   │                ║",
        "║       ^                └┘   └───┘   └───┘   └───┘   └───┘   └───┘            ║",
        "║                                                                              ║",
        "║               ── a VIm-keybound chiptune stepPER sequencer ──                ║",
        "║                                                                              ║",
        "╚══════════════════════════════════════════════════════════════════════════════╝",
    ];

    let border = Style::default().fg(theme.splash_logo);
    let logo = border.add_modifier(Modifier::BOLD);
    let snake = Style::default().fg(theme.splash_snake);
    let base = Style::default().fg(theme.splash_base);
    let dim = Style::default().fg(theme.hint).add_modifier(Modifier::ITALIC);

    let styled: Vec<Line> = ART
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let style = match i {
                0 | 16 => logo,                    // top / bottom border
                2..=7 => logo,                     // VIPER logo rows
                8..=11 => snake,                   // snake head
                12 => base,                        // keyboard base (snake body)
                14 => dim,                         // tagline
                _ => border,                       // blank border rows
            };
            Line::from(Span::styled((*s).to_string(), style))
        })
        .collect();

    let mut lines = styled;
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "                             press any key to begin",
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

    // Overlay floating music notes — drawn after the box, but we skip cells
    // covered by `inner` so they only appear in the margin around the splash.
    let buf = f.buffer_mut();
    let ax = area.x as i32;
    let ay = area.y as i32;
    let ax_end = ax + area.width as i32;
    let ay_end = ay + area.height as i32;
    let ix = inner.x as i32;
    let iy = inner.y as i32;
    let ix_end = ix + inner.width as i32;
    let iy_end = iy + inner.height as i32;
    for p in particles {
        let cx = ax + p.x as i32;
        let cy = ay + p.y.round() as i32;
        if cx < ax || cx >= ax_end || cy < ay || cy >= ay_end {
            continue;
        }
        if cx >= ix && cx < ix_end && cy >= iy && cy < iy_end {
            continue;
        }
        let t = (p.age as f32 / p.lifetime.max(1) as f32).clamp(0.0, 1.0);
        let k = 1.0 - t;
        let r = (255.0 * k) as u8;
        let g = (200.0 * k) as u8;
        let b = (90.0 * k) as u8;
        buf.set_string(
            cx as u16,
            cy as u16,
            p.glyph.to_string(),
            Style::default().fg(Color::Rgb(r, g, b)),
        );
    }
}

fn render_instrument(f: &mut Frame, area: Rect, app: &App) {
    let idx = app.selected_instr as usize;
    let inst = app.song.instruments[idx];
    let theme = &app.theme;

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        format!("  INSTRUMENT {:02X}", idx),
        Style::default().fg(theme.instr_title).add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(""));

    for (i, name) in INSTR_PARAM_NAMES.iter().enumerate() {
        let sel = i == app.instr_param;
        let marker = if sel { ">" } else { " " };
        let style = if sel {
            Style::default().fg(theme.instr_row_fg).bg(theme.instr_row_bg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.instr_label)
        };
        lines.push(Line::from(vec![
            Span::raw(format!("  {} ", marker)),
            Span::styled(format!("{:<8}", name), style),
            Span::raw("  "),
            Span::styled(inst.display(i), Style::default().fg(theme.instr_value)),
        ]));
    }

    let block = Block::default()
        .title(format!(" INSTRUMENT EDITOR  (current: {:02X}) ", idx))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // The hint spans the full width along the bottom; splitting it into the
    // parameter column would clip it mid-sentence.
    let (body, hint_area) = if inner.height > 2 {
        let v = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);
        (v[0], Some(v[1]))
    } else {
        (inner, None)
    };

    // The parameter list keeps the left; the graphs take whatever is left
    // over. Below ~54 columns there is no room for a legible curve, so the
    // list simply gets the whole pane — the editor still works, it just
    // stops drawing pictures.
    let (list_area, graph_area) = if body.width >= 54 {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(32), Constraint::Min(20)])
            .split(body);
        (split[0], Some(split[1]))
    } else {
        (body, None)
    };
    f.render_widget(Paragraph::new(lines), list_area);
    if let Some(h) = hint_area {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  j/k select · h/l or -/+ adjust · [ ] prev/next instr · Esc back",
                Style::default().fg(theme.hint).add_modifier(Modifier::ITALIC),
            ))),
            h,
        );
    }

    if let Some(g) = graph_area {
        render_instrument_graphs(f, g, app, inst);
    }
}

/// The ADSR envelope and the waveform it shapes, drawn in half-blocks.
///
/// The segment belonging to the selected parameter is drawn in the accent
/// colour, so `h`/`l` on `attack` visibly moves that ramp. That connection
/// is real; a "the envelope lights up as a note plays" animation is not,
/// because an instrument is not bound to a channel anywhere in the model,
/// so there is no honest way to know which voice is sounding this one.
fn render_instrument_graphs(f: &mut Frame, area: Rect, app: &App, inst: Instrument) {
    let theme = &app.theme;
    if area.height < 6 || area.width < 12 {
        return;
    }
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Min(2)])
        .split(area);

    let label = |t: &str| Line::from(Span::styled(t.to_string(), Style::default().fg(theme.instr_label)));
    f.render_widget(Paragraph::new(label(" envelope")), split[0]);
    f.render_widget(Paragraph::new(label(" waveform")), split[2]);

    // ---- envelope ----
    let mut c = viz::HalfBlock::new(split[1].width, split[1].height);
    let (w, h) = (c.width() as i32, c.height() as i32);
    if w > 4 && h > 1 {
        // Time is shared out proportionally between the three timed stages,
        // with a quarter of the width reserved for the sustain hold so a
        // held note is visible even when the ramps are long.
        let hold = (w / 4).max(1);
        let avail = (w - hold).max(1);
        let (ta, td, tr) = (inst.attack_ms as f32, inst.decay_ms as f32, inst.release_ms as f32);
        let sum = (ta + td + tr).max(1.0);
        let ca = (avail as f32 * ta / sum).round() as i32;
        let cd = (avail as f32 * td / sum).round() as i32;
        let cr = (avail - ca - cd).max(0);
        let peak = inst.volume.clamp(0.0, 1.0);
        let sus = (inst.sustain.clamp(0.0, 1.0) * peak).clamp(0.0, 1.0);
        // Envelope amplitude → pixel row, with 0 at the bottom.
        let y_of = |a: f32| ((1.0 - a) * (h - 1) as f32).round() as i32;

        let seg_color = |seg: usize| {
            if seg == app.instr_param { theme.accent } else { theme.instr_value }
        };
        let mut x = 0;
        let mut prev = y_of(0.0);
        // Four segments, indexed to match INSTR_PARAM_NAMES: 0 attack,
        // 1 decay, 2 sustain, 3 release.
        let stages: [(i32, f32, f32, usize); 4] = [
            (ca, 0.0, peak, 0),
            (cd, peak, sus, 1),
            (hold, sus, sus, 2),
            (cr, sus, 0.0, 3),
        ];
        for (cols, from, to, seg) in stages {
            let color = seg_color(seg);
            for i in 0..cols {
                let t = if cols <= 1 { 1.0 } else { i as f32 / (cols - 1) as f32 };
                let y = y_of(from + (to - from) * t);
                // Join to the previous sample so the curve reads as a line
                // rather than a dotted scatter on steep ramps.
                c.column(x, prev, y, color);
                prev = y;
                x += 1;
            }
            if cols == 0 {
                // A zero-length stage is a vertical jump, which is exactly
                // what a 0 ms attack sounds like.
                let y = y_of(to);
                c.column(x, prev, y, color);
                prev = y;
            }
        }
        f.render_widget(Paragraph::new(c.lines()), split[1]);
    }

    // ---- waveform ----
    let mut c = viz::HalfBlock::new(split[3].width, split[3].height);
    let (w, h) = (c.width() as i32, c.height() as i32);
    if w > 3 && h > 1 {
        let duty = inst.duty.clamp(0.05, 0.95);
        let amp = inst.volume.clamp(0.0, 1.0);
        let cycles = 2;
        let period = (w as f32 / cycles as f32).max(2.0);
        let hi = ((0.5 - 0.5 * amp) * (h - 1) as f32).round() as i32;
        let lo = ((0.5 + 0.5 * amp) * (h - 1) as f32).round() as i32;
        let color = if app.instr_param == 4 || app.instr_param == 5 { theme.accent } else { theme.instr_value };
        let mut prev: Option<i32> = None;
        for x in 0..w {
            let phase = (x as f32 % period) / period;
            let y = if phase < duty { hi } else { lo };
            // Draw the vertical edge where the level flips, so a square
            // wave looks square instead of two dashed lines.
            match prev {
                Some(p) if p != y => c.column(x, p, y, color),
                _ => c.set(x, y, color),
            }
            prev = Some(y);
        }
        f.render_widget(Paragraph::new(c.lines()), split[3]);
    }
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let mode_str = match app.mode {
        Mode::Normal => "NORMAL",
        Mode::Insert => "INSERT",
        Mode::Visual => "VISUAL",
        Mode::Command => "COMMAND",
        Mode::Help => "HELP",
        Mode::Instrument => "INSTR",
        Mode::Live => "LIVE",
    };
    let theme = &app.theme;
    let mode_color = match app.mode {
        Mode::Normal => theme.mode_normal,
        Mode::Insert => theme.mode_insert,
        Mode::Visual => theme.mode_visual,
        Mode::Command => theme.mode_command,
        Mode::Help => theme.mode_help,
        Mode::Instrument => theme.mode_instr,
        Mode::Live => theme.mode_live,
    };

    let breath = Breath::new(app);
    let mut left_spans = vec![
        // The mode chip pulses on beats 1 and 3.
        Span::styled(format!(" {} ", mode_str),
            Style::default()
                .bg(mix(mode_color, brighter(mode_color), breath.mode()))
                .fg(theme.mode_fg)
                .add_modifier(Modifier::BOLD)),
    ];
    // Stage 6: ● REC badge listing armed channels, pulsing red while playing.
    if app.any_recording() {
        let armed: Vec<&'static str> = (0..CHANNELS)
            .filter(|&i| app.recording[i])
            .map(channel_name)
            .collect();
        left_spans.push(Span::raw(" "));
        // A breath once per beat rather than a hard blink.
        left_spans.push(Span::styled(
            format!(" ● REC {} ", armed.join(" ")),
            Style::default()
                .bg(mix(theme.mode_live, brighter(theme.mode_live), breath.rec()))
                .fg(theme.mode_fg)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Stage 8: macro-recording badge. `q<letter>` is on, captured op count ticks up.
    if let Some((letter, ops)) = app.recording_macro.as_ref() {
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!(" ◉ q{} ({}) ", letter, ops.len()),
            Style::default().bg(theme.mode_live).fg(theme.mode_fg).add_modifier(Modifier::BOLD),
        ));
    }
    // Stage 7: queued-scene badge with live step countdown.
    if let Some(slot) = app.queued_scene {
        let wait = steps_to_next_bar(app.play_step);
        let target = app.scenes[slot].map_or("??".into(), |p| format!("{:02X}", p));
        left_spans.push(Span::raw(" "));
        left_spans.push(Span::styled(
            format!(" ▸ {} → {} ", slot + 1, target),
            Style::default().bg(theme.accent).fg(theme.mode_fg).add_modifier(Modifier::BOLD),
        ));
        // DESIGN.md's queue-drain: the countdown as a bar that empties, so
        // the commit point is something you watch arrive rather than a
        // number you have to read. The remaining fraction comes off the
        // sub-step phase as well as the step count, so it drains smoothly
        // instead of in four jerks.
        const DRAIN_W: usize = 8;
        let frac = drain_fraction(wait, app.viz_frame.step_phase);
        let full = (frac * DRAIN_W as f32).round() as usize;
        left_spans.push(Span::styled(
            "█".repeat(full),
            Style::default().fg(theme.accent),
        ));
        left_spans.push(Span::styled(
            "░".repeat(DRAIN_W - full),
            Style::default().fg(theme.dim),
        ));
    }
    left_spans.push(Span::raw("  "));
    left_spans.push(Span::raw(&app.status));
    let left = Line::from(left_spans);
    let right_line = if app.mode == Mode::Command {
        let buf = &app.command_buf;
        let cur = app.command_cursor.min(buf.len());
        let (before, rest) = buf.split_at(cur);
        let (at, after) = match rest.chars().next() {
            Some(c) => rest.split_at(c.len_utf8()),
            None => (" ", ""),
        };
        let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
        Line::from(vec![
            Span::raw(":"),
            Span::raw(before.to_string()),
            Span::styled(at.to_string(), cursor_style),
            Span::raw(after.to_string()),
        ])
    } else if app.count > 0 || app.pending != Pending::None {
        Line::from(format!("{}{}",
            if app.count > 0 { app.count.to_string() } else { String::new() },
            app.pending.display()))
    } else if app.event_log.len() > 1 {
        // Idle: a rolling log of what just happened, oldest to newest so it
        // reads left to right. The newest entry is already the status line
        // above, so it is skipped rather than shown twice.
        let recent: Vec<String> = app.event_log.iter().skip(1).take(4).rev().cloned().collect();
        Line::from(Span::styled(
            format!("  {} ·", recent.join("  ·  ")),
            Style::default().fg(theme.hint),
        ))
    } else {
        Line::from(String::new())
    };
    let content = vec![left, right_line];
    f.render_widget(Paragraph::new(content), area);
}

fn ui(f: &mut Frame, app: &App) {
    if app.show_splash {
        render_splash(f, f.area(), &app.theme, &app.splash_particles);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(2)])
        .split(f.area());
    // Stage 10: when the viz pane is on, split the main area horizontally.
    // Help and Instrument full-screens skip the split — those modes are
    // modal takeovers of the whole working surface.
    let is_fullscreen_mode = matches!(app.mode, Mode::Help | Mode::Instrument);
    // Phrase needs ~65 cols to render without truncating; give the viz pane
    // the rest. On a typical 120-col terminal that yields ≈55 cols of viz.
    let (main_area, viz_area) = if app.show_viz && !is_fullscreen_mode && chunks[0].width >= 115 {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(65), Constraint::Length(50)])
            .split(chunks[0]);
        (split[0], Some(split[1]))
    } else {
        (chunks[0], None)
    };
    match app.mode {
        Mode::Help => render_help(f, chunks[0], &app.theme),
        Mode::Instrument => render_instrument(f, chunks[0], app),
        _ => {
            // Stage 23: the song pane sits above the grid when toggled on.
            if app.show_song && main_area.height >= 14 {
                let rows = song_pane_height(app).min(main_area.height.saturating_sub(10));
                let split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(rows), Constraint::Min(10)])
                    .split(main_area);
                render_song_pane(f, split[0], app);
                render_phrase(f, split[1], app);
            } else {
                render_phrase(f, main_area, app);
            }
        }
    }
    if let Some(area) = viz_area {
        let ctx = viz::VizCtx {
            frame: &app.viz_frame,
            tick: app.viz_tick,
            sheets: &app.sprite_sheets,
            placements: &app.effective_placements,
            palettes: &app.sprite_palettes,
            bg: app.theme.viz_bg,
            register: &app.register,
            fg: app.theme.note,
            dim: app.theme.dim,
        };
        viz::render(f, area, app.viz_kind, &ctx);
    }
    render_status(f, chunks[1], app);
}

// ---------- Input handling ----------

fn handle_key(app: &mut App, key: KeyEvent) {
    if app.show_splash {
        app.show_splash = false;
        app.playing = false;
        return;
    }
    match app.mode {
        Mode::Normal => handle_normal(app, key),
        Mode::Insert => handle_insert(app, key),
        Mode::Live => handle_live(app, key),
        Mode::Command => handle_command(app, key),
        Mode::Visual => handle_visual(app, key),
        Mode::Help => handle_help(app, key),
        Mode::Instrument => handle_instrument(app, key),
    }
}

fn handle_visual(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('v') => {
            app.mode = Mode::Normal;
            app.visual_anchor = None;
            app.visual_linewise = false;
            app.count = 0;
            app.status = "".into();
        }
        // Toggle linewise on/off without leaving visual mode.
        KeyCode::Char('V') => {
            app.visual_linewise = !app.visual_linewise;
            app.status = if app.visual_linewise { "-- VISUAL LINE --".into() } else { "-- VISUAL --".into() };
        }
        KeyCode::Char(c) if c.is_ascii_digit() && !(c == '0' && app.count == 0) => {
            app.count = app.count * 10 + c.to_digit(10).unwrap();
        }
        KeyCode::Char('j') | KeyCode::Down  => { let n = app.take_count(); app.motion_j(n); }
        KeyCode::Char('k') | KeyCode::Up    => { let n = app.take_count(); app.motion_k(n); }
        KeyCode::Char('h') | KeyCode::Left  => { let n = app.take_count(); app.motion_h(n); }
        KeyCode::Char('l') | KeyCode::Right => { let n = app.take_count(); app.motion_l(n); }
        KeyCode::Char('w') => { app.motion_j(4); }
        KeyCode::Char('b') => { app.motion_k(4); }
        KeyCode::Char('0') => { app.cursor_ch = 0; app.count = 0; }
        KeyCode::Char('$') => { app.cursor_ch = CHANNELS - 1; }
        KeyCode::Char('g') => { app.cursor_step = 0; }
        KeyCode::Char('G') => { app.cursor_step = STEPS_PER_PHRASE - 1; }
        // Operators act on the rectangle.
        KeyCode::Char('d') | KeyCode::Char('x') => { app.op_visual('d'); }
        KeyCode::Char('y') => { app.op_visual('y'); }
        KeyCode::Char('c') => { app.op_visual('c'); }
        _ => {}
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
            app.snapshot();
            let p = app.instr_param;
            app.song.instruments[app.selected_instr as usize].adjust(p, -1);
        }
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Char('+')
            | KeyCode::Char('=') => {
            app.snapshot();
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
            // `ZZ` saves and quits; `ZQ` quits without saving. Anything else
            // cancels and re-interprets the key normally.
            match key.code {
                KeyCode::Char('Z') => {
                    app.pending = Pending::None;
                    save_and_quit(app);
                    return;
                }
                KeyCode::Char('Q') => {
                    app.pending = Pending::None;
                    app.quit = true;
                    return;
                }
                _ => {
                    app.pending = Pending::None;
                }
            }
        }
        Pending::Replace => {
            app.pending = Pending::None;
            match key.code {
                KeyCode::Esc => { app.status = "cancelled".into(); }
                KeyCode::Char(c) => {
                    if let Some(note) = App::piano_row_note(c, app.insert_octave) {
                        app.snapshot();
                        let (s, ch) = (app.cursor_step, app.cursor_ch);
                        let instr = app.selected_instr;
                        let cell = &mut app.phrase_mut().cells[s][ch];
                        cell.note = Some(note);
                        cell.instr = instr;
                        app.status = format!(
                            "replaced [{:02X},ch{}] with {}",
                            s, ch + 1, note_name(Some(note)),
                        );
                    } else {
                        app.status = format!("r: not a piano-row key: {:?}", c);
                    }
                }
                _ => { app.status = "r: expected piano-row key".into(); }
            }
            return;
        }
        Pending::MacroRecord => {
            app.pending = Pending::None;
            match key.code {
                KeyCode::Esc => { app.status = "cancelled".into(); }
                KeyCode::Char(c) if c.is_ascii_alphabetic() => {
                    let letter = c.to_ascii_lowercase();
                    app.recording_macro = Some((letter, Vec::new()));
                    app.status = format!("q{}: recording macro (press q to stop)", letter);
                }
                _ => { app.status = "q: expected register letter (a-z)".into(); }
            }
            return;
        }
        Pending::MacroPlay => {
            app.pending = Pending::None;
            let letter = match key.code {
                KeyCode::Esc => { app.status = "cancelled".into(); return; }
                KeyCode::Char('@') => match app.last_macro {
                    Some(l) => l,
                    None => { app.status = "@@: no previous macro".into(); return; }
                },
                KeyCode::Char(c) if c.is_ascii_alphabetic() => c.to_ascii_lowercase(),
                _ => { app.status = "@: expected register letter (a-z)".into(); return; }
            };
            play_macro(app, letter);
            return;
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
        KeyCode::Char('0') => { app.cursor_ch = 0; app.count = 0; }
        KeyCode::Char('$') => { app.cursor_ch = CHANNELS - 1; }
        KeyCode::Char('g') => { app.cursor_step = 0; }
        KeyCode::Char('G') => { app.cursor_step = STEPS_PER_PHRASE - 1; }
        KeyCode::Char('w') => { app.motion_j(4); }
        KeyCode::Char('b') => { app.motion_k(4); }

        KeyCode::Char('x') => {
            let n = app.take_count().max(1);
            for _ in 0..n {
                app.op_delete_cell();
                if app.cursor_step + 1 >= STEPS_PER_PHRASE { break; }
                app.motion_j(1);
            }
        }
        KeyCode::Char('d') => { app.pending = Pending::Op('d'); }
        KeyCode::Char('y') => { app.pending = Pending::Op('y'); }
        KeyCode::Char('c') => { app.pending = Pending::Op('c'); }
        KeyCode::Char('p') => { app.paste(true); }
        KeyCode::Char('P') => { app.paste(false); }
        KeyCode::Char('.') => { app.replay_last_action(); }

        KeyCode::Char('i') => {
            app.snapshot();
            app.mode = Mode::Insert;
            app.status = "-- INSERT --".into();
        }
        KeyCode::Char('a') => {
            app.snapshot();
            app.motion_j(1);
            app.mode = Mode::Insert;
            app.status = "-- INSERT (append) --".into();
        }
        KeyCode::Char('u') => { app.undo(); }
        KeyCode::Char('r') if ctrl => { app.redo(); }
        KeyCode::Char('r') => {
            app.pending = Pending::Replace;
            app.status = "r — press a piano-row key to replace the cell".into();
        }
        KeyCode::Char('v') => {
            app.mode = Mode::Visual;
            app.visual_anchor = Some((app.cursor_step, app.cursor_ch));
            app.visual_linewise = false;
            app.status = "-- VISUAL --".into();
        }
        KeyCode::Char('V') => {
            app.mode = Mode::Visual;
            app.visual_anchor = Some((app.cursor_step, app.cursor_ch));
            app.visual_linewise = true;
            app.status = "-- VISUAL LINE --".into();
        }
        KeyCode::Char('{') => { prev_phrase(app); }
        KeyCode::Char('}') => { next_phrase(app); }
        KeyCode::Char(':') => {
            app.mode = Mode::Command;
            app.command_buf.clear();
            app.command_cursor = 0;
        }
        KeyCode::Char(' ') => { perform(app, MacroOp::TogglePlay); }
        KeyCode::Char('q') if ctrl => { app.quit = true; }
        KeyCode::Char('?') | KeyCode::F(1) => {
            app.mode = Mode::Help;
            app.status = "help — q/Esc/? to close".into();
        }
        KeyCode::F(2) => { enter_instrument_mode(app); }
        KeyCode::Char('K') => { enter_live_mode(app); }
        KeyCode::Char('R') => { toggle_record_arm(app); }
        KeyCode::Char('M') => { perform(app, MacroOp::ToggleMute(app.cursor_ch)); }
        KeyCode::Char('q') => { toggle_macro_record(app); }
        KeyCode::Char('@') => { app.pending = Pending::MacroPlay; }
        KeyCode::Char('Z') => { app.pending = Pending::Z; }
        KeyCode::Esc => {
            // Esc cancels any pending count and clears transient status.
            // It also disarms any record-armed channels so there's a cheap
            // "stop everything" escape hatch from Normal.
            app.pending = Pending::None;
            app.count = 0;
            if app.diff.take().is_some() {
                // A standing diff is a mode you can forget you are in; Esc
                // is where hands already go to get out of one.
                app.status = "diff off".into();
            } else if app.recording_macro.is_some() {
                let (letter, _) = app.recording_macro.take().unwrap();
                app.status = format!("q{}: recording cancelled", letter);
            } else if app.any_recording() {
                app.disarm_all();
                app.status = "rec disarmed".into();
            } else {
                app.status = "".into();
            }
        }
        _ => {}
    }
}

fn handle_pending_op(app: &mut App, op: char, key: KeyEvent) {
    match key.code {
        // Extra digits after the operator extend the count (`d3d`, like vim's `3dd`).
        KeyCode::Char(c) if c.is_ascii_digit() && !(c == '0' && app.count == 0) => {
            app.count = app.count * 10 + c.to_digit(10).unwrap();
        }
        KeyCode::Char(c) if c == op => {
            app.pending = Pending::None;
            let n = app.take_count();
            app.op_row(op, n);
        }
        KeyCode::Char('a') => { app.pending = Pending::OpScope(op, 'a'); }
        KeyCode::Char('i') => { app.pending = Pending::OpScope(op, 'i'); }
        KeyCode::Esc => {
            app.pending = Pending::None;
            app.count = 0;
            app.status = "".into();
        }
        _ => {
            app.pending = Pending::None;
            app.count = 0;
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
    let n = app.take_count();
    if let Some(o) = obj {
        app.op_object(op, scope, o, n);
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
        KeyCode::Char('<') => {
            app.insert_octave = app.insert_octave.saturating_sub(1);
            app.status = format!("octave {}", app.insert_octave);
        }
        KeyCode::Char('>') => {
            app.insert_octave = (app.insert_octave + 1).min(8);
            app.status = format!("octave {}", app.insert_octave);
        }
        KeyCode::Char(c) => {
            if let Some(note) = App::piano_row_note(c, app.insert_octave) {
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
    handle_command_key(app, key);
    // Every arm above either edits the command buffer or leaves the mode,
    // and the ghost is a pure function of that buffer — so refresh it in
    // one place rather than sprinkling calls through fifteen arms.
    // `refresh_ghost` clears it unless we are still in Command mode, which
    // covers both the Esc and the Enter path.
    refresh_ghost(app);
}

fn handle_command_key(app: &mut App, key: KeyEvent) {
    use crossterm::event::KeyModifiers;
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.command_buf.clear();
            app.command_cursor = 0;
        }
        KeyCode::Enter => {
            let cmd = app.command_buf.trim().to_string();
            execute_command(app, &cmd);
            app.command_buf.clear();
            app.command_cursor = 0;
            // Only fall back to Normal if the command didn't switch modes itself.
            if app.mode == Mode::Command {
                app.mode = Mode::Normal;
            }
        }
        KeyCode::Backspace => {
            if let Some(prev) = prev_char_boundary(&app.command_buf, app.command_cursor) {
                app.command_buf.replace_range(prev..app.command_cursor, "");
                app.command_cursor = prev;
            }
        }
        KeyCode::Delete => {
            if let Some(next) = next_char_boundary(&app.command_buf, app.command_cursor) {
                app.command_buf.replace_range(app.command_cursor..next, "");
            }
        }
        KeyCode::Left => {
            if let Some(prev) = prev_char_boundary(&app.command_buf, app.command_cursor) {
                app.command_cursor = prev;
            }
        }
        KeyCode::Right => {
            if let Some(next) = next_char_boundary(&app.command_buf, app.command_cursor) {
                app.command_cursor = next;
            }
        }
        KeyCode::Home => { app.command_cursor = 0; }
        KeyCode::End => { app.command_cursor = app.command_buf.len(); }
        KeyCode::Tab => { complete_path(app); }
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match c {
                'a' => { app.command_cursor = 0; }
                'e' => { app.command_cursor = app.command_buf.len(); }
                'b' => {
                    if let Some(prev) = prev_char_boundary(&app.command_buf, app.command_cursor) {
                        app.command_cursor = prev;
                    }
                }
                'f' => {
                    if let Some(next) = next_char_boundary(&app.command_buf, app.command_cursor) {
                        app.command_cursor = next;
                    }
                }
                'u' => {
                    app.command_buf.replace_range(..app.command_cursor, "");
                    app.command_cursor = 0;
                }
                'k' => { app.command_buf.truncate(app.command_cursor); }
                'w' => { delete_word_back(app); }
                _ => {}
            }
        }
        KeyCode::Char(c) => {
            app.command_buf.insert(app.command_cursor, c);
            app.command_cursor += c.len_utf8();
        }
        _ => {}
    }
}

fn prev_char_boundary(s: &str, byte: usize) -> Option<usize> {
    if byte == 0 { return None; }
    s[..byte].chars().next_back().map(|c| byte - c.len_utf8())
}

fn next_char_boundary(s: &str, byte: usize) -> Option<usize> {
    if byte >= s.len() { return None; }
    s[byte..].chars().next().map(|c| byte + c.len_utf8())
}

fn delete_word_back(app: &mut App) {
    let end = app.command_cursor;
    let bytes = app.command_buf.as_bytes();
    let mut i = end;
    // Skip trailing whitespace immediately before the cursor.
    while i > 0 && bytes[i - 1].is_ascii_whitespace() { i -= 1; }
    // Then skip non-whitespace back to the previous word boundary.
    while i > 0 && !bytes[i - 1].is_ascii_whitespace() { i -= 1; }
    app.command_buf.replace_range(i..end, "");
    app.command_cursor = i;
}

/// Single-shot prefix completion for `:w`, `:wq`, `:e` path args.
/// Extends the path fragment to the longest common prefix of matching
/// filesystem entries. Appends `/` when the unique match is a directory.
fn complete_path(app: &mut App) {
    let buf = app.command_buf.clone();
    // Find the start of the path fragment: chars after the last whitespace.
    let path_start = buf.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let head = &buf[..path_start];
    let frag = &buf[path_start..];

    // Only complete for file-taking commands.
    let cmd = head.trim();
    let want_vip = matches!(cmd, "e" | "edit");
    let want_png = matches!(cmd, "sprite load" | "sprites load");
    if !matches!(cmd, "w" | "e" | "wq" | "edit" | "write")
        && !want_png
    {
        return;
    }

    // Expand leading `~` for directory lookup but keep the display form.
    let (dir, name_prefix, display_dir) = split_path_fragment(frag);

    let Ok(entries) = std::fs::read_dir(&dir) else {
        app.status = format!("no such directory: {}", dir.display());
        return;
    };

    let mut matches: Vec<(String, bool)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&name_prefix) { continue; }
        if name.starts_with('.') && !name_prefix.starts_with('.') { continue; }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if want_vip && !is_dir && !name.ends_with(".vip") { continue; }
        if want_png && !is_dir && !name.to_ascii_lowercase().ends_with(".png") { continue; }
        matches.push((name, is_dir));
    }

    if matches.is_empty() {
        app.status = format!("no matches for '{}'", frag);
        return;
    }

    matches.sort_by(|a, b| a.0.cmp(&b.0));
    let common = longest_common_prefix(matches.iter().map(|m| m.0.as_str()));
    let completed = if matches.len() == 1 {
        let (ref name, is_dir) = matches[0];
        if is_dir { format!("{}/", name) } else { name.clone() }
    } else {
        common
    };

    let new_frag = format!("{}{}", display_dir, completed);
    app.command_buf = format!("{}{}", head, new_frag);
    app.command_cursor = app.command_buf.len();
    if matches.len() > 1 {
        let preview: Vec<String> = matches
            .iter()
            .take(6)
            .map(|(n, is_dir)| if *is_dir { format!("{}/", n) } else { n.clone() })
            .collect();
        app.status = format!(
            "{} matches: {}{}",
            matches.len(),
            preview.join(" "),
            if matches.len() > 6 { " ..." } else { "" },
        );
    } else {
        app.status = "".into();
    }
}

fn split_path_fragment(frag: &str) -> (PathBuf, String, String) {
    // Expand leading `~` / `~/` for filesystem lookup while preserving the
    // original display form in the command buffer.
    fn expand(s: &str) -> PathBuf {
        if let Some(rest) = s.strip_prefix("~/") {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home).join(rest);
            }
        }
        if s == "~" {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home);
            }
        }
        PathBuf::from(s)
    }

    // Split into (display_dir_kept_verbatim, basename_prefix_to_match).
    // `display_dir` always ends in `/` when present, so `frag` = "projects/"
    // → display_dir = "projects/", basename = "" (list directory contents).
    let (display_dir, basename) = match frag.rfind('/') {
        Some(i) => (&frag[..=i], &frag[i + 1..]),
        None    => ("", frag),
    };
    let dir = if display_dir.is_empty() {
        PathBuf::from(".")
    } else {
        expand(display_dir)
    };
    (dir, basename.to_string(), display_dir.to_string())
}

fn longest_common_prefix<'a, I: IntoIterator<Item = &'a str>>(strs: I) -> String {
    let mut iter = strs.into_iter();
    let Some(first) = iter.next() else { return String::new() };
    let mut prefix = first.to_string();
    for s in iter {
        while !s.starts_with(&prefix) {
            prefix.pop();
            if prefix.is_empty() { return String::new(); }
        }
    }
    prefix
}

fn execute_command(app: &mut App, cmd: &str) {
    // `:bind` takes a free-form expression (with '=', whitespace, operators)
    // that doesn't map onto slice-pattern tokenization; peel it off first.
    if let Some(rest) = cmd.strip_prefix("bind ").or_else(|| cmd.strip_prefix("bind\t")) {
        bind_command(app, rest.trim());
        return;
    }
    if cmd.trim() == "bind" {
        bind_command(app, "");
        return;
    }
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    // Note: the commit paths for the previewable commands are the ordinary
    // arms below. They already call the very functions `PreviewCmd::apply`
    // calls — `gen::dispatch` at the seed `take_gen_seed` hands out, and
    // `transpose_phrase_cells` — so preview and commit agree by sharing the
    // transformation, without routing the commit through a second dispatch
    // that would bypass `perform` and stop `:transpose` recording into
    // macros.
    match parts.as_slice() {
        ["q"] | ["q!"] | ["quit"] | ["quit!"] => { app.quit = true; }
        ["diff", rest @ ..] => diff_cmd(app, rest),
        ["help"] | ["h"] => {
            app.mode = Mode::Help;
            app.status = "help — q/Esc/? to close".into();
        }
        ["inst"] | ["instrument"] => { enter_instrument_mode(app); }
        ["inst", n] | ["instrument", n] => {
            if let Ok(i) = u8::from_str_radix(n, 16) {
                app.selected_instr = i.min((INSTRUMENTS - 1) as u8);
            }
            app.mode = Mode::Instrument;
            app.status = format!("instrument {:02X} — Esc to return", app.selected_instr);
        }
        ["w"] => { write_current(app); }
        ["w", path] => { write_to(app, Path::new(path)); }
        ["wq"] => { save_and_quit(app); }
        ["wq", path] => {
            if write_to(app, Path::new(path)) {
                app.quit = true;
            }
        }
        ["e", path] => { edit_file(app, Path::new(path)); }
        ["new"] => { new_song(app); }
        ["e!"] | ["edit!"] => {
            if let Some(p) = app.current_file.clone() {
                edit_file(app, &p);
            } else {
                app.status = "error: no file loaded".into();
            }
        }
        ["play"] => { app.playing = true; app.status = "playing".into(); }
        ["stop"] => { app.playing = false; app.status = "stopped".into(); }
        ["order"] => {
            let (order, loop_pos) = app.song.flat_order();
            let list: Vec<String> = order.iter().map(|i| format!("{:02X}", i)).collect();
            let from = if app.song.arrangement.is_empty() { "" } else { " (from arrangement)" };
            app.status = if list.is_empty() {
                "order: (none) — :order 00,01,.. to set".into()
            } else {
                format!("order: [{}] loop={:02X}{} song mode {}", list.join(","), loop_pos, from,
                    if app.song_mode { "on" } else { "off" })
            };
        }
        ["order", "off"] => {
            app.snapshot();
            app.song.order.clear();
            app.song.chains.clear();
            app.song.arrangement.clear();
            app.song.arr_loop = 0;
            app.song_mode = false;
            app.dirty = true;
            app.status = "order cleared".into();
        }
        ["order", "loop", n] => match usize::from_str_radix(n, 16) {
            Ok(i) if !app.song.arrangement.is_empty() => {
                if i < app.song.arrangement.len() {
                    app.song.arr_loop = i; app.dirty = true;
                    app.status = format!("arrangement loop slot → {:02X}", i);
                } else {
                    app.status = "order loop: slot out of range (use :arr loop N)".into();
                }
            }
            Ok(i) if i < app.song.order.len().max(1) => { app.song.loop_pos = i; app.dirty = true; app.status = format!("loop point → {:02X}", i); }
            _ => app.status = "order loop: index out of range".into(),
        },
        ["order", list] => match parse_order_list(list, app.song.phrases.len()) {
            Ok(o) => {
                app.snapshot();
                let replaced = !app.song.arrangement.is_empty();
                app.song.chains.clear();
                app.song.arrangement.clear();
                app.song.arr_loop = 0;
                app.song.order = o;
                app.song.loop_pos = app.song.loop_pos.min(app.song.order.len().saturating_sub(1));
                app.song_mode = true;
                app.dirty = true;
                app.status = format!("order set ({} entries), song mode on{}", app.song.order.len(),
                    if replaced { " — arrangement replaced" } else { "" });
            }
            Err(e) => app.status = format!("order: {}", e),
        },
        ["song", "on"] => {
            if app.song.flat_order().0.is_empty() {
                app.song.order = (0..app.song.phrases.len()).collect();
                app.dirty = true;
            }
            app.song_mode = true;
            app.status = "song mode on".into();
        }
        ["song", "off"] => { app.song_mode = false; app.status = "song mode off (looping current phrase)".into(); }
        ["song"] => {
            app.show_song = !app.show_song;
            app.status = if app.show_song { "song pane on (:song again to hide)".into() } else { "song pane off".into() };
        }
        ["song", "show"] => song_show(app),
        ["chain", rest @ ..] => chain_cmd(app, rest),
        ["arr", rest @ ..] | ["arrangement", rest @ ..] => arr_cmd(app, rest),
        ["len", rest @ ..] | ["length", rest @ ..] => len_cmd(app, rest),
        ["groove", rest @ ..] => groove_cmd(app, rest),
        ["engine", "apu"] => {
            app.engine = audio::Engine::Apu;
            app.nsf_cache = None;
            app.status = match &app.song.driver {
                Some(_) => "engine: APU (compiles on play)".into(),
                None => "engine: APU — set :driver BIN SYM first".into(),
            };
        }
        ["engine", "synth"] => { app.engine = audio::Engine::Synth; app.status = "engine: synth".into(); }
        ["engine"] => { app.status = format!("engine: {:?}", app.engine); }
        ["driver", bin, sym] => {
            app.song.driver = Some((PathBuf::from(bin), PathBuf::from(sym)));
            app.nsf_cache = None;
            app.dirty = true;
            app.status = format!("driver: {} + {}", bin, sym);
        }
        ["compile", path] => compile_cmd(app, path),
        ["rec"] => { toggle_record_arm(app); }
        ["rec", "off"] => {
            app.disarm_all();
            app.status = "rec: all channels disarmed".into();
        }
        ["viz"] => {
            app.show_viz = !app.show_viz;
            app.status = if app.show_viz {
                format!("viz: {} (bars / scope / grid / orbit)", app.viz_kind.name())
            } else {
                "viz: off".into()
            };
        }
        ["viz", "off"] => {
            app.show_viz = false;
            app.status = "viz: off".into();
        }
        ["sprite"] | ["sprites"] => sprite_list(app),
        ["sprite", "list"] | ["sprites", "list"] => sprite_list(app),
        ["sprite", "clear"] | ["sprites", "clear"] => {
            let n = app.sprite_placements.len();
            app.sprite_placements.clear();
            app.status = format!("sprite: cleared {} placement{}",
                n, if n == 1 { "" } else { "s" });
        }
        ["sprite", "load", path] => sprite_load_cmd(app, path, None, false),
        ["sprite", "load", path, tail] if is_quantize_flag(tail) => sprite_load_cmd(app, path, None, true),
        ["sprite", "load", path, cell] => sprite_load_cmd(app, path, Some(cell), false),
        ["sprite", "load", path, cell, tail] if is_quantize_flag(tail) => sprite_load_cmd(app, path, Some(cell), true),
        ["sprites", "load", path] => sprite_load_cmd(app, path, None, false),
        ["sprites", "load", path, tail] if is_quantize_flag(tail) => sprite_load_cmd(app, path, None, true),
        ["sprites", "load", path, cell] => sprite_load_cmd(app, path, Some(cell), false),
        ["sprites", "load", path, cell, tail] if is_quantize_flag(tail) => sprite_load_cmd(app, path, Some(cell), true),
        ["sprite", "place", name, idx, x, y] => sprite_place_cmd(app, name, idx, x, y),
        ["sprite", "palette", pname, c0, c1, c2, c3] =>
            sprite_palette_cmd(app, pname, &[c0, c1, c2, c3]),
        ["sprite", "repalette", sheet, pname] => sprite_repalette_cmd(app, sheet, pname),
        ["viz", kind] => match viz::VizKind::parse(kind) {
            Some(k) => {
                app.viz_kind = k;
                app.show_viz = true;
                app.status = format!("viz: {}", k.name());
            }
            None => app.status = format!("viz: unknown kind '{}' (bars/scope/grid/orbit)", kind),
        },
        ["mute"] => { toggle_mute(app, app.cursor_ch); }
        ["mute", "off"] | ["unmute"] => { unmute_all(app); }
        ["mute", tok] => match parse_channel_token(tok) {
            Some(ch) => toggle_mute(app, ch),
            None => app.status = format!("mute: bad channel '{}'", tok),
        },
        ["unmute", tok] => match parse_channel_token(tok) {
            Some(ch) => {
                if app.muted[ch] {
                    toggle_mute(app, ch);
                } else {
                    app.status = format!("unmute: {} already live", channel_name(ch));
                }
            }
            None => app.status = format!("unmute: bad channel '{}'", tok),
        },
        ["scene"] => {
            // `:scene` with no args lists current bindings.
            let bound: Vec<String> = (0..9)
                .filter_map(|i| app.scenes[i].map(|p| format!("{}→{:02X}", i + 1, p)))
                .collect();
            app.status = if bound.is_empty() {
                "scenes: (none saved) — :scene N save to bind slot N".into()
            } else {
                format!("scenes: {}", bound.join(" "))
            };
        }
        ["scene", "off"] | ["scene", "cancel"] => {
            if app.queued_scene.is_some() {
                app.queued_scene = None;
                app.status = "scene queue cancelled".into();
            } else {
                app.status = "no scene queued".into();
            }
        }
        ["scene", n, "save"] => scene_save(app, n),
        ["scene", n, "clear"] | ["scene", n, "del"] => scene_clear(app, n),
        ["scene", n] => scene_launch_by_name(app, n),
        ["phrase"] | ["p"] => {
            app.status = format!(
                "phrase {:02X}/{:02X}",
                app.song.current_phrase,
                app.song.phrases.len().saturating_sub(1),
            );
        }
        ["phrase", "new"] => { new_phrase(app); }
        ["phrase", "del"] | ["phrase", "delete"] => { delete_phrase_cmd(app); }
        ["phrase", n] => {
            match u8::from_str_radix(n, 16) {
                Ok(i) => goto_phrase(app, i as usize),
                Err(_) => app.status = format!("bad phrase index: {}", n),
            }
        }
        ["vol", tok] => { set_cursor_vol(app, tok); }
        ["transpose", n] | ["tr", n] => { transpose_phrase(app, n); }
        ["fx", "off"] | ["fx", "clear"] => { clear_cursor_fx(app); }
        ["fx", tok] => { set_cursor_fx(app, tok); }
        ["fx", cmd, param] => {
            let joined = format!("{}{}", cmd, param);
            set_cursor_fx(app, &joined);
        }
        ["bounce", path] => bounce_cmd(app, path, 1),
        ["bounce", path, loops] => match loops.parse::<u32>() {
            Ok(n) if n >= 1 => bounce_cmd(app, path, n),
            _ => app.status = format!("bounce: bad loop count '{}'", loops),
        },
        ["midi", path] => midi_cmd(app, path, 1),
        ["midi", path, loops] => match loops.parse::<u32>() {
            Ok(n) if n >= 1 => midi_cmd(app, path, n),
            _ => app.status = format!("midi: bad loop count '{}'", loops),
        },
        ["gen", "style", dir] | ["gen", "style", dir, _] => {
            let seed: u64 = parts.get(3).and_then(|s| s.parse().ok()).unwrap_or(1);
            let base = app.current_file.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
            let dir_path = match &base { Some(b) if Path::new(dir).is_relative() => b.join(dir), _ => PathBuf::from(dir) };
            match style::Style::load(&dir_path).and_then(|st| style::generate(&st, &style::GenParams { seed, ..Default::default() })) {
                Ok(song) => {
                    app.snapshot();
                    let driver = app.song.driver.clone();
                    app.song = song;
                    app.song.driver = driver;
                    reload_bank(app);
                    app.song_mode = true;
                    app.cursor_step = 0;
                    app.dirty = true;
                    app.status = format!("gen style {} seed {} → {} phrases, {} bars", dir, seed, app.song.phrases.len(), app.song.order.len());
                }
                Err(e) => app.status = format!("gen style: {:#}", e),
            }
        }
        ["gen", rest @ ..] => {
            app.snapshot();
            let seed = app.take_gen_seed();
            match gen::dispatch(&mut app.song, rest, seed) {
                Ok(msg) => { app.status = msg; }
                Err(e) => {
                    // Gen failed — our optimistic snapshot doesn't reflect
                    // a real change, so drop it to keep the undo stack clean.
                    app.undo_stack.pop();
                    app.status = format!("gen: {}", e);
                }
            }
        }
        ["set", rest @ ..] => {
            // Accept `bpm=140`, `bpm =140`, `bpm= 140`, `bpm = 140`, etc.
            let joined = rest.join(" ");
            let Some((k, v)) = joined.split_once('=') else {
                app.status = "usage: :set key=value".into();
                return;
            };
            let k = k.trim();
            let v = v.trim();
            match k {
                "bpm" => match v.parse::<u16>() {
                    Ok(n) if (20..=999).contains(&n) => {
                        app.snapshot();
                        app.song.bpm = n;
                        app.status = format!("bpm = {}", app.song.bpm);
                    }
                    Ok(n) => app.status = format!("bpm out of range (20–999): {}", n),
                    Err(_) => app.status = format!("bad bpm value: {:?}", v),
                },
                "step" => match v.parse::<usize>() {
                    Ok(n) if (1..=STEPS_PER_PHRASE).contains(&n) => {
                        app.snapshot();
                        app.song.edit_step = n;
                        app.status = format!("edit step = {}", app.song.edit_step);
                    }
                    Ok(n) => app.status = format!(
                        "step out of range (1–{}): {}",
                        STEPS_PER_PHRASE, n,
                    ),
                    Err(_) => app.status = format!("bad step value: {:?}", v),
                },
                "octave" => match v.parse::<u8>() {
                    Ok(n) if n <= 8 => {
                        app.insert_octave = n;
                        app.status = format!("octave = {}", app.insert_octave);
                    }
                    Ok(n) => app.status = format!("octave out of range (0–8): {}", n),
                    Err(_) => app.status = format!("bad octave value: {:?}", v),
                },
                "theme" => match Theme::by_name(v) {
                    Some(t) => {
                        app.theme = t;
                        app.status = format!("theme = {}", t.name);
                    }
                    None => app.status = format!("unknown theme: {:?} (try nes or phosphor)", v),
                },
                "still" => match v {
                    "on" | "1" | "yes" => { app.still = true; app.status = "still = on (animations frozen)".into(); }
                    "off" | "0" | "no" => { app.still = false; app.status = "still = off (breathing)".into(); }
                    "toggle" => {
                        app.still = !app.still;
                        app.status = format!("still = {}", if app.still { "on" } else { "off" });
                    }
                    _ => app.status = format!("still: expected on / off / toggle, got {:?}", v),
                },
                _ => { app.status = format!("unknown setting: {}", k); }
            }
        }
        _ => { app.status = format!("unknown command: {}", cmd); }
    }
}

// ---------- Instrument-editor entry ----------

/// Enter the instrument editor. If the cell under the cursor has a note,
/// target that cell's instrument so F2 / `:inst` edits the sound you're
/// looking at. Otherwise keep whatever instrument was previously selected.
fn enter_instrument_mode(app: &mut App) {
    let (s, c) = (app.cursor_step, app.cursor_ch);
    let cell = app.phrase().cells[s][c];
    if cell.note.is_some() {
        app.selected_instr = (cell.instr as usize).min(INSTRUMENTS - 1) as u8;
    }
    app.mode = Mode::Instrument;
    app.status = format!("instrument {:02X} — Esc to return", app.selected_instr);
}

fn enter_live_mode(app: &mut App) {
    app.mode = Mode::Live;
    app.live_last_note = [None; CHANNELS];
    app.status = format!(
        "-- LIVE -- {} i{:02X} oct{} · z s x d c v g b h n j m plays · Tab/←→ channel · </> octave · R arm · Esc exit",
        channel_name(app.cursor_ch),
        app.selected_instr,
        app.insert_octave,
    );
}

/// Toggle record-arm on the cursor channel. Arming is purely a flag — the
/// actual cell writes happen inside the Live-mode piano-row handler.
fn toggle_record_arm(app: &mut App) {
    let ch = app.cursor_ch;
    app.recording[ch] = !app.recording[ch];
    let armed: Vec<&'static str> = (0..CHANNELS)
        .filter(|&i| app.recording[i])
        .map(channel_name)
        .collect();
    app.status = if armed.is_empty() {
        format!("rec: {} disarmed", channel_name(ch))
    } else {
        format!("rec: {} {} (armed: {})",
            channel_name(ch),
            if app.recording[ch] { "armed" } else { "disarmed" },
            armed.join(" "))
    };
}

/// `q` top-level key: if a recording is underway, stop it and save the
/// captured ops. Otherwise arm Pending::MacroRecord so the next keypress
/// is interpreted as the register letter to record into.
fn toggle_macro_record(app: &mut App) {
    if let Some((letter, ops)) = app.recording_macro.take() {
        let count = ops.len();
        if count == 0 {
            app.macros.remove(&letter);
            app.status = format!("q{}: empty — nothing saved", letter);
        } else {
            app.macros.insert(letter, ops);
            app.status = format!("q{}: saved ({} op{})",
                letter, count, if count == 1 { "" } else { "s" });
        }
    } else {
        app.pending = Pending::MacroRecord;
        app.status = "q: register letter to record into".into();
    }
}

/// Replay a saved macro. Each op runs through `perform` exactly the way
/// it would from a live keypress — scene launches respect the bar queue,
/// so replayed macros groove-lock instead of firing instantly.
fn play_macro(app: &mut App, letter: char) {
    let ops = match app.macros.get(&letter) {
        Some(v) => v.clone(),
        None => { app.status = format!("@{}: no macro saved", letter); return; }
    };
    app.last_macro = Some(letter);
    let count = ops.len();
    for op in ops {
        perform(app, op);
    }
    app.status = format!("@{}: ran {} op{}",
        letter, count, if count == 1 { "" } else { "s" });
}

/// Execute a single macro op against the app, and capture it into the
/// active recording buffer if one is live. All macro-recordable live
/// actions should route through this function so `q<letter>` catches them.
fn perform(app: &mut App, op: MacroOp) {
    if let Some((_, buf)) = app.recording_macro.as_mut() {
        buf.push(op);
    }
    match op {
        MacroOp::SceneLaunch(slot) => {
            if slot < 9 {
                queue_or_launch_scene(app, slot);
            }
        }
        MacroOp::Transpose(delta) => {
            transpose_delta(app, delta);
        }
        MacroOp::ToggleMute(ch) => {
            toggle_mute(app, ch);
        }
        MacroOp::TogglePlay => {
            app.playing = !app.playing;
            app.status = if app.playing { "playing...".into() } else { "stopped".into() };
        }
    }
}

/// Toggle mute on channel `ch`. The audio thread kills the voice on the
/// next callback, so a muted channel goes silent within ~one buffer.
fn toggle_mute(app: &mut App, ch: usize) {
    if ch >= CHANNELS {
        app.status = format!("mute: bad channel {}", ch);
        return;
    }
    app.muted[ch] = !app.muted[ch];
    let live: Vec<&'static str> = (0..CHANNELS)
        .filter(|&i| app.muted[i])
        .map(channel_name)
        .collect();
    app.status = if live.is_empty() {
        format!("mute: {} unmuted (all live)", channel_name(ch))
    } else {
        format!("mute: {} {} (muted: {})",
            channel_name(ch),
            if app.muted[ch] { "muted" } else { "unmuted" },
            live.join(" "))
    };
}

fn unmute_all(app: &mut App) {
    let any = app.muted.iter().any(|&b| b);
    app.muted = [false; CHANNELS];
    app.status = if any {
        "mute: all channels unmuted".into()
    } else {
        "mute: nothing was muted".into()
    };
}

/// Parse "1".."5" or "pu1/pu2/tri/noi/dpcm" into a channel index.
// ---------- Stage 23: chains, arrangement, groove, polymeter ----------

fn song_show(app: &mut App) {
    let arr: Vec<String> = app.song.arrangement.iter().map(|c| format!("{:02X}", c)).collect();
    let (order, _) = app.song.flat_order();
    app.status = format!(
        "song: {} phrases, {} chains, arr=[{}] loop={:02X} → {} order entries, chain {:02X} selected",
        app.song.phrases.len(), app.song.chains.len(), arr.join(","), app.song.arr_loop,
        order.len(), app.current_chain,
    );
}

/// `:chain show | new | del [NN] | sel NN | add NN | pop | name TEXT`
fn chain_cmd(app: &mut App, rest: &[&str]) {
    match rest {
        [] | ["show"] => {
            if app.song.chains.is_empty() {
                app.status = "chains: (none) — :chain new to start one".into();
                return;
            }
            let summary: Vec<String> = app.song.chains.iter().enumerate().map(|(i, c)| {
                let phrases: Vec<String> = c.phrases.iter().map(|p| format!("{:02X}", p)).collect();
                let marker = if i == app.current_chain { ">" } else { " " };
                let name = c.name.as_deref().map(|n| format!(" {}", n)).unwrap_or_default();
                format!("{}{:02X}{}:[{}]", marker, i, name, phrases.join(","))
            }).collect();
            app.status = format!("chains: {}", summary.join(" "));
        }
        ["new"] => {
            app.snapshot();
            app.song.chains.push(Chain::default());
            app.current_chain = app.song.chains.len() - 1;
            app.dirty = true;
            app.status = format!("chain {:02X} new (empty) — :chain add NN to fill it", app.current_chain);
        }
        ["del"] | ["delete"] => chain_delete(app, app.current_chain),
        ["del", idx] | ["delete", idx] => match usize::from_str_radix(idx, 16) {
            Ok(i) => chain_delete(app, i),
            Err(_) => app.status = format!("chain: bad index {:?}", idx),
        },
        ["sel", idx] | ["select", idx] => match usize::from_str_radix(idx, 16) {
            Ok(i) if i < app.song.chains.len() => {
                app.current_chain = i;
                app.status = format!("chain {:02X} selected", i);
            }
            Ok(i) => app.status = format!("chain: no such chain {:02X}", i),
            Err(_) => app.status = format!("chain: bad index {:?}", idx),
        },
        ["add", idx] => match usize::from_str_radix(idx, 16) {
            Ok(pi) if pi < app.song.phrases.len() => {
                app.snapshot();
                if app.song.chains.is_empty() {
                    app.song.chains.push(Chain::default());
                }
                let c = app.current_chain.min(app.song.chains.len() - 1);
                app.song.chains[c].phrases.push(pi as u8);
                app.current_chain = c;
                app.dirty = true;
                app.status = format!("chain {:02X}: +phrase {:02X} (len {})", c, pi, app.song.chains[c].phrases.len());
            }
            Ok(pi) => app.status = format!("chain: no such phrase {:02X}", pi),
            Err(_) => app.status = format!("chain: bad phrase index {:?}", idx),
        },
        ["pop"] => {
            let Some(c) = app.current_chain.checked_sub(0).filter(|&c| c < app.song.chains.len()) else {
                app.status = "chain: no chains".into();
                return;
            };
            match app.song.chains[c].phrases.last().copied() {
                Some(p) => {
                    app.snapshot();
                    app.song.chains[c].phrases.pop();
                    app.dirty = true;
                    app.status = format!("chain {:02X}: popped phrase {:02X}", c, p);
                }
                None => app.status = format!("chain {:02X}: already empty", c),
            }
        }
        ["name", name @ ..] if !name.is_empty() && app.current_chain < app.song.chains.len() => {
            app.snapshot();
            app.song.chains[app.current_chain].name = Some(name.join(" "));
            app.dirty = true;
            app.status = format!("chain {:02X} named {:?}", app.current_chain, name.join(" "));
        }
        _ => app.status = "usage: :chain show | new | del [NN] | sel NN | add NN | pop | name TEXT".into(),
    }
}

fn chain_delete(app: &mut App, idx: usize) {
    if idx >= app.song.chains.len() {
        app.status = format!("chain: no such chain {:02X}", idx);
        return;
    }
    app.snapshot();
    app.song.chains.remove(idx);
    // Drop arrangement slots that pointed at it and shift the rest down so
    // the remaining chains keep their slots.
    app.song.arrangement.retain(|&c| c as usize != idx);
    for c in app.song.arrangement.iter_mut() {
        if *c as usize > idx {
            *c -= 1;
        }
    }
    if app.song.arr_loop >= app.song.arrangement.len() {
        app.song.arr_loop = 0;
    }
    if app.current_chain >= app.song.chains.len() {
        app.current_chain = app.song.chains.len().saturating_sub(1);
    }
    app.dirty = true;
    app.status = format!("chain {:02X} deleted", idx);
}

/// `:arr show | add NN | del [pos] | loop pos | clear`
fn arr_cmd(app: &mut App, rest: &[&str]) {
    match rest {
        [] | ["show"] => {
            let s: Vec<String> = app.song.arrangement.iter().enumerate()
                .map(|(i, c)| if i == app.song.arr_loop { format!("[{:02X}]", c) } else { format!("{:02X}", c) })
                .collect();
            app.status = if s.is_empty() {
                "arr: (none) — :arr add NN appends chain NN".into()
            } else {
                format!("arr: {} ([..] = loop slot)", s.join(" "))
            };
        }
        ["add", idx] => match usize::from_str_radix(idx, 16) {
            Ok(ci) if ci < app.song.chains.len() => {
                app.snapshot();
                app.song.arrangement.push(ci as u8);
                app.song_mode = true;
                app.dirty = true;
                app.status = format!("arr: +chain {:02X} (slot {}), song mode on", ci, app.song.arrangement.len() - 1);
            }
            Ok(ci) => app.status = format!("arr: no such chain {:02X}", ci),
            Err(_) => app.status = format!("arr: bad chain index {:?}", idx),
        },
        ["del"] | ["pop"] => match app.song.arrangement.pop() {
            Some(c) => {
                app.snapshot();
                if app.song.arr_loop >= app.song.arrangement.len() { app.song.arr_loop = 0; }
                app.dirty = true;
                app.status = format!("arr: popped chain {:02X}", c);
            }
            None => app.status = "arr: already empty".into(),
        },
        ["del", pos] => match pos.parse::<usize>() {
            Ok(p) if p < app.song.arrangement.len() => {
                app.snapshot();
                let c = app.song.arrangement.remove(p);
                if app.song.arr_loop >= app.song.arrangement.len() { app.song.arr_loop = 0; }
                app.dirty = true;
                app.status = format!("arr: removed slot {} (chain {:02X})", p, c);
            }
            Ok(p) => app.status = format!("arr: no such slot {}", p),
            Err(_) => app.status = format!("arr: bad slot {:?}", pos),
        },
        ["loop", pos] => match pos.parse::<usize>() {
            Ok(p) if p < app.song.arrangement.len() => {
                app.song.arr_loop = p;
                app.dirty = true;
                app.status = format!("arr: loop slot → {}", p);
            }
            _ => app.status = "arr loop: slot out of range".into(),
        },
        ["clear"] => {
            app.snapshot();
            app.song.arrangement.clear();
            app.song.arr_loop = 0;
            app.dirty = true;
            app.status = "arr: cleared (chains kept)".into();
        }
        _ => app.status = "usage: :arr show | add NN | del [pos] | loop pos | clear".into(),
    }
}

/// `:len show | all N | <ch> N` — per-channel polymeter length (1–16).
fn len_cmd(app: &mut App, rest: &[&str]) {
    let parse_n = |v: &str| -> Result<u8, String> {
        match v.parse::<u8>() {
            Ok(n) if (1..=STEPS_PER_PHRASE as u8).contains(&n) => Ok(n),
            _ => Err(format!("length must be 1–{}, got {:?}", STEPS_PER_PHRASE, v)),
        }
    };
    match rest {
        [] | ["show"] => {
            let s: Vec<String> = (0..CHANNELS)
                .map(|i| format!("{}={}", channel_name(i), app.song.channel_length[i]))
                .collect();
            app.status = format!("len: {}", s.join(" "));
        }
        ["all", n] => match parse_n(n) {
            Ok(v) => {
                app.snapshot();
                app.song.channel_length = [v; CHANNELS];
                app.dirty = true;
                app.status = format!("len: all = {}", v);
            }
            Err(e) => app.status = format!("len: {}", e),
        },
        [ch, n] => match (parse_channel_token(ch), parse_n(n)) {
            (Some(c), Ok(v)) => {
                app.snapshot();
                app.song.channel_length[c] = v;
                app.dirty = true;
                app.status = format!("len {} = {}", channel_name(c), v);
            }
            (None, _) => app.status = format!("len: bad channel {:?}", ch),
            (_, Err(e)) => app.status = format!("len: {}", e),
        },
        _ => app.status = "usage: :len show | all N | <ch> N".into(),
    }
}

/// `:groove show | straight | swing N | <16 ints>` — per-16th sample
/// offsets on the synth step clock.
fn groove_cmd(app: &mut App, rest: &[&str]) {
    match rest {
        [] | ["show"] => {
            let joined: Vec<String> = app.song.groove.iter().map(|v| v.to_string()).collect();
            app.status = format!("groove: {}{}", joined.join(" "),
                if app.song.has_groove() { "" } else { " (straight)" });
        }
        ["straight"] | ["off"] | ["reset"] => {
            app.snapshot();
            app.song.groove = [0; 16];
            app.dirty = true;
            app.status = "groove: straight".into();
        }
        ["swing", n] => match n.parse::<i16>() {
            Ok(amt) => {
                app.snapshot();
                app.song.groove = vip::swing_groove(amt);
                app.dirty = true;
                app.status = format!("groove: swing ±{} samples", amt);
            }
            Err(_) => app.status = format!("groove: bad swing amount {:?}", n),
        },
        vals if vals.len() == 16 => {
            let mut g = [0i16; 16];
            for (i, tok) in vals.iter().enumerate() {
                match tok.parse::<i16>() {
                    Ok(v) => g[i] = v,
                    Err(_) => {
                        app.status = format!("groove: bad value {:?} at step {}", tok, i);
                        return;
                    }
                }
            }
            app.snapshot();
            app.song.groove = g;
            app.dirty = true;
            app.status = "groove: set (16 values)".into();
        }
        _ => app.status = "usage: :groove show | straight | swing N | <16 ints>".into(),
    }
}

fn parse_channel_token(tok: &str) -> Option<usize> {
    if let Ok(n) = tok.parse::<usize>() {
        if (1..=CHANNELS).contains(&n) {
            return Some(n - 1);
        }
    }
    match tok.to_ascii_lowercase().as_str() {
        "pu1" => Some(0),
        "pu2" => Some(1),
        "tri" => Some(2),
        "noi" => Some(3),
        "dpcm" | "dpc" | "dmc" => Some(4),
        _ => None,
    }
}

fn channel_name(ch: usize) -> &'static str {
    match ch {
        0 => "PU1",
        1 => "PU2",
        2 => "TRI",
        3 => "NOI",
        4 => "DPCM",
        _ => "???",
    }
}

// ---------- Stage 12: modulation bindings ----------

fn bind_command(app: &mut App, rest: &str) {
    if rest.is_empty() || rest == "list" {
        if app.bindings.is_empty() {
            app.status = "bind: none — :bind <sheet>.<N|*> <target> = <expr>".into();
            return;
        }
        let lines: Vec<String> = app.bindings.iter().enumerate()
            .map(|(i, b)| format!("{}:{} {}={}", i, b.addr(), b.target.name(), b.expr_src))
            .collect();
        app.status = format!("bindings: {}", lines.join(" | "));
        return;
    }
    if rest == "clear" || rest == "off" {
        let n = app.bindings.len();
        app.bindings.clear();
        app.status = format!("bind: cleared {} binding{}", n, if n == 1 { "" } else { "s" });
        return;
    }
    if let Some(idx_s) = rest.strip_prefix("del ").or_else(|| rest.strip_prefix("rm ")) {
        match idx_s.trim().parse::<usize>() {
            Ok(i) if i < app.bindings.len() => {
                let removed = app.bindings.remove(i);
                app.status = format!("bind: removed {} {}={}",
                    removed.addr(), removed.target.name(), removed.expr_src);
            }
            _ => app.status = format!("bind: bad index '{}'", idx_s.trim()),
        }
        return;
    }
    if rest == "demo" || rest.starts_with("demo ") {
        bind_demo(app, rest.strip_prefix("demo").unwrap().trim());
        return;
    }
    match modulation::parse_binding(rest) {
        Ok(b) => {
            app.status = format!("bound {} {} = {}", b.addr(), b.target.name(), b.expr_src);
            app.bindings.push(b);
        }
        Err(e) => app.status = format!("bind: {}", e),
    }
}

/// Apply a canned suite of bindings to a sheet — the set we use to smoke-
/// test every modulation target at once. Picks the first placed sheet if
/// no name is given; replaces any bindings already attached to that sheet.
fn bind_demo(app: &mut App, arg: &str) {
    let sheet = if arg.is_empty() {
        match app.sprite_placements.first().map(|p| p.sheet.clone()) {
            Some(s) => s,
            None => {
                app.status = "bind demo: no placements — :sprite place <sheet> …".into();
                return;
            }
        }
    } else {
        arg.to_string()
    };
    if !app.sprite_sheets.contains_key(&sheet) {
        app.status = format!("bind demo: unknown sheet '{}'", sheet);
        return;
    }

    app.bindings.retain(|b| b.sheet != sheet);

    let exprs = [
        "y = sin(time * 4) * 6",
        "scale = pu1.env * 1.5 + 1",
        "flipx = tri.gate",
        "rotate = sin(time * 2) * 15",
        "hue = master.rms * 180",
        "frame = clamp(floor(noi.age * 16), 0, 3)",
    ];
    let mut added = 0usize;
    for e in exprs {
        let full = format!("{} {}", sheet, e);
        match modulation::parse_binding(&full) {
            Ok(b) => { app.bindings.push(b); added += 1; }
            Err(err) => {
                app.status = format!("bind demo: parse error on '{}': {}", e, err);
                return;
            }
        }
    }
    app.status = format!("bind demo: {} bindings on {}", added, sheet);
}

// ---------- Stage 11: sprite commands ----------

fn sprite_list(app: &mut App) {
    if app.sprite_sheets.is_empty() && app.sprite_placements.is_empty() {
        app.status = "sprite: no sheets loaded (use :sprite load <path> [WxH])".into();
        return;
    }
    let sheets: Vec<String> = app.sprite_sheets.values()
        .map(|s| format!("{}({}×{}, {} tiles, {})",
            s.name, s.cell_w, s.cell_h, s.cell_count(),
            s.source.file_name().and_then(|n| n.to_str()).unwrap_or("?")))
        .collect();
    app.status = format!(
        "sprite: sheets[{}] placements={}",
        sheets.join(","), app.sprite_placements.len(),
    );
}

/// Parse "WxH" (e.g. "16x16"). Missing = treat the whole image as one cell.
fn parse_cell_dim(tok: &str) -> Option<(u32, u32)> {
    let lower = tok.to_ascii_lowercase();
    let (w, h) = lower.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

fn is_quantize_flag(tok: &str) -> bool {
    matches!(tok, "quantize" | "q" | "-q" | "--quantize")
}

fn sprite_load_cmd(app: &mut App, path_str: &str, cell: Option<&&str>, quantize: bool) {
    let path = resolve_sprite_path(app, Path::new(path_str));
    // Default cell dimension: the full image — auto-derived after load.
    let (cw, ch) = match cell {
        Some(c) => match parse_cell_dim(c) {
            Some(d) => d,
            None => {
                app.status = format!("sprite: bad cell dim '{}' (want WxH, e.g. 16x16)", c);
                return;
            }
        },
        None => (0, 0),
    };
    let stem = path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "sheet".into());
    // If no cell dim given, we need to peek at the image dims first.
    let (cw, ch) = if cw == 0 || ch == 0 {
        match image::image_dimensions(&path) {
            Ok((w, h)) => (w, h),
            Err(e) => {
                app.status = format!("sprite: load failed: {}", e);
                return;
            }
        }
    } else {
        (cw, ch)
    };
    match sprite::load_sheet(stem.clone(), &path, cw, ch, quantize) {
        Ok(sheet) => {
            let q_tag = if quantize { " [quantized]" } else { "" };
            let tiles = sheet.cell_count();
            // Auto-place at (0,0) on first load so `:viz sprites` shows
            // something immediately. A user who already has placements
            // for this sheet keeps them; otherwise we drop in frame 0.
            let already_placed = app.sprite_placements.iter().any(|p| p.sheet == stem);
            let auto_tag = if already_placed {
                ""
            } else {
                app.sprite_placements.push(sprite::Placement {
                    sheet: stem.clone(),
                    idx: 0,
                    x: 0,
                    y: 0,
                    palette: None,
                });
                " [placed 0,0]"
            };
            app.status = format!(
                "sprite: loaded {} ({}×{}, cells {}×{}, {} tiles){}{}",
                stem, sheet.width, sheet.height, cw, ch, tiles, q_tag, auto_tag,
            );
            app.sprite_sheets.insert(stem, sheet);
        }
        Err(e) => {
            app.status = format!("sprite: load failed: {}", e);
        }
    }
}

/// Resolve a sprite path: `~` expands to `$HOME`, relative paths are
/// anchored to the current song file's directory if one is loaded, so
/// `.vip` files and their assets ship together naturally.
fn resolve_sprite_path(app: &App, path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() { return expanded; }
    if let Some(vip) = &app.current_file {
        if let Some(dir) = vip.parent() {
            return dir.join(&expanded);
        }
    }
    expanded
}

fn expand_tilde(path: &Path) -> PathBuf {
    let s = match path.to_str() {
        Some(s) => s,
        None => return path.to_path_buf(),
    };
    if s == "~" {
        return std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

fn sprite_place_cmd(app: &mut App, name: &str, idx: &str, x: &str, y: &str) {
    let Ok(idx) = idx.parse::<u32>() else {
        app.status = format!("sprite: bad index '{}'", idx); return;
    };
    let Ok(x) = x.parse::<i32>() else {
        app.status = format!("sprite: bad x '{}'", x); return;
    };
    let Ok(y) = y.parse::<i32>() else {
        app.status = format!("sprite: bad y '{}'", y); return;
    };
    let Some(sheet) = app.sprite_sheets.get(name) else {
        app.status = format!("sprite: no sheet named '{}'", name); return;
    };
    if idx >= sheet.cell_count() {
        app.status = format!("sprite: {} has {} tiles (idx {} out of range)",
            name, sheet.cell_count(), idx);
        return;
    }
    app.sprite_placements.push(sprite::Placement {
        sheet: name.to_string(),
        idx, x, y,
        palette: None,
    });
    app.status = format!("sprite: placed {}.{} at ({},{})", name, idx, x, y);
}

fn sprite_palette_cmd(app: &mut App, name: &str, hex: &[&&str]) {
    let mut colors = [ratatui::style::Color::Rgb(0, 0, 0); sprite::PALETTE_SIZE];
    for (i, tok) in hex.iter().enumerate() {
        match sprite::parse_hex(tok) {
            Some(c) => colors[i] = c,
            None => {
                app.status = format!("sprite: bad hex '{}' (want #rrggbb or 'transparent')", tok);
                return;
            }
        }
    }
    app.sprite_palettes.insert(name.to_string(), colors);
    app.status = format!("sprite: palette '{}' defined", name);
}

fn sprite_repalette_cmd(app: &mut App, sheet: &str, pname: &str) {
    let Some(palette) = app.sprite_palettes.get(pname).copied() else {
        app.status = format!("sprite: no palette '{}'", pname); return;
    };
    let Some(s) = app.sprite_sheets.get_mut(sheet) else {
        app.status = format!("sprite: no sheet '{}'", sheet); return;
    };
    s.palette = palette;
    app.status = format!("sprite: {} repainted with palette '{}'", sheet, pname);
}

// ---------- Stage 7: scene launching ----------

/// Parse a `1`..`9` scene slot label from the user and run `f` with the 0-based
/// index. Factored out because `:scene <n> save/clear/launch` all share it.
fn with_scene_slot(app: &mut App, n: &str, f: impl FnOnce(&mut App, usize)) {
    match n.parse::<usize>() {
        Ok(i) if (1..=9).contains(&i) => f(app, i - 1),
        _ => app.status = format!("bad scene slot: {} (expected 1..9)", n),
    }
}

fn scene_save(app: &mut App, n: &str) {
    with_scene_slot(app, n, |a, i| {
        a.scenes[i] = Some(a.song.current_phrase);
        a.status = format!("scene {} saved → phrase {:02X}", i + 1, a.song.current_phrase);
    });
}

fn scene_clear(app: &mut App, n: &str) {
    with_scene_slot(app, n, |a, i| {
        a.scenes[i] = None;
        if a.queued_scene == Some(i) {
            a.queued_scene = None;
        }
        a.status = format!("scene {} cleared", i + 1);
    });
}

fn scene_launch_by_name(app: &mut App, n: &str) {
    with_scene_slot(app, n, |a, i| queue_or_launch_scene(a, i));
}

/// Queue a scene for the next bar boundary while playing; jump immediately
/// while stopped. Idempotent — calling with the same slot just re-announces.
fn queue_or_launch_scene(app: &mut App, slot: usize) {
    let Some(phrase_idx) = app.scenes[slot] else {
        app.status = format!("scene {} is empty — :scene {} save to bind it", slot + 1, slot + 1);
        return;
    };
    if phrase_idx >= app.song.phrases.len() {
        app.status = format!("scene {} points at phrase {:02X} (no longer exists)", slot + 1, phrase_idx);
        return;
    }
    if !app.playing {
        app.song.current_phrase = phrase_idx;
        app.cursor_step = 0;
        app.queued_scene = None;
        app.status = format!("scene {} → phrase {:02X} (launched)", slot + 1, phrase_idx);
        return;
    }
    app.queued_scene = Some(slot);
    let wait = steps_to_next_bar(app.play_step);
    app.status = format!(
        "scene {} queued → phrase {:02X} (launch in {} step{})",
        slot + 1, phrase_idx, wait, if wait == 1 { "" } else { "s" },
    );
}

/// Most rows in the log are worth one line of scrollback; beyond a handful
/// they stop being a log and start being noise.
const EVENT_LOG_MAX: usize = 6;

/// Fold a changed status line into the rolling event log.
fn record_event(app: &mut App) {
    if app.status == app.last_logged {
        return;
    }
    if !app.status.is_empty() {
        app.event_log.push_front(app.status.clone());
        app.event_log.truncate(EVENT_LOG_MAX);
    }
    app.last_logged = app.status.clone();
}

/// How much of the wait before a queued scene commits is still ahead, 1.0
/// down to 0.0. Counts the sub-step phase as well as whole steps so the
/// drain bar empties smoothly rather than in four jerks.
fn drain_fraction(wait: usize, step_phase: f32) -> f32 {
    ((wait as f32 - step_phase.clamp(0.0, 1.0)) / 4.0).clamp(0.0, 1.0)
}

/// Count of steps between `step` and the next bar boundary (step % 4 == 0).
/// Used for the countdown in the status line. A step that IS a bar boundary
/// returns 4, not 0 — "0 steps to launch" would lie.
fn steps_to_next_bar(step: usize) -> usize {
    let r = step % 4;
    if r == 0 { 4 } else { 4 - r }
}

/// Stage 5: live keyboard monitor.
/// Piano row triggers notes on the current channel through the audio engine;
/// no pattern writes, so `dirty` is never set here.
fn handle_live(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.live_events.push_back(audio::LiveEvent::AllOff);
            app.mode = Mode::Normal;
            app.status = "".into();
        }
        KeyCode::Char(' ') => {
            // Transport toggle keeps working from Live — pattern playback over the
            // live voice is the whole point of jamming along with the track.
            perform(app, MacroOp::TogglePlay);
            app.status = format!(
                "{}  ({} i{:02X} oct{})",
                if app.playing { "playing..." } else { "stopped" },
                channel_name(app.cursor_ch),
                app.selected_instr,
                app.insert_octave,
            );
        }
        KeyCode::Tab | KeyCode::Right => {
            app.cursor_ch = (app.cursor_ch + 1) % CHANNELS;
            app.status = format!("live: {} i{:02X} oct{}",
                channel_name(app.cursor_ch), app.selected_instr, app.insert_octave);
        }
        KeyCode::BackTab | KeyCode::Left => {
            app.cursor_ch = (app.cursor_ch + CHANNELS - 1) % CHANNELS;
            app.status = format!("live: {} i{:02X} oct{}",
                channel_name(app.cursor_ch), app.selected_instr, app.insert_octave);
        }
        KeyCode::Char('<') => {
            app.insert_octave = app.insert_octave.saturating_sub(1);
            app.status = format!("live: octave {}", app.insert_octave);
        }
        KeyCode::Char('>') => {
            app.insert_octave = (app.insert_octave + 1).min(8);
            app.status = format!("live: octave {}", app.insert_octave);
        }
        KeyCode::Char('R') => { toggle_record_arm(app); }
        KeyCode::Char('M') => { perform(app, MacroOp::ToggleMute(app.cursor_ch)); }
        KeyCode::Char('q') => { toggle_macro_record(app); }
        KeyCode::Char('@') => { app.pending = Pending::MacroPlay; }
        KeyCode::Char(c) if ('1'..='9').contains(&c) => {
            // Stage 7: scene launch from Live mode. Digits in Live never
            // double as counts, so this is an unambiguous hotkey.
            let slot = c.to_digit(10).unwrap() as usize - 1;
            perform(app, MacroOp::SceneLaunch(slot));
        }
        KeyCode::Char(c) => {
            if let Some(note) = App::piano_row_note(c, app.insert_octave) {
                let ch = app.cursor_ch;
                // Silence whatever was previously held on this channel so retriggers
                // sound like an instrument, not a stack of overlapping envelopes.
                app.live_events.push_back(audio::LiveEvent::GateOff { ch: ch as u8 });
                app.live_events.push_back(audio::LiveEvent::GateOn {
                    ch: ch as u8,
                    note,
                    instr: app.selected_instr,
                    vel: 1.0,
                    // ~180ms hold. Terminals don't emit KeyUp, so each press is
                    // a short pluck — the instrument's Release segment handles
                    // the tail. Hold long enough to be audible, short enough
                    // to retrigger freely.
                    hold_ms: Some(180),
                });
                app.live_last_note[ch] = Some(note);
                // Stage 6: if this channel is armed, commit the note to the
                // pattern at the current record target step.
                if app.recording[ch] {
                    let step = app.record_note(ch, note);
                    app.status = format!("● REC {} {} → step {:02X} (i{:02X})",
                        channel_name(ch), note_name(Some(note)), step, app.selected_instr);
                } else {
                    app.status = format!("live: {} {} (i{:02X})",
                        channel_name(ch), note_name(Some(note)), app.selected_instr);
                }
            }
        }
        KeyCode::Backspace => {
            let ch = app.cursor_ch;
            app.live_events.push_back(audio::LiveEvent::GateOff { ch: ch as u8 });
            app.live_last_note[ch] = None;
            app.status = format!("live: {} off", channel_name(ch));
        }
        _ => {}
    }
}

// ---------- Phrase navigation ----------

fn next_phrase(app: &mut App) {
    let n = app.song.phrases.len();
    if n <= 1 {
        app.status = "only one phrase".into();
        return;
    }
    app.song.current_phrase = (app.song.current_phrase + 1) % n;
    app.cursor_step = 0;
    app.status = format!("phrase {:02X}", app.song.current_phrase);
}

fn prev_phrase(app: &mut App) {
    let n = app.song.phrases.len();
    if n <= 1 {
        app.status = "only one phrase".into();
        return;
    }
    let cur = app.song.current_phrase;
    app.song.current_phrase = if cur == 0 { n - 1 } else { cur - 1 };
    app.cursor_step = 0;
    app.status = format!("phrase {:02X}", app.song.current_phrase);
}

fn goto_phrase(app: &mut App, idx: usize) {
    if idx >= app.song.phrases.len() {
        app.status = format!("no phrase {:02X} (have {})", idx, app.song.phrases.len());
        return;
    }
    app.song.current_phrase = idx;
    app.cursor_step = 0;
    // A diff names two specific phrases; walking away from one ends it.
    app.diff = None;
    app.status = format!("phrase {:02X}", idx);
}

fn new_phrase(app: &mut App) {
    app.snapshot();
    app.song.phrases.push(Phrase::default());
    app.song.current_phrase = app.song.phrases.len() - 1;
    app.cursor_step = 0;
    app.cursor_ch = 0;
    app.status = format!("new phrase {:02X}", app.song.current_phrase);
}

fn delete_phrase_cmd(app: &mut App) {
    app.snapshot();
    if app.song.phrases.len() == 1 {
        // Refuse to delete the last phrase — clear its contents instead.
        app.song.phrases[0] = Phrase::default();
        app.cursor_step = 0;
        app.cursor_ch = 0;
        app.status = "cleared phrase (last one, not deleted)".into();
        return;
    }
    let idx = app.song.current_phrase;
    app.song.phrases.remove(idx);
    if app.song.current_phrase >= app.song.phrases.len() {
        app.song.current_phrase = app.song.phrases.len() - 1;
    }
    app.cursor_step = 0;
    app.status = format!("deleted phrase {:02X}, now on {:02X}", idx, app.song.current_phrase);
}

// ---------- File I/O helpers ----------

/// Returns `true` on a successful write. Sets `app.status` either way.
/// Stage 15a: offline WAV bounce of the current phrase. Resolves `~` and
/// relative paths the same way sprite loading does, renders at 44.1kHz
/// 16-bit mono, N loops of the phrase + release tail.
/// The phrases `:bounce` / `:midi` render: the song order in song mode,
/// otherwise the current phrase.
fn playback_sequence(app: &App) -> Vec<Phrase> {
    let (order, _) = app.song.flat_order();
    if app.song_mode && !order.is_empty() {
        order.iter().filter_map(|&i| app.song.phrases.get(i).cloned()).collect()
    } else {
        app.song.phrases.get(app.song.current_phrase).cloned().into_iter().collect()
    }
}

fn parse_order_list(list: &str, phrases: usize) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for tok in list.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let i = usize::from_str_radix(tok, 16).map_err(|_| anyhow::anyhow!("bad phrase index {:?}", tok))?;
        if i >= phrases {
            anyhow::bail!("phrase {:02X} does not exist", i);
        }
        out.push(i);
    }
    if out.is_empty() {
        anyhow::bail!("empty order");
    }
    Ok(out)
}

/// Stage 18: `:compile PATH` — lower the song and emit an NSF against the
/// `@driver`. Reports data size and any lowering warnings.
fn compile_cmd(app: &mut App, path_str: &str) {
    let path = resolve_sprite_path(app, Path::new(path_str));
    let base = app.current_file.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
    let driver = match compile::load_song_driver(&app.song, base.as_deref()) {
        Ok(d) => d,
        Err(e) => { app.status = format!("compile: {}", e); return; }
    };
    match compile::compile(&app.song, &driver, base.as_deref()) {
        Ok(c) => match std::fs::write(&path, &c.nsf) {
            Ok(()) => {
                app.status = format!(
                    "compiled {} ({} bytes data, {} bytes samples, {} frames){}",
                    path.display(), c.data_bytes, c.sample_bytes, c.total_frames,
                    if c.warnings.is_empty() { String::new() } else { format!(" — {} warning(s): {}", c.warnings.len(), c.warnings[0]) }
                );
            }
            Err(e) => app.status = format!("compile: write {}: {}", path.display(), e),
        },
        Err(e) => app.status = format!("compile: {}", e),
    }
}

/// Compile the song for APU playback. Called on the play transition when
/// the engine is APU; failures fall back to the synth with a status.
fn refresh_nsf_for_playback(app: &mut App) {
    if app.engine != audio::Engine::Apu {
        return;
    }
    let base = app.current_file.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
    let driver = match compile::load_song_driver(&app.song, base.as_deref()) {
        Ok(d) => d,
        Err(e) => { app.status = format!("apu: {} — using synth", e); app.nsf_cache = None; return; }
    };
    match compile::compile(&app.song, &driver, base.as_deref()) {
        Ok(c) => {
            app.nsf_cache = Some(std::sync::Arc::new(c.nsf));
            app.nsf_generation = app.nsf_generation.wrapping_add(1);
            app.nsf_frames_per_row = c.frames_per_row;
            app.status = "playing (APU)".into();
        }
        Err(e) => { app.status = format!("apu: {} — using synth", e); app.nsf_cache = None; }
    }
}

/// Rebuild the DPCM preview bank from the song's `@dpcm` files (decoded
/// through the DMC model) or the built-in bank. Failures fall back to the
/// built-in bank with a status line.
fn reload_bank(app: &mut App) {
    let base = app.current_file.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
    match dpcm::load_bank(&app.song, base.as_deref()) {
        Ok(b) => app.bank = std::sync::Arc::new(b),
        Err(e) => {
            app.status = format!("dpcm: {:#} — previewing with the built-in bank", e);
            app.bank = std::sync::Arc::new(dpcm::default_bank());
        }
    }
    app.bank_generation = app.bank_generation.wrapping_add(1);
}

fn bounce_cmd(app: &mut App, path_str: &str, loops: u32) {
    let path = resolve_sprite_path(app, Path::new(path_str));
    let sequence = playback_sequence(app);
    if sequence.is_empty() {
        app.status = "bounce: no phrase loaded".into();
        return;
    }
    const SR: u32 = 44100;
    match audio::bounce_to_wav(
        &path, &sequence, &app.song.instruments, app.song.bpm, loops, SR, &app.bank,
        &app.song.groove, &app.song.channel_length,
    ) {
        Ok(frames) => {
            let secs = frames as f32 / SR as f32;
            app.status = format!(
                "bounce: {} ({}× loop, {:.2}s @ {}Hz)",
                path.display(), loops, secs, SR,
            );
        }
        Err(e) => app.status = format!("bounce: {}", e),
    }
}

/// Stage 15b: MIDI export of the current phrase. Same path conventions
/// as `:bounce`. Tracks: conductor (tempo) + PU1 / PU2 / TRI / NOI.
/// NOI is routed to MIDI channel 10 (GM drum map) with a kick/snare/hat
/// remap based on pitch.
fn midi_cmd(app: &mut App, path_str: &str, loops: u32) {
    let path = resolve_sprite_path(app, Path::new(path_str));
    let sequence = playback_sequence(app);
    if sequence.is_empty() {
        app.status = "midi: no phrase loaded".into();
        return;
    }
    match midi::export_phrase_to_midi(
        &path, &sequence, &app.song.instruments, app.song.bpm, loops,
    ) {
        Ok(()) => {
            app.status = format!("midi: {} ({}× loop)", path.display(), loops);
        }
        Err(e) => app.status = format!("midi: {}", e),
    }
}

fn write_current(app: &mut App) -> bool {
    let Some(path) = app.current_file.clone() else {
        app.status = "error: no filename (use :w <path>)".into();
        return false;
    };
    write_to(app, &path)
}

/// Returns `true` on a successful write. Sets `app.status` either way.
fn write_to(app: &mut App, path: &Path) -> bool {
    match vip::save(&app.song, path) {
        Ok(()) => {
            app.current_file = Some(path.to_path_buf());
            app.dirty = false;
            app.status = format!("wrote {}", path.display());
            true
        }
        Err(e) => {
            app.status = format!("error: {}", e);
            false
        }
    }
}

/// Save to the current file, then quit if the save succeeded. Used by both
/// `:wq` and `ZZ`.
fn save_and_quit(app: &mut App) {
    if write_current(app) {
        app.quit = true;
    }
}

fn edit_file(app: &mut App, path: &Path) {
    if !path.exists() {
        app.song = Song::default();
        app.current_file = Some(path.to_path_buf());
        app.cursor_step = 0;
        app.cursor_ch = 0;
        app.play_step = 0;
        app.undo_stack.clear();
        app.redo_stack.clear();
        app.dirty = false;
        app.status = format!("new file: {}", path.display());
        return;
    }
    match vip::load(path) {
        Ok((song, warnings)) => {
            app.song_mode = !song.order.is_empty();
            app.nsf_cache = None;
            app.song = song;
            app.current_file = Some(path.to_path_buf());
            reload_bank(app);
            app.cursor_step = 0;
            app.cursor_ch = 0;
            app.play_step = 0;
            app.undo_stack.clear();
            app.redo_stack.clear();
            app.dirty = false;
            app.status = if warnings.is_empty() {
                format!("loaded {}", path.display())
            } else {
                // Don't eprintln here — stderr writes into the alt screen and
                // corrupts the TUI until a resize forces a redraw. The status
                // line gets the count + first warning; the rest are dropped
                // for now (a `:messages` buffer would be the natural home).
                format!(
                    "loaded {} ({} warning{}: {})",
                    path.display(),
                    warnings.len(),
                    if warnings.len() == 1 { "" } else { "s" },
                    warnings[0],
                )
            };
        }
        Err(e) => { app.status = format!("error: {}", e); }
    }
}

fn set_cursor_vol(app: &mut App, tok: &str) {
    let (s, c) = (app.cursor_step, app.cursor_ch);
    let cell = app.phrase().cells[s][c];
    if cell.note.is_none() {
        app.status = "cursor cell has no note — vol only applies to notes".into();
        return;
    }
    let v = match u8::from_str_radix(tok, 16) {
        Ok(v) if v <= 0x0F => v,
        Ok(v) => {
            app.status = format!("vol out of range (00–0F): {:02X}", v);
            return;
        }
        Err(_) => {
            app.status = format!("bad vol hex: {:?}", tok);
            return;
        }
    };
    app.snapshot();
    app.phrase_mut().cells[s][c].vol = v;
    app.status = format!("vol = {:02X} at [{:02X},ch{}]", v, s, c + 1);
}

fn set_cursor_fx(app: &mut App, tok: &str) {
    let (s, c) = (app.cursor_step, app.cursor_ch);
    if app.phrase().cells[s][c].note.is_none() {
        app.status = "cursor cell has no note — fx only applies to notes".into();
        return;
    }
    let tok = tok.to_ascii_uppercase();
    if tok.len() != 3 {
        app.status = format!("fx form: CPP (e.g. A04) — got {:?}", tok);
        return;
    }
    let bytes = tok.as_bytes();
    let cmd = bytes[0];
    if !cmd.is_ascii_alphanumeric() {
        app.status = format!("fx command must be A-Z or 0-9 — got {:?}", cmd as char);
        return;
    }
    let param = match u8::from_str_radix(&tok[1..], 16) {
        Ok(p) => p,
        Err(_) => {
            app.status = format!("bad fx param hex: {:?}", &tok[1..]);
            return;
        }
    };
    app.snapshot();
    app.phrase_mut().cells[s][c].fx = Some((cmd, param));
    app.status = format!("fx = {}{:02X} at [{:02X},ch{}]", cmd as char, param, s, c + 1);
}

fn clear_cursor_fx(app: &mut App) {
    let (s, c) = (app.cursor_step, app.cursor_ch);
    if app.phrase().cells[s][c].fx.is_none() {
        app.status = "no fx to clear".into();
        return;
    }
    app.snapshot();
    app.phrase_mut().cells[s][c].fx = None;
    app.status = format!("fx cleared at [{:02X},ch{}]", s, c + 1);
}

/// Shift every pitched note in the current phrase by `delta` semitones.
/// NOI is skipped — noise has no pitch, so transposing it would be a no-op
/// that nonetheless changes the displayed note, surprising the composer.
/// Notes that would clamp to 0 or 127 hold at those edges rather than wrap.
fn transpose_phrase(app: &mut App, tok: &str) {
    let delta = match tok.parse::<i32>() {
        Ok(d) => d,
        Err(_) => {
            app.status = format!("bad transpose amount: {:?} (try +5 or -3)", tok);
            return;
        }
    };
    perform(app, MacroOp::Transpose(delta));
}

/// `:diff <A> <B>` compares phrase A (shown in the grid) against B;
/// `:diff <B>` compares the current phrase against B; `:diff off` dismisses.
///
/// DESIGN.md describes two phrases stacked vertically, but two 16-step grids
/// need about 40 rows and the grid is already 83 columns wide — it does not
/// survive an 80x24 terminal. A unified diff, A in the grid with B's changes
/// composited on top and `+ - ~` in the margin, needs no new layout, adds no
/// columns, and is closer to what `git diff` actually is.
fn diff_cmd(app: &mut App, rest: &[&str]) {
    let parse = |t: &str| usize::from_str_radix(t, 16).ok().filter(|&i| i < app.song.phrases.len());
    let (a, b) = match rest {
        [] | ["off"] | ["clear"] => {
            let had = app.diff.take().is_some();
            app.status = if had { "diff off".into() } else { "usage: :diff [phrase] <A> [B] | :diff off".into() };
            return;
        }
        ["phrase", x] | [x] => (app.song.current_phrase, parse(x)),
        ["phrase", x, y] | [x, y] => match parse(x) {
            Some(i) => (i, parse(y)),
            None => (usize::MAX, None),
        },
        _ => {
            app.status = "usage: :diff [phrase] <A> [B] | :diff off".into();
            return;
        }
    };
    let (Some(b), true) = (b, a < app.song.phrases.len()) else {
        app.status = format!("diff: no such phrase (have {})", app.song.phrases.len());
        return;
    };
    // Show A in the grid so "edit A until it matches B" is a real workflow.
    app.song.current_phrase = a;
    app.cursor_step = app.cursor_step.min(STEPS_PER_PHRASE - 1);
    let overlay = Overlay::diff(
        &app.song.phrases[a],
        &app.song.phrases[b],
        format!("◈ diff {:02X}→{:02X}", a, b),
    );
    let (add, del, chg) = overlay.counts();
    app.status = format!("diff {:02X} → {:02X}: +{} -{} ~{}", a, b, add, del, chg);
    app.diff = Some(overlay);
}

// ---------- Stage 27: previewable commands ----------

/// A `:` command that can run against a bare [`Song`]. Parsing is pure — no
/// `App`, no I/O, no status writes — so the ghost preview and Enter run
/// literally the same code and cannot disagree about what a command means.
enum PreviewCmd<'a> {
    Gen(&'a [&'a str]),
    Transpose(i32),
}

/// Generators cheap and self-contained enough to re-run on every keystroke.
///
/// `lsystem` expands exponentially in its iteration count and `cellular`
/// is only meaningful once its arguments are complete, so both are left out
/// until there is a measurement to justify them. `style` is excluded a level
/// up: it reads a directory off disk and replaces the whole song, which one
/// phrase of overlay could not honestly show anyway.
const PREVIEWABLE_GENERATORS: &[&str] = &[
    "four", "four_on_floor", "euclid", "scale", "arp", "drums",
    "chord_prog", "chords", "prog", "bassline", "bass",
];

impl<'a> PreviewCmd<'a> {
    /// `None` means "not previewable" — silently, with no status write.
    fn parse(parts: &'a [&'a str]) -> Option<Self> {
        match parts {
            ["gen", "style", ..] => None,
            ["gen", rest @ ..] => {
                let name = rest.first()?;
                PREVIEWABLE_GENERATORS.contains(name).then(|| Self::Gen(rest))
            }
            ["transpose", n] | ["tr", n] => n.parse().ok().map(Self::Transpose),
            _ => None,
        }
    }

    fn apply(&self, song: &mut Song, seed: u64) -> Result<String> {
        match self {
            Self::Gen(args) => gen::dispatch(song, args, seed),
            Self::Transpose(d) => transpose_song(song, *d),
        }
    }
}

/// Transpose the song's current phrase, returning the message the commit
/// would print. `Err` for the cases that should leave the song untouched.
fn transpose_song(song: &mut Song, delta: i32) -> Result<String> {
    if delta == 0 {
        anyhow::bail!("transpose: 0 semitones (no-op)");
    }
    let idx = song.current_phrase.min(song.phrases.len().saturating_sub(1));
    let moved = transpose_phrase_cells(&mut song.phrases[idx], delta);
    if moved == 0 {
        anyhow::bail!("transpose: nothing to move (or all clamped)");
    }
    let sign = if delta > 0 { "+" } else { "" };
    Ok(format!("transposed {} note(s) by {}{} semitones", moved, sign, delta))
}

/// Recompute the ghost preview from the command line. Called after every
/// keystroke in Command mode, which is the only thing that can change its
/// input — nothing else mutates `app.song` while the command line is open.
fn refresh_ghost(app: &mut App) {
    app.ghost = None;
    if app.mode != Mode::Command {
        return;
    }
    let cmd = app.command_buf.trim().to_string();
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let Some(pc) = PreviewCmd::parse(&parts) else { return };

    // Run against a throwaway clone at the *same* seed the commit will
    // read. A Song is all small data, so this costs nothing per keystroke.
    let mut proposed = app.song.clone();
    if pc.apply(&mut proposed, app.gen_seed).is_err() {
        return; // half-typed or invalid: preview nothing, say nothing
    }
    let idx = app.song.current_phrase;
    let (Some(base), Some(after)) = (app.song.phrases.get(idx), proposed.phrases.get(idx)) else {
        return;
    };
    // Some generators (chord_prog, bassline) write following phrases too.
    // The overlay can only show one, so the title says so rather than
    // quietly under-reporting.
    let extra = proposed.phrases.len().saturating_sub(app.song.phrases.len());
    let more = if extra > 0 { format!(" +{} more phrase(s)", extra) } else { String::new() };
    app.ghost = Overlay::ghost(base, after, format!("◈ preview{}", more));
}

/// Shift every pitched note in `phrase` by `delta` semitones, returning how
/// many actually moved. NOI is skipped — its "pitch" selects a noise period,
/// not a note. Pulled out of [`transpose_delta`] so the ghost preview can run
/// exactly the same transformation against a throwaway copy of the song.
fn transpose_phrase_cells(phrase: &mut Phrase, delta: i32) -> usize {
    let mut moved = 0;
    for row in phrase.cells.iter_mut() {
        for (ch, cell) in row.iter_mut().enumerate() {
            if ch == 3 /* NOI */ { continue; }
            if let Some(n) = cell.note {
                let new_n = (n as i32 + delta).clamp(0, 127) as u8;
                if new_n != n {
                    cell.note = Some(new_n);
                    moved += 1;
                }
            }
        }
    }
    moved
}

fn transpose_delta(app: &mut App, delta: i32) {
    if delta == 0 {
        app.status = "transpose: 0 semitones (no-op)".into();
        return;
    }
    let was_dirty = app.dirty;
    app.snapshot();
    let moved = transpose_phrase_cells(app.phrase_mut(), delta);
    if moved == 0 {
        // No pitched notes moved — drop the snapshot so undo stays clean.
        app.undo_stack.pop();
        app.dirty = was_dirty;
        app.status = "transpose: nothing to move (or all clamped)".into();
    } else {
        let sign = if delta > 0 { "+" } else { "" };
        app.status = format!("transposed {} note(s) by {}{} semitones", moved, sign, delta);
    }
}

fn new_song(app: &mut App) {
    app.song = Song::default();
    app.current_file = None;
    app.cursor_step = 0;
    app.cursor_ch = 0;
    app.play_step = 0;
    app.undo_stack.clear();
    app.redo_stack.clear();
    app.dirty = false;
    app.status = "new song (no filename — :w <path> to save)".into();
}

// ---------- Main loop ----------

fn sync_audio(app: &mut App, engine: Option<&audio::AudioEngine>) {
    let Some(engine) = engine else {
        // No audio — don't let the queue grow forever if the user stays in Live.
        app.live_events.clear();
        return;
    };
    // Play transition: compile for the APU engine before the audio thread
    // sees `playing`.
    if app.playing && !app.prev_playing {
        refresh_nsf_for_playback(app);
    }
    app.prev_playing = app.playing;
    if let Ok(mut tr) = engine.transport.lock() {
        tr.bpm = app.song.bpm;
        tr.playing = app.playing;
        tr.phrase = app.phrase().clone();
        tr.instruments = app.song.instruments;
        tr.muted = app.muted;
        let (order, loop_pos) = app.song.flat_order();
        tr.song_mode = app.song_mode && !order.is_empty();
        if tr.song_mode {
            tr.order = order;
            tr.loop_pos = loop_pos;
            tr.phrases = app.song.phrases.clone();
            tr.arrangement_map = app.song.arrangement_map();
        } else {
            tr.order.clear();
            tr.phrases.clear();
            tr.arrangement_map.clear();
        }
        tr.groove = app.song.groove;
        tr.channel_length = app.song.channel_length;
        tr.engine = app.engine;
        tr.nsf = app.nsf_cache.clone();
        tr.nsf_generation = app.nsf_generation;
        tr.frames_per_row = app.nsf_frames_per_row;
        if tr.bank_generation != app.bank_generation {
            tr.bank = app.bank.clone();
            tr.bank_generation = app.bank_generation;
        }
        if let Some(e) = tr.engine_error.take() {
            app.status = format!("apu: {} — using synth", e);
        }
        // Forward any live gate events queued since the last frame.
        tr.live_events.extend(app.live_events.drain(..));
        app.play_step = tr.step;
        // Song mode: the grid follows the playing phrase.
        if app.playing && tr.song_mode && tr.playing_phrase < app.song.phrases.len()
            && app.song.current_phrase != tr.playing_phrase
        {
            app.song.current_phrase = tr.playing_phrase;
            app.cursor_step = app.cursor_step.min(STEPS_PER_PHRASE - 1);
        }
        // Stage 9: snapshot the viz state while we're inside the lock.
        app.viz_frame = tr.frame;
    }
    record_event(app);
    app.viz_tick = app.viz_tick.wrapping_add(1);
    let time_s = app.viz_tick as f32 / 60.0;
    // Stage 13: detect note-on edges and stamp the per-voice "last on" time.
    // Paired with EvalCtx::voice_ages, this gives `<ch>.age = seconds since
    // last trigger`, which composes with the expression language to produce
    // event-driven animations without a separate state-machine DSL.
    for ch in 0..CHANNELS {
        let g = app.viz_frame.voices[ch].gate;
        if g && !app.prev_gates[ch] {
            app.voice_last_on[ch] = time_s;
        }
        app.prev_gates[ch] = g;
    }
    let mut voice_ages = [f32::MAX; CHANNELS];
    for ch in 0..CHANNELS {
        voice_ages[ch] = (time_s - app.voice_last_on[ch]).max(0.0);
    }
    // Stage 12: fold binding overrides onto each placement once per UI tick.
    // Always computed (even without bindings) so the viz renderer has a
    // single input shape.
    let eval_ctx = modulation::EvalCtx {
        frame: &app.viz_frame,
        tempo: app.song.bpm as f32,
        scene_index: app.song.current_phrase as i32,
        phrase: app.song.current_phrase as i32,
        arr_slot: app.viz_frame.arr_slot as i32,
        chain_pos: app.viz_frame.chain_pos as i32,
        time_s,
        voice_ages,
    };
    app.effective_placements = modulation::apply_bindings(
        &app.sprite_placements,
        &app.bindings,
        &eval_ctx,
    );
    // Stage 12.1: resolve `palette = N` bindings to named palettes.
    // Alphabetical ordering of `:sprite palette <name> ...` registrations
    // gives a stable, user-controllable ring via naming convention.
    let mut palette_names: Vec<String> = app.sprite_palettes.keys().cloned().collect();
    palette_names.sort();
    modulation::resolve_palette_indices(&mut app.effective_placements, &palette_names);
    // Stage 7: fire any queued scene launch at the next bar boundary. We
    // detect the boundary by comparing to `prev_play_step` so we fire once
    // per crossing, not once per frame.
    if app.playing
        && app.queued_scene.is_some()
        && app.play_step != app.prev_play_step
        && app.play_step % 4 == 0
    {
        if let Some(slot) = app.queued_scene.take() {
            if let Some(phrase_idx) = app.scenes[slot] {
                if phrase_idx < app.song.phrases.len() {
                    app.song.current_phrase = phrase_idx;
                    app.status = format!("scene {} launched → phrase {:02X}",
                        slot + 1, phrase_idx);
                }
            }
        }
    }
    app.prev_play_step = app.play_step;
}

fn run<B: Backend>(terminal: &mut Terminal<B>, audio: Option<&audio::AudioEngine>, open: Option<PathBuf>) -> Result<()> {
    let mut app = App::new();
    if audio.is_none() {
        app.status = "audio disabled (no output device)".into();
    }
    if let Some(path) = open {
        edit_file(&mut app, &path);
        app.show_splash = false;
    }
    loop {
        sync_audio(&mut app, audio);
        if app.show_splash {
            let size = terminal.size()?;
            app.tick_splash(size.width, size.height);
        } else if !app.splash_particles.is_empty() {
            app.splash_particles.clear();
        }
        terminal.draw(|f| ui(f, &app))?;
        // 16ms poll ≈ 60Hz UI refresh — needed for the viz pane to animate
        // smoothly and for the DESIGN.md "breath / pulse" aesthetic.
        if event::poll(Duration::from_millis(16))? {
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(first) = args.first() {
        match first.as_str() {
            "check" | "compile" | "render" | "info" | "verify" | "fmt" | "gen" | "dpcm" | "--help" | "-h" | "help" => {
                return cli::run(&args);
            }
            _ => {}
        }
    }
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

    let open = args.first().map(PathBuf::from);
    let res = run(&mut terminal, audio.as_ref(), open);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn playing_app(step: usize, phase: f32) -> App {
        let mut app = App::new();
        app.show_splash = false;
        app.playing = true;
        app.play_step = step;
        app.viz_frame.step_phase = phase;
        app
    }

    /// Column the playhead glyph lives in: inside the border, one pad.
    const GLYPH_X: usize = 2;

    /// Render the phrase pane and return one row's symbols.
    fn row_symbols(app: &App, y: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| render_phrase(f, f.area(), app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area().width)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
            .collect()
    }

    #[test]
    fn mix_blends_rgb_and_switches_named_colors_at_the_halfway_point() {
        let a = Color::Rgb(0, 0, 0);
        let b = Color::Rgb(100, 200, 40);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        assert_eq!(mix(a, b, 0.5), Color::Rgb(50, 100, 20));
        // Out-of-range factors clamp rather than overshooting into garbage.
        assert_eq!(mix(a, b, 2.0), b);
        assert_eq!(mix(a, b, -1.0), a);
        // Named colors are never resolved to fixed RGB — the terminal
        // palette keeps deciding what they look like.
        assert_eq!(mix(Color::Yellow, Color::Red, 0.4), Color::Yellow);
        assert_eq!(mix(Color::Yellow, Color::Red, 0.6), Color::Red);
    }

    #[test]
    fn brighter_prefers_the_terminals_own_light_variants() {
        assert_eq!(brighter(Color::Red), Color::LightRed);
        assert_eq!(brighter(Color::Cyan), Color::LightCyan);
        assert_eq!(brighter(Color::DarkGray), Color::Gray);
        assert_eq!(brighter(Color::Rgb(0, 0, 0)), Color::Rgb(64, 64, 64));
        assert_eq!(brighter(Color::LightRed), Color::LightRed, "already bright, left alone");
    }

    #[test]
    fn breath_is_bounded_and_lands_on_the_downbeat() {
        // Step 0 with no sub-step phase is the top of the bar: every pulse
        // is at full, and they decay from there.
        let top = Breath::new(&playing_app(0, 0.0));
        assert!((top.pane() - 1.0).abs() < 1e-6);
        assert!((top.mode() - 1.0).abs() < 1e-6);
        let later = Breath::new(&playing_app(8, 0.0));
        assert!(later.pane() < top.pane(), "the bar pulse decays across the bar");
        assert!((later.mode() - 1.0).abs() < 1e-6, "the half-bar pulse restarts on beat 3");

        for step in 0..STEPS_PER_PHRASE {
            for phase in [0.0, 0.25, 0.5, 0.75] {
                let b = Breath::new(&playing_app(step, phase));
                for v in [b.pane(), b.mode(), b.cursor(), b.rec()] {
                    assert!((0.0..=1.0).contains(&v), "step {} phase {}: {}", step, phase, v);
                }
            }
        }
    }

    #[test]
    fn still_freezes_every_animation() {
        let mut app = playing_app(0, 0.0);
        app.still = true;
        let b = Breath::new(&app);
        assert_eq!((b.pane(), b.mode(), b.cursor(), b.rec()), (0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn breath_free_runs_at_tempo_while_stopped() {
        // Stopped, phase comes off the UI tick at the song's tempo: at 150
        // BPM a sixteenth is 10 sixteenths per second, so 60 ticks (one
        // second of UI) advances exactly ten steps — back on the downbeat.
        let mut app = App::new();
        app.playing = false;
        app.song.bpm = 150;
        app.viz_tick = 0;
        assert!((Breath::new(&app).pane() - 1.0).abs() < 1e-6);
        app.viz_tick = 30;
        assert!(Breath::new(&app).pane() < 1.0);
        app.viz_tick = 96; // 16 steps = one full bar
        assert!((Breath::new(&app).pane() - 1.0).abs() < 1e-3);
    }

    #[test]
    fn playhead_draws_a_diamond_with_a_two_step_fading_trail() {
        let app = playing_app(4, 0.0);
        // Step rows: border, header, blank, then step 00 — so step N is at
        // y = N + 3.
        let head = row_symbols(&app, 4 + 3);
        assert_eq!(head[GLYPH_X], "◆");
        assert_eq!(
            (head[GLYPH_X + 2].as_str(), head[GLYPH_X + 3].as_str()),
            ("0", "4"),
            "gutter reads the step index",
        );
        assert_eq!(row_symbols(&app, 3 + 3)[GLYPH_X], "◇", "one step behind");
        assert_eq!(row_symbols(&app, 2 + 3)[GLYPH_X], "·", "two steps behind");
        assert_eq!(row_symbols(&app, 1 + 3)[GLYPH_X], " ", "the trail is only two steps long");
        assert_eq!(row_symbols(&app, 5 + 3)[GLYPH_X], " ", "nothing ahead of the playhead");
    }

    #[test]
    fn the_playhead_glyph_does_not_shift_the_grid() {
        // The gutter is a fixed five columns whether or not a glyph is in
        // it, so every cell column lands in the same place stopped as
        // playing. Compare a row's symbols with only the glyph masked out.
        let playing = playing_app(4, 0.0);
        let mut stopped = playing_app(4, 0.0);
        stopped.playing = false;
        let with = row_symbols(&playing, 4 + 3);
        let without = row_symbols(&stopped, 4 + 3);
        assert_eq!(with[GLYPH_X + 1..], without[GLYPH_X + 1..]);
        assert_ne!(with[GLYPH_X], without[GLYPH_X]);
        // The header's lead matches the gutter, so PU1's column header sits
        // over PU1's cells.
        let first_letter = |row: &[String]| {
            row.iter()
                .skip(GLYPH_X + 1)
                .position(|s| s.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
                .expect("a letter after the gutter")
        };
        let header = row_symbols(&playing, 1);
        assert_eq!(
            first_letter(&with),
            first_letter(&header),
            "PU1's column header sits over PU1's cells",
        );
    }

    /// Every channel's header must sit over its own data, not just PU1's.
    /// The header emitted 16 columns per channel against a 15-column data
    /// column until Stage 27, so labels drifted one column right per
    /// channel — four by the DPCM column — and the PU1-only assertion above
    /// never noticed.
    #[test]
    fn every_channel_header_aligns_with_its_column() {
        let app = playing_app(0, 0.0);
        let header = row_symbols(&app, 1);
        let row = row_symbols(&app, 3);
        let starts = |line: &[String]| -> Vec<usize> {
            let mut out = Vec::new();
            let mut prev_blank = true;
            for (x, s) in line.iter().enumerate().skip(GLYPH_X + 2) {
                let ch = s.chars().next().unwrap_or(' ');
                let solid = ch.is_ascii_alphanumeric() || ch == '-';
                if solid && prev_blank {
                    out.push(x);
                }
                prev_blank = !solid;
            }
            out
        };
        let label_x = starts(&header);
        let field_x = starts(&row);
        assert_eq!(label_x.len(), CHANNELS, "one header label per channel: {:?}", label_x);
        for (ch, &x) in label_x.iter().enumerate() {
            assert!(field_x.contains(&x), "{} header at {} has no field there: {:?}", CH_NAMES[ch], x, field_x);
        }
    }

    // ---------- Stage 27: ghost preview and diff ----------

    fn press(app: &mut App, code: KeyCode) {
        handle_key(app, KeyEvent::from(code));
    }

    /// Open the command line and type `cmd`, exactly as a user would.
    fn type_command(app: &mut App, cmd: &str) {
        app.show_splash = false;
        press(app, KeyCode::Char(':'));
        for c in cmd.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    /// What the grid would hold if the ghost committed: the authored
    /// phrase with every mark applied.
    fn materialize(app: &App) -> Phrase {
        let mut p = app.phrase().clone();
        let o = app.ghost.as_ref().expect("a ghost");
        for s in 0..STEPS_PER_PHRASE {
            for c in 0..CHANNELS {
                match o.grid[s][c] {
                    Some(MarkedCell { cell, mark: Mark::Proposed }) => p.cells[s][c] = cell,
                    Some(MarkedCell { mark: Mark::Vanishing, .. }) => p.cells[s][c] = Cell::default(),
                    _ => {}
                }
            }
        }
        p
    }

    #[test]
    fn a_ghost_preview_changes_nothing() {
        let mut app = App::new();
        let before = app.song.phrases.clone();
        let seed = app.gen_seed;
        type_command(&mut app, "gen euclid pu1 5 16");
        assert!(app.ghost.is_some(), "a complete command should preview");
        assert_eq!(app.song.phrases, before, "preview must not touch the song");
        assert!(app.undo_stack.is_empty(), "preview must not touch undo");
        assert!(!app.dirty, "preview must not dirty the file");
        assert_eq!(app.gen_seed, seed, "only the commit may advance the seed");
    }

    #[test]
    fn escape_discards_the_preview() {
        let mut app = App::new();
        let before = app.song.phrases.clone();
        type_command(&mut app, "gen euclid noi 7 16");
        assert!(app.ghost.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.ghost.is_none());
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.song.phrases, before);
    }

    #[test]
    fn what_you_preview_is_what_enter_commits() {
        // The whole point of the feature: the ghost and the commit are the
        // same call at the same seed, so they cannot disagree.
        for cmd in ["gen euclid pu1 5 16", "gen four", "gen scale pu1 A minor 0.5", "transpose +5", "transpose -12"] {
            let mut app = App::new();
            // From an empty phrase, so even `gen four` has something to
            // propose — against the demo song it is a genuine no-op.
            app.song.phrases[0] = Phrase::default();
            app.song.phrases[0].cells[0][0] = Cell { note: Some(60), instr: 0, vol: 0, fx: None };
            type_command(&mut app, cmd);
            let expected = materialize(&app);
            press(&mut app, KeyCode::Enter);
            assert_eq!(*app.phrase(), expected, "{:?} committed something other than it previewed", cmd);
            assert!(app.ghost.is_none(), "{:?} left a ghost behind", cmd);
        }
    }


    // ---------- Stage 28: the panels ----------

    #[test]
    fn half_block_packs_two_pixel_rows_into_one_cell() {
        let mut c = viz::HalfBlock::new(4, 1);
        assert_eq!((c.width(), c.height()), (4, 2));
        c.set(0, 0, Color::Red);                 // top only
        c.set(1, 1, Color::Red);                 // bottom only
        c.set(2, 0, Color::Red);
        c.set(2, 1, Color::Red);                 // both, same colour
        c.set(3, 0, Color::Red);
        c.set(3, 1, Color::Blue);                // both, different
        let lines = c.lines();
        assert_eq!(lines.len(), 1);
        let glyphs: Vec<String> = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(glyphs, vec!["▀", "▄", "█", "▀"]);
        assert_eq!(lines[0].spans[3].style.bg, Some(Color::Blue));
        // Out-of-range writes are dropped rather than panicking, so callers
        // can plot a curve without clamping every point.
        c.set(-1, 0, Color::Red);
        c.set(0, 99, Color::Red);
        c.set(99, 0, Color::Red);
    }

    #[test]
    fn half_block_column_joins_a_vertical_run() {
        let mut c = viz::HalfBlock::new(1, 2);
        c.column(0, 3, 0, Color::Red); // reversed ends still fill
        let glyphs: Vec<String> = c.lines().iter().flat_map(|l| l.spans.iter().map(|s| s.content.to_string())).collect();
        assert_eq!(glyphs, vec!["█", "█"]);
    }

    #[test]
    fn the_instrument_panel_draws_a_curve_and_survives_any_size() {
        let mut app = App::new();
        app.show_splash = false;
        app.mode = Mode::Instrument;
        // Wide enough for graphs: the envelope should put ink on screen.
        let mut t = Terminal::new(TestBackend::new(80, 16)).unwrap();
        t.draw(|f| render_instrument(f, f.area(), &app)).unwrap();
        let buf = t.backend().buffer().clone();
        let ink = (0..buf.area().width)
            .flat_map(|x| (0..buf.area().height).map(move |y| (x, y)))
            .filter(|&(x, y)| matches!(buf.cell((x, y)).map(|c| c.symbol()), Some("█") | Some("▀") | Some("▄")))
            .count();
        assert!(ink > 20, "the envelope and waveform should draw, got {} cells", ink);
        // Narrow and tiny panes must not panic, and must still show the list.
        for (w, h) in [(40u16, 16u16), (80, 6), (20, 4), (8, 3), (4, 2)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| render_instrument(f, f.area(), &app)).unwrap();
        }
    }

    #[test]
    fn a_seeded_generator_previews_and_commits_the_same_notes() {
        // The randomised path is where a stray seed bump would show up.
        let mut app = App::new();
        type_command(&mut app, "gen scale pu2 C major 0.4");
        let expected = materialize(&app);
        let seed = app.gen_seed;
        press(&mut app, KeyCode::Enter);
        assert_eq!(*app.phrase(), expected);
        assert_eq!(app.gen_seed, seed + 1, "the commit consumes exactly one seed");
    }

    #[test]
    fn transpose_previews_without_recording_a_macro() {
        // Preview must not go through `perform`, which appends to the macro
        // buffer — previewing while recording would poison the macro.
        let mut app = App::new();
        app.recording_macro = Some(('a', Vec::new()));
        type_command(&mut app, "transpose +5");
        assert_eq!(app.recording_macro.as_ref().unwrap().1.len(), 0, "preview must not record");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.recording_macro.as_ref().unwrap().1.len(), 1, "the commit records once");
    }

    #[test]
    fn partial_and_unpreviewable_commands_say_nothing() {
        for cmd in ["gen", "gen eucl", "gen euclid pu1 x 16", "wq", "q", "set bpm=140", "gen style styles/neutral", "gen lsystem axiom=A rules=A=AB"] {
            let mut app = App::new();
            let status = app.status.clone();
            type_command(&mut app, cmd);
            assert!(app.ghost.is_none(), "{:?} should not preview", cmd);
            assert_eq!(app.status, status, "{:?} should not write a status", cmd);
            press(&mut app, KeyCode::Esc);
        }
    }

    #[test]
    fn a_no_op_command_previews_nothing() {
        let mut app = App::new();
        type_command(&mut app, "transpose +0");
        assert!(app.ghost.is_none(), "a no-op must not flicker an empty overlay");
    }

    #[test]
    fn ghost_cells_show_the_note_the_command_would_write() {
        let mut app = App::new();
        app.song.phrases[0] = Phrase::default(); // start empty so ghosts stand out
        app.still = true;
        type_command(&mut app, "gen four");
        let o = app.ghost.as_ref().expect("ghost");
        let marked: Vec<(usize, usize)> = (0..STEPS_PER_PHRASE)
            .flat_map(|s| (0..CHANNELS).map(move |c| (s, c)))
            .filter(|&(s, c)| o.grid[s][c].is_some())
            .collect();
        assert!(!marked.is_empty(), "four-on-the-floor should propose notes");
        let (s, c) = marked[0];
        assert_eq!(o.grid[s][c].unwrap().mark, Mark::Proposed);
        assert!(app.song.phrases[0].cells[s][c].note.is_none(), "the song itself is still empty there");
        // The proposed note reaches the screen, with its margin sigil.
        let row = row_symbols(&app, s as u16 + 3);
        let text: String = row.concat();
        assert!(text.contains('·'), "a proposed cell carries the ghost sigil: {:?}", text);
    }

    #[test]
    fn diff_classifies_added_removed_and_changed() {
        let note = |n: u8| Cell { note: Some(n), instr: 0, vol: 0, fx: None };
        let mut a = Phrase::default();
        let mut b = Phrase::default();
        b.cells[0][0] = note(60);                       // added
        a.cells[1][0] = note(62);                       // removed
        a.cells[2][0] = note(64);
        b.cells[2][0] = note(65);                       // changed: note
        a.cells[3][0] = note(67);
        b.cells[3][0] = Cell { vol: 9, ..note(67) };    // changed: volume only
        a.cells[4][0] = note(69);
        b.cells[4][0] = note(69);                       // identical

        let d = Overlay::diff(&a, &b, String::new());
        assert_eq!(d.grid[0][0].unwrap().mark, Mark::Added);
        assert_eq!(d.grid[1][0].unwrap().mark, Mark::Removed);
        assert_eq!(d.grid[1][0].unwrap().cell.note, Some(62), "a removal shows what is going away");
        assert!(matches!(d.grid[2][0].unwrap().mark, Mark::Changed(m) if m.note));
        match d.grid[3][0].unwrap().mark {
            Mark::Changed(m) => assert_eq!((m.note, m.vol, m.instr, m.fx), (false, true, false, false),
                "a volume-only change marks only the volume"),
            other => panic!("expected Changed, got {:?}", other),
        }
        assert!(d.grid[4][0].is_none(), "identical cells are not marked");
        assert_eq!(d.counts(), (1, 1, 2));
    }

    #[test]
    fn empty_cells_do_not_diff_on_fields_that_never_render() {
        // instr and vol print as `--` when there is no note, so a difference
        // there is invisible; marking it would put a sigil beside two cells
        // that look identical.
        let a = Phrase::default();
        let mut b = Phrase::default();
        b.cells[0][0] = Cell { note: None, instr: 7, vol: 9, fx: None };
        assert!(Overlay::diff(&a, &b, String::new()).grid[0][0].is_none());
        // An fx-only difference does render, so it is marked.
        let mut c = Phrase::default();
        c.cells[0][0] = Cell { note: None, instr: 0, vol: 0, fx: Some((b'V', 0x52)) };
        assert!(Overlay::diff(&a, &c, String::new()).grid[0][0].is_some());
    }

    #[test]
    fn a_standing_diff_is_cleared_by_anything_that_invalidates_it() {
        let with_diff = || {
            let mut app = App::new();
            app.show_splash = false;
            app.song.phrases.push(Phrase::default());
            execute_command(&mut app, "diff phrase 00 01");
            assert!(app.diff.is_some(), "diff should be showing");
            app
        };
        // Any edit: every mutation snapshots first, so one clear covers all.
        let mut app = with_diff();
        app.snapshot();
        assert!(app.diff.is_none(), "an edit invalidates the comparison");
        // Undo and redo replace the song wholesale without snapshotting.
        let mut app = with_diff();
        app.undo_stack.push(app.song.clone());
        app.undo();
        assert!(app.diff.is_none(), "undo invalidates it");
        // Walking to another phrase.
        let mut app = with_diff();
        goto_phrase(&mut app, 1);
        assert!(app.diff.is_none(), "leaving the phrase ends the diff");
        // Esc, the "stop everything" key.
        let mut app = with_diff();
        press(&mut app, KeyCode::Esc);
        assert!(app.diff.is_none(), "Esc dismisses it");
        assert_eq!(app.status, "diff off");
    }

    #[test]
    fn diff_reports_its_counts_and_rejects_missing_phrases() {
        let mut app = App::new();
        app.song.phrases[0] = Phrase::default();
        app.song.phrases.push(Phrase::default());
        app.song.phrases[1].cells[0][0] = Cell { note: Some(60), instr: 0, vol: 0, fx: None };
        execute_command(&mut app, "diff phrase 00 01");
        assert!(app.status.contains("+1"), "{}", app.status);
        execute_command(&mut app, "diff off");
        assert!(app.diff.is_none());
        execute_command(&mut app, "diff phrase 00 09");
        assert!(app.diff.is_none() && app.status.contains("no such phrase"), "{}", app.status);
    }

    #[test]
    fn the_overlay_never_hides_the_cursor() {
        let mut app = App::new();
        app.still = true;
        app.song.phrases[0] = Phrase::default();
        type_command(&mut app, "gen four");
        let o = app.ghost.as_ref().unwrap();
        let (s, c) = (0..STEPS_PER_PHRASE)
            .flat_map(|s| (0..CHANNELS).map(move |c| (s, c)))
            .find(|&(s, c)| o.grid[s][c].is_some())
            .expect("a marked cell");
        app.cursor_step = s;
        app.cursor_ch = c;
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| render_phrase(f, f.area(), &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // Find the cursor's note glyph and check it kept the cursor colour.
        let y = s as u16 + 3;
        let x = 2 + 5 + (c as u16) * 15 + 1;
        let bg = buf.cell((x, y)).unwrap().bg;
        assert_eq!(bg, app.theme.cursor_bg, "the mark tint must not cover the cursor");
    }


    #[test]
    fn the_selected_parameter_highlights_its_own_envelope_segment() {
        // Editing `attack` should visibly mark the attack ramp — that link
        // is the honest version of "the envelope lights up".
        let mut app = App::new();
        app.show_splash = false;
        app.mode = Mode::Instrument;
        let accent_cells = |app: &App| {
            let mut t = Terminal::new(TestBackend::new(80, 16)).unwrap();
            t.draw(|f| render_instrument(f, f.area(), app)).unwrap();
            let buf = t.backend().buffer().clone();
            (0..buf.area().width)
                .flat_map(|x| (0..buf.area().height).map(move |y| (x, y)))
                .filter(|&(x, y)| {
                    let c = buf.cell((x, y)).unwrap();
                    x >= 32 && c.fg == app.theme.accent && matches!(c.symbol(), "█" | "▀" | "▄")
                })
                .count()
        };
        app.instr_param = 3; // release — the longest stage by default
        let release = accent_cells(&app);
        app.instr_param = 0; // attack — 2 ms, so a much shorter ramp
        let attack = accent_cells(&app);
        assert!(release > 0 && attack > 0, "both segments should be drawn");
        assert!(release > attack, "release ({}) is far longer than attack ({})", release, attack);
    }

    #[test]
    fn the_queue_drain_empties_smoothly_and_never_overflows() {
        // Four steps out, full bar; on the boundary step with the phase run
        // out, empty. Monotonic in between, and clamped at both ends.
        assert_eq!(drain_fraction(4, 0.0), 1.0);
        assert_eq!(drain_fraction(1, 1.0), 0.0);
        assert!(drain_fraction(2, 0.5) < drain_fraction(2, 0.0));
        assert!(drain_fraction(1, 0.0) < drain_fraction(2, 0.0));
        // Out-of-range phases from a stale frame must not escape 0..=1.
        assert_eq!(drain_fraction(9, 0.0), 1.0);
        assert_eq!(drain_fraction(0, 5.0), 0.0);
    }

    #[test]
    fn the_register_thumbnail_shows_shape_and_says_when_it_is_empty() {
        let mut app = App::new();
        app.show_splash = false;
        app.viz_kind = viz::VizKind::Register;
        let text = |app: &App| {
            let mut t = Terminal::new(TestBackend::new(50, 12)).unwrap();
            t.draw(|f| {
                let ctx = viz::VizCtx {
                    frame: &app.viz_frame, tick: 0, sheets: &app.sprite_sheets,
                    placements: &app.effective_placements, palettes: &app.sprite_palettes,
                    bg: app.theme.viz_bg, register: &app.register,
                    fg: app.theme.note, dim: app.theme.dim,
                };
                viz::render(f, f.area(), viz::VizKind::Register, &ctx);
            }).unwrap();
            let buf = t.backend().buffer().clone();
            (0..buf.area().height)
                .map(|y| (0..buf.area().width).map(|x| buf.cell((x, y)).unwrap().symbol().to_string()).collect::<String>())
                .collect::<Vec<_>>().join("\n")
        };
        assert!(text(&app).contains("register empty"), "an empty register says so");
        // Yank two rows of the demo phrase and check the shape reads out.
        app.cursor_step = 0;
        app.yank_range(0..2, 0..CHANNELS);
        let out = text(&app);
        assert!(out.contains("2 row(s)"), "{}", out);
        assert!(out.contains('·'), "empty cells render faint: {}", out);
        assert!(out.contains('A'), "a note shows its letter: {}", out);
    }

    // ---------- Stage 29: atmosphere ----------

    fn cell_at(app: &App, x: u16, y: u16) -> ratatui::buffer::Cell {
        let mut t = Terminal::new(TestBackend::new(90, 24)).unwrap();
        t.draw(|f| render_phrase(f, f.area(), app)).unwrap();
        t.backend().buffer().cell((x, y)).unwrap().clone()
    }

    #[test]
    fn phosphor_scanlines_tint_alternate_rows_and_nes_has_none() {
        let mut app = playing_app(0, 0.0);
        app.still = true;
        app.cursor_ch = 0;
        app.cursor_step = 0;
        // A column well clear of the cursor column and the playhead rows.
        // Step 5 is an odd row (scanlined), step 6 an even one.
        let (x, even_y, odd_y) = (60, 6 + 3, 5 + 3);
        app.theme = Theme::by_name("nes").unwrap();
        assert_eq!(cell_at(&app, x, odd_y).bg, Color::Reset, "nes asks for no scanline");
        app.theme = Theme::by_name("phosphor").unwrap();
        let scan = app.theme.scanline_bg.expect("phosphor declares a scanline");
        assert_eq!(cell_at(&app, x, odd_y).bg, scan, "odd rows sit on the scanline ground");
        assert_eq!(cell_at(&app, x, even_y).bg, Color::Reset, "even rows do not");
    }

    #[test]
    fn the_scanline_never_outranks_something_meaningful() {
        // Lowest precedence: the cursor, the playhead and the column tint
        // all still win on a scanline row.
        let mut app = playing_app(3, 0.0);
        app.still = true;
        app.theme = Theme::by_name("phosphor").unwrap();
        app.cursor_step = 5;
        app.cursor_ch = 0;
        assert_eq!(cell_at(&app, 8, 5 + 3).bg, app.theme.cursor_bg, "cursor wins");
        assert_ne!(cell_at(&app, 60, 3 + 3).bg, app.theme.scanline_bg.unwrap(), "playhead row wins");
    }

    #[test]
    fn insert_mode_dims_the_channels_you_are_not_typing_into() {
        let mut app = playing_app(0, 0.0);
        app.still = true;
        app.cursor_ch = 0;
        let other_x = 8 + 15; // PU2's note field
        let normal = cell_at(&app, other_x, 3);
        app.mode = Mode::Insert;
        let insert = cell_at(&app, other_x, 3);
        assert!(!normal.modifier.contains(Modifier::DIM));
        assert!(insert.modifier.contains(Modifier::DIM), "other channels recede in INSERT");
        // The column being typed into does not.
        assert!(!cell_at(&app, 8, 3).modifier.contains(Modifier::DIM));
    }

    #[test]
    fn live_mode_outlines_the_pane_and_a_snare_flashes_it() {
        let mut app = playing_app(0, 0.5);
        app.still = true;
        let quiet = cell_at(&app, 40, 0).fg;
        app.mode = Mode::Live;
        let live = cell_at(&app, 40, 0).fg;
        assert_ne!(quiet, live, "LIVE recolours the frame");
        // A NOI hit pushes the border toward the playhead colour, so the
        // viz visibly bleeds into the editor chrome.
        app.mode = Mode::Normal;
        app.viz_frame.voices[3].env_level = 1.0;
        assert_ne!(cell_at(&app, 40, 0).fg, quiet, "a snare flashes the border");
    }

    #[test]
    fn the_event_log_follows_the_status_line_without_repeating_it() {
        let mut app = App::new();
        app.status = "first".into();
        record_event(&mut app);
        record_event(&mut app); // unchanged status must not double-log
        app.status = "second".into();
        record_event(&mut app);
        assert_eq!(app.event_log.iter().cloned().collect::<Vec<_>>(), vec!["second", "first"]);
        // It stays a log, not a transcript.
        for i in 0..20 {
            app.status = format!("event {}", i);
            record_event(&mut app);
        }
        assert_eq!(app.event_log.len(), EVENT_LOG_MAX);
        assert_eq!(app.event_log[0], "event 19");
        // An empty status clears nothing and adds nothing.
        app.status = String::new();
        record_event(&mut app);
        assert_eq!(app.event_log[0], "event 19");
    }

    #[test]
    fn the_modeline_shows_the_log_only_when_it_is_otherwise_idle() {
        let mut app = App::new();
        app.show_splash = false;
        for s in ["alpha", "bravo"] {
            app.status = s.into();
            record_event(&mut app);
        }
        let row2 = |app: &App| {
            let mut t = Terminal::new(TestBackend::new(90, 2)).unwrap();
            t.draw(|f| render_status(f, f.area(), app)).unwrap();
            let b = t.backend().buffer().clone();
            (0..b.area().width).map(|x| b.cell((x, 1)).unwrap().symbol().to_string()).collect::<String>()
        };
        assert!(row2(&app).contains("alpha"), "idle row 2 shows the log");
        assert!(!row2(&app).contains("bravo"), "the newest entry is already the status line");
        // A pending count owns row 2 while it is live.
        app.count = 4;
        assert!(row2(&app).contains('4') && !row2(&app).contains("alpha"));
    }
}
