//! Running a cartridge far enough to see its graphics.
//!
//! About half the NES library keeps no character data in the file at all.
//! Those games hold their tiles compressed, or generated, or simply copied
//! from PRG, and write them into 8 KB of CHR-RAM through the PPU's data port
//! as they boot. The file has nothing to extract; the machine does, a
//! fraction of a second later.
//!
//! So this module boots the cartridge and reads the RAM afterwards. It is not
//! an emulator in any complete sense — there is no picture, no sprite
//! evaluation, no scanline timing, no sound requirement — because none of
//! that is needed to answer "what tiles did this game upload?". What *is*
//! needed is enough of the machine that the upload code runs at all:
//!
//! * **Bank switching**, since the loader almost never lives in the fixed
//!   bank. Mappers 0, 1, 2, 3 and 7 cover the great majority of cartridges.
//! * **A real NMI**, because the upload usually happens in the vblank
//!   handler and nowhere else.
//! * **The PPU's address and data ports**, which is where the tiles are
//!   actually written. `$2006` sets an address, `$2007` writes a byte and
//!   advances — and the first 8 KB of that address space is CHR-RAM.
//!
//! What is deliberately faked: `$2002` always reports vblank, so a boot loop
//! waiting for one exits at once rather than after two frames of real
//! timing. That makes the machine wrong and the graphics right.

use anyhow::{bail, Result};

use crate::apu::Apu;
use crate::cpu::{Bus, Cpu};
use crate::host::RegWrite;

/// CPU cycles in one NTSC frame, near enough for a boot sequence.
const CYCLES_PER_FRAME: u32 = 29_781;

/// An iNES cartridge, unpacked.
pub struct Cart {
    pub prg: Vec<u8>,
    /// CHR-ROM if the file carried any; otherwise 8 KB of zeroed CHR-RAM
    /// for the game to fill.
    pub chr: Vec<u8>,
    pub mapper: u8,
    /// True when `chr` is RAM the cartridge writes rather than ROM it reads.
    pub chr_is_ram: bool,
}

impl Cart {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 16 || &bytes[0..4] != b"NES\x1A" {
            bail!("not an iNES ROM");
        }
        let trainer = if bytes[6] & 0x04 != 0 { 512 } else { 0 };
        let prg_len = bytes[4] as usize * 16 * 1024;
        let chr_len = bytes[5] as usize * 8 * 1024;
        let prg_start = 16 + trainer;
        let prg = bytes
            .get(prg_start..prg_start + prg_len)
            .ok_or_else(|| anyhow::anyhow!("PRG runs past the end of the file"))?
            .to_vec();
        let chr_is_ram = chr_len == 0;
        let chr = if chr_is_ram {
            vec![0u8; 8 * 1024]
        } else {
            bytes
                .get(prg_start + prg_len..prg_start + prg_len + chr_len)
                .ok_or_else(|| anyhow::anyhow!("CHR runs past the end of the file"))?
                .to_vec()
        };
        if prg.is_empty() {
            bail!("ROM has no PRG");
        }
        Ok(Cart { prg, chr, mapper: (bytes[6] >> 4) | (bytes[7] & 0xF0), chr_is_ram })
    }

    fn prg_banks_16k(&self) -> usize {
        (self.prg.len() / (16 * 1024)).max(1)
    }
}

/// The PRG banking state for the mappers this understands.
///
/// Only PRG matters here. CHR banking decides which tiles the *picture*
/// shows, and there is no picture — what is wanted is every tile the game
/// ever wrote, which is the whole of CHR-RAM regardless of which window was
/// selected when.
enum Mapper {
    /// No banking: the whole PRG is visible at once.
    Nrom,
    /// MMC1. A five-bit serial register, written one bit at a time.
    Mmc1 { shift: u8, count: u8, control: u8, prg_bank: u8 },
    /// UxROM: `$8000` switches, `$C000` is fixed to the last bank.
    UxRom { bank: u8 },
    /// CNROM switches CHR only, so its PRG is fixed like NROM.
    CnRom,
    /// AxROM switches 32 KB at a time.
    AxRom { bank: u8 },
}

