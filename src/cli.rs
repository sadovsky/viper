//! Headless subcommands: `check`, `compile`, `render`, `info`.
//!
//! ```text
//! viper check   song.vip
//! viper compile song.vip --driver driver.bin [--sym driver.sym] -o song.nsf [--title T]
//! viper render  song.nsf [-o mix.wav] [--stems DIR] [--triggers drums.mid]
//!               [--log writes.txt] [--loops N | --frames N] [--song N]
//!               [--rate HZ] [--bpm B]
//! viper info    song.nsf
//! viper gen     --style DIR --seed N [-o songs/] [--count N] [--key K] [--bpm B]
//! ```

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::{compile, vip};

const USAGE: &str = "\
viper — vim-keybound chiptune tracker + NSF compiler

usage:
  viper [song.vip]                          open the tracker
  viper check song.vip                      parse and report problems
  viper compile song.vip [more.vip ...] --driver BIN [--sym SYM] -o out.nsf [--title T]
                [--nsfe out.nsfe]
      several .vip files become one multi-song NSF (album bundle); an .nsfe
      output (by extension or --nsfe) carries per-track titles and times
  viper fmt song.vip [-o out.vip]           rewrite in canonical form
  viper render song.nsf [-o mix.wav] [--stems DIR] [--triggers drums.mid]
                        [--log writes.txt] [--loops N] [--frames N]
                        [--vip song.vip] [--song N] [--rate HZ] [--bpm B]
                        [--tail SEC]
      --vip takes the exact intro/loop length from the source; without it
      the loop is detected from the driver's RAM state, which can overshoot
      by up to one pass.
  viper info song.nsf
  viper dpcm encode in.wav -o out.dmc [--rate 15] [--gain 1.0] [--level 64]
                   [--mode trellis|greedy] [--hp 20] [--trim -60|off] [--max-bytes 4081]
  viper dpcm decode in.dmc -o out.wav [--rate 15] [--level 64]   # through the DMC model
  viper dpcm info a.dmc [b.dmc ...] [--rate 15]                  # sizes, levels, drift, budget
  viper dpcm synth kick|snare|hat|tom -o out.wav [--rate 15]     # built-in recipes as WAV
  viper import [song.mid] --map map.vmap [-o song.vip]            # MIDI → .vip (see docs/FORMAT.md)
  viper verify song.nsf|writes.log --against other.log [--vip song.vip]
                [--loops N | --frames N] [--song N] [-o normalized.log]
      diff viper's register-write log against another emulator's dump
      (NSFPlay, Mesen, FCEUX Lua — any `frame addr value` shape); exits 1
      on the first PLAY frame that differs. -o writes the other log in
      viper's canonical form.
  viper gen --style DIR [--seed N] [--count N] [-o DIR|FILE] [--key E] [--bpm 200]
            [--scale NAME] [--motif on|off] [--form N] [--driver BIN --sym SYM]
            [--artist NAME] [--prefix NAME]
      one song per seed (seed..seed+count-1) written as .vip; prints a
      one-line report per song
";

struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(args: &[String]) -> Self {
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(name) = a.strip_prefix("--") {
                let takes_value = !matches!(name, "help" | "stems-only");
                if takes_value && i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    flags.push((name.to_string(), Some(args[i + 1].clone())));
                    i += 2;
                    continue;
                }
                flags.push((name.to_string(), None));
            } else if a == "-o" {
                flags.push(("out".to_string(), args.get(i + 1).cloned()));
                i += 2;
                continue;
            } else {
                positional.push(a.clone());
            }
            i += 1;
        }
        Self { positional, flags }
    }
    fn get(&self, name: &str) -> Option<&str> {
        self.flags.iter().rev().find(|(n, _)| n == name).and_then(|(_, v)| v.as_deref())
    }
    fn num<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>> {
        self.get(name).map(|v| v.parse::<T>().map_err(|_| anyhow!("--{} expects a number, got {:?}", name, v))).transpose()
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let cmd = args[0].as_str();
    let a = Args::parse(&args[1..]);
    match cmd {
        "check" => check(&a),
        "dpcm" => dpcm_cmd(&a),
        "import" => import_cmd(&a),
        "gen" => gen(&a),
        "fmt" => fmt(&a),
        "compile" => compile_cmd(&a),
        "render" => render(&a),
        "info" => info(&a),
        "verify" => verify(&a),
        _ => {
            print!("{}", USAGE);
            Ok(())
        }
    }
}

