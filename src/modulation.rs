//! Stage 12: modulation bindings. A tiny expression language ties audio
//! sources (per-voice env / pitch / gate / vel, master rms, transport
//! counters) to sprite-placement properties (x, y, scale, flipx, flipy,
//! frame, visible). Bindings are parsed from `:bind` commands and
//! evaluated every UI tick to produce a derived `EffectivePlacement` list
//! that the viz pane renders.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};

use crate::audio::VizFrame;
use crate::sprite::Placement;
use crate::CHANNELS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    X,
    Y,
    Scale,
    FlipX,
    FlipY,
    Frame,
    Visible,
    Rotate,
    Hue,
    Saturation,
    Value,
    Palette,
}

impl Target {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "x" => Some(Self::X),
            "y" => Some(Self::Y),
            "scale" => Some(Self::Scale),
            "flipx" => Some(Self::FlipX),
            "flipy" => Some(Self::FlipY),
            "frame" => Some(Self::Frame),
            "visible" | "show" => Some(Self::Visible),
            "rotate" | "rot" => Some(Self::Rotate),
            "hue" => Some(Self::Hue),
            "saturation" | "sat" => Some(Self::Saturation),
            "value" | "val" | "brightness" => Some(Self::Value),
            "palette" | "pal" => Some(Self::Palette),
            _ => None,
        }
    }
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::X => "x",
            Self::Y => "y",
            Self::Scale => "scale",
            Self::FlipX => "flipx",
            Self::FlipY => "flipy",
            Self::Frame => "frame",
            Self::Visible => "visible",
            Self::Rotate => "rotate",
            Self::Hue => "hue",
            Self::Saturation => "saturation",
            Self::Value => "value",
            Self::Palette => "palette",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum VoiceField {
    Env,
    Pitch,
    Gate,
    Vel,
    Age,
}

#[derive(Clone, Copy, Debug)]
enum Source {
    Voice(usize, VoiceField),
    MasterRms,
    Step,
    StepPhase,
    Beat,
    Bar,
    SceneIndex,
    Phrase,
    Tempo,
    Time,
    Playing,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug)]
enum Func {
    Abs,
    Sin,
    Cos,
    Floor,
    Min,
    Max,
    Clamp,
}

impl Func {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "abs" => Some(Self::Abs),
            "sin" => Some(Self::Sin),
            "cos" => Some(Self::Cos),
            "floor" => Some(Self::Floor),
            "min" => Some(Self::Min),
            "max" => Some(Self::Max),
            "clamp" => Some(Self::Clamp),
            _ => None,
        }
    }
    fn arity(&self) -> usize {
        match self {
            Self::Abs | Self::Sin | Self::Cos | Self::Floor => 1,
            Self::Min | Self::Max => 2,
            Self::Clamp => 3,
        }
    }
}