impl Mapper {
    fn new(mapper: u8) -> Result<Self> {
        Ok(match mapper {
            0 => Mapper::Nrom,
            // 0x0C is the reset value that puts PRG in "switch $8000, fix
            // the last bank at $C000" mode, which is what a game's reset
            // vector relies on before it has configured anything.
            1 => Mapper::Mmc1 { shift: 0, count: 0, control: 0x0C, prg_bank: 0 },
            2 => Mapper::UxRom { bank: 0 },
            3 => Mapper::CnRom,
            7 => Mapper::AxRom { bank: 0 },
            m => bail!(
                "mapper {} is not supported; this understands 0 (NROM), 1 (MMC1), \
                 2 (UxROM), 3 (CNROM) and 7 (AxROM)",
                m
            ),
        })
    }

    fn write(&mut self, addr: u16, v: u8) {
        match self {
            Mapper::Nrom | Mapper::CnRom => {}
            Mapper::UxRom { bank } => *bank = v & 0x0F,
            Mapper::AxRom { bank } => *bank = v & 0x07,
            Mapper::Mmc1 { shift, count, control, prg_bank } => {
                // Bit 7 resets the register and forces PRG mode 3, which is
                // how every MMC1 game starts its banking sequence.
                if v & 0x80 != 0 {
                    *shift = 0;
                    *count = 0;
                    *control |= 0x0C;
                    return;
                }
                *shift |= (v & 1) << *count;
                *count += 1;
                if *count == 5 {
                    match (addr >> 13) & 3 {
                        0 => *control = *shift,
                        // CHR bank selects; ignored, see the enum's comment.
                        1 | 2 => {}
                        _ => *prg_bank = *shift & 0x0F,
                    }
                    *shift = 0;
                    *count = 0;
                }
            }
        }
    }

    /// Map a CPU address in `$8000..=$FFFF` to an offset in PRG.
    fn prg_offset(&self, cart: &Cart, addr: u16) -> usize {
        let banks = cart.prg_banks_16k();
        let last = banks - 1;
        let bank_16k = 16 * 1024;
        let off = (addr as usize - 0x8000) & (bank_16k - 1);
        let high = addr >= 0xC000;
        let bank = match self {
            // A 16 KB cartridge is mirrored into both halves.
            Mapper::Nrom | Mapper::CnRom => {
                if banks == 1 { 0 } else if high { 1 } else { 0 }
            }
            Mapper::UxRom { bank } => {
                if high { last } else { (*bank as usize).min(last) }
            }
            Mapper::AxRom { bank } => {
                // 32 KB at a time, so the high half follows the low one.
                let base = (*bank as usize * 2).min(last.saturating_sub(1));
                if high { base + 1 } else { base }
            }
            Mapper::Mmc1 { control, prg_bank, .. } => {
                let b = *prg_bank as usize;
                match (*control >> 2) & 3 {
                    // 32 KB switching ignores the low bit of the bank.
                    0 | 1 => {
                        let base = (b & !1).min(last.saturating_sub(1));
                        if high { base + 1 } else { base }
                    }
                    // Fix the first bank at $8000, switch $C000.
                    2 => {
                        if high { b.min(last) } else { 0 }
                    }
                    // Switch $8000, fix the last bank at $C000.
                    _ => {
                        if high { last } else { b.min(last) }
                    }
                }
            }
        };
        (bank.min(last)) * bank_16k + off
    }
}

