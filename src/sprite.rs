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
    /// Whether the source PNG was remapped to its top three colours on load.
    /// Recorded so a saved song can reload the sheet: without the flag a
    /// quantized sheet fails on reload, because the file still holds more
    /// than three opaque colours.
    pub quantize: bool,
    /// The named palette applied by `:sprite repalette`, if any. `palette`
    /// alone cannot round-trip, since it holds the resulting colours rather
    /// than the name that produced them.
    pub palette_name: Option<String>,
    /// Which CHR bank this came from, when the source was a ROM. `None`
    /// means a PNG, or a ROM taken whole.
    pub chr_bank: Option<u32>,
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

    let chosen: Vec<[u8; 3]> = if counts.len() < PALETTE_SIZE {
        counts.iter().map(|(c, _)| *c).collect()
    } else if quantize {
        counts.sort_by_key(|c| std::cmp::Reverse(c.1));
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
        quantize,
        palette_name: None,
        chr_bank: None,
    })
}

/// Tiles per row when a CHR bank is laid out as a sheet. The PPU addresses
/// pattern tables as 16x16 grids of tiles, and every tile viewer ever
/// written shows them that way, so a ripped sheet looks like what a person
/// expects to see.
pub(crate) const CHR_TILES_PER_ROW: u32 = 16;

/// Tiles in one pattern table — the unit the PPU can address at once, and
/// so the natural slice to pull out of a ROM with many banks.
pub(crate) const CHR_BANK_TILES: u32 = 256;

/// The default palette for tiles read out of a ROM.
///
/// CHR data carries no colour: it is two bitplanes giving an index of 0..=3,
/// and which actual colours those mean lives in the game's code, chosen per
/// frame per attribute block. So there is nothing to recover, and picking a
/// legible ramp is the honest default. `:sprite repalette` sets a real one.
const CHR_DEFAULT: [Color; PALETTE_SIZE] = [
    Color::Rgb(0, 0, 0),       // slot 0 is transparent regardless
    Color::Rgb(0x3c, 0x3c, 0x6c),
    Color::Rgb(0x88, 0x88, 0xb8),
    Color::Rgb(0xfc, 0xfc, 0xfc),
];

/// What an iNES file says about itself.
struct INes {
    /// Byte offset of the CHR-ROM inside the file.
    chr_offset: usize,
    chr_bytes: usize,
}

/// Read the 16-byte iNES header.
///
/// The CHR sits after the header, an optional 512-byte trainer, and the
/// PRG-ROM — none of which are fixed sizes, so the offset has to be computed
/// rather than assumed.
fn ines_header(bytes: &[u8], what: &Path) -> Result<INes> {
    if bytes.len() < 16 || &bytes[0..4] != b"NES\x1A" {
        bail!("{} is not an iNES ROM (no NES\\x1A magic)", what.display());
    }
    let trainer = if bytes[6] & 0x04 != 0 { 512 } else { 0 };
    let prg = bytes[4] as usize * 16 * 1024;
    let chr_bytes = bytes[5] as usize * 8 * 1024;
    Ok(INes {
        chr_offset: 16 + trainer + prg,
        chr_bytes,
    })
}

