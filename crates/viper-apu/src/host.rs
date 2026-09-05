//! The NSF host: CPU + memory + APU stepped together. Runs INIT once and
//! PLAY at the frame rate, records every APU register write, and hands
//! out downsampled audio.

use crate::apu::{Apu, CH_ALL};
use crate::cpu::{Bus, Cpu};
use crate::nsf::{Memory, Nsf};
use anyhow::{bail, Result};

pub const CPU_HZ: f64 = 1_789_773.0;
/// Return sentinel: INIT/PLAY are entered via a synthetic JSR whose return
/// address is $FFFF; the run loop stops when PC lands there.
const SENTINEL: u16 = 0xFFFF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegWrite {
    pub frame: u32,
    pub addr: u16,
    pub value: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerKind {
    /// A DPCM sample started; `sample` is its $4012 value.
    Dpcm { addr_reg: u8 },
    /// Noise channel retriggered ($400F write).
    Noise,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trigger {
    pub frame: u32,
    /// CPU cycle within the frame.
    pub cycle: u32,
    pub kind: TriggerKind,
}

struct HostBus<'a> {
    mem: &'a mut Memory,
    apu: &'a mut Apu,
    log: &'a mut Vec<RegWrite>,
    frame: u32,
    pending: Vec<(u16, u8)>,
}

impl Bus for HostBus<'_> {
    fn read(&mut self, addr: u16) -> u8 {
        if addr == 0x4015 {
            self.apu.read_status()
        } else {
            self.mem.read(addr)
        }
    }
    fn write(&mut self, addr: u16, v: u8) {
        if (0x4000..=0x4017).contains(&addr) {
            self.log.push(RegWrite { frame: self.frame, addr, value: v });
            self.pending.push((addr, v));
        } else {
            self.mem.write(addr, v);
        }
    }
}

pub struct Player {
    pub nsf: Nsf,
    cpu: Cpu,
    mem: Memory,
    pub apu: Apu,
    pub log: Vec<RegWrite>,
    pub triggers: Vec<Trigger>,
    pub frame: u32,
    /// Cycles per frame as an exact rational to keep long renders honest.
    cycles_per_frame: f64,
    cycle_acc: f64,
    // audio
    pub sample_rate: u32,
    resample_acc: f32,
    resample_n: u32,
    resample_step: f64,
    resample_pos: f64,
    hp_prev_in: f32,
    hp_prev_out: f32,
    lp_prev: f32,
    pub samples: Vec<f32>,
    pub keep_log: bool,
}

impl Player {
    pub fn new(nsf: Nsf, sample_rate: u32) -> Self {
        let mem = Memory::new(&nsf);
        let speed_us = if nsf.ntsc_speed_us == 0 { 16639 } else { nsf.ntsc_speed_us };
        let cycles_per_frame = speed_us as f64 * CPU_HZ / 1_000_000.0;
        Self {
            nsf,
            cpu: Cpu::default(),
            mem,
            apu: Apu::new(),
            log: Vec::new(),
            triggers: Vec::new(),
            frame: 0,
            cycles_per_frame,
            cycle_acc: 0.0,
            sample_rate,
            resample_acc: 0.0,
            resample_n: 0,
            resample_step: sample_rate as f64 / CPU_HZ,
            resample_pos: 0.0,
            hp_prev_in: 0.0,
            hp_prev_out: 0.0,
            lp_prev: 0.0,
            samples: Vec::new(),
            keep_log: true,
        }
    }

    pub fn set_mask(&mut self, mask: u8) {
        self.apu.mask = mask & CH_ALL;
    }

    /// Run INIT for `song` (0-based). Register writes are logged as frame 0.
    pub fn init(&mut self, song: u8) -> Result<()> {
        if song >= self.nsf.songs {
            bail!("song {} out of range (NSF has {})", song, self.nsf.songs);
        }
        self.frame = 0;
        // Clear RAM like a player would.
        self.mem.ram.fill(0);
        self.cpu = Cpu::default();
        self.cpu.a = song;
        self.cpu.x = if self.nsf.pal { 1 } else { 0 };
        self.cpu.sp = 0xFF;
        let init = self.nsf.init;
        self.call(init, 4_000_000)?;
        Ok(())
    }