#[derive(Clone, Debug)]
enum Expr {
    Lit(f32),
    Src(Source),
    Neg(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum IdxPat {
    Exact(u32),
    Wildcard,
}

#[derive(Clone, Debug)]
pub(crate) struct Binding {
    pub sheet: String,
    pub idx: IdxPat,
    pub target: Target,
    expr: Expr,
    /// Original source text of the expression, for `:bind list`.
    pub expr_src: String,
}

impl Binding {
    pub(crate) fn addr(&self) -> String {
        match self.idx {
            IdxPat::Exact(n) => format!("{}.{}", self.sheet, n),
            IdxPat::Wildcard => format!("{}.*", self.sheet),
        }
    }
}

pub(crate) struct EvalCtx<'a> {
    pub frame: &'a VizFrame,
    pub tempo: f32,
    pub scene_index: i32,
    pub phrase: i32,
    pub time_s: f32,
    /// Seconds since the last note-on edge per channel. Very large when
    /// no note has fired yet, so `<ch>.age < 0.5`-style thresholds behave
    /// correctly on startup.
    pub voice_ages: [f32; crate::CHANNELS],
}

fn eval(expr: &Expr, ctx: &EvalCtx) -> f32 {
    match expr {
        Expr::Lit(v) => *v,
        Expr::Neg(e) => -eval(e, ctx),
        Expr::Bin(op, a, b) => {
            let x = eval(a, ctx);
            let y = eval(b, ctx);
            match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => {
                    if y == 0.0 { 0.0 } else { x / y }
                }
                Op::Mod => {
                    if y == 0.0 { 0.0 } else { x.rem_euclid(y) }
                }
            }
        }
        Expr::Call(f, args) => match f {
            Func::Abs => eval(&args[0], ctx).abs(),
            Func::Sin => eval(&args[0], ctx).sin(),
            Func::Cos => eval(&args[0], ctx).cos(),
            Func::Floor => eval(&args[0], ctx).floor(),
            Func::Min => eval(&args[0], ctx).min(eval(&args[1], ctx)),
            Func::Max => eval(&args[0], ctx).max(eval(&args[1], ctx)),
            Func::Clamp => {
                let v = eval(&args[0], ctx);
                let lo = eval(&args[1], ctx);
                let hi = eval(&args[2], ctx);
                v.clamp(lo, hi)
            }
        },
        Expr::Src(s) => {
            let v = &ctx.frame.voices;
            match s {
                Source::Voice(ch, f) => match f {
                    VoiceField::Env => v[*ch].env_level,
                    VoiceField::Pitch => v[*ch].freq,
                    VoiceField::Gate => {
                        if v[*ch].gate { 1.0 } else { 0.0 }
                    }
                    VoiceField::Vel => v[*ch].vel,
                    VoiceField::Age => ctx.voice_ages[*ch],
                },
                Source::MasterRms => {
                    let mut sum = 0.0_f32;
                    for vi in v {
                        let g = vi.env_level * vi.vel;
                        sum += g * g;
                    }
                    (sum / CHANNELS as f32).sqrt()
                }
                Source::Step => ctx.frame.step as f32,
                Source::StepPhase => ctx.frame.step_phase,
                Source::Beat => {
                    (ctx.frame.step as f32 + ctx.frame.step_phase) / 4.0
                }
                Source::Bar => {
                    (ctx.frame.step as f32 + ctx.frame.step_phase) / 16.0
                }
                Source::SceneIndex => ctx.scene_index as f32,
                Source::Phrase => ctx.phrase as f32,
                Source::Tempo => ctx.tempo,
                Source::Time => ctx.time_s,
                Source::Playing => {
                    if ctx.frame.playing { 1.0 } else { 0.0 }
                }
            }
        }
    }
}

/// An original `Placement` with modulation-derived overrides folded in.
/// Produced by `apply_bindings` and consumed by `viz::render_sprites`.
#[derive(Clone, Debug)]
pub(crate) struct EffectivePlacement {
    pub sheet: String,
    pub idx: u32,
    pub x: i32,
    pub y: i32,
    pub flipx: bool,
    pub flipy: bool,
    pub scale: f32,
    pub visible: bool,
    pub palette: Option<String>,
    /// Rotation in degrees, 0 = no rotation. Rendered by nearest-neighbor
    /// inverse-mapping around the sprite center.
    pub rotate: f32,
    /// Hue shift in degrees added to each pixel's hue (HSV space).
    pub hue_shift: f32,
    /// Saturation multiplier applied in HSV space. 1.0 = identity.
    pub saturation: f32,
    /// Value (brightness) multiplier applied in HSV space. 1.0 = identity.
    pub value: f32,
    /// When Some, resolved to an index into the sorted list of named
    /// palettes post-binding; overrides `palette` for this frame.
    pub palette_idx: Option<i32>,
}

/// Resolve `palette_idx` bindings to a concrete palette name from the
/// alphabetically-sorted list. `:bind mario palette = scene.index` picks
/// the Nth named palette (wrapping). No-op when there are no named
/// palettes or no placement has a `palette_idx` override.
pub(crate) fn resolve_palette_indices(
    placements: &mut [EffectivePlacement],
    sorted_palette_names: &[String],
) {
    if sorted_palette_names.is_empty() { return; }
    let n = sorted_palette_names.len() as i32;
    for p in placements {
        if let Some(idx) = p.palette_idx {
            let i = idx.rem_euclid(n) as usize;
            p.palette = Some(sorted_palette_names[i].clone());
        }
    }
}