/// The machine: enough of one to boot a cartridge and watch what it writes
/// into video memory.
struct Nes {
    cart: Cart,
    mapper: Mapper,
    ram: [u8; 0x800],
    /// Work RAM at `$6000`. Plenty of games decompress through it.
    wram: [u8; 0x2000],
    /// The PPU's whole 16 KB address space. The first 8 KB is the pattern
    /// table — which on a CHR-RAM cartridge is exactly what we are after.
    ppu: [u8; 0x4000],
    ppu_addr: u16,
    ppu_latch: bool,
    ctrl: u8,
    /// Set at the top of each frame, cleared when `$2002` is read.
    vblank: bool,
    /// How many bytes have been written through `$2007` into CHR.
    chr_writes: usize,
    /// The sound chip, clocked alongside the CPU. It is here so that reads
    /// of `$4015` answer truthfully: a music driver polls it to learn when a
    /// note's length counter ran out or a DPCM sample finished, and one that
    /// always reads zero will sit waiting for a sample that never ends.
    apu: Apu,
    /// Every APU register write, stamped with the frame it happened in —
    /// the same shape `viper rip` reads from an emulator dump, so a game's
    /// music arrives in the transcriber through a path that already exists.
    log: Vec<RegWrite>,
    frame: u32,
    /// The buttons held this frame.
    pad: u8,
    pad_shift: u8,
}