fn load_song(path: &Path) -> Result<(crate::Song, Vec<String>)> {
    vip::load(path)
}

fn check(a: &Args) -> Result<()> {
    let path = a.positional.first().map(PathBuf::from).context("check: need a .vip path")?;
    let (song, warnings) = load_song(&path)?;
    for w in &warnings {
        println!("warning: {}", w);
    }
    let lowered = compile::lower(&song, path.parent())?;
    for w in &lowered.warnings {
        println!("warning: {}", w);
    }
    let s = &lowered.module.songs[0];
    println!(
        "{}: {} phrases, order {} entries, {} BPM ({:.2} frames/row), {} frames ≈ {:.1}s",
        path.display(),
        song.phrases.len(),
        s.order.len(),
        song.bpm,
        s.frames_per_row,
        s.total_frames(),
        s.total_frames() as f64 / 60.0988
    );
    if warnings.is_empty() && lowered.warnings.is_empty() {
        println!("ok");
    }
    Ok(())
}

fn gen(a: &Args) -> Result<()> {
    let style_dir = a.get("style").map(PathBuf::from).context("gen: need --style DIR")?;
    let style = crate::style::Style::load(&style_dir)?;
    let seed0: u64 = a.num::<u64>("seed")?.unwrap_or(1);
    let count: u64 = a.num::<u64>("count")?.unwrap_or(1).max(1);
    let out = a.get("out").map(PathBuf::from);
    let prefix = a.get("prefix").unwrap_or("gen").to_string();
    let key = match a.get("key") {
        Some(k) => Some(crate::gen::parse_key(k).ok_or_else(|| anyhow!("bad --key {:?}", k))?),
        None => None,
    };
    let driver = a.get("driver").map(|bin| {
        let bin = PathBuf::from(bin);
        let sym = a.get("sym").map(PathBuf::from).unwrap_or_else(|| bin.with_extension("sym"));
        (bin, sym)
    });
    for i in 0..count {
        let seed = seed0 + i;
        let params = crate::style::GenParams {
            seed,
            key,
            bpm: a.num::<u16>("bpm")?,
            scale: a.get("scale").map(String::from),
            motif: match a.get("motif") { Some("on") => Some(true), Some("off") => Some(false), _ => None },
            form: a.num::<usize>("form")?,
            driver: driver.clone(),
            artist: a.get("artist").unwrap_or("").to_string(),
        };
        let (song, info) = crate::style::generate_with_info(&style, &params)?;
        let r = crate::style::report(&song);
        let text = vip::to_vip(&song);
        let header = format!(
            "# generated by viper gen — style {} v{} seed {} key {} progression {} form {} motif {}\n# {} bars, {} unique phrases, lead density {:.2}, lead range {} semitones\n",
            style.name, style.version, seed, song.key_name, info.progression, info.form, if info.motif { "on" } else { "off" },
            r.bars, r.unique_phrases, r.lead_density, r.lead_range
        );
        let text = format!("{}{}", header, text);
        let path = match &out {
            Some(p) if p.is_dir() || count > 1 || p.extension().is_none() => {
                std::fs::create_dir_all(p)?;
                p.join(format!("{}_{:03}.vip", prefix, seed))
            }
            Some(p) => p.clone(),
            None => PathBuf::from(format!("{}_{:03}.vip", prefix, seed)),
        };
        std::fs::write(&path, &text).with_context(|| format!("write {}", path.display()))?;
        println!(
            "{}\t{}\tseed={}\tkey={} {}\tbpm={}\tbars={}\tphrases={}\tdensity={:.2}\trange={}\tdrums={}",
            path.display(), song.title, seed, crate::vip::key_name(info.key), info.scale, song.bpm, r.bars, r.unique_phrases, r.lead_density, r.lead_range, r.drum_hits
        );
    }
    Ok(())
}

