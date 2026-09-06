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
    /// Whether this NSF's header declares VRC6. Gating the extra write
    /// interception on it is load-bearing: it guarantees a plain 2A03 file
    /// cannot gain a single new line in its register log, which is what keeps
    /// the golden log byte-identical.
    vrc6: bool,
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
        if (0x4000..=0x4017).contains(&addr) || (self.vrc6 && crate::vrc6::is_vrc6_reg(addr)) {
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
    /// Present only when the NSF header declares VRC6; see `HostBus::vrc6`.
    pub vrc6: crate::vrc6::Vrc6,
    has_vrc6: bool,
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
        let has_vrc6 = nsf.expansion & 0x01 != 0;
        let speed_us = if nsf.ntsc_speed_us == 0 { 16639 } else { nsf.ntsc_speed_us };
        let cycles_per_frame = speed_us as f64 * CPU_HZ / 1_000_000.0;
        Self {
            nsf,
            cpu: Cpu::default(),
            mem,
            apu: Apu::new(),
            vrc6: crate::vrc6::Vrc6::new(),
            has_vrc6,
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

    /// One `u8` covers both chips: bits 0-4 are the 2A03, bits 5-7 the VRC6.
    pub fn set_mask(&mut self, mask: u8) {
        self.apu.mask = mask & CH_ALL;
        self.vrc6.mask = mask & crate::vrc6::CH_VRC6_ALL;
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
                    vrc6: self.has_vrc6,
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
        if crate::vrc6::is_vrc6_reg(addr) {
            self.vrc6.write(addr, v);
            return;
        }
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
        // The VRC6 is summed onto the audio pin externally, so it is a second
        // term here rather than a channel inside the 2A03's mixing tables.
        let out = self.apu.clock(|a| mem.read(a)) + self.vrc6.clock();
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

#[cfg(test)]
mod host_tests {
    use super::*;
    use crate::vrc6::is_vrc6_reg;

    /// A whole VRC6 NSF, hand-assembled. viper cannot assemble 6502, but the
    /// program is four stores and a return, so the opcodes go in as a literal.
    /// This is the fixture that makes the VRC6 path testable end to end with
    /// no driver and no third-party file.
    fn vrc6_nsf(expansion: u8) -> Nsf {
        // INIT: set duty/volume, period lo, then enable + period hi. PLAY: rts.
        //   A9 7F  LDA #$7F      ; duty 7 (50%), volume 15
        //   8D 00 90  STA $9000
        //   A9 7F  LDA #$7F      ; period lo
        //   8D 01 90  STA $9001
        //   A9 80  LDA #$80      ; enable, period hi = 0
        //   8D 02 90  STA $9002
        //   60     RTS
        let mut code: Vec<u8> = vec![
            0xA9, 0x7F, 0x8D, 0x00, 0x90,
            0xA9, 0x7F, 0x8D, 0x01, 0x90,
            0xA9, 0x80, 0x8D, 0x02, 0x90,
            0x60,
        ];
        let play = 0x8000 + code.len() as u16;
        code.push(0x60); // PLAY: rts
        let header = Nsf::build_header(0x8000, 0x8000, play, 1, "vrc6", "", "", expansion);
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(&code);
        Nsf::parse(&bytes).unwrap()
    }

    /// Peak amplitude *after* the startup transient has settled.
    ///
    /// A 2A03 that has never been written leaves its triangle DAC at a
    /// constant level, and the 90 Hz high-pass takes about 50 ms to decay that
    /// DC step. Measuring from sample zero would read that as "sound", so
    /// these tests look at the tail.
    fn tail_peak(p: &Player) -> f32 {
        let skip = p.samples.len() / 2;
        p.samples[skip..].iter().cloned().fold(0f32, |a, b| a.max(b.abs()))
    }

    fn run(nsf: Nsf, mask: Option<u8>) -> Player {
        let mut p = Player::new(nsf, 44_100);
        if let Some(m) = mask {
            p.set_mask(m);
        }
        p.init(0).unwrap();
        for _ in 0..30 {
            p.frame().unwrap();
        }
        p
    }

    /// A whole 2A03 NSF, hand-assembled the same way as `vrc6_nsf`: a pulse
    /// at 50% duty, volume `vol`, period 253 — about 440 Hz, comfortably
    /// between the output stage's 90 Hz high-pass and its 14 kHz low-pass.
    fn pulse_nsf(vol: u8) -> Nsf {
        //   A9 01  LDA #$01      ; enable pulse 1 only
        //   8D 15 40  STA $4015
        //   A9 Bx  LDA #$Bx      ; duty 2, halt set, constant volume, vol
        //   8D 00 40  STA $4000
        //   A9 08  LDA #$08      ; sweep off, shift 0 — the mute cannot fire
        //   8D 01 40  STA $4001
        //   A9 FD  LDA #$FD      ; period low = 253
        //   8D 02 40  STA $4002
        //   A9 08  LDA #$08      ; length index 1, period high = 0
        //   8D 03 40  STA $4003
        //   60     RTS
        let mut code: Vec<u8> = vec![
            0xA9, 0x01, 0x8D, 0x15, 0x40,
            0xA9, 0xB0 | (vol & 0x0F), 0x8D, 0x00, 0x40,
            0xA9, 0x08, 0x8D, 0x01, 0x40,
            0xA9, 0xFD, 0x8D, 0x02, 0x40,
            0xA9, 0x08, 0x8D, 0x03, 0x40,
            0x60,
        ];
        let play = 0x8000 + code.len() as u16;
        code.push(0x60); // PLAY: rts
        let header = Nsf::build_header(0x8000, 0x8000, play, 1, "pulse", "", "", 0);
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(&code);
        Nsf::parse(&bytes).unwrap()
    }

    #[test]
    fn a_2a03_pulse_comes_out_at_the_amplitude_the_mixer_predicts() {
        // The one thing the golden log cannot check: that a register write
        // reaching the CPU bus arrives at the resampler at the level the
        // hardware says it should. The log records what the driver wrote;
        // this records what came out.
        //
        // Derived here rather than recorded, so it still means something
        // after the golden log is regenerated.
        //
        // A 50%-duty pulse alone at volume 15 alternates between
        // pulse_table[0] = 0 and pulse_table[15], from the published mixer
        // formula. The tempting prediction is half of that once the 90 Hz
        // high-pass removes the DC — and it is wrong. A first-order
        // high-pass does not merely centre a square wave, it droops within
        // each half cycle, so the steady-state excursion is A / (1 + a^N)
        // for a decay `a` over `N` samples of half period: about 0.61 A
        // here, not 0.5 A. Getting that wrong is how a perfectly correct
        // mixer looks broken.
        let a = 95.52 / (8128.0 / 15.0 + 100.0); // pulse_table[15]
        let hp_rc = 1.0 / (2.0 * std::f64::consts::PI * 90.0);
        let hp_a = hp_rc / (hp_rc + 1.0 / 44_100.0);
        let half = (16.0 * (253.0 + 1.0) / 2.0) * 44_100.0 / CPU_HZ; // samples
        let predicted = (a / (1.0 + hp_a.powf(half))) as f32;
        let got = tail_peak(&run(pulse_nsf(15), None));
        assert!(
            (got - predicted).abs() < predicted * 0.15,
            "expected about {:.5} from the mixer and the filter, got {:.5}",
            predicted,
            got
        );
    }

    #[test]
    fn the_non_linear_mixer_is_live_all_the_way_to_the_output() {
        // Volume 3 against volume 15. A linear mixer would give exactly
        // 3/15 = 0.200; the real tables give pulse_table[3] / pulse_table[15]
        // = 0.2285. The 14% gap is far outside measurement noise, so this
        // proves the lookup tables are still in the path — through the bus,
        // the resampler and both filters — and not quietly replaced by a
        // multiply somewhere.
        let loud = tail_peak(&run(pulse_nsf(15), None));
        let soft = tail_peak(&run(pulse_nsf(3), None));
        let ratio = soft / loud;
        assert!((ratio - 0.2285).abs() < 0.05 * 0.2285, "ratio {:.4}, linear would be 0.2000", ratio);
        assert!(ratio > 0.21, "and it is audibly not linear");
    }

    #[test]
    fn masking_a_channel_does_not_change_its_timing() {
        // apu.rs claims stems are "timing-identical to the full mix" because
        // masking silences at the mixer without touching channel state. If
        // that were ever untrue, every stem would drift against the mix it
        // was split from, and nothing else would notice.
        let full = run(pulse_nsf(15), Some(crate::apu::CH_ALL));
        let solo = run(pulse_nsf(15), Some(crate::apu::CH_PU1));
        let without = run(pulse_nsf(15), Some(crate::apu::CH_ALL & !crate::apu::CH_PU1));
        assert_eq!(full.log, solo.log, "the same writes happen either way");
        assert_eq!(full.samples.len(), solo.samples.len(), "and take the same time");
        // Not sample-identical: the unmasked triangle sits at its ultrasonic
        // mid-level and contributes a DC step the high-pass then removes. By
        // the tail that difference is gone, which is the level that matters.
        assert!((tail_peak(&full) - tail_peak(&solo)).abs() < 0.002, "same steady level");
        assert!(tail_peak(&without) < 0.001, "muting pulse 1 leaves silence");
    }

    #[test]
    fn a_vrc6_nsf_reaches_the_chip_and_makes_sound() {
        let p = run(vrc6_nsf(0x01), None);
        // The three register writes are logged, at frame 0 (INIT).
        let writes: Vec<(u16, u8)> = p
            .log
            .iter()
            .filter(|w| is_vrc6_reg(w.addr))
            .map(|w| (w.addr, w.value))
            .collect();
        assert_eq!(writes, vec![(0x9000, 0x7F), (0x9001, 0x7F), (0x9002, 0x80)]);
        assert!(p.log.iter().all(|w| w.frame == 0 || !is_vrc6_reg(w.addr)));
        // And it is audible at the level the mix constant predicts. A 50%-duty
        // pulse at volume 15 swings between 0 and 15 units; the 90 Hz
        // high-pass removes the DC half, leaving ±7.5 units.
        let predicted = 7.5 * crate::vrc6::VRC6_UNIT;
        let got = tail_peak(&p);
        assert!(
            (got - predicted).abs() < predicted * 0.15,
            "expected about {:.4}, got {:.4}",
            predicted,
            got,
        );
    }

    #[test]
    fn a_plain_2a03_nsf_never_sees_a_vrc6_write() {
        // The gate on the header bit is what keeps the golden log unchanged:
        // the same program with expansion 0 must route those stores to ROM.
        let p = run(vrc6_nsf(0x00), None);
        assert!(
            p.log.iter().all(|w| !is_vrc6_reg(w.addr)),
            "a 2A03 file must not log VRC6 writes",
        );
        assert!(tail_peak(&p) < 1e-3, "and must stay silent, peak {}", tail_peak(&p));
    }

    #[test]
    fn muting_the_vrc6_channels_silences_only_them() {
        let render = |mask: u8| tail_peak(&run(vrc6_nsf(0x01), Some(mask)));
        let full = render(crate::apu::CH_ALL | crate::vrc6::CH_VRC6_ALL);
        assert!(full > 0.05, "everything audible, got {}", full);
        assert!(render(crate::apu::CH_ALL) < 1e-3, "2A03-only mask drops the VRC6");
        assert!(
            (render(crate::vrc6::CH_VP1) - full).abs() < 1e-6,
            "the VP1 stem carries the whole signal, since only VP1 is sounding",
        );
    }
}

