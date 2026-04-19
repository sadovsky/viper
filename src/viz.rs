//! Stage 10: built-in visualizer pane. Reads `VizFrame` from the audio
//! thread (Stage 9 bus) and draws one of four views using half-block
//! characters for 2× vertical resolution.
//!
//! Each row we render is actually two rows of "pixels": upper half from
//! the fg color, lower half from the bg color, joined by `▀`.

use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::audio::VizFrame;
use crate::sprite::{Placement, SpriteSheet, PALETTE_SIZE};
use crate::CHANNELS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VizKind {
    Bars,
    Scope,
    Grid,
    Orbit,
    Sprites,
}

impl VizKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            VizKind::Bars => "bars",
            VizKind::Scope => "scope",
            VizKind::Grid => "grid",
            VizKind::Orbit => "orbit",
            VizKind::Sprites => "sprites",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "bars" => Some(VizKind::Bars),
            "scope" => Some(VizKind::Scope),
            "grid" => Some(VizKind::Grid),
            "orbit" => Some(VizKind::Orbit),
            "sprites" | "sprite" => Some(VizKind::Sprites),
            _ => None,
        }
    }
}

/// Everything the viz renderers need from the app. Bundled so the render
/// signature doesn't grow every time a new renderer needs extra state.
pub(crate) struct VizCtx<'a> {
    pub frame: &'a VizFrame,
    pub tick: u32,
    pub sheets: &'a HashMap<String, SpriteSheet>,
    pub placements: &'a [Placement],
    pub palettes: &'a HashMap<String, [Color; PALETTE_SIZE]>,
}

/// Per-channel palette. NES-ish: green pulse, amber pulse, violet triangle,
/// pink noise. Matches DESIGN.md's channel-color discipline.
fn channel_color(ch: usize) -> Color {
    match ch {
        0 => Color::Rgb(120, 220, 110), // PU1 — green
        1 => Color::Rgb(240, 180, 80),  // PU2 — amber
        2 => Color::Rgb(150, 130, 240), // TRI — blue-violet
        3 => Color::Rgb(240, 120, 180), // NOI — pink
        _ => Color::Gray,
    }
}

fn channel_name(ch: usize) -> &'static str {
    match ch {
        0 => "PU1",
        1 => "PU2",
        2 => "TRI",
        3 => "NOI",
        _ => "???",
    }
}

/// Render the viz pane. Called from `ui()` when `app.show_viz` is true.
pub(crate) fn render(f: &mut Frame, area: Rect, kind: VizKind, ctx: &VizCtx) {
    let title = format!(" viz · {} ", kind.name());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width < 4 || inner.height < 2 {
        return;
    }

    match kind {
        VizKind::Bars => render_bars(f, inner, ctx.frame),
        VizKind::Scope => render_scope(f, inner, ctx.frame, ctx.tick),
        VizKind::Grid => render_grid(f, inner, ctx.frame),
        VizKind::Orbit => render_orbit(f, inner, ctx.frame),
        VizKind::Sprites => render_sprites(f, inner, ctx),
    }
}

// ---------- Bars ----------

/// Four vertical bars, one per voice, scaled by `env_level * vel`. Each
/// row uses `▀` so the bar has 2× vertical resolution: we fill both halves
/// when the bar crosses a row, and the upper half alone when it lands
/// between them.
fn render_bars(f: &mut Frame, area: Rect, frame: &VizFrame) {
    let rows = area.height as usize;
    let pixels = rows * 2; // half-block doubling
    let cols = area.width as usize;
    let bar_w = (cols / CHANNELS).max(1);

    // Heights in "pixels" (0..=pixels).
    let heights: [usize; CHANNELS] = std::array::from_fn(|ch| {
        let amp = (frame.voices[ch].env_level * frame.voices[ch].vel).clamp(0.0, 1.0);
        (amp * pixels as f32).round() as usize
    });

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for row in 0..rows {
        // `row` counts from the top; bars grow from the bottom, so each
        // row corresponds to pixel indices `p_top` (upper half) and
        // `p_bot` (lower half), counted from the bottom.
        let p_top = 2 * (rows - row) - 1;
        let p_bot = 2 * (rows - row) - 2;

        let mut spans: Vec<Span> = Vec::with_capacity(CHANNELS + 1);
        for ch in 0..CHANNELS {
            let h = heights[ch];
            let color = channel_color(ch);
            let (ch_upper, ch_lower) = (h > p_top, h > p_bot);
            let (glyph, style) = match (ch_upper, ch_lower) {
                (true, true) => ("█", Style::default().fg(color)),
                (false, true) => ("▄", Style::default().fg(color)),
                (true, false) => ("▀", Style::default().fg(color)),
                (false, false) => (" ", Style::default()),
            };
            spans.push(Span::styled(glyph.repeat(bar_w), style));
        }
        // Last row: draw a baseline label underneath the bar stack.
        if row == rows - 1 {
            spans.clear();
            for ch in 0..CHANNELS {
                let color = channel_color(ch);
                let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
                let label = format!("{:^width$}", channel_name(ch), width = bar_w);
                spans.push(Span::styled(label, style));
            }
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), area);
}