/// Read a sprite sheet straight out of an NES ROM's character data.
///
/// CHR is 2bpp planar: sixteen bytes per 8x8 tile, the first eight holding
/// the low bit of each pixel and the second eight the high bit. That yields
/// an index of 0..=3 per pixel — which is exactly what a [`SpriteSheet`]
/// already is, so unlike a PNG there is nothing to quantise and nothing to
/// lose. The graphics arrive as the artist drew them.
///
/// `bank` selects one 256-tile pattern table; `None` takes the whole of CHR,
/// which for a large game is several thousand tiles.
pub(crate) fn load_chr(
    name: impl Into<String>,
    path: &Path,
    cell_w: u32,
    cell_h: u32,
    bank: Option<u32>,
    frames: u32,
) -> Result<SpriteSheet> {
    if cell_w == 0 || cell_h == 0 || !cell_w.is_multiple_of(8) || !cell_h.is_multiple_of(8) {
        bail!("CHR cells are whole 8x8 tiles, so {}x{} will not do", cell_w, cell_h);
    }
    let bytes = std::fs::read(path).with_context(|| format!("read ROM {}", path.display()))?;
    let h = ines_header(&bytes, path)?;
    // Two kinds of cartridge, and the difference is where the graphics
    // live. Most keep their tiles in the file and we read them straight off
    // disk. The rest — about half of the ones I tried — hold them
    // compressed or generated, and write them into CHR-RAM as they boot.
    // For those there is nothing in the file, so the game has to be run.
    let ram;
    let chr: &[u8] = if h.chr_bytes == 0 {
        let dump = viper_apu::nes::run_for_chr(&bytes, frames).with_context(|| {
            format!("{} keeps its tiles in RAM, so viper ran it to see them", path.display())
        })?;
        if dump.written == 0 {
            bail!(
                "{} wrote no tiles in {} frames (mapper {}); some games take longer to \
                 get past a licence screen, so try frames={}",
                path.display(),
                frames,
                dump.mapper,
                frames.max(1) * 4
            );
        }
        ram = dump.chr;
        &ram
    } else {
        let end = h.chr_offset + h.chr_bytes;
        bytes.get(h.chr_offset..end).ok_or_else(|| {
            anyhow!("{} claims {} bytes of CHR but is too short", path.display(), h.chr_bytes)
        })?
    };

    let total = (chr.len() / 16) as u32;
    let (first, count) = match bank {
        Some(b) => {
            let start = b * CHR_BANK_TILES;
            if start >= total {
                bail!("bank {} is past the end: this ROM has {} banks of {} tiles", b, total.div_ceil(CHR_BANK_TILES), CHR_BANK_TILES);
            }
            (start, CHR_BANK_TILES.min(total - start))
        }
        None => (0, total),
    };

    // Lay the tiles out in rows, then cut cells out of that image. A cell
    // taller or wider than one tile therefore spans several — which is what
    // the hardware does too in 8x16 sprite mode.
    let width = CHR_TILES_PER_ROW * 8;
    let tile_rows = count.div_ceil(CHR_TILES_PER_ROW);
    let height = tile_rows * 8;
    let mut indices = vec![0u8; (width * height) as usize];
    for t in 0..count {
        let base = ((first + t) * 16) as usize;
        let ox = (t % CHR_TILES_PER_ROW) * 8;
        let oy = (t / CHR_TILES_PER_ROW) * 8;
        for y in 0..8u32 {
            let lo = chr[base + y as usize];
            let hi = chr[base + 8 + y as usize];
            for x in 0..8u32 {
                let bit = 7 - x;
                let v = ((lo >> bit) & 1) | (((hi >> bit) & 1) << 1);
                indices[((oy + y) * width + ox + x) as usize] = v;
            }
        }
    }

    if !width.is_multiple_of(cell_w) || height % cell_h != 0 {
        bail!("{} tiles lay out {}x{}, which {}x{} cells do not divide", count, width, height, cell_w, cell_h);
    }

    Ok(SpriteSheet {
        name: name.into(),
        source: path.to_path_buf(),
        width,
        height,
        cell_w,
        cell_h,
        indices,
        palette: CHR_DEFAULT,
        quantize: false,
        palette_name: None,
        chr_bank: bank,
    })
}