/// `viper import song.mid --map map.vmap -o song.vip`
fn import_cmd(a: &Args) -> Result<()> {
    let map_path = a.get("map").map(PathBuf::from).context("import: need --map map.vmap")?;
    let map = crate::import::parse_map(&std::fs::read_to_string(&map_path).with_context(|| format!("read {}", map_path.display()))?)
        .with_context(|| format!("parsing {}", map_path.display()))?;
    let midi_path = match a.positional.first() {
        Some(p) => PathBuf::from(p),
        None => {
            let s = map.source.clone().context("import: need a .mid path, or @source midi=... in the map")?;
            match map_path.parent() {
                Some(d) if s.is_relative() => d.join(s),
                _ => s,
            }
        }
    };
    let out = a.get("out").map(PathBuf::from).unwrap_or_else(|| midi_path.with_extension("vip"));
    let midi = crate::import::parse_midi(&std::fs::read(&midi_path).with_context(|| format!("read {}", midi_path.display()))?)
        .with_context(|| format!("{}", midi_path.display()))?;
    let (song, report) = crate::import::import(&midi, &map)?;
    let header = format!("# imported by viper import from {} with {}\n# {}\n", midi_path.display(), map_path.display(), report.summary().replace('\n', "\n# "));
    std::fs::write(&out, format!("{}{}", header, vip::to_vip(&song))).with_context(|| format!("write {}", out.display()))?;
    println!("{}", report.summary());
    println!("→ {}", out.display());
    Ok(())
}

