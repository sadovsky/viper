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
    /// NSFe `tlbl` / `time` (ms) when present.
    pub track_names: Vec<String>,
    pub track_times: Vec<i32>,
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

impl Nsf {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() >= 4 && &bytes[0..4] == b"NSFE" {
            return Self::parse_nsfe(bytes);
        }
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
            track_names: Vec::new(),
            track_times: Vec::new(),
        })
    }

    fn parse_nsfe(bytes: &[u8]) -> Result<Self> {
        let mut nsf = Nsf {
            songs: 1, start_song: 1, load: 0x8000, init: 0x8000, play: 0x8000,
            name: String::new(), artist: String::new(), copyright: String::new(),
            ntsc_speed_us: 0x411A, banks: [0; 8], pal: false, expansion: 0, data: Vec::new(),
            track_names: Vec::new(), track_times: Vec::new(),
        };
        let mut pos = 4;
        let mut have_info = false;
        while pos + 8 <= bytes.len() {
            let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
            let id = &bytes[pos + 4..pos + 8];
            let body = bytes.get(pos + 8..pos + 8 + len).ok_or_else(|| anyhow::anyhow!("NSFe chunk {} truncated", String::from_utf8_lossy(id)))?;
            match id {
                b"INFO" => {
                    if body.len() < 8 { bail!("NSFe INFO chunk too short"); }
                    nsf.load = u16::from_le_bytes([body[0], body[1]]);
                    nsf.init = u16::from_le_bytes([body[2], body[3]]);
                    nsf.play = u16::from_le_bytes([body[4], body[5]]);
                    nsf.pal = body[6] & 1 != 0;
                    nsf.expansion = body[7];
                    nsf.songs = body.get(8).copied().unwrap_or(1);
                    nsf.start_song = body.get(9).copied().unwrap_or(0) + 1;
                    have_info = true;
                }
                b"DATA" => nsf.data = body.to_vec(),
                b"BANK" => {
                    for (i, b) in body.iter().take(8).enumerate() { nsf.banks[i] = *b; }
                }
                b"auth" => {
                    let mut it = body.split(|&c| c == 0).map(|s| String::from_utf8_lossy(s).into_owned());
                    nsf.name = it.next().unwrap_or_default();
                    nsf.artist = it.next().unwrap_or_default();
                    nsf.copyright = it.next().unwrap_or_default();
                }
                b"tlbl" => {
                    nsf.track_names = body.split(|&c| c == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
                    nsf.track_names.truncate(nsf.songs as usize);
                }
                b"time" => {
                    nsf.track_times = body.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
                }
                b"NEND" => break,
                _ => {}
            }
            pos += 8 + len;
        }
        if !have_info || nsf.data.is_empty() {
            bail!("NSFe file is missing INFO or DATA");
        }
        Ok(nsf)
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