/// Whether this file is an NES ROM rather than an image, by its magic
/// number rather than its extension — a ROM is a ROM whatever it is called.
pub(crate) fn is_nes_rom(path: &Path) -> bool {
    let mut buf = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_ok()
        && &buf == b"NES\x1A"
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an iNES file with `chr_banks` 8 KB banks of character data.
    ///
    /// Synthetic rather than a real game: viper's tests must not carry
    /// commercial ROMs, and a made-up one exercises the parser just as well
    /// because the format is the format.
    fn rom(prg_banks: u8, chr_banks: u8, trainer: bool, fill: impl Fn(usize) -> u8) -> Vec<u8> {
        let mut v = b"NES\x1A".to_vec();
        v.push(prg_banks);
        v.push(chr_banks);
        v.push(if trainer { 0x04 } else { 0x00 });
        v.resize(16, 0);
        if trainer {
            v.extend(std::iter::repeat_n(0xAA, 512));
        }
        v.extend(std::iter::repeat_n(0x11, prg_banks as usize * 16 * 1024));
        let chr = chr_banks as usize * 8 * 1024;
        v.extend((0..chr).map(&fill));
        v
    }

    fn write(bytes: &[u8]) -> PathBuf {
        // A counter rather than anything derived from the contents: these
        // tests run in parallel threads, and two of them writing the same
        // path would fail in a way that looks like a decode bug.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("viper_chr_{}_{}.nes", std::process::id(), n));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// One tile: a diagonal in the low plane, the top row in the high plane.
    /// Pixel (0,0) is then index 3, (1,1) index 1, and (7,0) index 2.
    fn diagonal_tile() -> Vec<u8> {
        let mut t = vec![0u8; 16];
        for y in 0..8 {
            t[y] = 0x80 >> y; // low plane: a diagonal
        }
        t[8] = 0xFF; // high plane: the whole top row
        t
    }

    #[test]
    fn chr_is_two_bitplanes_and_lands_where_the_hardware_says() {
        // The decode the whole feature rests on: sixteen bytes a tile, the
        // first eight the low bit of each pixel and the second eight the
        // high bit, most significant bit leftmost.
        let mut chr = diagonal_tile();
        chr.resize(8 * 1024, 0);
        let p = write(&rom(1, 1, false, |i| chr[i]));
        let s = load_chr("t", &p, 8, 8, None, 0).unwrap();
        assert_eq!(s.pixel(0, 0, 0), Some(3), "both planes set");
        assert_eq!(s.pixel(0, 7, 0), Some(2), "high plane only");
        assert_eq!(s.pixel(0, 1, 1), Some(1), "low plane only");
        assert_eq!(s.pixel(0, 0, 1), Some(0), "neither");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn tiles_are_laid_out_sixteen_to_a_row_like_a_pattern_table() {
        // Every tile viewer ever written shows them this way, so a ripped
        // sheet looks like what a person expects.
        let p = write(&rom(1, 1, false, |i| if i / 16 == 17 { 0xFF } else { 0 }));
        let s = load_chr("t", &p, 8, 8, None, 0).unwrap();
        assert_eq!(s.width, 128);
        assert_eq!(s.cell_count(), 512, "an 8 KB bank is 512 tiles");
        // Tile 17 is the second tile of the second row.
        assert_eq!(s.pixel(17, 3, 3), Some(3), "both planes set in the fixture");
        assert_eq!(s.pixel(16, 3, 3), Some(0));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_chr_is_found_past_the_prg_and_any_trainer() {
        // Neither is a fixed size, so the offset has to be computed. A
        // trainer is 512 bytes that almost no ROM has and every parser has
        // to skip anyway.
        for trainer in [false, true] {
            for prg in [1u8, 2, 4] {
                let p = write(&rom(prg, 1, trainer, |i| if i < 16 { 0xFF } else { 0 }));
                let s = load_chr("t", &p, 8, 8, None, 0).unwrap();
                assert_eq!(s.pixel(0, 0, 0), Some(3), "prg {} trainer {}", prg, trainer);
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    #[test]
    fn a_bank_is_one_pattern_table_and_past_the_end_is_an_error() {
        let p = write(&rom(1, 2, false, |i| if i / 16 == 256 { 0xFF } else { 0 }));
        let whole = load_chr("t", &p, 8, 8, None, 0).unwrap();
        assert_eq!(whole.cell_count(), 1024, "two 8 KB banks");
        let one = load_chr("t", &p, 8, 8, Some(0), 0).unwrap();
        assert_eq!(one.cell_count(), CHR_BANK_TILES, "one pattern table");
        // Tile 256 is the first tile of bank 1.
        assert_eq!(one.pixel(255, 0, 0), Some(0));
        assert_eq!(load_chr("t", &p, 8, 8, Some(1), 0).unwrap().pixel(0, 0, 0), Some(3));
        let err = load_chr("t", &p, 8, 8, Some(99), 0).unwrap_err().to_string();
        assert!(err.contains("past the end") && err.contains("4 banks"), "{}", err);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_rom_with_no_chr_is_run_rather_than_refused() {
        // Seven of the seventeen ROMs I checked keep nothing in the file —
        // Metroid, Zelda, Contra, Final Fantasy among them — because they
        // write their tiles into CHR-RAM as they boot. So the game is run.
        //
        // This fixture is not a game: its PRG is filler that decodes to
        // nothing useful, so no tiles appear and the error has to say what
        // to try next rather than blaming the file.
        let p = write(&rom(1, 0, false, |_| 0));
        let err = load_chr("t", &p, 8, 8, None, 2).unwrap_err().to_string();
        assert!(err.contains("wrote no tiles"), "{}", err);
        assert!(err.contains("frames=8"), "and suggests running it longer: {}", err);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn something_that_is_not_a_rom_is_refused() {
        let p = write(b"this is not a ROM at all, not even close");
        assert!(load_chr("t", &p, 8, 8, None, 0).is_err());
        assert!(!is_nes_rom(&p), "and is not mistaken for one");
        let _ = std::fs::remove_file(&p);
        // A ROM is recognised by its magic, not its extension.
        let r = write(&rom(1, 1, false, |_| 0));
        let named = r.with_extension("png");
        std::fs::rename(&r, &named).unwrap();
        assert!(is_nes_rom(&named), "a ROM called .png is still a ROM");
        let _ = std::fs::remove_file(&named);
    }

    #[test]
    fn cells_must_be_whole_tiles() {
        let p = write(&rom(1, 1, false, |_| 0));
        // 8x16 is the hardware's tall-sprite mode and has to work.
        assert!(load_chr("t", &p, 8, 16, None, 0).is_ok());
        assert!(load_chr("t", &p, 16, 16, None, 0).is_ok());
        for bad in [(5, 8), (8, 5), (0, 8)] {
            assert!(load_chr("t", &p, bad.0, bad.1, None, 0).is_err(), "{:?}", bad);
        }
        let _ = std::fs::remove_file(&p);
    }

    /// Point this at a directory of real ROMs to check the loader against
    /// games rather than fixtures:
    ///
    /// ```text
    /// VIPER_ROM_DIR=~/roms cargo test --bins chr_against_real_roms -- --ignored --nocapture
    /// ```
    ///
    /// Ignored by default and reading a path from the environment, because
    /// commercial ROMs cannot live in this repository. It asserts only what
    /// is true of every ROM — that each either yields tiles or explains that
    /// it has no CHR-ROM — so it passes on any collection.
    #[test]
    #[ignore]
    fn chr_against_real_roms() {
        let Ok(dir) = std::env::var("VIPER_ROM_DIR") else {
            eprintln!("set VIPER_ROM_DIR to a folder of .nes files");
            return;
        };
        let mut with = 0;
        let mut without = 0;
        for entry in std::fs::read_dir(&dir).expect("read the ROM directory") {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "nes") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            match load_chr("rom", &path, 8, 8, None, 300) {
                Ok(s) => {
                    with += 1;
                    assert!(s.cell_count() > 0);
                    assert!(s.indices.iter().all(|&v| v <= 3));
                    let live = s.indices.iter().filter(|&&v| v != 0).count();
                    println!("  {:<36} {:>5} tiles, {:>6} lit pixels", name, s.cell_count(), live);
                }
                Err(e) => {
                    // A mapper viper does not know is a fair failure; a
                    // decode error is not.
                    let msg = e.to_string();
                    assert!(msg.contains("not supported") || msg.contains("wrote no tiles"), "{}: {}", name, msg);
                    without += 1;
                    println!("  {:<36} {}", name, msg.lines().next().unwrap_or(""));
                }
            }
        }
        println!("{} ROMs yielded tiles, {} did not", with, without);
    }

    #[test]
    fn a_chr_sheet_needs_no_quantising_because_it_is_already_indexed() {
        // The reason this is worth having at all: a PNG has to be reduced to
        // four colours and something is lost. CHR *is* four indices, so the
        // graphics arrive exactly as the artist drew them.
        let p = write(&rom(1, 1, false, |i| (i % 251) as u8));
        let s = load_chr("t", &p, 8, 8, None, 0).unwrap();
        assert!(!s.quantize, "nothing was approximated");
        assert!(s.indices.iter().all(|&v| v <= 3), "every pixel is already an index");
        assert!(s.indices.iter().any(|&v| v == 3), "and the range is used");
        let _ = std::fs::remove_file(&p);
    }
}
