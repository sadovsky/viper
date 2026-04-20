//! Stage 11: sprite engine. Loads PNG sprite sheets and exposes them
//! to the visualizer as indexed 4-color tiles addressed by grid index.
//!
//! The NES-style constraint (≤4 colors per sheet, slot 0 = transparent)
//! is not cosmetic — it enforces the same discipline as the rest of the
//! project and keeps palettes tiny, swappable, and composable with
//! modulation bindings in Stage 12.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use ratatui::style::Color;

pub(crate) const PALETTE_SIZE: usize = 4;

/// A decoded PNG reduced to ≤4 indexed colors. `indices[y * width + x]`
/// is 0..=3, where 0 is the canonical transparent slot.
#[derive(Clone, Debug)]
pub(crate) struct SpriteSheet {
    /// Stable identifier used by placements; defaults to the file stem.
    pub name: String,
    pub source: PathBuf,
    /// Full-image pixel dimensions.
    pub width: u32,
    pub height: u32,
    /// Per-cell dimensions. `(width / cell_w, height / cell_h)` gives the
    /// grid — cells are addressed by `y * cols + x` in row-major order.
    pub cell_w: u32,
    pub cell_h: u32,
    /// Indexed-color image, 0..=3 per pixel. Index 0 means transparent
    /// regardless of palette.
    pub indices: Vec<u8>,
    /// The sheet's active 4-entry palette. Slot 0 always renders as
    /// transparent; slots 1..=3 render with their RGB.
    pub palette: [Color; PALETTE_SIZE],
}

impl SpriteSheet {
    pub(crate) fn cols(&self) -> u32 {
        self.width / self.cell_w.max(1)
    }

    pub(crate) fn rows(&self) -> u32 {
        self.height / self.cell_h.max(1)
    }

    pub(crate) fn cell_count(&self) -> u32 {
        self.cols() * self.rows()
    }

    /// Return the indexed pixel at `(px, py)` of cell `idx`, or None if
    /// the index is out of range.
    pub(crate) fn pixel(&self, idx: u32, px: u32, py: u32) -> Option<u8> {
        let cols = self.cols();
        if cols == 0 { return None; }
        let cx = idx % cols;
        let cy = idx / cols;
        if cy >= self.rows() { return None; }
        if px >= self.cell_w || py >= self.cell_h { return None; }
        let x = cx * self.cell_w + px;
        let y = cy * self.cell_h + py;
        Some(self.indices[(y * self.width + x) as usize])
    }
}

/// Load a PNG and derive a ≤4-entry palette (slot 0 = transparent, 1..=3
/// opaque). Alpha < 8 always maps to slot 0.
///
/// With `quantize=false`, errors if the image uses more than 3 distinct
/// opaque colors — preserves NES discipline and catches accidentally-rich
/// sheets. With `quantize=true`, keeps the 3 most-frequent opaque colors
/// and remaps the rest to the nearest by squared RGB distance.
pub(crate) fn load_sheet(
    name: impl Into<String>,
    path: &Path,
    cell_w: u32,
    cell_h: u32,
    quantize: bool,
) -> Result<SpriteSheet> {
    if cell_w == 0 || cell_h == 0 {
        bail!("cell dimensions must be ≥ 1 (got {}×{})", cell_w, cell_h);
    }
    let img = image::open(path)
        .with_context(|| format!("open sprite sheet {}", path.display()))?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w % cell_w != 0 || h % cell_h != 0 {
        bail!(
            "sheet {}×{} not divisible by cell size {}×{}",
            w, h, cell_w, cell_h
        );
    }

    // Pass 1: count opaque color occurrences.
    let mut counts: Vec<([u8; 3], usize)> = Vec::new();
    for pixel in rgba.pixels() {
        let [r, g, b, a] = pixel.0;
        if a < 8 { continue; }
        let key = [r, g, b];
        match counts.iter().position(|(c, _)| *c == key) {
            Some(i) => counts[i].1 += 1,
            None => counts.push((key, 1)),
        }
    }

    let chosen: Vec<[u8; 3]> = if counts.len() <= PALETTE_SIZE - 1 {
        counts.iter().map(|(c, _)| *c).collect()
    } else if quantize {
        counts.sort_by(|a, b| b.1.cmp(&a.1));
        counts.truncate(PALETTE_SIZE - 1);
        counts.into_iter().map(|(c, _)| c).collect()
    } else {
        return Err(anyhow!(
            "sheet {} has {} opaque colors (>3) — reduce palette or add \
             'quantize' flag to remap to top-3", path.display(), counts.len()
        ));
    };

    // Pass 2: index each pixel into the chosen palette (exact match first,
    // nearest-neighbor by sq RGB distance for the quantized remainder).
    let mut indices: Vec<u8> = Vec::with_capacity((w * h) as usize);
    for pixel in rgba.pixels() {
        let [r, g, b, a] = pixel.0;
        if a < 8 { indices.push(0); continue; }
        let key = [r, g, b];
        let idx = match chosen.iter().position(|c| *c == key) {
            Some(i) => (i + 1) as u8,
            None => {
                let mut best = 0usize;
                let mut best_d = u32::MAX;
                for (i, c) in chosen.iter().enumerate() {
                    let dr = c[0] as i32 - r as i32;
                    let dg = c[1] as i32 - g as i32;
                    let db = c[2] as i32 - b as i32;
                    let d = (dr * dr + dg * dg + db * db) as u32;
                    if d < best_d { best_d = d; best = i; }
                }
                (best + 1) as u8
            }
        };
        indices.push(idx);
    }

    let mut palette = [Color::Rgb(0, 0, 0); PALETTE_SIZE];
    for (i, rgb) in chosen.iter().enumerate() {
        palette[i + 1] = Color::Rgb(rgb[0], rgb[1], rgb[2]);
    }

    Ok(SpriteSheet {
        name: name.into(),
        source: path.to_path_buf(),
        width: w,
        height: h,
        cell_w,
        cell_h,
        indices,
        palette,
    })
}

/// Parse `"#rrggbb"`, `"rrggbb"`, or `"transparent"` into a palette entry.
/// Returning None for the transparent literal signals "keep slot 0 as
/// transparent"; real colors return Some.
pub(crate) fn parse_hex(tok: &str) -> Option<Color> {
    let s = tok.trim().trim_start_matches('#').to_ascii_lowercase();
    if s == "transparent" || s == "none" || s == "-" {
        return Some(Color::Rgb(0, 0, 0)); // placeholder; slot 0 renders transparent anyway
    }
    if s.len() != 6 { return None; }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

/// A sprite placed in the viz pane. `x`/`y` are in sprite-pixel coordinates
/// (0,0 = top-left of the viz-pane pixel grid, which is 2× vertical
/// resolution of the terminal rows via half-blocks).
#[derive(Clone, Debug)]
pub(crate) struct Placement {
    pub sheet: String,
    pub idx: u32,
    pub x: i32,
    pub y: i32,
    /// Optional palette override, looked up in `App.sprite_palettes`.
    /// None = use the sheet's own palette.
    pub palette: Option<String>,
}
