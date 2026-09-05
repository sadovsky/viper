//! End-to-end: compile the bundled stress song against a vendored ABI v1
//! driver (tests/fixtures, built from nintendo-metal/driver), render it
//! through viper-apu, and check the things that would break silently:
//! the NSF header, the exact loop length, deterministic output, and the
//! register-write log's shape.

use std::path::PathBuf;
use std::process::Command;

fn viper() -> Command {
    Command::new(env!("CARGO_BIN_EXE_viper"))
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn stress_song_compiles_renders_and_loops_exactly() {
    let tmp = std::env::temp_dir().join(format!("viper_pipeline_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let vip = root().join("projects/stress_melodeath.vip");
    let nsf = tmp.join("stress.nsf");
    let out = viper()
        .args(["compile"])
        .arg(&vip)
        .arg("--driver")
        .arg(root().join("tests/fixtures/driver.bin"))
        .arg("-o")
        .arg(&nsf)
        .output()
        .unwrap();
    assert!(out.status.success(), "compile failed: {}", String::from_utf8_lossy(&out.stderr));
    let bytes = std::fs::read(&nsf).unwrap();
    assert_eq!(&bytes[..5], b"NESM\x1A");
    assert_eq!(bytes[6], 1, "one song");
    assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 0x8000);

    let log1 = tmp.join("a.log");
    let wav1 = tmp.join("a.wav");
    let out = viper().args(["render"]).arg(&nsf).arg("--log").arg(&log1).arg("-o").arg(&wav1).output().unwrap();
    assert!(out.status.success(), "render failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // 3 phrases × 16 rows at 220 BPM = 196 frames under the driver's 8.8 clock.
    assert!(stdout.contains("loop: 196 frames"), "unexpected render report: {}", stdout);

    let log2 = tmp.join("b.log");
    let wav2 = tmp.join("b.wav");
    viper().args(["render"]).arg(&nsf).arg("--log").arg(&log2).arg("-o").arg(&wav2).output().unwrap();
    assert_eq!(std::fs::read(&log1).unwrap(), std::fs::read(&log2).unwrap(), "register log must be deterministic");
    assert_eq!(std::fs::read(&wav1).unwrap(), std::fs::read(&wav2).unwrap(), "audio must be deterministic");

    let log = std::fs::read_to_string(&log1).unwrap();
    let first: Vec<&str> = log.lines().take(4).collect();
    assert_eq!(first[0], "0 4015 0F", "INIT must enable the channels first: {:?}", first);
    assert!(log.lines().any(|l| l.starts_with("1 4003 ")), "PU1 gets a period-hi write on the first PLAY frame");
    assert!(log.lines().count() > 2000);

    // nsfe by extension carries the track label
    let nsfe = tmp.join("stress.nsfe");
    let out = viper().args(["compile"]).arg(&vip).arg("--driver").arg(root().join("tests/fixtures/driver.bin")).arg("-o").arg(&nsfe).output().unwrap();
    assert!(out.status.success());
    let info = viper().args(["info"]).arg(&nsfe).output().unwrap();
    let info = String::from_utf8_lossy(&info.stdout);
    assert!(info.contains("stress test: melodic death"), "{}", info);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn neutral_style_generates_a_song_that_compiles() {
    let tmp = std::env::temp_dir().join(format!("viper_gen_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let out = viper()
        .args(["gen", "--style"])
        .arg(root().join("styles/neutral"))
        .args(["--seed", "3", "-o"])
        .arg(tmp.join("song.vip"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = viper()
        .args(["compile"])
        .arg(tmp.join("song.vip"))
        .arg("--driver")
        .arg(root().join("tests/fixtures/driver.bin"))
        .arg("-o")
        .arg(tmp.join("song.nsf"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_dir_all(&tmp);
}
