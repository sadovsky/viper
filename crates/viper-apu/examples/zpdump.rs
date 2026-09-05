use viper_apu::{Nsf, Player};
fn dump(p: &Player, label: &str) {
    let m = p.memory();
    print!("{} ZP:", label);
    for a in 0xE0..0x100u16 { print!(" {:02X}", m.read(a)); }
    println!();
    for row in 0..5 {
        print!("  ch{}:", row);
        for field in 0..23u16 { print!(" {:02X}", m.read(0x0300 + field * 5 + row)); }
        println!();
    }
}
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let nsf = Nsf::parse(&std::fs::read(&args[1])?)?;
    let mut p = Player::new(nsf, 44100);
    p.init(0)?;
    dump(&p, "init");
    let m = p.memory();
    print!("song_table @8615:");
    for a in 0x8615..0x8640u16 { print!(" {:02X}", m.read(a)); }
    println!();
    for f in 0..3 { p.frame()?; dump(&p, &format!("frame{}", f + 1)); }
    Ok(())
}