    /// Execute a subroutine until it returns to the sentinel or runs out
    /// of the cycle budget. Every CPU cycle also clocks the APU.
    fn call(&mut self, addr: u16, budget: u32) -> Result<u32> {
        // synthetic JSR: push SENTINEL-1
        let ret = SENTINEL.wrapping_sub(1);
        self.mem.ram[0x100 + self.cpu.sp as usize] = (ret >> 8) as u8;
        self.cpu.sp = self.cpu.sp.wrapping_sub(1);
        self.mem.ram[0x100 + self.cpu.sp as usize] = ret as u8;
        self.cpu.sp = self.cpu.sp.wrapping_sub(1);
        self.cpu.pc = addr;
        let mut spent = 0u32;
        let mut pending: Vec<(u16, u8)> = Vec::new();
        while self.cpu.pc != SENTINEL && spent < budget {
            let cyc = {
                let mut bus = HostBus {
                    mem: &mut self.mem,
                    apu: &mut self.apu,
                    log: &mut self.log,
                    frame: self.frame,
                    pending: std::mem::take(&mut pending),
                };
                let c = self.cpu.step(&mut bus);
                pending = bus.pending;
                c
            };
            // Clock the APU for the instruction's cycles; apply the
            // register writes on the last cycle (write happens late in
            // the instruction).
            for i in 0..cyc {
                if i == cyc - 1 {
                    for (a, v) in pending.drain(..) {
                        self.apu_write(a, v, spent + i);
                    }
                }
                self.tick_apu();
            }
            spent += cyc;
        }
        if !self.keep_log {
            self.log.clear();
        }
        Ok(spent)
    }

    fn apu_write(&mut self, addr: u16, v: u8, cycle: u32) {
        self.apu.write(addr, v);
        if let Some(s) = self.apu.last_dmc_start.take() {
            self.triggers.push(Trigger { frame: self.frame, cycle, kind: TriggerKind::Dpcm { addr_reg: s.addr_reg } });
        }
        if self.apu.noise_trigger {
            self.apu.noise_trigger = false;
            self.triggers.push(Trigger { frame: self.frame, cycle, kind: TriggerKind::Noise });
        }
    }

    fn tick_apu(&mut self) {
        let mem = &self.mem;
        let out = self.apu.clock(|a| mem.read(a));
        // Box-filter decimation to the output rate.
        self.resample_acc += out;
        self.resample_n += 1;
        self.resample_pos += self.resample_step;
        if self.resample_pos >= 1.0 {
            self.resample_pos -= 1.0;
            let avg = self.resample_acc / self.resample_n as f32;
            self.resample_acc = 0.0;
            self.resample_n = 0;
            self.emit(avg);
        }
    }

    fn emit(&mut self, x: f32) {
        // 90 Hz first-order high-pass, 14 kHz first-order low-pass — the
        // 2A03 output stage as documented on nesdev, applied at the
        // output rate.
        let sr = self.sample_rate as f32;
        let hp_rc = 1.0 / (2.0 * std::f32::consts::PI * 90.0);
        let hp_a = hp_rc / (hp_rc + 1.0 / sr);
        let hp = hp_a * (self.hp_prev_out + x - self.hp_prev_in);
        self.hp_prev_in = x;
        self.hp_prev_out = hp;
        let lp_rc = 1.0 / (2.0 * std::f32::consts::PI * 14_000.0);
        let lp_a = (1.0 / sr) / (lp_rc + 1.0 / sr);
        let lp = self.lp_prev + lp_a * (hp - self.lp_prev);
        self.lp_prev = lp;
        self.samples.push(lp);
    }

    /// Run one frame: call PLAY, then idle the APU for the rest of the
    /// frame's cycles. Returns cycles the PLAY routine consumed.
    pub fn frame(&mut self) -> Result<u32> {
        self.frame += 1;
        self.cycle_acc += self.cycles_per_frame;
        let frame_cycles = self.cycle_acc.floor() as u32;
        self.cycle_acc -= frame_cycles as f64;
        self.cpu.sp = 0xFF;
        let play = self.nsf.play;
        let spent = self.call(play, frame_cycles)?;
        for _ in spent..frame_cycles {
            self.tick_apu();
        }
        Ok(spent)
    }

    /// Hash of all CPU-visible RAM. Two frames with identical hashes at
    /// their boundary mean the driver is in an identical state: the song
    /// has looped.
    pub fn ram_hash(&self) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in self.mem.ram.iter().chain(self.mem.wram.iter()) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    pub fn take_samples(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.samples)
    }

    pub fn memory(&self) -> &Memory {
        &self.mem
    }
}