/// `viper dpcm encode|decode|info|synth` — the sample workbench.
fn dpcm_cmd(a: &Args) -> Result<()> {
    use crate::dpcm;
    let sub = a.positional.first().map(String::as_str).unwrap_or("");
    let rate: u8 = a.num::<u8>("rate")?.unwrap_or(15);
    if rate > 15 {
        bail!("--rate must be 0..=15");
    }
    let level: u8 = a.num::<u8>("level")?.unwrap_or(64).min(127);
    match sub {
        "encode" => {
            let input = a.positional.get(1).map(PathBuf::from).context("dpcm encode: need an input .wav")?;
            let out = a.get("out").map(PathBuf::from).unwrap_or_else(|| input.with_extension("dmc"));
            let (src_hz, wave) = viper_apu::wav::read_wav(&std::fs::read(&input).with_context(|| format!("read {}", input.display()))?)
                .with_context(|| format!("{}", input.display()))?;
            let opts = dpcm::EncodeOptions {
                rate,
                gain: a.num::<f32>("gain")?.unwrap_or(1.0),
                level,
                mode: match a.get("mode") {
                    None | Some("trellis") => dpcm::Mode::Trellis,
                    Some("greedy") => dpcm::Mode::Greedy,
                    Some(m) => bail!("--mode {}: want trellis or greedy", m),
                },
                hp_hz: a.num::<f32>("hp")?.unwrap_or(20.0),
                trim_db: match a.get("trim") {
                    Some("off") => None,
                    Some(v) => Some(v.parse::<f32>().map_err(|_| anyhow!("--trim wants dB or off"))?),
                    None => Some(-60.0),
                },
                max_bytes: a.num::<usize>("max-bytes")?.unwrap_or(4081),
            };
            let e = dpcm::encode(&wave, src_hz as f32, &opts)?;
            std::fs::write(&out, &e.data).with_context(|| format!("write {}", out.display()))?;
            let s = &e.stats;
            println!(
                "{} → {}: {} bytes ({:.0}% of 4081), {:.1} ms @ rate {} ({:.0} Hz), levels {}..{}, {} clamped, SNR trellis {:.1} dB / greedy {:.1} dB",
                input.display(), out.display(), s.bytes, s.bytes as f32 * 100.0 / 4081.0, s.seconds * 1000.0, rate,
                dpcm::rate_hz(rate), s.min_level, s.max_level, s.clamps, s.snr_db, s.snr_greedy_db
            );
            Ok(())
        }
        "decode" => {
            let input = a.positional.get(1).map(PathBuf::from).context("dpcm decode: need an input .dmc")?;
            let out = a.get("out").map(PathBuf::from).unwrap_or_else(|| input.with_extension("wav"));
            let data = std::fs::read(&input).with_context(|| format!("read {}", input.display()))?;
            let wave = dpcm::decode_wave(&data, rate, level);
            let f = std::fs::File::create(&out).with_context(|| format!("create {}", out.display()))?;
            viper_apu::wav::write_wav(std::io::BufWriter::new(f), dpcm::rate_hz(rate).round() as u32, &wave)?;
            println!("{} → {}: {} samples at {:.0} Hz", input.display(), out.display(), wave.len(), dpcm::rate_hz(rate));
            Ok(())
        }
        "info" => {
            if a.positional.len() < 2 {
                bail!("dpcm info: need one or more .dmc files");
            }
            let mut total_aligned = 0usize;
            for p in &a.positional[1..] {
                let data = std::fs::read(p).with_context(|| format!("read {}", p))?;
                let levels = viper_apu::apu::decode_dmc(&data, rate, level);
                let (lo, hi) = (levels.iter().copied().min().unwrap_or(level), levels.iter().copied().max().unwrap_or(level));
                let end = levels.last().copied().unwrap_or(level);
                let mean: f32 = levels.iter().map(|&l| l as f32).sum::<f32>() / levels.len().max(1) as f32;
                let ok = data.len() % 16 == 1 && data.len() <= 4081;
                total_aligned += (data.len() + 63) & !63;
                println!(
                    "{:<40} {:>5} bytes {} {:6.1} ms  levels {:>3}..{:<3} start {} → end {} (drift {:+})  DC {:+.1}",
                    p, data.len(), if ok { "ok  " } else { "BAD " }, data.len() as f32 * 8.0 / dpcm::rate_hz(rate) * 1000.0,
                    lo, hi, level, end, end as i32 - level as i32, mean - level as f32
                );
            }
            println!("total {} bytes 64-byte aligned = {:.1}% of the 16 KB DPCM window", total_aligned, total_aligned as f32 * 100.0 / 16384.0);
            Ok(())
        }
        "synth" => {
            let name = a.positional.get(1).map(String::as_str).context("dpcm synth: need kick|snare|hat|tom")?;
            let hz = dpcm::rate_hz(rate);
            let wave = dpcm::synth(name, hz).ok_or_else(|| anyhow!("unknown recipe {:?} (have {})", name, dpcm::SYNTH_NAMES.join(", ")))?;
            let out = a.get("out").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(format!("{}.wav", name)));
            let f = std::fs::File::create(&out).with_context(|| format!("create {}", out.display()))?;
            viper_apu::wav::write_wav(std::io::BufWriter::new(f), hz.round() as u32, &wave)?;
            println!("{} → {} ({:.0} ms at {:.0} Hz)", name, out.display(), wave.len() as f32 / hz * 1000.0, hz);
            Ok(())
        }
        _ => bail!("usage: viper dpcm encode|decode|info|synth ... (see viper --help)"),
    }
}

fn fmt(a: &Args) -> Result<()> {
    let path = a.positional.first().map(PathBuf::from).context("fmt: need a .vip path")?;
    let out = a.get("out").map(PathBuf::from).unwrap_or_else(|| path.clone());
    let (song, warnings) = load_song(&path)?;
    for w in &warnings {
        eprintln!("warning: {}", w);
    }
    std::fs::write(&out, vip::to_vip(&song)).with_context(|| format!("write {}", out.display()))?;
    println!("{} → {}", path.display(), out.display());
    Ok(())
}

