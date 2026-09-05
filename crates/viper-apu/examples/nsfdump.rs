//! nsfdump <file.nsf> [song] [out_dir]
//! Prints header info, loop detection, the first register writes, and
//! per-stem RMS; writes mix + stems as WAV when out_dir is given.
use std::io::Write;
use viper_apu::{render, Nsf, RenderOptions};

fn rms(s: &[f32]) -> f32 {
    if s.is_empty() { return 0.0; }
    (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&args[1])?;
    let nsf = Nsf::parse(&bytes)?;
    let song: u8 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(0);
    println!("{} — {} ({}) songs={} load={:04X} init={:04X} play={:04X} banks={:?}",
        nsf.name, nsf.artist, nsf.copyright, nsf.songs, nsf.load, nsf.init, nsf.play, nsf.banks);
    let opts = RenderOptions { song, loops: 1, stems: true, tail_seconds: 0.5, max_seconds: 120.0, ..Default::default() };
    let r = render(&nsf, &opts)?;
    println!("loop_frames={:?} total_frames={} writes={} triggers={} dpcm_samples={:?}",
        r.loop_frames, r.total_frames, r.log.len(), r.triggers.len(), r.dpcm_samples);
    for w in r.log.iter().take(70) {
        println!("  {:>4} {:04X} {:02X}", w.frame, w.addr, w.value);
    }
    println!("mix rms={:.4} peak={:.4}", rms(&r.mix), r.mix.iter().fold(0f32, |m, x| m.max(x.abs())));
    for s in &r.stems {
        println!("stem {:<6} rms={:.4}", s.name, rms(&s.samples));
    }
    let kinds: Vec<String> = r.triggers.iter().take(12).map(|t| format!("{}:{:?}", t.frame, t.kind)).collect();
    println!("triggers: {}", kinds.join(" "));
    if let Some(dir) = args.get(3) {
        std::fs::create_dir_all(dir)?;
        let mut f = std::fs::File::create(format!("{}/mix.wav", dir))?;
        viper_apu::wav::write_wav(&mut f, r.sample_rate, &r.mix)?;
        for s in &r.stems {
            let mut f = std::fs::File::create(format!("{}/{}.wav", dir, s.name))?;
            viper_apu::wav::write_wav(&mut f, r.sample_rate, &s.samples)?;
        }
        let mut f = std::fs::File::create(format!("{}/reglog.txt", dir))?;
        f.write_all(viper_apu::render::format_log(&r.log).as_bytes())?;
        println!("wrote {}", dir);
    }
    Ok(())
}
