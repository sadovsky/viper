//! A straightforward 6502 core: every official opcode, documented cycle
//! counts including page-cross penalties, no decimal mode (the 2A03 has
//! none), unofficial opcodes treated as NOPs of the right length.
//!
//! The core is deliberately unclever. It exists to run a ~1 KB sound
//! driver at 60 Hz, and the property that matters is that the sequence
//! of writes it produces matches what a real 2A03 would produce.

pub trait Bus {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, v: u8);
}

pub const C: u8 = 0x01;
pub const Z: u8 = 0x02;
pub const I: u8 = 0x04;
pub const D: u8 = 0x08;
pub const B: u8 = 0x10;
pub const U: u8 = 0x20;
pub const V: u8 = 0x40;
pub const N: u8 = 0x80;

#[derive(Clone, Debug)]
pub struct Cpu {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub pc: u16,
    pub p: u8,
}

impl Default for Cpu {
    fn default() -> Self {
        Self { a: 0, x: 0, y: 0, sp: 0xFD, pc: 0, p: I | U }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Imm,
    Zp,
    Zpx,
    Zpy,
    Abs,
    Abx,
    Aby,
    Ind,
    Izx,
    Izy,
    Rel,
}

impl Cpu {
    fn set_zn(&mut self, v: u8) {
        self.p = (self.p & !(Z | N)) | if v == 0 { Z } else { 0 } | (v & N);
    }
    fn flag(&self, f: u8) -> bool {
        self.p & f != 0
    }
    fn set_flag(&mut self, f: u8, on: bool) {
        if on {
            self.p |= f
        } else {
            self.p &= !f
        }
    }
    fn fetch<Bu: Bus>(&mut self, bus: &mut Bu) -> u8 {
        let v = bus.read(self.pc);
        self.pc = self.pc.wrapping_add(1);
        v
    }
    fn fetch16<Bu: Bus>(&mut self, bus: &mut Bu) -> u16 {
        let lo = self.fetch(bus) as u16;
        let hi = self.fetch(bus) as u16;
        lo | (hi << 8)
    }
    fn push<Bu: Bus>(&mut self, bus: &mut Bu, v: u8) {
        bus.write(0x0100 | self.sp as u16, v);
        self.sp = self.sp.wrapping_sub(1);
    }
    fn pop<Bu: Bus>(&mut self, bus: &mut Bu) -> u8 {
        self.sp = self.sp.wrapping_add(1);
        bus.read(0x0100 | self.sp as u16)
    }
    fn read16_zp<Bu: Bus>(&mut self, bus: &mut Bu, zp: u8) -> u16 {
        let lo = bus.read(zp as u16) as u16;
        let hi = bus.read(zp.wrapping_add(1) as u16) as u16;
        lo | (hi << 8)
    }

    /// Resolve an operand address. Returns (addr, page_crossed).
    fn operand<Bu: Bus>(&mut self, bus: &mut Bu, m: Mode) -> (u16, bool) {
        match m {
            Mode::Imm => {
                let a = self.pc;
                self.pc = self.pc.wrapping_add(1);
                (a, false)
            }
            Mode::Zp => (self.fetch(bus) as u16, false),
            Mode::Zpx => (self.fetch(bus).wrapping_add(self.x) as u16, false),
            Mode::Zpy => (self.fetch(bus).wrapping_add(self.y) as u16, false),
            Mode::Abs => (self.fetch16(bus), false),
            Mode::Abx => {
                let b = self.fetch16(bus);
                let a = b.wrapping_add(self.x as u16);
                (a, (a & 0xFF00) != (b & 0xFF00))
            }
            Mode::Aby => {
                let b = self.fetch16(bus);
                let a = b.wrapping_add(self.y as u16);
                (a, (a & 0xFF00) != (b & 0xFF00))
            }
            Mode::Ind => {
                let p = self.fetch16(bus);
                let lo = bus.read(p) as u16;
                // 6502 bug: no page carry on the indirect vector fetch.
                let hi = bus.read((p & 0xFF00) | ((p + 1) & 0x00FF)) as u16;
                (lo | (hi << 8), false)
            }
            Mode::Izx => {
                let z = self.fetch(bus).wrapping_add(self.x);
                (self.read16_zp(bus, z), false)
            }
            Mode::Izy => {
                let z = self.fetch(bus);
                let b = self.read16_zp(bus, z);
                let a = b.wrapping_add(self.y as u16);
                (a, (a & 0xFF00) != (b & 0xFF00))
            }
            Mode::Rel => {
                let off = self.fetch(bus) as i8 as i16;
                let a = (self.pc as i16).wrapping_add(off) as u16;
                (a, (a & 0xFF00) != (self.pc & 0xFF00))
            }
        }
    }

