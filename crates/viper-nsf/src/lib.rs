//! viper-nsf — the `.vip` → IR → NSF compiler's back half.
//!
//! The IR here is row-based: a [`Song`] is patterns of rows, each row a
//! list of [`Event`]s per channel. Frame timing is the driver's job (an
//! 8.8 fixed-point row clock), so the emitter only has to serialize
//! events into the driver's bytecode and lay out the NSF image.
//!
//! viper does not own the driver. [`Driver::load`] reads a position-fixed
//! binary plus its ld65 symbol map (ABI v1, see nintendo-metal/driver/ABI.md)
//! and the emitter appends song data at `song_table`.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;

pub mod ir;
pub mod emit;

pub use emit::{emit, EmitResult};
pub use ir::*;

/// The driver ABI this build emits by default, and the oldest it can
/// still link against. Keeping the older layouts alive means a driver
/// binary captured with a song — an external emulator's receipt, say —
/// stays reproducible after the ABI moves on.
pub const DRIVER_ABI_VERSION: u32 = 3;
pub const DRIVER_ABI_MIN: u32 = 1;

/// A sound driver binary + the symbols the emitter links against.
#[derive(Clone, Debug)]
pub struct Driver {
    pub bin: Vec<u8>,
    pub load: u16,
    pub init: u16,
    pub play: u16,
    pub song_table: u16,
    pub abi: u32,
    pub symbols: HashMap<String, u32>,
}

/// Parse an ld65 VICE label file: lines of `al <hex addr> .<name>`.
pub fn parse_symbols(text: &str) -> HashMap<String, u32> {
    let mut m = HashMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if it.next() != Some("al") {
            continue;
        }
        let (Some(addr), Some(name)) = (it.next(), it.next()) else { continue };
        if let Ok(a) = u32::from_str_radix(addr, 16) {
            m.insert(name.trim_start_matches('.').to_string(), a);
        }
    }
    m
}

impl Driver {
    pub fn from_parts(bin: Vec<u8>, symbols: HashMap<String, u32>) -> Result<Self> {
        let get = |n: &str| -> Result<u32> {
            symbols.get(n).copied().ok_or_else(|| anyhow!("driver.sym is missing symbol `{}`", n))
        };
        let abi = get("DRIVER_ABI_VERSION")?;
        if !(DRIVER_ABI_MIN..=DRIVER_ABI_VERSION).contains(&abi) {
            bail!("driver ABI version {} is not supported (viper speaks v{}..v{})", abi, DRIVER_ABI_MIN, DRIVER_ABI_VERSION);
        }
        let init = get("driver_init")? as u16;
        let play = get("driver_play")? as u16;
        let song_table = get("song_table")? as u16;
        // The blob is position-fixed; its load address is where init lives
        // rounded down to the start of the image — for ABI v1 that is $8000.
        let load = 0x8000u16;
        if (song_table as usize) < load as usize || song_table as usize - load as usize != bin.len() {
            bail!(
                "driver.bin is {} bytes but song_table is at ${:04X} (expected ${:04X})",
                bin.len(),
                song_table,
                load as usize + bin.len()
            );
        }
        Ok(Self { bin, load, init, play, song_table, abi, symbols })
    }

    pub fn load(bin_path: &Path, sym_path: &Path) -> Result<Self> {
        let bin = std::fs::read(bin_path).with_context(|| format!("read {}", bin_path.display()))?;
        let sym = std::fs::read_to_string(sym_path).with_context(|| format!("read {}", sym_path.display()))?;
        Self::from_parts(bin, parse_symbols(&sym))
    }
}