fn compile_cmd(a: &Args) -> Result<()> {
    if a.positional.is_empty() {
        bail!("compile: need at least one .vip path");
    }
    let paths: Vec<PathBuf> = a.positional.iter().map(PathBuf::from).collect();
    let out = a.get("out").map(PathBuf::from).unwrap_or_else(|| paths[0].with_extension("nsf"));
    let mut driver: Option<viper_nsf::Driver> = match a.get("driver") {
        Some(bin) => {
            let bin = PathBuf::from(bin);
            let sym = a.get("sym").map(PathBuf::from).unwrap_or_else(|| bin.with_extension("sym"));
            Some(viper_nsf::Driver::load(&bin, &sym)?)
        }
        None => None,
    };
    let mut module: Option<viper_nsf::Module> = None;
    let mut total_frames = 0u32;
    for path in &paths {
        let (mut song, warnings) = load_song(path)?;
        for w in &warnings {
            eprintln!("warning: {}: {}", path.display(), w);
        }
        if song.title.is_empty() {
            song.title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        }
        if driver.is_none() {
            driver = Some(
                compile::load_song_driver(&song, path.parent())
                    .context("compile: pass --driver BIN or add an @driver directive to the song")?,
            );
        }
        let lowered = compile::lower(&song, path.parent())?;
        for w in &lowered.warnings {
            eprintln!("warning: {}: {}", path.display(), w);
        }
        total_frames += lowered.module.songs[0].total_frames();
        match module.as_mut() {
            None => module = Some(lowered.module),
            Some(m) => m.songs.extend(lowered.module.songs),
        }
    }
    let mut module = module.unwrap();
    if let Some(t) = a.get("title") {
        if module.songs.len() == 1 {
            module.songs[0].title = t.to_string();
        } else {
            module.album = t.to_string();
        }
    }
    let driver = driver.unwrap();
    let emitted = viper_nsf::emit(&module, &driver)?;
    for w in &emitted.warnings {
        eprintln!("warning: {}", w);
    }
    let is_nsfe = out.extension().map_or(false, |e| e.eq_ignore_ascii_case("nsfe"));
    let bytes = if is_nsfe { &emitted.nsfe } else { &emitted.nsf };
    std::fs::write(&out, bytes).with_context(|| format!("write {}", out.display()))?;
    if let Some(extra) = a.get("nsfe") {
        std::fs::write(extra, &emitted.nsfe).with_context(|| format!("write {}", extra))?;
        println!("nsfe     → {}", extra);
    }
    println!(
        "{} song(s) → {}: {} bytes ({} song data, {} samples), {} frames ≈ {:.1}s",
        module.songs.len(),
        out.display(),
        bytes.len(),
        emitted.data_bytes,
        emitted.sample_bytes,
        total_frames,
        total_frames as f64 / 60.0988
    );
    Ok(())
}

fn info(a: &Args) -> Result<()> {
    let path = a.positional.first().map(PathBuf::from).context("info: need an .nsf path")?;
    let nsf = viper_apu::Nsf::parse(&std::fs::read(&path)?)?;
    println!("{}", path.display());
    println!("  title      {}", nsf.name);
    println!("  artist     {}", nsf.artist);
    println!("  copyright  {}", nsf.copyright);
    println!("  songs      {} (start {})", nsf.songs, nsf.start_song);
    println!("  load/init/play  ${:04X} / ${:04X} / ${:04X}", nsf.load, nsf.init, nsf.play);
    println!("  data       {} bytes{}", nsf.data.len(), if nsf.bankswitched() { " (bankswitched)" } else { "" });
    println!("  region     {}  expansion ${:02X}", if nsf.pal { "PAL" } else { "NTSC" }, nsf.expansion);
    for (i, t) in nsf.track_names.iter().enumerate() {
        let ms = nsf.track_times.get(i).copied().unwrap_or(0);
        println!("  {:>2}. {:<28} {}:{:02}", i + 1, t, ms / 60000, (ms / 1000) % 60);
    }
    Ok(())
}