impl Bus for Nes {
    fn read(&mut self, a: u16) -> u8 {
        match a {
            0x0000..=0x1FFF => self.ram[(a & 0x7FF) as usize],
            0x2000..=0x3FFF => match a & 7 {
                // Reading $2002 clears the vblank flag, as the hardware
                // does. Returning it permanently set is the obvious shortcut
                // and it hangs any loop that waits for vblank to *end* —
                // which is how a game holds off until the picture is safe to
                // touch. Two of the seven cartridges I tried never wrote a
                // byte until this was modelled properly.
                2 => {
                    self.ppu_latch = false;
                    let v = if self.vblank { 0x80 } else { 0x00 };
                    self.vblank = false;
                    v
                }
                7 => {
                    let v = self.ppu[(self.ppu_addr & 0x3FFF) as usize];
                    self.ppu_addr = self.ppu_addr.wrapping_add(self.vram_step());
                    v
                }
                _ => 0,
            },
            0x4015 => self.apu.read_status(),
            0x4016 => {
                let b = self.pad_shift & 1;
                self.pad_shift >>= 1;
                b
            }
            0x6000..=0x7FFF => self.wram[(a - 0x6000) as usize],
            0x8000..=0xFFFF => {
                let off = self.mapper.prg_offset(&self.cart, a);
                self.cart.prg.get(off).copied().unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn write(&mut self, a: u16, v: u8) {
        match a {
            0x0000..=0x1FFF => self.ram[(a & 0x7FF) as usize] = v,
            0x2000..=0x3FFF => match a & 7 {
                0 => self.ctrl = v,
                6 => {
                    self.ppu_addr = if self.ppu_latch {
                        (self.ppu_addr & 0xFF00) | v as u16
                    } else {
                        (v as u16) << 8
                    };
                    self.ppu_latch = !self.ppu_latch;
                }
                7 => {
                    let i = (self.ppu_addr & 0x3FFF) as usize;
                    self.ppu[i] = v;
                    if i < 0x2000 {
                        self.chr_writes += 1;
                    }
                    self.ppu_addr = self.ppu_addr.wrapping_add(self.vram_step());
                }
                _ => {}
            },
            0x4016 => {
                if v & 1 == 0 {
                    self.pad_shift = self.pad;
                }
            }
            0x4000..=0x4017 => {
                self.log.push(RegWrite { frame: self.frame, addr: a, value: v });
                self.apu.write(a, v);
            }
            0x6000..=0x7FFF => self.wram[(a - 0x6000) as usize] = v,
            0x8000..=0xFFFF => self.mapper.write(a, v),
            _ => {}
        }
    }
}

impl Nes {
    /// `$2000` bit 2 picks whether `$2007` steps one byte or a whole row of
    /// the nametable. Tile uploads use the former, but a game that has left
    /// the flag set from drawing a screen would otherwise scatter its tiles
    /// through CHR at 32-byte intervals.
    /// One APU cycle, with DPCM fetches served out of PRG through whatever
    /// bank the mapper currently has selected.
    fn tick_apu(&mut self) {
        let (cart, mapper) = (&self.cart, &self.mapper);
        self.apu.clock(|a| {
            if a >= 0x8000 {
                cart.prg.get(mapper.prg_offset(cart, a)).copied().unwrap_or(0)
            } else {
                0
            }
        });
    }

    fn vram_step(&self) -> u16 {
        if self.ctrl & 0x04 != 0 {
            32
        } else {
            1
        }
    }
}

/// Controller bits, in the order `$4016` shifts them out.
pub const BTN_A: u8 = 0x01;
pub const BTN_B: u8 = 0x02;
pub const BTN_SELECT: u8 = 0x04;
pub const BTN_START: u8 = 0x08;
pub const BTN_UP: u8 = 0x10;
pub const BTN_DOWN: u8 = 0x20;
pub const BTN_LEFT: u8 = 0x40;
pub const BTN_RIGHT: u8 = 0x80;

/// Parse `"start"`, `"a"`, `"up"` and so on into a controller bit.
pub fn button(name: &str) -> Option<u8> {
    Some(match name.to_ascii_lowercase().as_str() {
        "a" => BTN_A,
        "b" => BTN_B,
        "select" => BTN_SELECT,
        "start" => BTN_START,
        "up" => BTN_UP,
        "down" => BTN_DOWN,
        "left" => BTN_LEFT,
        "right" => BTN_RIGHT,
        _ => return None,
    })
}

/// A button held from `frame` for `hold` frames.
///
/// Games do not play their music until something asks them to: a title
/// screen waits for Start, a menu for a selection. Without a way to press a
/// button, a capture only ever hears whatever plays on the title screen.
#[derive(Clone, Copy, Debug)]
pub struct Press {
    pub frame: u32,
    pub buttons: u8,
    pub hold: u32,
}

/// What running the cartridge produced.
#[derive(Clone, Debug)]
pub struct ChrDump {
    /// The 8 KB pattern-table region of video memory.
    pub chr: Vec<u8>,
    /// Bytes the game wrote through `$2007` into that region.
    pub written: usize,
    pub frames: u32,
    pub mapper: u8,
}

/// Boot a cartridge and return whatever it put in its pattern tables.
///
/// `frames` is how long to let it run. Tiles usually arrive in the first
/// handful, but a game with a licence screen or a decompressor may take a
/// second or two to reach the graphics worth having.
pub fn run_for_chr(bytes: &[u8], frames: u32) -> Result<ChrDump> {
    Ok(run(bytes, frames, &[])?.0)
}

/// Boot a cartridge and return both what it drew and what it played.
///
/// The register log comes back in the same shape an emulator dump does, so
/// a game's music reaches the transcriber through the path `viper rip`
/// already had for foreign logs — there is no second transcriber.
pub fn run(bytes: &[u8], frames: u32, presses: &[Press]) -> Result<(ChrDump, Vec<RegWrite>)> {
    let cart = Cart::parse(bytes)?;
    let mapper = Mapper::new(cart.mapper)?;
    let mut nes = Nes {
        cart,
        mapper,
        ram: [0; 0x800],
        wram: [0; 0x2000],
        ppu: [0; 0x4000],
        ppu_addr: 0,
        ppu_latch: false,
        ctrl: 0,
        vblank: false,
        chr_writes: 0,
        apu: Apu::new(),
        log: Vec::new(),
        frame: 0,
        pad: 0,
        pad_shift: 0,
    };
    let mut cpu = Cpu::default();
    let lo = nes.read(0xFFFC) as u16;
    let hi = nes.read(0xFFFD) as u16;
    cpu.pc = lo | (hi << 8);

    for f in 0..frames {
        nes.frame = f;
        nes.pad = presses
            .iter()
            .filter(|p| f >= p.frame && f < p.frame + p.hold.max(1))
            .fold(0, |a, p| a | p.buttons);
        // The visible frame, then vblank: raise the flag, take the interrupt
        // the upload code is almost certainly waiting for, and give the
        // handler room to run before the next frame starts.
        let mut spent = 0u32;
        while spent < CYCLES_PER_FRAME {
            let used = cpu.step(&mut nes);
            for _ in 0..used {
                nes.tick_apu();
            }
            spent += used;
        }
        nes.vblank = true;
        // Follow $2000 bit 7 as it stands now, rather than latching that the
        // game once enabled interrupts: a game turns NMI off around a
        // critical section precisely so the handler cannot run, and firing
        // one anyway corrupts whatever it was protecting.
        if nes.ctrl & 0x80 != 0 {
            cpu.nmi(&mut nes);
        }
    }

    let dump = ChrDump {
        chr: nes.ppu[0..0x2000].to_vec(),
        written: nes.chr_writes,
        frames,
        mapper: nes.cart.mapper,
    };
    Ok((dump, nes.log))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an iNES file around `code`, placed at `$8000`, with the reset
    /// vector pointing at it and the NMI vector at `nmi` if given.
    ///
    /// Hand-assembled, the same way the APU tests build an NSF: viper cannot
    /// assemble 6502, and these programs are a handful of stores.
    fn cart(prg_banks: u8, chr_banks: u8, mapper: u8, code: &[u8], nmi: Option<u16>) -> Vec<u8> {
        let mut v = b"NES\x1A".to_vec();
        v.push(prg_banks);
        v.push(chr_banks);
        v.push((mapper & 0x0F) << 4);
        v.push(mapper & 0xF0);
        v.resize(16, 0);
        let prg_len = prg_banks as usize * 16 * 1024;
        let mut prg = vec![0u8; prg_len];
        prg[..code.len()].copy_from_slice(code);
        // Vectors live at the top of the last bank, which is what the CPU
        // reads through $FFFA..$FFFF however the mapper is configured.
        let top = prg_len - 6;
        prg[top] = nmi.unwrap_or(0x8000) as u8;
        prg[top + 1] = (nmi.unwrap_or(0x8000) >> 8) as u8;
        prg[top + 2] = 0x00; // reset -> $8000
        prg[top + 3] = 0x80;
        v.extend(prg);
        v.extend(std::iter::repeat_n(0u8, chr_banks as usize * 8 * 1024));
        v
    }

    /// Write `$FF` twice through the PPU data port at address `$0000`, then
    /// spin. That is a tile upload in miniature.
    const UPLOAD: &[u8] = &[
        0xA9, 0x00, // LDA #$00
        0x8D, 0x06, 0x20, // STA $2006  (high byte)
        0x8D, 0x06, 0x20, // STA $2006  (low byte) -> addr $0000
        0xA9, 0xFF, // LDA #$FF
        0x8D, 0x07, 0x20, // STA $2007
        0x8D, 0x07, 0x20, // STA $2007
        0x4C, 0x0F, 0x80, // JMP $800F  (spin)
    ];

    #[test]
    fn a_rom_that_writes_through_the_ppu_port_lands_in_chr() {
        // The whole mechanism in one test: CHR-RAM is not a region of the
        // file, it is whatever the game put there through $2006 and $2007.
        let d = run_for_chr(&cart(1, 0, 0, UPLOAD, None), 1).unwrap();
        assert_eq!(&d.chr[0..2], &[0xFF, 0xFF]);
        assert_eq!(d.written, 2);
        assert!(d.chr[2..].iter().all(|&b| b == 0), "and nothing else moved");
    }

    #[test]
    fn the_data_port_steps_by_one_or_by_a_whole_row() {
        // $2000 bit 2 chooses. Tile uploads use the byte step, but a game
        // that left the flag set from drawing a screen would otherwise
        // scatter its tiles through CHR at 32-byte intervals.
        let mut code = vec![
            0xA9, 0x04, 0x8D, 0x00, 0x20, // LDA #$04 / STA $2000 -> step 32
        ];
        code.extend_from_slice(UPLOAD);
        // Re-point the spin at the end of the longer program.
        let end = code.len() as u16 - 3 + 0x8000;
        let n = code.len();
        code[n - 2] = end as u8;
        code[n - 1] = (end >> 8) as u8;
        let d = run_for_chr(&cart(1, 0, 0, &code, None), 1).unwrap();
        assert_eq!(d.chr[0], 0xFF);
        assert_eq!(d.chr[32], 0xFF, "the second write skipped a row");
        assert_eq!(d.chr[1], 0x00);
    }

    #[test]
    fn reading_the_status_port_clears_vblank() {
        // Returning vblank permanently set is the obvious shortcut and it
        // hangs any loop that waits for vblank to *end*. Two of the seven
        // CHR-RAM cartridges I tried wrote nothing at all until this was
        // modelled properly, so it is worth pinning.
        //
        //   LDA $2002   ; the flag is up after a frame
        //   STA $0010
        //   LDA $2002   ; and down again straight after
        //   STA $0011
        let code = &[
            0xAD, 0x02, 0x20, 0x85, 0x10, 0xAD, 0x02, 0x20, 0x85, 0x11, 0x4C, 0x0A, 0x80,
        ];
        // One frame to raise the flag, a second for the program to see it.
        let bytes = cart(1, 0, 0, code, None);
        let cartridge = Cart::parse(&bytes).unwrap();
        let mut nes = Nes {
            cart: cartridge,
            mapper: Mapper::new(0).unwrap(),
            ram: [0; 0x800],
            wram: [0; 0x2000],
            ppu: [0; 0x4000],
            ppu_addr: 0,
            ppu_latch: false,
            ctrl: 0,
            vblank: true,
            chr_writes: 0,
            apu: Apu::new(),
            log: Vec::new(),
            frame: 0,
            pad: 0,
            pad_shift: 0,
        };
        let mut cpu = Cpu::default();
        cpu.pc = 0x8000;
        for _ in 0..8 {
            cpu.step(&mut nes);
        }
        assert_eq!(nes.ram[0x10], 0x80, "the first read sees vblank");
        assert_eq!(nes.ram[0x11], 0x00, "and clears it for the second");
    }

    #[test]
    fn an_nmi_runs_the_handler_only_while_it_is_enabled() {
        // Games turn NMI off around a critical section precisely so the
        // handler cannot run. Latching "this game once enabled interrupts"
        // and firing anyway corrupts whatever was being protected.
        //
        // Handler at $9000 writes through the PPU port; the main program
        // never does. So a byte in CHR means the handler ran.
        let mut code = vec![0x4C, 0x00, 0x80]; // JMP self, NMI never enabled
        code.resize(0x1000, 0);
        code.extend_from_slice(UPLOAD);
        let quiet = run_for_chr(&cart(1, 0, 0, &code, Some(0x9000)), 4).unwrap();
        assert_eq!(quiet.written, 0, "no NMI without $2000 bit 7");

        // Now enable it first: LDA #$80 / STA $2000 / JMP self.
        let mut code = vec![0xA9, 0x80, 0x8D, 0x00, 0x20, 0x4C, 0x05, 0x80];
        code.resize(0x1000, 0);
        code.extend_from_slice(UPLOAD);
        let loud = run_for_chr(&cart(1, 0, 0, &code, Some(0x9000)), 4).unwrap();
        assert!(loud.written > 0, "the handler ran once NMI was enabled");
        assert_eq!(loud.chr[0], 0xFF);
    }

    #[test]
    fn a_cartridge_with_chr_rom_keeps_it() {
        let d = run_for_chr(&cart(1, 1, 0, UPLOAD, None), 1).unwrap();
        assert!(!Cart::parse(&cart(1, 1, 0, UPLOAD, None)).unwrap().chr_is_ram);
        assert_eq!(d.written, 2, "the port still works even so");
    }

    #[test]
    fn uxrom_switches_the_low_half_and_fixes_the_high_one() {
        // Mapper 2, which four of the seven CHR-RAM games use. The bank
        // written to any address in $8000..=$FFFF selects the low window;
        // the high one is always the last bank, which is where the reset
        // vector and the loader live.
        let bytes = cart(4, 0, 2, UPLOAD, None);
        let c = Cart::parse(&bytes).unwrap();
        let mut m = Mapper::new(2).unwrap();
        assert_eq!(m.prg_offset(&c, 0x8000), 0, "bank 0 by default");
        assert_eq!(m.prg_offset(&c, 0xC000), 3 * 16 * 1024, "and the last at $C000");
        m.write(0x8000, 2);
        assert_eq!(m.prg_offset(&c, 0x8000), 2 * 16 * 1024);
        assert_eq!(m.prg_offset(&c, 0xC000), 3 * 16 * 1024, "the high half does not move");
        // A bank past the end clamps rather than reading off the end of PRG.
        m.write(0x8000, 15);
        assert_eq!(m.prg_offset(&c, 0x8000), 3 * 16 * 1024);
    }

    #[test]
    fn mmc1_takes_its_bank_five_writes_at_a_time() {
        // The register is serial: one bit per write, low bit first, and the
        // address of the fifth write says which register it lands in. A
        // parallel reading of it would put every game in the wrong bank.
        let bytes = cart(8, 0, 1, UPLOAD, None);
        let c = Cart::parse(&bytes).unwrap();
        let mut m = Mapper::new(1).unwrap();
        // Reset control is PRG mode 3: switch $8000, fix the last at $C000.
        assert_eq!(m.prg_offset(&c, 0xC000), 7 * 16 * 1024);
        assert_eq!(m.prg_offset(&c, 0x8000), 0);
        // Shift 5 into the PRG register at $E000, one bit at a time.
        for bit in [1u8, 0, 1, 0, 0] {
            m.write(0xE000, bit);
        }
        assert_eq!(m.prg_offset(&c, 0x8000), 5 * 16 * 1024);
        assert_eq!(m.prg_offset(&c, 0xC000), 7 * 16 * 1024, "the fixed half stays put");
        // Four writes are not enough to commit anything.
        for bit in [1u8, 1, 1, 1] {
            m.write(0xE000, bit);
        }
        assert_eq!(m.prg_offset(&c, 0x8000), 5 * 16 * 1024, "still the old bank");
        // And bit 7 resets the sequence, discarding those four.
        m.write(0xE000, 0x80);
        for bit in [0u8, 0, 0, 0, 0] {
            m.write(0xE000, bit);
        }
        assert_eq!(m.prg_offset(&c, 0x8000), 0, "back to bank 0, not 15");
    }

    #[test]
    fn what_a_cartridge_plays_comes_back_as_a_register_log() {
        // The whole point of the music path: a game's writes to the sound
        // chip arrive in the same shape an emulator dump does, so `viper
        // rip` transcribes a cartridge through the code it already had for
        // foreign logs rather than through a second transcriber.
        //
        //   LDA #$BF / STA $4000   ; duty 2, constant volume 15
        //   LDA #$C9 / STA $4002   ; period low
        //   LDA #$08 / STA $4003   ; length + period high -> a note starts
        //   JMP self
        let code = &[
            0xA9, 0xBF, 0x8D, 0x00, 0x40,
            0xA9, 0xC9, 0x8D, 0x02, 0x40,
            0xA9, 0x08, 0x8D, 0x03, 0x40,
            0x4C, 0x0F, 0x80,
        ];
        let (_, log) = run(&cart(1, 1, 0, code, None), 3, &[]).unwrap();
        let writes: Vec<(u16, u8)> = log.iter().map(|w| (w.addr, w.value)).collect();
        assert_eq!(writes, vec![(0x4000, 0xBF), (0x4002, 0xC9), (0x4003, 0x08)]);
        assert!(log.iter().all(|w| w.frame == 0), "stamped with the frame they happened in");
    }

    #[test]
    fn the_sound_chip_answers_reads_rather_than_returning_zero() {
        // A music driver polls $4015 to learn when a note's length counter
        // ran out. One that always reads zero leaves the driver waiting for
        // a note that, as far as it can tell, never ends.
        //
        //   LDA #$01 / STA $4015   ; enable pulse 1
        //   LDA #$BF / STA $4000
        //   LDA #$08 / STA $4003   ; a note with a length
        //   LDA $4015 / STA $0020
        //   JMP self
        let code = &[
            0xA9, 0x01, 0x8D, 0x15, 0x40,
            0xA9, 0xBF, 0x8D, 0x00, 0x40,
            0xA9, 0x08, 0x8D, 0x03, 0x40,
            0xAD, 0x15, 0x40, 0x85, 0x20,
            0x4C, 0x14, 0x80,
        ];
        let bytes = cart(1, 1, 0, code, None);
        let (_, log) = run(&bytes, 1, &[]).unwrap();
        // The status read is not a write, so it does not appear in the log —
        // but the enable that preceded it does, which is what proves the
        // chip was actually wired up rather than stubbed out.
        assert!(log.iter().any(|w| w.addr == 0x4015 && w.value == 0x01));
    }

    #[test]
    fn a_button_press_reaches_the_game() {
        // Games do not play until something asks them to. This cartridge
        // strobes the controller, shifts out four buttons, and only touches
        // the sound chip once Start comes back set — which is exactly the
        // shape of a title screen.
        let code = &[
            0xA9, 0x01, 0x8D, 0x16, 0x40, // strobe on
            0xA9, 0x00, 0x8D, 0x16, 0x40, // strobe off: latch the buttons
            0xAD, 0x16, 0x40, // A
            0xAD, 0x16, 0x40, // B
            0xAD, 0x16, 0x40, // select
            0xAD, 0x16, 0x40, // start
            0x29, 0x01, // AND #$01
            0xF0, 0xE6, // BEQ back to the top
            0xA9, 0xBF, 0x8D, 0x00, 0x40, // it was pressed: make a sound
            0x4C, 0x1F, 0x80,
        ];
        let bytes = cart(1, 1, 0, code, None);

        let (_, silent) = run(&bytes, 10, &[]).unwrap();
        assert!(
            !silent.iter().any(|w| w.addr == 0x4000),
            "nothing plays while the title screen waits"
        );

        let press = Press { frame: 4, buttons: BTN_START, hold: 8 };
        let (_, played) = run(&bytes, 10, &[press]).unwrap();
        let first = played.iter().find(|w| w.addr == 0x4000).expect("Start started it");
        assert!(first.frame >= 4, "and not before the button was pressed");
    }

    #[test]
    fn a_press_only_lasts_as_long_as_it_is_held() {
        // Held forever, a button reads as stuck down, and a game that
        // advances on release never advances.
        let p = Press { frame: 10, buttons: BTN_A, hold: 3 };
        let held = |f: u32| f >= p.frame && f < p.frame + p.hold.max(1);
        assert!(!held(9));
        assert!(held(10) && held(12));
        assert!(!held(13));
    }

    #[test]
    fn button_names_are_the_ones_on_the_controller() {
        assert_eq!(button("start"), Some(BTN_START));
        assert_eq!(button("START"), Some(BTN_START));
        assert_eq!(button("a"), Some(BTN_A));
        assert_eq!(button("up"), Some(BTN_UP));
        assert_eq!(button("turbo"), None);
    }

    #[test]
    fn a_mapper_nobody_taught_it_says_so_rather_than_guessing() {
        // Guessing NROM for an MMC3 game produces a machine that runs the
        // wrong code and reports empty graphics, which looks like a bug in
        // the extractor rather than a missing feature.
        let err = run_for_chr(&cart(1, 0, 4, UPLOAD, None), 1).unwrap_err().to_string();
        assert!(err.contains("mapper 4") && err.contains("not supported"), "{}", err);
        assert!(err.contains("MMC1") && err.contains("UxROM"), "and lists what it does know: {}", err);
    }

    #[test]
    fn a_file_that_is_not_a_cartridge_is_refused() {
        assert!(Cart::parse(b"not a rom").is_err());
        assert!(Cart::parse(&[]).is_err());
        // A header promising more PRG than the file holds.
        let mut short = cart(1, 0, 0, UPLOAD, None);
        short.truncate(200);
        assert!(Cart::parse(&short).is_err());
    }
}
