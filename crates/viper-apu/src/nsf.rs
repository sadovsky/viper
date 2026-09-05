//! NSF (NES Sound Format) container: header parse and the ROM/bank image.

use anyhow::{bail, Result};

#[derive(Clone, Debug)]
pub struct Nsf {
    pub songs: u8,
    pub start_song: u8,
    pub load: u16,
    pub init: u16,
    pub play: u16,
    pub name: String,
    pub artist: String,
    pub copyright: String,
    pub ntsc_speed_us: u16,
    pub banks: [u8; 8],
    pub pal: bool,
    pub expansion: u8,
    pub data: Vec<u8>,
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

impl Nsf {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 128 || &bytes[0..5] != b"NESM\x1A" {
            bail!("not an NSF file");
        }
        let w = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let mut banks = [0u8; 8];
        banks.copy_from_slice(&bytes[0x70..0x78]);
        Ok(Self {
            songs: bytes[6],
            start_song: bytes[7],
            load: w(8),
            init: w(0xA),
            play: w(0xC),
            name: cstr(&bytes[0xE..0x2E]),
            artist: cstr(&bytes[0x2E..0x4E]),
            copyright: cstr(&bytes[0x4E..0x6E]),
            ntsc_speed_us: w(0x6E),
            banks,
            pal: bytes[0x7A] & 1 != 0,
            expansion: bytes[0x7B],
            data: bytes[128..].to_vec(),
        })
    }

    pub fn bankswitched(&self) -> bool {
        self.banks.iter().any(|&b| b != 0)
    }

    /// Build the NSF header for an image already laid out at `load`.
    pub fn build_header(
        load: u16, init: u16, play: u16, songs: u8, name: &str, artist: &str, copyright: &str, expansion: u8,
    ) -> [u8; 128] {
        let mut h = [0u8; 128];
        h[0..5].copy_from_slice(b"NESM\x1A");
        h[5] = 1;
        h[6] = songs;
        h[7] = 1;
        h[8..10].copy_from_slice(&load.to_le_bytes());
        h[10..12].copy_from_slice(&init.to_le_bytes());
        h[12..14].copy_from_slice(&play.to_le_bytes());
        let put = |h: &mut [u8; 128], o: usize, s: &str| {
            let b = s.as_bytes();
            let n = b.len().min(31);
            h[o..o + n].copy_from_slice(&b[..n]);
        };
        put(&mut h, 0x0E, name);
        put(&mut h, 0x2E, artist);
        put(&mut h, 0x4E, copyright);
        h[0x6E..0x70].copy_from_slice(&0x411Au16.to_le_bytes());
        h[0x78..0x7A].copy_from_slice(&0x4E20u16.to_le_bytes());
        h[0x7B] = expansion;
        h
    }
}

/// The 6502-visible memory of an NSF player: 2 KB RAM, 8 KB at $6000,
/// and either a flat ROM image or eight 4 KB switchable banks.
pub struct Memory {
    pub ram: [u8; 0x800],
    pub wram: Vec<u8>,
    rom: Vec<u8>,
    bank_regs: [u8; 8],
    bankswitched: bool,
    load: u16,
}

impl Memory {
    pub fn new(nsf: &Nsf) -> Self {
        let bankswitched = nsf.bankswitched();
        let mut rom;
        if bankswitched {
            // Pad so that bank 0 starts at (load & $0FFF) into the first bank.
            let pad = (nsf.load & 0x0FFF) as usize;
            rom = vec![0u8; pad];
            rom.extend_from_slice(&nsf.data);
            let rounded = (rom.len() + 0xFFF) & !0xFFF;
            rom.resize(rounded, 0);
        } else {
            rom = vec![0u8; 0x8000];
            let start = (nsf.load as usize).saturating_sub(0x8000);
            let n = nsf.data.len().min(0x8000 - start);
            rom[start..start + n].copy_from_slice(&nsf.data[..n]);
        }
        let mut m = Self {
            ram: [0; 0x800],
            wram: vec![0; 0x2000],
            rom,
            bank_regs: [0; 8],
            bankswitched,
            load: nsf.load,
        };
        if bankswitched {
            for i in 0..8 {
                m.write_bank(i, nsf.banks[i]);
            }
        }
        m
    }

    pub fn write_bank(&mut self, slot: usize, bank: u8) {
        self.bank_regs[slot] = bank;
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x7FF) as usize],
            0x6000..=0x7FFF => self.wram[(addr - 0x6000) as usize],
            0x8000..=0xFFFF => {
                if self.bankswitched {
                    let slot = ((addr - 0x8000) >> 12) as usize;
                    let off = self.bank_regs[slot] as usize * 0x1000 + (addr & 0xFFF) as usize;
                    self.rom.get(off).copied().unwrap_or(0)
                } else {
                    self.rom[(addr - 0x8000) as usize]
                }
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, addr: u16, v: u8) {
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x7FF) as usize] = v,
            0x5FF8..=0x5FFF => self.write_bank((addr - 0x5FF8) as usize, v),
            0x6000..=0x7FFF => self.wram[(addr - 0x6000) as usize] = v,
            _ => {}
        }
    }

    pub fn load_addr(&self) -> u16 {
        self.load
    }
}