/// Stage 24: `viper verify song.nsf --against other.log`. Our log comes
/// from rendering the NSF (same `--vip` / `--frames` / `--loops` rules as
/// `render`) or, when the positional is not an NSF, from a saved log.
fn verify(a: &Args) -> Result<()> {
    let path = a.positional.first().map(PathBuf::from).context("verify: need an .nsf or a viper .log")?;
    let against = a.get("against").map(PathBuf::from).context("verify: need --against other.log")?;
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let ours: Vec<viper_apu::RegWrite> = if bytes.starts_with(b"NESM\x1A") || bytes.starts_with(b"NSFE") {
        let nsf = viper_apu::Nsf::parse(&bytes)?;
        let mut opts = viper_apu::RenderOptions {
            song: a.num::<u8>("song")?.unwrap_or(0),
            loops: a.num::<u32>("loops")?.unwrap_or(1),
            tail_seconds: a.num::<f64>("tail")?.unwrap_or(0.0),
            ..Default::default()
        };
        let mut fixed_frames = a.num::<u32>("frames")?;
        if let Some(vp) = a.get("vip") {
            let vp = PathBuf::from(vp);
            let (song, _) = load_song(&vp)?;
            let lowered = compile::lower(&song, vp.parent())?;
            let (intro, looped) = lowered.module.songs[0].intro_and_loop_frames();
            fixed_frames = Some(intro + looped * opts.loops.max(1) + (opts.tail_seconds * 60.0988) as u32);
        }
        let r = match fixed_frames {
            Some(frames) => {
                opts.max_seconds = frames as f64 / 60.0988;
                viper_apu::render::render_frames(&nsf, &opts, frames)?
            }
            None => viper_apu::render(&nsf, &opts)?,
        };
        println!("viper: {} frames, {} writes", r.total_frames, r.log.len());
        r.log
    } else {
        let log = viper_apu::verify::parse_log(&String::from_utf8_lossy(&bytes));
        println!("viper log {}: {} writes", path.display(), log.len());
        log
    };
    let other_text = std::fs::read_to_string(&against).with_context(|| format!("read {}", against.display()))?;
    let theirs = viper_apu::verify::parse_log(&other_text);
    if theirs.is_empty() {
        bail!("verify: no register writes recognised in {}", against.display());
    }
    println!("other log {}: {} writes", against.display(), theirs.len());
    if let Some(out) = a.get("out") {
        std::fs::write(out, viper_apu::verify::format_log(&theirs)).with_context(|| format!("write {}", out))?;
        println!("normalized → {}", out);
    }
    let report = viper_apu::verify::compare(&ours, &theirs);
    print!("{}", report.summary());
    if report.ok() {
        println!("ok");
        Ok(())
    } else {
        std::process::exit(1);
    }
}

