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
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum VoiceField {
    Env,
    Pitch,
    Gate,
    Vel,
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
            _ => bail!("unknown field '{}' on voice '{}'", field, ch_name),
        };
        return Ok(Source::Voice(ch, f));
    }
    bail!("unknown identifier '{}'", name)
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
    let (sheet, idx_str) = addr
        .rsplit_once('.')
        .ok_or_else(|| anyhow!("address must be <sheet>.<idx|*>"))?;
    let idx = match idx_str {
        "*" => IdxPat::Wildcard,
        s => IdxPat::Exact(
            s.parse::<u32>()
                .map_err(|_| anyhow!("bad index '{}' (want integer or '*')", s))?,
        ),
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
