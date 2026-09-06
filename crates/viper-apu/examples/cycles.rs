//! Report the CPU cycles the driver's INIT and PLAY routines use per frame.
//! On NTSC hardware a frame is 29780 cycles; PLAY must fit inside one,
//! leaving room for whatever else the player ROM does.
use viper_apu::{Nsf, Player};

fn main() -> anyhow::Result<()> {
    for path in std::env::args().skip(1) {
        let nsf = Nsf::parse(&std::fs::read(&path)?)?;
        let songs = nsf.songs;
        let mut p = Player::new(nsf, 44_100);
        p.keep_log = false;
        let init = p.init(0).map(|_| ()).map(|_| 0u32).unwrap_or(0);
        let _ = init;
        let mut worst = 0u32;
        let mut total: u64 = 0;
        let mut frames = 0u32;
        for _ in 0..3600 {
            let c = p.frame()?;
            worst = worst.max(c);
            total += c as u64;
            frames += 1;
            p.samples.clear();
        }
        println!(
            "{:52} songs {} | PLAY worst {:5} cycles ({:4.1}% of a 29780-cycle frame), mean {:5.0}",
            path.rsplit('/').next().unwrap(), songs, worst, worst as f64 * 100.0 / 29780.0, total as f64 / frames as f64
        );
    }
    Ok(())
}