pub(crate) fn apply_bindings(
    placements: &[Placement],
    bindings: &[Binding],
    ctx: &EvalCtx,
) -> Vec<EffectivePlacement> {
    let mut per_sheet_ix: HashMap<&str, u32> = HashMap::new();
    let mut out: Vec<EffectivePlacement> = Vec::with_capacity(placements.len());
    for p in placements {
        let counter = per_sheet_ix.entry(p.sheet.as_str()).or_insert(0);
        let my_idx = *counter;
        *counter += 1;

        let mut eff = EffectivePlacement {
            sheet: p.sheet.clone(),
            idx: p.idx,
            x: p.x,
            y: p.y,
            flipx: false,
            flipy: false,
            scale: 1.0,
            visible: true,
            palette: p.palette.clone(),
            rotate: 0.0,
            hue_shift: 0.0,
            saturation: 1.0,
            value: 1.0,
            palette_idx: None,
        };

        for b in bindings {
            if b.sheet != p.sheet { continue; }
            match b.idx {
                IdxPat::Wildcard => {}
                IdxPat::Exact(n) => {
                    if n != my_idx { continue; }
                }
            }
            let v = eval(&b.expr, ctx);
            match b.target {
                // x/y are additive offsets on top of the placed base coord —
                // keeps `:sprite place` as the home position and bindings as
                // nudges.
                Target::X => eff.x = p.x + v.round() as i32,
                Target::Y => eff.y = p.y + v.round() as i32,
                Target::Scale => eff.scale = v.max(0.01),
                Target::FlipX => eff.flipx = v != 0.0,
                Target::FlipY => eff.flipy = v != 0.0,
                Target::Frame => {
                    let n = v.floor().max(0.0) as u32;
                    eff.idx = n;
                }
                Target::Visible => eff.visible = v != 0.0,
                Target::Rotate => eff.rotate = v,
                Target::Hue => eff.hue_shift = v,
                Target::Saturation => eff.saturation = v.max(0.0),
                Target::Value => eff.value = v.max(0.0),
                Target::Palette => eff.palette_idx = Some(v.floor() as i32),
            }
        }
        out.push(eff);
    }
    out
}

// ---------- Parser ----------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f32),
    Ident(String),
    Punct(char),
    End,
}

fn tokenize(s: &str) -> Result<Vec<Tok>> {
    let mut toks = Vec::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_whitespace() { i += 1; continue; }
        if c.is_ascii_digit() || (c == '.' && bytes.get(i + 1).map_or(false, |d| d.is_ascii_digit())) {
            let mut buf = String::new();
            let mut saw_dot = false;
            while i < bytes.len() {
                let d = bytes[i];
                if d.is_ascii_digit() { buf.push(d); i += 1; }
                else if d == '.' && !saw_dot { saw_dot = true; buf.push(d); i += 1; }
                else { break; }
            }
            let n: f32 = buf.parse().map_err(|_| anyhow!("bad number '{}'", buf))?;
            toks.push(Tok::Num(n));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let mut buf = String::new();
            while i < bytes.len() {
                let d = bytes[i];
                if d.is_ascii_alphanumeric() || d == '_' || d == '.' {
                    buf.push(d); i += 1;
                } else { break; }
            }
            toks.push(Tok::Ident(buf));
            continue;
        }
        if "+-*/%(),".contains(c) {
            toks.push(Tok::Punct(c));
            i += 1;
            continue;
        }
        bail!("unexpected '{}' in expression", c);
    }
    toks.push(Tok::End);
    Ok(toks)
}

struct Parser {
    toks: Vec<Tok>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> &Tok { &self.toks[self.i] }
    fn eat(&mut self) -> Tok { let t = self.toks[self.i].clone(); self.i += 1; t }

    fn parse_expr(&mut self) -> Result<Expr> { self.parse_add() }

    fn parse_add(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Tok::Punct('+') => Op::Add,
                Tok::Punct('-') => Op::Sub,
                _ => break,
            };
            self.eat();
            let rhs = self.parse_mul()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Punct('*') => Op::Mul,
                Tok::Punct('/') => Op::Div,
                Tok::Punct('%') => Op::Mod,
                _ => break,
            };
            self.eat();
            let rhs = self.parse_unary()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Tok::Punct('-')) {
            self.eat();
            let inner = self.parse_unary()?;
            return Ok(Expr::Neg(Box::new(inner)));
        }
        if matches!(self.peek(), Tok::Punct('+')) {
            self.eat();
            return self.parse_unary();
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        match self.eat() {
            Tok::Num(v) => Ok(Expr::Lit(v)),
            Tok::Ident(name) => {
                if matches!(self.peek(), Tok::Punct('(')) {
                    self.eat();
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Tok::Punct(')')) {
                        args.push(self.parse_expr()?);
                        while matches!(self.peek(), Tok::Punct(',')) {
                            self.eat();
                            args.push(self.parse_expr()?);
                        }
                    }
                    match self.eat() {
                        Tok::Punct(')') => {}
                        t => bail!("expected ')', got {:?}", t),
                    }
                    let f = Func::parse(&name)
                        .ok_or_else(|| anyhow!("unknown function '{}'", name))?;
                    if args.len() != f.arity() {
                        bail!("{} takes {} args, got {}", name, f.arity(), args.len());
                    }
                    Ok(Expr::Call(f, args))
                } else {
                    Ok(Expr::Src(parse_source(&name)?))
                }
            }
            Tok::Punct('(') => {
                let e = self.parse_expr()?;
                match self.eat() {
                    Tok::Punct(')') => Ok(e),
                    t => bail!("expected ')', got {:?}", t),
                }
            }
            t => bail!("unexpected token {:?}", t),
        }
    }
}

