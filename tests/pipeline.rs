//! End-to-end: compile the bundled stress song against a vendored ABI v3
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

    // Stage 24: the exact-length render must equal the checked-in golden
    // log. Regenerate it deliberately when the driver fixture or the
    // compiler changes:
    //   viper render stress.nsf --vip projects/stress_melodeath.vip \
    //       --log tests/golden/stress_melodeath.log
    let golden_log = tmp.join("golden.log");
    let out = viper().args(["render"]).arg(&nsf).arg("--vip").arg(&vip).arg("--log").arg(&golden_log).output().unwrap();
    assert!(out.status.success(), "render failed: {}", String::from_utf8_lossy(&out.stderr));
    let got = std::fs::read_to_string(&golden_log).unwrap();
    let want = std::fs::read_to_string(root().join("tests/golden/stress_melodeath.log")).unwrap();
    if got != want {
        let first_diff = got.lines().zip(want.lines()).position(|(a, b)| a != b);
        panic!(
            "register log differs from tests/golden/stress_melodeath.log (first differing line: {:?}); regenerate the golden log if the change is intended",
            first_diff.map(|i| i + 1)
        );
    }
    // `viper verify` accepts the golden log as the other side and passes.
    let out = viper().args(["verify"]).arg(&nsf).arg("--vip").arg(&vip).arg("--against").arg(root().join("tests/golden/stress_melodeath.log")).output().unwrap();
    let verify_out = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && verify_out.contains("PLAY frames match"), "{}\n{}", verify_out, String::from_utf8_lossy(&out.stderr));
    // The external receipt: FCEUX 2.6.6 playing the same NSF, logged by
    // tools/fceux_apu_log.lua (INIT and PLAY 1 share a frame there, and
    // its frame counter starts at 3). Every PLAY frame must match.
    //
    // It is pinned to driver-fceux.bin, the exact build FCEUX played. The
    // capture is evidence about that driver; re-linking it against a newer
    // one would silently rewrite the evidence. When the driver changes in
    // a way that alters its register writes, recapture with FCEUX and
    // replace both the fixture and the log together.
    let fceux_nsf = tmp.join("stress-fceux.nsf");
    let out = viper().args(["compile"]).arg(&vip).arg("--driver").arg(root().join("tests/fixtures/driver-fceux.bin")).arg("--sym").arg(root().join("tests/fixtures/driver-fceux.sym")).arg("-o").arg(&fceux_nsf).output().unwrap();
    assert!(out.status.success(), "compile against the FCEUX-era driver: {}", String::from_utf8_lossy(&out.stderr));
    let out = viper().args(["verify"]).arg(&fceux_nsf).arg("--vip").arg(&vip).arg("--against").arg(root().join("tests/golden/stress_melodeath.fceux.log")).output().unwrap();
    let verify_out = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && verify_out.contains("frames compared: 196") && verify_out.contains("PLAY frames match"), "{}", verify_out);
    // A tampered log is caught, with the frame named.
    let tampered = tmp.join("tampered.log");
    std::fs::write(&tampered, want.replacen("1 4003 ", "1 4007 ", 1)).unwrap();
    let out = viper().args(["verify"]).arg(&golden_log).arg("--against").arg(&tampered).output().unwrap();
    let verify_out = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success() && verify_out.contains("first mismatch at frame 1"), "{}", verify_out);

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