    fn branch<Bu: Bus>(&mut self, bus: &mut Bu, cond: bool) -> u32 {
        let (target, cross) = self.operand(bus, Mode::Rel);
        if cond {
            self.pc = target;
            if cross { 4 } else { 3 }
        } else {
            2
        }
    }

    fn adc(&mut self, v: u8) {
        let a = self.a as u16;
        let sum = a + v as u16 + (self.p & C) as u16;
        let r = sum as u8;
        self.set_flag(C, sum > 0xFF);
        self.set_flag(V, (!(self.a ^ v) & (self.a ^ r) & 0x80) != 0);
        self.a = r;
        self.set_zn(r);
    }
    fn sbc(&mut self, v: u8) {
        self.adc(!v);
    }
    fn cmp(&mut self, r: u8, v: u8) {
        self.set_flag(C, r >= v);
        self.set_zn(r.wrapping_sub(v));
    }

    /// Execute one instruction. Returns the cycle count.
    /// Take an NMI: push the return address and flags, then vector through
    /// `$FFFA`.
    ///
    /// Unlike `BRK` the pushed status has the B flag *clear* — that bit is
    /// how an `RTI` handler tells a software break from a hardware
    /// interrupt, and a game that checks it will take the wrong branch if
    /// this is got wrong. The NSF host never needs this, since it drives
    /// PLAY through a synthetic JSR, but a real cartridge does almost all
    /// its work in the vblank handler.
    pub fn nmi<Bu: Bus>(&mut self, bus: &mut Bu) -> u32 {
        let pc = self.pc;
        self.push(bus, (pc >> 8) as u8);
        self.push(bus, pc as u8);
        self.push(bus, (self.p & !B) | U);
        self.set_flag(I, true);
        let lo = bus.read(0xFFFA) as u16;
        let hi = bus.read(0xFFFB) as u16;
        self.pc = lo | (hi << 8);
        7
    }

