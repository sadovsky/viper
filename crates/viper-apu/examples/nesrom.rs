//! Boot an iNES ROM on the 6502 + 2A03 core with a stub PPU, then report
//! what it drew and whether it made sound.
//!
//!     cargo run -p viper-apu --example nesrom -- album.nes [frames] [--press A]
//!
//! There is no picture: $2002 always reads "vblank", and $2006/$2007
//! writes are captured into a VRAM array. Because the ROM's font puts
//! each glyph at its own ASCII code, the nametable *is* the text, so the
//! screen can be printed back as characters.
use viper_apu::apu::Apu;
use viper_apu::cpu::{Bus, Cpu};

struct Nes {
    ram: [u8; 0x800],
    prg: Vec<u8>,
    vram: [u8; 0x1000],
    addr: u16,
    latch: bool,
    ctrl: u8,
    apu: Apu,
    pending: Vec<(u16, u8)>,
    pad: u8,
    pad_shift: u8,
}

impl Bus for Nes {
    fn read(&mut self, a: u16) -> u8 {
        match a {
            0x0000..=0x1FFF => self.ram[(a & 0x7FF) as usize],
            0x2002 => { self.latch = false; 0x80 }
            0x4015 => self.apu.read_status(),
            0x4016 => { let b = self.pad_shift & 1; self.pad_shift >>= 1; b }
            0x8000..=0xFFFF => self.prg[(a - 0x8000) as usize],
            _ => 0,
        }
    }
    fn write(&mut self, a: u16, v: u8) {
        match a {
            0x0000..=0x1FFF => self.ram[(a & 0x7FF) as usize] = v,
            0x2000 => self.ctrl = v,
            0x2006 => {
                self.addr = if self.latch { (self.addr & 0xFF00) | v as u16 } else { (v as u16) << 8 };
                self.latch = !self.latch;
            }
            0x2007 => {
                let i = (self.addr & 0x0FFF) as usize;
                self.vram[i] = v;
                self.addr = self.addr.wrapping_add(1);
            }
            0x4016 => { if v & 1 == 0 { self.pad_shift = self.pad; } }
            0x4000..=0x4017 => self.pending.push((a, v)),
            _ => {}
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rom = std::fs::read(&args[0])?;
    anyhow::ensure!(&rom[0..4] == b"NES\x1A", "not an iNES ROM");
    let prg_banks = rom[4] as usize;
    let prg = rom[16..16 + prg_banks * 16384].to_vec();
    anyhow::ensure!(prg.len() == 0x8000, "expected 32 KB PRG, got {}", prg.len());
    let frames: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(180);
    let press: u8 = if args.iter().any(|a| a == "--press") { 0x80 } else { 0 };

    let mut nes = Nes {
        ram: [0; 0x800], prg, vram: [0; 0x1000], addr: 0, latch: false, ctrl: 0,
        apu: Apu::new(), pending: Vec::new(), pad: 0, pad_shift: 0,
    };
    let mut cpu = Cpu::default();
    cpu.pc = u16::from_le_bytes([nes.read(0xFFFC), nes.read(0xFFFD)]);
    println!("reset vector ${:04X}", cpu.pc);

    let mut samples: Vec<f32> = Vec::new();
    let mut writes = 0usize;
    let run = |cpu: &mut Cpu, nes: &mut Nes, cycles: u32, samples: &mut Vec<f32>, writes: &mut usize| {
        let mut spent = 0;
        while spent < cycles {
            let c = cpu.step(nes);
            for i in 0..c {
                if i == c - 1 {
                    let pend: Vec<(u16, u8)> = nes.pending.drain(..).collect();
                    for (a, v) in pend { nes.apu.write(a, v); *writes += 1; }
                }
                let prg = nes.prg.clone();
                let out = nes.apu.clock(|addr| prg.get((addr as usize).wrapping_sub(0x8000)).copied().unwrap_or(0x55));
                samples.push(out);
            }
            spent += c;
        }
    };
    // boot
    run(&mut cpu, &mut nes, 200_000, &mut samples, &mut writes);
    println!("after boot: NMI {}, rendering enabled by ${:02X}, APU writes {}", if nes.ctrl & 0x80 != 0 { "on" } else { "OFF" }, nes.ctrl, writes);
    // frames, with the button held down for the second half if asked
    for f in 0..frames {
        nes.pad = if press != 0 && f == frames / 2 { press } else { 0 };
        if nes.ctrl & 0x80 != 0 {
            let ret = cpu.pc;
            let p = cpu.p;
            nes.write(0x0100 | cpu.sp as u16, (ret >> 8) as u8); cpu.sp = cpu.sp.wrapping_sub(1);
            nes.write(0x0100 | cpu.sp as u16, ret as u8); cpu.sp = cpu.sp.wrapping_sub(1);
            nes.write(0x0100 | cpu.sp as u16, p); cpu.sp = cpu.sp.wrapping_sub(1);
            cpu.pc = u16::from_le_bytes([nes.read(0xFFFA), nes.read(0xFFFB)]);
        }
        run(&mut cpu, &mut nes, 29_780, &mut samples, &mut writes);
    }
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    println!("{} frames: {} APU writes, output rms {:.4} ({})", frames, writes, rms, if rms > 0.005 { "playing" } else { "SILENT" });
    println!("--- screen ---");
    for row in 0..30 {
        let line: String = (0..32)
            .map(|c| nes.vram[row * 32 + c])
            .map(|b| if (32..127).contains(&b) { b as char } else { ' ' })
            .collect();
        if line.trim_end().is_empty() { continue; }
        println!("{:2} |{}", row, line.trim_end());
    }
    Ok(())
}