/// Stage 23: a song arranged with chains compiles to the flattened order,
/// and the loop point lands on the arrangement's loop slot. Two chains
/// (`00 01 00` and `01`) arranged `00 01 00` with loop slot 1 flatten to
/// seven entries with the loop at entry 3; at 120 BPM a phrase is 120
/// frames, so intro = 360 and loop = 480.
#[test]
fn chained_song_compiles_to_the_flattened_order() {
    let tmp = std::env::temp_dir().join(format!("viper_chain_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let vip = tmp.join("chained.vip");
    std::fs::write(&vip, "\
@song bpm=120
@phrase 00
  00  C-4:00:0F  ---  C-2:02:0F  ---  ---
  08  E-4:00:0F  ---  ---  ---  ---
@phrase 01
  00  G-4:00:0F  ---  G-2:02:0F  ---  ---
@chain 00  name=\"verse\"
  00 01 00
@chain 01
  01
@arrangement loop=01
  00 01 00
@length  pu1=8
").unwrap();
    let check = viper().args(["check"]).arg(&vip).output().unwrap();
    let check_out = String::from_utf8_lossy(&check.stdout);
    assert!(check.status.success() && check_out.contains("order 7 entries"), "{}", check_out);
    let nsf = tmp.join("chained.nsf");
    let out = viper().args(["compile"]).arg(&vip).arg("--driver").arg(root().join("tests/fixtures/driver.bin")).arg("-o").arg(&nsf).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let render = viper().args(["render"]).arg(&nsf).arg("--vip").arg(&vip).arg("--log").arg(tmp.join("w.log")).output().unwrap();
    let render_out = String::from_utf8_lossy(&render.stdout);
    assert!(render_out.contains("intro 360 frames, loop 480 frames"), "{}", render_out);
    // Polymeter lowering: PU1 wraps every 8 rows, so the E-4 at row 8 is
    // replaced by C-4 again and every PU1 period-low write in the first
    // phrase (120 frames) carries the same value. Without the wrap the
    // second note-on would write E-4's period.
    let log = std::fs::read_to_string(tmp.join("w.log")).unwrap();
    let pu1_periods: Vec<&str> = log
        .lines()
        .filter_map(|l| {
            let mut it = l.split(' ');
            let frame: u32 = it.next()?.parse().ok()?;
            (it.next()? == "4002" && (1..=120).contains(&frame)).then(|| it.next()).flatten()
        })
        .collect();
    assert!(pu1_periods.len() >= 2, "expected two PU1 note-ons in the first phrase: {:?}", pu1_periods);
    assert!(pu1_periods.iter().all(|p| *p == pu1_periods[0]), "row 8 should repeat row 0 on PU1: {:?}", pu1_periods);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Regression for the driver's order-list lookup: a song with more than
/// 25 order entries must keep advancing. The RAM-state loop the renderer
/// detects has to match the loop length computed from the source; when
/// the driver stalls, the detected loop collapses to a handful of frames.
#[test]
fn long_order_lists_keep_playing() {
    let tmp = std::env::temp_dir().join(format!("viper_longorder_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let vip = tmp.join("song.vip");
    let out = viper()
        .args(["gen", "--style"])
        .arg(root().join("styles/neutral"))
        .args(["--seed", "3", "-o"])
        .arg(&vip)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&vip).unwrap();
    let order_len = text.lines().find(|l| l.starts_with("@song")).and_then(|l| l.split("order=[").nth(1)).map(|o| o.split(']').next().unwrap().split(',').count()).unwrap();
    assert!(order_len > 26, "test needs a long order list, got {}", order_len);
    let nsf = tmp.join("song.nsf");
    let out = viper().args(["compile"]).arg(&vip).arg("--driver").arg(root().join("tests/fixtures/driver.bin")).arg("-o").arg(&nsf).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let expected = viper().args(["render"]).arg(&nsf).arg("--vip").arg(&vip).arg("--log").arg(tmp.join("a.log")).output().unwrap();
    let expected = String::from_utf8_lossy(&expected.stdout);
    let loop_from_source: u32 = expected.lines().find(|l| l.contains("loop ")).and_then(|l| l.split("loop ").nth(1)).and_then(|s| s.split(' ').next()).and_then(|n| n.parse().ok()).expect("intro/loop line");
    let detected = viper().args(["render"]).arg(&nsf).arg("--log").arg(tmp.join("b.log")).output().unwrap();
    let detected = String::from_utf8_lossy(&detected.stdout);
    let loop_detected: u32 = detected.lines().find(|l| l.starts_with("loop: ")).and_then(|l| l.split(' ').nth(1)).and_then(|n| n.parse().ok()).expect("loop line");
    assert_eq!(loop_detected, loop_from_source, "driver stalled: detected loop {} vs {} frames from source\n{}", loop_detected, loop_from_source, detected);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Hand-crafted DPCM end to end: synth a source, encode it, reference it
/// from a .vip through @dpcm, compile against the fixture driver, and see
/// the sample trigger in the render. Renders must be byte-identical.
#[test]
fn custom_dpcm_bank_round_trip() {
    let tmp = std::env::temp_dir().join(format!("viper_dpcm_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    for name in ["kick", "snare"] {
        let out = viper().args(["dpcm", "synth", name, "-o"]).arg(tmp.join(format!("{}.wav", name))).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let out = viper().args(["dpcm", "encode"]).arg(tmp.join(format!("{}.wav", name))).arg("-o").arg(tmp.join(format!("{}.dmc", name))).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let dmc = std::fs::read(tmp.join(format!("{}.dmc", name))).unwrap();
        assert_eq!(dmc.len() % 16, 1);
    }
    let info = viper().args(["dpcm", "info"]).arg(tmp.join("kick.dmc")).arg(tmp.join("snare.dmc")).output().unwrap();
    let info = String::from_utf8_lossy(&info.stdout);
    assert!(info.contains("drift +0"), "{}", info);
    let vip = "@song bpm=150 order=[00] loop=00\n@dpcm 00 name=kick path=kick.dmc\n@dpcm 01 name=snare path=snare.dmc\n@phrase 00\n  00 --- --- --- --- C-4\n  04 --- --- --- --- C#4\n  08 --- --- --- --- C-4\n  0C --- --- --- --- C#4\n";
    std::fs::write(tmp.join("song.vip"), vip).unwrap();
    let out = viper().args(["compile"]).arg(tmp.join("song.vip")).arg("--driver").arg(root().join("tests/fixtures/driver.bin")).arg("-o").arg(tmp.join("song.nsf")).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("0 samples)"), "samples should be in the NSF: {}", stdout);
    let mut logs = Vec::new();
    for i in 0..2 {
        let out = viper().args(["render"]).arg(tmp.join("song.nsf")).arg("--vip").arg(tmp.join("song.vip")).arg("--triggers").arg(tmp.join(format!("d{}.mid", i))).arg("--log").arg(tmp.join(format!("l{}.log", i))).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let s = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(s.contains("hits"), "{}", s);
        logs.push(std::fs::read(tmp.join(format!("l{}.log", i))).unwrap());
    }
    assert_eq!(logs[0], logs[1]);
    let log = String::from_utf8_lossy(&logs[0]);
    assert!(log.lines().any(|l| l.contains(" 4015 1F")), "DPCM start expected in the log");
    let _ = std::fs::remove_dir_all(&tmp);
}