fn parse_source(name: &str) -> Result<Source> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "master.rms" | "rms" => return Ok(Source::MasterRms),
        "step" => return Ok(Source::Step),
        "step_phase" | "phase" => return Ok(Source::StepPhase),
        "beat" => return Ok(Source::Beat),
        "bar" => return Ok(Source::Bar),
        "scene.index" | "scene" => return Ok(Source::SceneIndex),
        "phrase" => return Ok(Source::Phrase),
        "tempo" | "bpm" => return Ok(Source::Tempo),
        "time" | "t" => return Ok(Source::Time),
        "playing" => return Ok(Source::Playing),
        _ => {}
    }
    if let Some((ch_name, field)) = lower.split_once('.') {
        let ch = match ch_name {
            "pu1" => 0,
            "pu2" => 1,
            "tri" => 2,
            "noi" => 3,
            _ => bail!("unknown source '{}'", name),
        };
        let f = match field {
            "env" => VoiceField::Env,
            "pitch" | "freq" => VoiceField::Pitch,
            "gate" => VoiceField::Gate,
            "vel" => VoiceField::Vel,
            "age" => VoiceField::Age,
            _ => bail!("unknown field '{}' on voice '{}'", field, ch_name),
        };
        return Ok(Source::Voice(ch, f));
    }
    bail!("unknown identifier '{}'", name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{VizFrame, VoiceFrame};

    fn ctx_with_voices(voices: [VoiceFrame; 4]) -> (VizFrame, ()) {
        (VizFrame { playing: true, step: 3, step_phase: 0.5, voices }, ())
    }

    fn run(expr_src: &str, frame: &VizFrame) -> f32 {
        let toks = tokenize(expr_src).unwrap();
        let mut p = Parser { toks, i: 0 };
        let expr = p.parse_expr().unwrap();
        let ctx = EvalCtx {
            frame,
            tempo: 120.0,
            scene_index: 2,
            phrase: 2,
            time_s: 1.0,
            voice_ages: [0.25, 0.5, 1.0, 10.0],
        };
        eval(&expr, &ctx)
    }

    #[test]
    fn arithmetic() {
        let (frame, _) = ctx_with_voices([VoiceFrame::default(); 4]);
        assert_eq!(run("1 + 2 * 3", &frame), 7.0);
        assert_eq!(run("(1 + 2) * 3", &frame), 9.0);
        assert_eq!(run("10 % 3", &frame), 1.0);
        assert_eq!(run("-5 + 2", &frame), -3.0);
        assert_eq!(run("abs(-4)", &frame), 4.0);
        assert_eq!(run("min(3, 5)", &frame), 3.0);
        assert_eq!(run("clamp(10, 0, 5)", &frame), 5.0);
    }

    #[test]
    fn sources() {
        let voices = [
            VoiceFrame { gate: true,  env_level: 0.8, freq: 440.0, vel: 1.0 },
            VoiceFrame { gate: false, env_level: 0.0, freq: 0.0,   vel: 0.0 },
            VoiceFrame { gate: true,  env_level: 0.5, freq: 220.0, vel: 0.5 },
            VoiceFrame { gate: false, env_level: 0.0, freq: 0.0,   vel: 0.0 },
        ];
        let (frame, _) = ctx_with_voices(voices);
        assert!((run("pu1.env", &frame) - 0.8).abs() < 1e-6);
        assert_eq!(run("pu2.gate", &frame), 0.0);
        assert_eq!(run("tri.gate", &frame), 1.0);
        assert_eq!(run("pu1.pitch", &frame), 440.0);
        assert_eq!(run("step", &frame), 3.0);
        assert!((run("step_phase", &frame) - 0.5).abs() < 1e-6);
        assert_eq!(run("tempo", &frame), 120.0);
        assert_eq!(run("scene.index", &frame), 2.0);
        assert_eq!(run("playing", &frame), 1.0);
    }

    #[test]
    fn binding_parse_and_apply() {
        let b = parse_binding("mario.0 scale = tri.env * 2 + 1").unwrap();
        assert_eq!(b.sheet, "mario");
        assert!(matches!(b.idx, IdxPat::Exact(0)));
        assert_eq!(b.target, Target::Scale);
        assert_eq!(b.expr_src, "tri.env * 2 + 1");

        let wild = parse_binding("bg.* y = sin(time)").unwrap();
        assert!(matches!(wild.idx, IdxPat::Wildcard));

        // Bare sheet name is sugar for `.*`.
        let bare = parse_binding("bub y = sin(time)").unwrap();
        assert_eq!(bare.sheet, "bub");
        assert!(matches!(bare.idx, IdxPat::Wildcard));

        // Apply: one placement, scale binding → scale = 0.5*2+1 = 2
        let placements = vec![Placement {
            sheet: "mario".into(), idx: 0, x: 10, y: 20, palette: None,
        }];
        let voices = [
            VoiceFrame::default(),
            VoiceFrame::default(),
            VoiceFrame { gate: true, env_level: 0.5, freq: 0.0, vel: 1.0 },
            VoiceFrame::default(),
        ];
        let frame = VizFrame { playing: false, step: 0, step_phase: 0.0, voices };
        let ctx = EvalCtx {
            frame: &frame, tempo: 120.0, scene_index: 0, phrase: 0,
            time_s: 0.0, voice_ages: [0.0; 4],
        };
        let eff = apply_bindings(&placements, &[b], &ctx);
        assert_eq!(eff.len(), 1);
        assert!((eff[0].scale - 2.0).abs() < 1e-6);
        assert_eq!(eff[0].x, 10);
    }

    #[test]
    fn voice_age() {
        let (frame, _) = ctx_with_voices([VoiceFrame::default(); 4]);
        // voice_ages in `run` ctx = [0.25, 0.5, 1.0, 10.0] → pu1/pu2/tri/noi.
        assert!((run("pu1.age", &frame) - 0.25).abs() < 1e-6);
        assert!((run("noi.age", &frame) - 10.0).abs() < 1e-6);
        // Classic "one-shot animation after NOI hits" frame expression.
        assert_eq!(run("clamp(floor(noi.age * 16), 0, 3)", &frame), 3.0);
        assert_eq!(run("clamp(floor(pu1.age * 16), 0, 3)", &frame), 3.0); // 0.25*16=4, clamped to 3
    }

    #[test]
    fn error_messages() {
        assert!(parse_binding("no equals here").is_err());
        assert!(parse_binding("mario.0 badtarget = 1").is_err());
        assert!(parse_binding("mario.0 x = 1 +").is_err());
        assert!(parse_binding("mario.0 x = unknown.src").is_err());
        assert!(parse_binding("mario.0 x = min(1)").is_err()); // wrong arity
    }
}