fn render(a: &Args) -> Result<()> {
    let path = a.positional.first().map(PathBuf::from).context("render: need an .nsf path")?;
    let nsf = viper_apu::Nsf::parse(&std::fs::read(&path).with_context(|| format!("read {}", path.display()))?)?;
    let stems_dir = a.get("stems").map(PathBuf::from);
    let out = a.get("out").map(PathBuf::from);
    let triggers = a.get("triggers").map(PathBuf::from);
    let log = a.get("log").map(PathBuf::from);
    if out.is_none() && stems_dir.is_none() && triggers.is_none() && log.is_none() {
        bail!("render: nothing to do — pass -o, --stems, --triggers, or --log");
    }
    let mut opts = viper_apu::RenderOptions {
        song: a.num::<u8>("song")?.unwrap_or(0),
        sample_rate: a.num::<u32>("rate")?.unwrap_or(44_100),
        loops: a.num::<u32>("loops")?.unwrap_or(1),
        tail_seconds: a.num::<f64>("tail")?.unwrap_or(1.0),
        stems: stems_dir.is_some(),
        ..Default::default()
    };
    let mut fixed_frames = a.num::<u32>("frames")?;
    let mut bpm_from_vip: Option<f64> = None;
    // DPCM stem names: from the song's sample list when --vip is given,
    // otherwise the built-in bank order.
    let mut sample_names: Vec<String> = crate::dpcm::default_bank().iter().map(|s| s.name.to_string()).collect();
    if let Some(vp) = a.get("vip") {
        let vp = PathBuf::from(vp);
        let (song, _) = load_song(&vp)?;
        let lowered = compile::lower(&song, vp.parent())?;
        let s = &lowered.module.songs[0];
        sample_names = s.samples.iter().map(|x| x.name.clone()).collect();
        let (intro, looped) = s.intro_and_loop_frames();
        let tail = (opts.tail_seconds * 60.0988) as u32;
        fixed_frames = Some(intro + looped * opts.loops.max(1) + tail);
        bpm_from_vip = Some(song.bpm as f64);
        println!("from {}: intro {} frames, loop {} frames × {}", vp.display(), intro, looped, opts.loops.max(1));
    }
    if let Some(frames) = fixed_frames {
        opts.max_seconds = frames as f64 / 60.0988;
    }
    let r = if let Some(frames) = fixed_frames {
        viper_apu::render::render_frames(&nsf, &opts, frames)?
    } else {
        viper_apu::render(&nsf, &opts)?
    };
    let secs = r.total_frames as f64 / 60.0988;
    match r.loop_frames {
        Some(l) => println!("loop: {} frames ({:.2}s); rendered {} frames ({:.1}s)", l, l as f64 / 60.0988, r.total_frames, secs),
        None => println!("rendered {} frames ({:.1}s){}", r.total_frames, secs, if fixed_frames.is_none() { " — no loop detected" } else { "" }),
    }
    if let Some(p) = &out {
        let f = std::fs::File::create(p).with_context(|| format!("create {}", p.display()))?;
        viper_apu::wav::write_wav(std::io::BufWriter::new(f), r.sample_rate, &r.mix)?;
        println!("mix      → {}", p.display());
    }
    if let Some(dir) = &stems_dir {
        std::fs::create_dir_all(dir)?;
        // Name DPCM stems after the sample that produced them. Stems are
        // indexed by order of first use in the register log; the NSF's
        // sample table order (= .vip order) is recovered from the $4012
        // value, which increases with table position for viper output.
        let mut used = r.dpcm_samples.clone();
        used.sort_unstable();
        for s in &r.stems {
            let name = match s.name.strip_prefix("dpcm").and_then(|n| n.parse::<usize>().ok()) {
                Some(i) if i < r.dpcm_samples.len() => {
                    let table_pos = used.iter().position(|&a| a == r.dpcm_samples[i]).unwrap_or(i);
                    sample_names.get(table_pos).cloned().unwrap_or_else(|| s.name.clone())
                }
                _ => s.name.clone(),
            };
            let p = dir.join(format!("{}.wav", name));
            let f = std::fs::File::create(&p).with_context(|| format!("create {}", p.display()))?;
            viper_apu::wav::write_wav(std::io::BufWriter::new(f), r.sample_rate, &s.samples)?;
            println!("stem     → {}", p.display());
        }
    }
    if let Some(p) = &triggers {
        let bpm = a.num::<f64>("bpm")?.or(bpm_from_vip).unwrap_or(120.0);
        let mut hits = Vec::new();
        for t in &r.triggers {
            let time_s = (t.frame.saturating_sub(1)) as f64 / 60.0988 + t.cycle as f64 / viper_apu::host::CPU_HZ;
            let note = match t.kind {
                viper_apu::TriggerKind::Dpcm { addr_reg } => {
                    let idx = r.dpcm_samples.iter().position(|&x| x == addr_reg).unwrap_or(0);
                    [36u8, 38, 42, 46, 49, 45][idx.min(5)]
                }
                viper_apu::TriggerKind::Noise => 42,
            };
            hits.push(viper_apu::midi::DrumHit { time_s, note, velocity: 100 });
        }
        let f = std::fs::File::create(p).with_context(|| format!("create {}", p.display()))?;
        viper_apu::midi::write_drum_midi(std::io::BufWriter::new(f), bpm, &hits)?;
        println!("triggers → {} ({} hits @ {} BPM)", p.display(), hits.len(), bpm);
    }
    if let Some(p) = &log {
        std::fs::write(p, viper_apu::render::format_log(&r.log))?;
        println!("log      → {} ({} writes)", p.display(), r.log.len());
    }
    Ok(())
}