// ---------- Scope ----------

/// Synthesized waveform trace. We don't share audio-thread samples — we
/// re-run each voice's oscillator shape across the pane width, scale by
/// env*vel, sum, and render the trace. `tick` is the 60Hz UI counter;
/// we convert to seconds and advance each voice's phase by `freq * time`
/// so the trace scrolls at the voice's pitch. A visual-scale factor slows
/// real pitches down to a readable handful of cycles/sec; without it,
/// 440Hz at 60fps would alias into stroboscopic noise.
fn render_scope(f: &mut Frame, area: Rect, frame: &VizFrame, tick: u32) {
    let cols = area.width as usize;
    let rows = area.height as usize;
    let pixels = rows * 2;
    if cols == 0 || rows == 0 {
        return;
    }

    // ~60Hz UI tick. Treat as seconds, then slow visual pitch to ~1/200th
    // of real so a middle-C voice scrolls at ≈1.3 cycles/sec on screen.
    let time_s = tick as f32 / 60.0;
    const VIS_SCALE: f32 = 0.005;

    // Sample the combined waveform once per column. Range is x ∈ 0..1
    // across the pane; each voice contributes a different cycle count so
    // they stay distinguishable when multiple play at once.
    let mut samples: Vec<f32> = Vec::with_capacity(cols);
    for i in 0..cols {
        let t = i as f32 / cols as f32;
        let mut y = 0.0f32;
        for ch in 0..CHANNELS {
            let v = frame.voices[ch];
            let amp = (v.env_level * v.vel).clamp(0.0, 1.0);
            if amp <= 0.01 { continue; }
            let cycles = 2.0 + ch as f32;
            // Per-voice scroll: real voices (freq>0) scroll at freq-scaled
            // rate; noise freq is 0, so we fall back to a fixed scroll.
            let scroll = if v.freq > 1.0 {
                v.freq * time_s * VIS_SCALE
            } else {
                time_s * 0.8
            };
            let phase = ((t * cycles) + scroll).rem_euclid(1.0);
            let s = match ch {
                0 | 1 => if phase < 0.5 { 1.0 } else { -1.0 },
                2 => 1.0 - 4.0 * (phase - 0.5).abs(),
                _ => (phase * 17.0).sin(), // noise-ish
            };
            y += s * amp;
        }
        // Soft-clip to -1..1 then lift to 0..pixels.
        let y = y.clamp(-1.0, 1.0);
        samples.push(y);
    }

    // Build a 2D occupancy bitmap at half-block resolution, then collapse
    // pairs of rows into one line of `▀`/`▄`/`█`/` ` glyphs.
    let mut grid: Vec<Vec<bool>> = vec![vec![false; cols]; pixels];
    let mid = pixels as f32 / 2.0;
    for (x, &y) in samples.iter().enumerate() {
        let py = (mid - y * (mid - 1.0)).round().clamp(0.0, (pixels - 1) as f32) as usize;
        grid[py][x] = true;
    }

    // Trace color: the loudest voice "wins" the frame's tint, so it reads
    // as "pu1 is singing" vs a dead gray line.
    let (loud_ch, loud_amp) = (0..CHANNELS).fold((0usize, 0.0f32), |acc, ch| {
        let a = frame.voices[ch].env_level * frame.voices[ch].vel;
        if a > acc.1 { (ch, a) } else { acc }
    });
    let trace = if loud_amp > 0.01 { channel_color(loud_ch) } else { Color::DarkGray };

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for row in 0..rows {
        let upper = &grid[2 * row];
        let lower = &grid[2 * row + 1];
        let mut s = String::with_capacity(cols);
        for x in 0..cols {
            s.push(match (upper[x], lower[x]) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        lines.push(Line::from(Span::styled(s, Style::default().fg(trace))));
    }
    f.render_widget(Paragraph::new(lines), area);
}

// ---------- Grid ----------

/// 16-step sequence lit around the playhead. Each step = a cell; the
/// active step glows the brightest, neighbours fade. `step_phase`
/// brightens the playhead as we approach the next step, so it pulses.
fn render_grid(f: &mut Frame, area: Rect, frame: &VizFrame) {
    let rows = area.height as usize;
    let cols = area.width as usize;
    if rows == 0 || cols == 0 { return; }

    // 4×4 grid of steps. Each cell is roughly cols/4 wide × rows/4 tall.
    let cw = (cols / 4).max(1);
    let rh = (rows / 4).max(1);

    let active = frame.step.min(15);
    let pulse = 0.6 + 0.4 * (1.0 - frame.step_phase); // full at step boundary, dims before the next.

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for row in 0..rows {
        let gy = (row / rh).min(3);
        let mut spans: Vec<Span> = Vec::with_capacity(4);
        for gx in 0..4 {
            let idx = gy * 4 + gx;
            let distance = idx.abs_diff(active);
            let (glyph, color) = if idx == active {
                let bright = (255.0 * pulse) as u8;
                ("◆", Color::Rgb(bright, bright, 100))
            } else if distance <= 1 && frame.playing {
                ("◇", Color::Rgb(120, 120, 80))
            } else {
                ("·", Color::Rgb(60, 60, 70))
            };
            let cell_str = format!("{:^width$}", glyph, width = cw);
            spans.push(Span::styled(cell_str, Style::default().fg(color)));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), area);
}

// ---------- Orbit ----------

/// Each voice is a body on a shared orbit. Angle comes from the note's
/// position in the octave (pitch class), radius from velocity, brightness
/// from env level. Silent voices leave no trace.
fn render_orbit(f: &mut Frame, area: Rect, frame: &VizFrame) {
    let cols = area.width as usize;
    let rows = area.height as usize;
    let pixels = rows * 2;
    if cols == 0 || rows == 0 { return; }

    let cx = cols as f32 / 2.0;
    let cy = pixels as f32 / 2.0;
    let r_max = (cx.min(cy) - 1.0).max(1.0);

    // Bitmap of occupancy + per-pixel color.
    let mut color_grid: Vec<Vec<Option<Color>>> = vec![vec![None; cols]; pixels];

    // Ring guide (dim), so the pane doesn't look empty at rest.
    let ring_steps = 64;
    for i in 0..ring_steps {
        let theta = (i as f32 / ring_steps as f32) * std::f32::consts::TAU;
        let x = (cx + theta.cos() * r_max * 0.75).round() as isize;
        let y = (cy + theta.sin() * r_max * 0.75).round() as isize;
        if x >= 0 && (x as usize) < cols && y >= 0 && (y as usize) < pixels {
            color_grid[y as usize][x as usize] = Some(Color::Rgb(40, 40, 60));
        }
    }

    // Each voice is a body. Freq → angle via log2(f/C4). Env → brightness.
    for ch in 0..CHANNELS {
        let v = frame.voices[ch];
        let amp = v.env_level.clamp(0.0, 1.0);
        if amp < 0.02 || v.freq < 1.0 { continue; }
        // log2 pitch class — one full orbit per octave. NOI freq is 0,
        // so it was filtered above; we give it a dedicated angle so it
        // still shows when gating.
        let theta = if ch == 3 {
            std::f32::consts::PI / 2.0
        } else {
            let semitones = 12.0 * (v.freq / 261.63).log2();
            let wrapped = semitones.rem_euclid(12.0);
            (wrapped / 12.0) * std::f32::consts::TAU
        };
        let r = r_max * (0.35 + 0.5 * v.vel.clamp(0.0, 1.0));
        let x = (cx + theta.cos() * r).round() as isize;
        let y = (cy + theta.sin() * r).round() as isize;
        let base = channel_color(ch);
        let c = scale_color(base, amp);
        // Draw the body + 4-neighbour cross so it reads as a dot.
        for (dx, dy) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
            let px = x + dx;
            let py = y + dy;
            if px >= 0 && (px as usize) < cols && py >= 0 && (py as usize) < pixels {
                color_grid[py as usize][px as usize] = Some(c);
            }
        }
    }

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for row in 0..rows {
        let upper = &color_grid[2 * row];
        let lower = &color_grid[2 * row + 1];
        let mut spans: Vec<Span> = Vec::with_capacity(cols);
        for x in 0..cols {
            let (u, l) = (upper[x], lower[x]);
            let (glyph, style) = match (u, l) {
                (Some(a), Some(b)) => ("▀", Style::default().fg(a).bg(b)),
                (Some(a), None) => ("▀", Style::default().fg(a)),
                (None, Some(b)) => ("▄", Style::default().fg(b)),
                (None, None) => (" ", Style::default()),
            };
            spans.push(Span::styled(glyph.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), area);
}

// ---------- Sprites ----------

/// Draw every placement into a half-block pixel buffer, then collapse pairs
/// of rows into terminal cells. Later placements win pixel conflicts so
/// the paint order is predictable (FIFO). Slot 0 is transparent — a
/// transparent pixel never writes, so placements can overlap cleanly.
fn render_sprites(f: &mut Frame, area: Rect, ctx: &VizCtx) {
    let cols = area.width as usize;
    let rows = area.height as usize;
    let pixels = rows * 2;
    if cols == 0 || rows == 0 { return; }

    if ctx.placements.is_empty() {
        let hint = Line::from(Span::styled(
            "no placements — :sprite load <path> [WxH] then :sprite place <name> <idx> <x> <y>",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(Paragraph::new(hint), area);
        return;
    }

    let mut grid: Vec<Vec<Option<Color>>> = vec![vec![None; cols]; pixels];

    for p in ctx.placements {
        let Some(sheet) = ctx.sheets.get(&p.sheet) else { continue; };
        // Palette override via named palette, else the sheet's own.
        let palette = p
            .palette
            .as_ref()
            .and_then(|n| ctx.palettes.get(n).copied())
            .unwrap_or(sheet.palette);
        for py in 0..sheet.cell_h {
            for px in 0..sheet.cell_w {
                let Some(idx) = sheet.pixel(p.idx, px, py) else { continue; };
                if idx == 0 { continue; } // transparent
                let color = palette[idx as usize];
                let sx = p.x + px as i32;
                let sy = p.y + py as i32;
                if sx < 0 || sy < 0 { continue; }
                let (sx, sy) = (sx as usize, sy as usize);
                if sx >= cols || sy >= pixels { continue; }
                grid[sy][sx] = Some(color);
            }
        }
    }

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for row in 0..rows {
        let upper = &grid[2 * row];
        let lower = &grid[2 * row + 1];
        let mut spans: Vec<Span> = Vec::with_capacity(cols);
        for x in 0..cols {
            let (u, l) = (upper[x], lower[x]);
            let (glyph, style) = match (u, l) {
                (Some(a), Some(b)) => ("▀", Style::default().fg(a).bg(b)),
                (Some(a), None) => ("▀", Style::default().fg(a)),
                (None, Some(b)) => ("▄", Style::default().fg(b)),
                (None, None) => (" ", Style::default()),
            };
            spans.push(Span::styled(glyph.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Scale an RGB color toward black by `t ∈ 0..=1`; `t=1` returns the
/// original. Used to dim orbit bodies by their envelope level.
fn scale_color(c: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match c {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as f32 * t) as u8,
            (g as f32 * t) as u8,
            (b as f32 * t) as u8,
        ),
        other => other,
    }
}