/// Parse a full binding spec: `<sheet>.<idx|*> <target> = <expr>`.
pub(crate) fn parse_binding(spec: &str) -> Result<Binding> {
    let (lhs, rhs) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("expected '=' between target and expression"))?;
    let rhs_trim = rhs.trim();
    if rhs_trim.is_empty() {
        bail!("expression is empty");
    }
    let mut parts = lhs.split_whitespace();
    let addr = parts.next().ok_or_else(|| anyhow!("expected <sheet>.<idx|*>"))?;
    let target_s = parts
        .next()
        .ok_or_else(|| anyhow!("expected target (x, y, scale, flipx, flipy, frame, visible)"))?;
    if parts.next().is_some() {
        bail!("too many tokens before '='");
    }
    // Address forms: `<sheet>` (alias for `<sheet>.*`), `<sheet>.N`, `<sheet>.*`.
    // Bare sheet name is the common case when there's only one placement.
    let (sheet, idx): (&str, IdxPat) = match addr.rsplit_once('.') {
        Some((s, "*")) => (s, IdxPat::Wildcard),
        Some((s, num)) => (s, IdxPat::Exact(
            num.parse::<u32>()
                .map_err(|_| anyhow!("bad index '{}' (want integer or '*')", num))?,
        )),
        None => (addr, IdxPat::Wildcard),
    };
    let target = Target::parse(target_s)
        .ok_or_else(|| anyhow!("unknown target '{}'", target_s))?;
    let toks = tokenize(rhs_trim)?;
    let mut p = Parser { toks, i: 0 };
    let expr = p.parse_expr()?;
    if !matches!(p.peek(), Tok::End) {
        bail!("trailing tokens after expression");
    }
    Ok(Binding {
        sheet: sheet.to_string(),
        idx,
        target,
        expr,
        expr_src: rhs_trim.to_string(),
    })
}