    pub fn step<Bu: Bus>(&mut self, bus: &mut Bu) -> u32 {
        let op = self.fetch(bus);
        // Read-modify-write helper.
        macro_rules! rmw {
            ($m:expr, $cyc:expr, $f:expr) => {{
                let (addr, _) = self.operand(bus, $m);
                let v = bus.read(addr);
                let r = $f(self, v);
                bus.write(addr, r);
                $cyc
            }};
        }
        macro_rules! rmw_acc {
            ($f:expr) => {{
                let v = self.a;
                self.a = $f(self, v);
                2
            }};
        }
        macro_rules! load {
            ($m:expr, $cyc:expr, $f:expr) => {{
                let (addr, cross) = self.operand(bus, $m);
                let v = bus.read(addr);
                $f(self, v);
                $cyc + cross as u32
            }};
        }
        macro_rules! store {
            ($m:expr, $cyc:expr, $v:expr) => {{
                let (addr, _) = self.operand(bus, $m);
                let v = $v(self);
                bus.write(addr, v);
                $cyc
            }};
        }
        let asl = |c: &mut Cpu, v: u8| { c.set_flag(C, v & 0x80 != 0); let r = v << 1; c.set_zn(r); r };
        let lsr = |c: &mut Cpu, v: u8| { c.set_flag(C, v & 1 != 0); let r = v >> 1; c.set_zn(r); r };
        let rol = |c: &mut Cpu, v: u8| { let ci = c.p & C; c.set_flag(C, v & 0x80 != 0); let r = (v << 1) | ci; c.set_zn(r); r };
        let ror = |c: &mut Cpu, v: u8| { let ci = (c.p & C) << 7; c.set_flag(C, v & 1 != 0); let r = (v >> 1) | ci; c.set_zn(r); r };
        let inc = |c: &mut Cpu, v: u8| { let r = v.wrapping_add(1); c.set_zn(r); r };
        let dec = |c: &mut Cpu, v: u8| { let r = v.wrapping_sub(1); c.set_zn(r); r };
        let lda = |c: &mut Cpu, v: u8| { c.a = v; c.set_zn(v); };
        let ldx = |c: &mut Cpu, v: u8| { c.x = v; c.set_zn(v); };
        let ldy = |c: &mut Cpu, v: u8| { c.y = v; c.set_zn(v); };
        let and = |c: &mut Cpu, v: u8| { c.a &= v; c.set_zn(c.a); };
        let ora = |c: &mut Cpu, v: u8| { c.a |= v; c.set_zn(c.a); };
        let eor = |c: &mut Cpu, v: u8| { c.a ^= v; c.set_zn(c.a); };
        let adc = |c: &mut Cpu, v: u8| c.adc(v);
        let sbc = |c: &mut Cpu, v: u8| c.sbc(v);
        let cmpa = |c: &mut Cpu, v: u8| { let a = c.a; c.cmp(a, v) };
        let cpx = |c: &mut Cpu, v: u8| { let x = c.x; c.cmp(x, v) };
        let cpy = |c: &mut Cpu, v: u8| { let y = c.y; c.cmp(y, v) };
        let bit = |c: &mut Cpu, v: u8| {
            c.set_flag(Z, c.a & v == 0);
            c.set_flag(V, v & V != 0);
            c.set_flag(N, v & N != 0);
        };
        let sta = |c: &mut Cpu| c.a;
        let stx = |c: &mut Cpu| c.x;
        let sty = |c: &mut Cpu| c.y;

        match op {
            // --- loads / stores ---
            0xA9 => load!(Mode::Imm, 2, lda), 0xA5 => load!(Mode::Zp, 3, lda), 0xB5 => load!(Mode::Zpx, 4, lda),
            0xAD => load!(Mode::Abs, 4, lda), 0xBD => load!(Mode::Abx, 4, lda), 0xB9 => load!(Mode::Aby, 4, lda),
            0xA1 => load!(Mode::Izx, 6, lda), 0xB1 => load!(Mode::Izy, 5, lda),
            0xA2 => load!(Mode::Imm, 2, ldx), 0xA6 => load!(Mode::Zp, 3, ldx), 0xB6 => load!(Mode::Zpy, 4, ldx),
            0xAE => load!(Mode::Abs, 4, ldx), 0xBE => load!(Mode::Aby, 4, ldx),
            0xA0 => load!(Mode::Imm, 2, ldy), 0xA4 => load!(Mode::Zp, 3, ldy), 0xB4 => load!(Mode::Zpx, 4, ldy),
            0xAC => load!(Mode::Abs, 4, ldy), 0xBC => load!(Mode::Abx, 4, ldy),
            0x85 => store!(Mode::Zp, 3, sta), 0x95 => store!(Mode::Zpx, 4, sta), 0x8D => store!(Mode::Abs, 4, sta),
            0x9D => store!(Mode::Abx, 5, sta), 0x99 => store!(Mode::Aby, 5, sta), 0x81 => store!(Mode::Izx, 6, sta),
            0x91 => store!(Mode::Izy, 6, sta),
            0x86 => store!(Mode::Zp, 3, stx), 0x96 => store!(Mode::Zpy, 4, stx), 0x8E => store!(Mode::Abs, 4, stx),
            0x84 => store!(Mode::Zp, 3, sty), 0x94 => store!(Mode::Zpx, 4, sty), 0x8C => store!(Mode::Abs, 4, sty),
            // --- transfers ---
            0xAA => { self.x = self.a; self.set_zn(self.x); 2 }
            0xA8 => { self.y = self.a; self.set_zn(self.y); 2 }
            0x8A => { self.a = self.x; self.set_zn(self.a); 2 }
            0x98 => { self.a = self.y; self.set_zn(self.a); 2 }
            0xBA => { self.x = self.sp; self.set_zn(self.x); 2 }
            0x9A => { self.sp = self.x; 2 }
            // --- stack ---
            0x48 => { let a = self.a; self.push(bus, a); 3 }
            0x68 => { let v = self.pop(bus); self.a = v; self.set_zn(v); 4 }
            0x08 => { let p = self.p | B | U; self.push(bus, p); 3 }
            0x28 => { let v = self.pop(bus); self.p = (v | U) & !B; 4 }
            // --- arithmetic / logic ---
            0x69 => load!(Mode::Imm, 2, adc), 0x65 => load!(Mode::Zp, 3, adc), 0x75 => load!(Mode::Zpx, 4, adc),
            0x6D => load!(Mode::Abs, 4, adc), 0x7D => load!(Mode::Abx, 4, adc), 0x79 => load!(Mode::Aby, 4, adc),
            0x61 => load!(Mode::Izx, 6, adc), 0x71 => load!(Mode::Izy, 5, adc),
            0xE9 => load!(Mode::Imm, 2, sbc), 0xE5 => load!(Mode::Zp, 3, sbc), 0xF5 => load!(Mode::Zpx, 4, sbc),
            0xED => load!(Mode::Abs, 4, sbc), 0xFD => load!(Mode::Abx, 4, sbc), 0xF9 => load!(Mode::Aby, 4, sbc),
            0xE1 => load!(Mode::Izx, 6, sbc), 0xF1 => load!(Mode::Izy, 5, sbc),
            0x29 => load!(Mode::Imm, 2, and), 0x25 => load!(Mode::Zp, 3, and), 0x35 => load!(Mode::Zpx, 4, and),
            0x2D => load!(Mode::Abs, 4, and), 0x3D => load!(Mode::Abx, 4, and), 0x39 => load!(Mode::Aby, 4, and),
            0x21 => load!(Mode::Izx, 6, and), 0x31 => load!(Mode::Izy, 5, and),
            0x09 => load!(Mode::Imm, 2, ora), 0x05 => load!(Mode::Zp, 3, ora), 0x15 => load!(Mode::Zpx, 4, ora),
            0x0D => load!(Mode::Abs, 4, ora), 0x1D => load!(Mode::Abx, 4, ora), 0x19 => load!(Mode::Aby, 4, ora),
            0x01 => load!(Mode::Izx, 6, ora), 0x11 => load!(Mode::Izy, 5, ora),
            0x49 => load!(Mode::Imm, 2, eor), 0x45 => load!(Mode::Zp, 3, eor), 0x55 => load!(Mode::Zpx, 4, eor),
            0x4D => load!(Mode::Abs, 4, eor), 0x5D => load!(Mode::Abx, 4, eor), 0x59 => load!(Mode::Aby, 4, eor),
            0x41 => load!(Mode::Izx, 6, eor), 0x51 => load!(Mode::Izy, 5, eor),
            0xC9 => load!(Mode::Imm, 2, cmpa), 0xC5 => load!(Mode::Zp, 3, cmpa), 0xD5 => load!(Mode::Zpx, 4, cmpa),
            0xCD => load!(Mode::Abs, 4, cmpa), 0xDD => load!(Mode::Abx, 4, cmpa), 0xD9 => load!(Mode::Aby, 4, cmpa),
            0xC1 => load!(Mode::Izx, 6, cmpa), 0xD1 => load!(Mode::Izy, 5, cmpa),
            0xE0 => load!(Mode::Imm, 2, cpx), 0xE4 => load!(Mode::Zp, 3, cpx), 0xEC => load!(Mode::Abs, 4, cpx),
            0xC0 => load!(Mode::Imm, 2, cpy), 0xC4 => load!(Mode::Zp, 3, cpy), 0xCC => load!(Mode::Abs, 4, cpy),
            0x24 => load!(Mode::Zp, 3, bit), 0x2C => load!(Mode::Abs, 4, bit),
            // --- shifts / inc / dec ---
            0x0A => rmw_acc!(asl), 0x06 => rmw!(Mode::Zp, 5, asl), 0x16 => rmw!(Mode::Zpx, 6, asl),
            0x0E => rmw!(Mode::Abs, 6, asl), 0x1E => rmw!(Mode::Abx, 7, asl),
            0x4A => rmw_acc!(lsr), 0x46 => rmw!(Mode::Zp, 5, lsr), 0x56 => rmw!(Mode::Zpx, 6, lsr),
            0x4E => rmw!(Mode::Abs, 6, lsr), 0x5E => rmw!(Mode::Abx, 7, lsr),
            0x2A => rmw_acc!(rol), 0x26 => rmw!(Mode::Zp, 5, rol), 0x36 => rmw!(Mode::Zpx, 6, rol),
            0x2E => rmw!(Mode::Abs, 6, rol), 0x3E => rmw!(Mode::Abx, 7, rol),
            0x6A => rmw_acc!(ror), 0x66 => rmw!(Mode::Zp, 5, ror), 0x76 => rmw!(Mode::Zpx, 6, ror),
            0x6E => rmw!(Mode::Abs, 6, ror), 0x7E => rmw!(Mode::Abx, 7, ror),
            0xE6 => rmw!(Mode::Zp, 5, inc), 0xF6 => rmw!(Mode::Zpx, 6, inc), 0xEE => rmw!(Mode::Abs, 6, inc),
            0xFE => rmw!(Mode::Abx, 7, inc),
            0xC6 => rmw!(Mode::Zp, 5, dec), 0xD6 => rmw!(Mode::Zpx, 6, dec), 0xCE => rmw!(Mode::Abs, 6, dec),
            0xDE => rmw!(Mode::Abx, 7, dec),
            0xE8 => { self.x = self.x.wrapping_add(1); self.set_zn(self.x); 2 }
            0xC8 => { self.y = self.y.wrapping_add(1); self.set_zn(self.y); 2 }
            0xCA => { self.x = self.x.wrapping_sub(1); self.set_zn(self.x); 2 }
            0x88 => { self.y = self.y.wrapping_sub(1); self.set_zn(self.y); 2 }
            // --- flags ---
            0x18 => { self.set_flag(C, false); 2 }
            0x38 => { self.set_flag(C, true); 2 }
            0x58 => { self.set_flag(I, false); 2 }
            0x78 => { self.set_flag(I, true); 2 }
            0xB8 => { self.set_flag(V, false); 2 }
            0xD8 => { self.set_flag(D, false); 2 }
            0xF8 => { self.set_flag(D, true); 2 }
            // --- branches ---
            0x10 => { let c = !self.flag(N); self.branch(bus, c) }
            0x30 => { let c = self.flag(N); self.branch(bus, c) }
            0x50 => { let c = !self.flag(V); self.branch(bus, c) }
            0x70 => { let c = self.flag(V); self.branch(bus, c) }
            0x90 => { let c = !self.flag(C); self.branch(bus, c) }
            0xB0 => { let c = self.flag(C); self.branch(bus, c) }
            0xD0 => { let c = !self.flag(Z); self.branch(bus, c) }
            0xF0 => { let c = self.flag(Z); self.branch(bus, c) }
            // --- jumps ---
            0x4C => { let (a, _) = self.operand(bus, Mode::Abs); self.pc = a; 3 }
            0x6C => { let (a, _) = self.operand(bus, Mode::Ind); self.pc = a; 5 }
            0x20 => {
                let target = self.fetch16(bus);
                let ret = self.pc.wrapping_sub(1);
                self.push(bus, (ret >> 8) as u8);
                self.push(bus, ret as u8);
                self.pc = target;
                6
            }
            0x60 => {
                let lo = self.pop(bus) as u16;
                let hi = self.pop(bus) as u16;
                self.pc = (lo | (hi << 8)).wrapping_add(1);
                6
            }
            0x40 => {
                let p = self.pop(bus);
                self.p = (p | U) & !B;
                let lo = self.pop(bus) as u16;
                let hi = self.pop(bus) as u16;
                self.pc = lo | (hi << 8);
                6
            }
            0x00 => {
                // BRK: push PC+2 and P, vector through $FFFE.
                self.pc = self.pc.wrapping_add(1);
                let pc = self.pc;
                self.push(bus, (pc >> 8) as u8);
                self.push(bus, pc as u8);
                let p = self.p | B | U;
                self.push(bus, p);
                self.set_flag(I, true);
                let lo = bus.read(0xFFFE) as u16;
                let hi = bus.read(0xFFFF) as u16;
                self.pc = lo | (hi << 8);
                7
            }
            0xEA => 2,
            // --- unofficial: treat as NOPs with the documented operand size ---
            0x1A | 0x3A | 0x5A | 0x7A | 0xDA | 0xFA => 2,
            0x80 | 0x82 | 0x89 | 0xC2 | 0xE2 => { self.pc = self.pc.wrapping_add(1); 2 }
            0x04 | 0x44 | 0x64 => { self.pc = self.pc.wrapping_add(1); 3 }
            0x14 | 0x34 | 0x54 | 0x74 | 0xD4 | 0xF4 => { self.pc = self.pc.wrapping_add(1); 4 }
            0x0C => { self.pc = self.pc.wrapping_add(2); 4 }
            0x1C | 0x3C | 0x5C | 0x7C | 0xDC | 0xFC => {
                let (_, cross) = self.operand(bus, Mode::Abx);
                4 + cross as u32
            }
            _ => 2, // remaining illegal opcodes: 1-byte NOP
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);
    impl Bus for Ram {
        fn read(&mut self, a: u16) -> u8 { self.0[a as usize] }
        fn write(&mut self, a: u16, v: u8) { self.0[a as usize] = v; }
    }

    #[test]
    fn an_nmi_vectors_through_fffa_and_can_be_returned_from() {
        let mut ram = Ram(vec![0; 0x10000]);
        ram.0[0xFFFA] = 0x34;
        ram.0[0xFFFB] = 0x12;
        ram.0[0x1234] = 0x40; // RTI
        let mut cpu = Cpu { pc: 0xC0DE, sp: 0xFF, p: U | C, ..Default::default() };
        assert_eq!(cpu.nmi(&mut ram), 7);
        assert_eq!(cpu.pc, 0x1234, "vectored through $FFFA");
        assert!(cpu.p & I != 0, "and masked further interrupts");
        // The pushed status has B clear: that bit is how an RTI handler tells
        // a software BRK from a hardware interrupt, and a game that checks it
        // takes the wrong branch if this is got wrong.
        assert_eq!(ram.0[0x01FD] & B, 0, "B clear on the stack");
        assert_eq!(ram.0[0x01FD] & U, U, "with the unused bit set");
        assert_eq!(ram.0[0x01FF], 0xC0, "return address high");
        assert_eq!(ram.0[0x01FE], 0xDE, "and low");
        // RTI puts everything back.
        cpu.step(&mut ram);
        assert_eq!(cpu.pc, 0xC0DE);
        assert_eq!(cpu.p & C, C, "the carry survived");
        assert_eq!(cpu.sp, 0xFF);
    }

    fn run(prog: &[u8]) -> (Cpu, Ram) {
        let mut ram = Ram(vec![0; 0x10000]);
        ram.0[0x8000..0x8000 + prog.len()].copy_from_slice(prog);
        let mut cpu = Cpu { pc: 0x8000, ..Default::default() };
        // run until BRK (0x00) at the end of the program
        while ram.0[cpu.pc as usize] != 0x00 {
            cpu.step(&mut ram);
        }
        (cpu, ram)
    }

    #[test]
    fn adc_sets_carry_and_overflow() {
        // LDA #$7F ; ADC #$01 -> $80, V=1, C=0, N=1
        let (cpu, _) = run(&[0xA9, 0x7F, 0x69, 0x01]);
        assert_eq!(cpu.a, 0x80);
        assert!(cpu.flag(V));
        assert!(!cpu.flag(C));
        assert!(cpu.flag(N));
        // LDA #$FF ; CLC ; ADC #$01 -> $00, C=1, Z=1
        let (cpu, _) = run(&[0xA9, 0xFF, 0x18, 0x69, 0x01]);
        assert_eq!(cpu.a, 0);
        assert!(cpu.flag(C));
        assert!(cpu.flag(Z));
    }

    #[test]
    fn sbc_borrow_semantics() {
        // SEC ; LDA #$05 ; SBC #$03 -> 2, C=1
        let (cpu, _) = run(&[0x38, 0xA9, 0x05, 0xE9, 0x03]);
        assert_eq!(cpu.a, 2);
        assert!(cpu.flag(C));
        // SEC ; LDA #$03 ; SBC #$05 -> $FE, C=0
        let (cpu, _) = run(&[0x38, 0xA9, 0x03, 0xE9, 0x05]);
        assert_eq!(cpu.a, 0xFE);
        assert!(!cpu.flag(C));
    }

    #[test]
    fn jsr_rts_and_indexed_stores() {
        // JSR sub ; BRK ... sub: LDX #3 ; loop: STA $0300,X ; DEX ; BPL loop ; RTS
        let prog = [
            0x20, 0x05, 0x80, // JSR $8005
            0x00, 0x00,
            0xA9, 0x42,       // LDA #$42
            0xA2, 0x03,       // LDX #3
            0x9D, 0x00, 0x03, // STA $0300,X
            0xCA,             // DEX
            0x10, 0xFA,       // BPL -6
            0x60,             // RTS
        ];
        let (cpu, ram) = run(&prog);
        assert_eq!(cpu.pc, 0x8003);
        assert_eq!(&ram.0[0x300..0x304], &[0x42; 4]);
        assert_eq!(cpu.sp, 0xFD);
    }

    #[test]
    fn indirect_y_and_ror_carry_chain() {
        // ptr at $10 -> $0300. LDA #$81 ; STA ($10),Y with Y=1 ; then ROR $0301
        let mut prog = vec![0xA9, 0x00, 0x85, 0x10, 0xA9, 0x03, 0x85, 0x11];
        prog.extend_from_slice(&[0xA0, 0x01, 0xA9, 0x81, 0x91, 0x10, 0x18, 0x6E, 0x01, 0x03]);
        let (cpu, ram) = run(&prog);
        assert_eq!(ram.0[0x301], 0x40);
        assert!(cpu.flag(C));
    }
}
