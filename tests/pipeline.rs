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

/// The frame table, against the real golden log rather than hand-written
/// registers. This is the decode every rip depends on, so it is worth
/// pinning to music whose source is checked in beside it: every number
/// below is derived from `projects/stress_melodeath.vip`.
#[test]
fn the_frame_table_decodes_the_golden_log_back_into_the_stress_song() {
    let text = std::fs::read_to_string(root().join("tests/golden/stress_melodeath.log")).unwrap();
    let log = viper_apu::verify::parse_log(&text);
    let t = viper_apu::trace(&log);
    assert_eq!(t.len(), 257, "one entry per frame in the log");

    use viper_apu::trace::{NOI, PU1, PU2, TRI};
    let keys = |c: usize| t.iter().filter(|f| f.ch[c].keyed).count();
    let audible = |c: usize| t.iter().filter(|f| f.ch[c].keyed && f.ch[c].level > 0).count();
    assert_eq!([keys(PU1), keys(PU2), keys(TRI), keys(NOI)], [52, 52, 48, 49]);

    // Frame 127 is the last row of phrase 01, which is empty. The driver
    // keys three channels there anyway, with every volume at zero. Reading
    // key-ons as notes would invent three notes that were never audible —
    // so the level, not the key-on, is what makes a note.
    assert_eq!([audible(PU1), audible(PU2), audible(TRI)], [51, 51, 47]);
    for c in [PU1, PU2, TRI] {
        assert!(t[127].ch[c].keyed && t[127].ch[c].level == 0, "silent key-on at frame 127");
    }

    // Pitch: the source's first note on each pitched channel. Periods
    // invert as f = CPU / (16(p+1)) for the pulses and 32(p+1) for the
    // triangle, giving E-5, C-5 (the harmonised third) and E-2.
    let note = |p: u16, div: f64| {
        let f = 1_789_773.0 / (div * (p as f64 + 1.0));
        (69.0 + 12.0 * (f / 440.0).log2()).round() as i32
    };
    assert_eq!(note(t[1].ch[PU1].period, 16.0), 76, "E-5");
    assert_eq!(note(t[1].ch[PU2].period, 16.0), 72, "C-5");
    assert_eq!(note(t[1].ch[TRI].period, 32.0), 40, "E-2");

    // Volume: the driver's envelope after the held-note fix is a clean
    // attack, sustain and single release. The doubled release the old
    // driver produced (10 9 7 4 7 4 2 0) is what made note ends unreadable.
    let pu1: Vec<u8> = (66..82).map(|f| t[f].ch[PU1].level).collect();
    assert_eq!(pu1, vec![10, 9, 9, 9, 7, 4, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    // Noise keeps its $400E index, which is already a tracker's value.
    assert_eq!(t[1].ch[NOI].period, 14);
    // The driver parks both sweep units at INIT with the usual $08, which
    // moves no pitch and so is not reported as an unsupported feature.
    assert!(text.lines().any(|l| l.ends_with(" 4001 08")), "the driver does write $4001");
    assert!(t.iter().all(|f| f.ch.iter().all(|c| c.sweep == 0)));
}

/// The two ways into the frame table must agree.
///
/// Running the NSF takes each level from the chip, which resolves hardware
/// envelopes and sweep muting exactly. Reading a register log simulates them
/// instead, because a log is all there is. Where a song uses neither — as
/// almost every driver-produced song does — the two must land on the same
/// answer, and that is what makes the log path trustworthy for material
/// that can only ever arrive as a dump.
#[test]
fn tracing_the_nsf_and_tracing_its_log_agree() {
    let tmp = std::env::temp_dir().join(format!("viper_trace_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let vip = root().join("projects/stress_melodeath.vip");
    let nsf_path = tmp.join("stress.nsf");
    let out = viper().args(["compile"]).arg(&vip).arg("--driver").arg(root().join("tests/fixtures/driver.bin")).arg("-o").arg(&nsf_path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let text = std::fs::read_to_string(root().join("tests/golden/stress_melodeath.log")).unwrap();
    let from_log = viper_apu::trace(&viper_apu::verify::parse_log(&text));

    let nsf = viper_apu::Nsf::parse(&std::fs::read(&nsf_path).unwrap()).unwrap();
    let (from_nsf, looped) = viper_apu::trace::trace_nsf(&nsf, 0, Some(from_log.len() as u32 - 1)).unwrap();
    assert_eq!(looped.map(|(_, len)| len), Some(196), "the loop period is exact even when its start is not");

    assert_eq!(from_nsf.len(), from_log.len());
    for (a, b) in from_nsf.iter().zip(&from_log) {
        assert_eq!(a, b, "frame {} differs between the NSF and its log", a.frame);
    }

    // And the CLI reaches the same place, including through the log.
    for src in [nsf_path.as_path(), &root().join("tests/golden/stress_melodeath.log")] {
        let out = viper().args(["rip"]).arg(src).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.contains("key-ons"), "{}", s);
        assert!(s.contains("suppressed (no volume at onset)"), "the silent key-ons must be reported: {}", s);
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The rip, end to end, against a song whose source is checked in beside it.
///
/// Every number here was measured, and the two kinds of difference that
/// remain are both understood — which is the point. A transcriber that
/// scores well without anyone knowing why is not evidence of anything.
#[test]
fn ripping_the_stress_song_recovers_its_notes_and_structure() {
    let tmp = std::env::temp_dir().join(format!("viper_rip_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let vip = root().join("projects/stress_melodeath.vip");
    let nsf = tmp.join("stress.nsf");
    let out = viper().args(["compile"]).arg(&vip).arg("--driver").arg(root().join("tests/fixtures/driver.bin")).arg("-o").arg(&nsf).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let ripped = tmp.join("ripped.vip");
    let out = viper().args(["rip"]).arg(&nsf).arg("-o").arg(&ripped).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report = String::from_utf8_lossy(&out.stdout);

    // The tempo is not read from the file — an NSF does not record one. It is
    // recovered by simulating the driver's fixed-point row clock at candidate
    // integer tempos, and "exact" means every onset landed on a row start.
    assert!(report.contains("220 BPM"), "{}", report);
    assert!(report.contains("INFERRED, exact"), "{}", report);
    assert!(report.contains("rows     48 → 3 phrases (3 unique)"), "{}", report);
    // The driver keys three channels on an empty row with no volume; those
    // are not notes and must not be transcribed as any.
    assert!(report.contains("3 key-ons suppressed"), "{}", report);

    let src = std::fs::read_to_string(&vip).unwrap();
    let got = std::fs::read_to_string(&ripped).unwrap();
    let (a, b) = (note_grid(&src), note_grid(&got));
    assert_eq!(a.len(), 48, "the source is three phrases");
    assert_eq!(b.len(), a.len(), "and so is the rip");

    let mut wrong: Vec<(usize, usize, String, String)> = Vec::new();
    for r in 0..a.len() {
        for c in 0..5 {
            if a[r][c] != b[r][c] {
                wrong.push((r, c, a[r][c].clone(), b[r][c].clone()));
            }
        }
    }
    // Both kinds of remaining difference are understood, so name them rather
    // than accepting a percentage on trust.
    for (r, c, want, got) in &wrong {
        let noise_bucket = *c == 3 && want == "C-3" && got == "D#3";
        // `compile::noise_period_index` folds four semitones onto each index,
        // so a ripped noise note can only ever be somewhere in the right
        // bucket. This one is irreducible, not a bug to be fixed later.
        let triangle_rang_on = *c == 2 && want == "---" && got == "===";
        // The source writes a note-off here, but the register log shows the
        // triangle still sounding into the next row: the driver's release
        // outlives the row that ended it. The rip describes the chip, so
        // `===` is the truthful transcription of what actually happened.
        assert!(noise_bucket || triangle_rang_on, "unexplained difference at row {} channel {}: {} → {}", r, c, want, got);
    }
    assert!(wrong.len() <= 16, "{} cells differ, expected at most 16", wrong.len());

    // Instruments are recovered from the envelopes the notes played, not
    // guessed. The source's lead is `a=0 d=20 s=0.90 r=60 duty=0.25
    // vol=0.70`, and the rip has to land near it without ever having seen
    // the file — the only evidence is that its notes played 10 9 9 9 7 4 2 0.
    assert!(report.contains("instr    4 synthesised"), "one instrument per voice: {}", report);
    let lead = got.lines().find(|l| l.starts_with("@instr 00")).expect("a lead instrument");
    for (field, want, tol) in [("s=", 0.90f32, 0.05f32), ("duty=", 0.25, 0.001), ("vol=", 0.70, 0.05)] {
        let v: f32 = lead.split(field).nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        assert!((v - want).abs() <= tol, "instr 00 {}{} should be near {}", field, v, want);
    }
    // The release is only visible on notes long enough to show one, and most
    // of this song's are cut off by the next key-on. Finding it at all means
    // the longest note in each group is what was read.
    let r: u16 = lead.split("r=").nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap();
    assert!((50..=80).contains(&r), "release {} ms should be near the source's 60", r);

    // A rip must settle. Recompiling one and ripping it again may move once,
    // because the placeholder instruments are a flat gate rather than the
    // envelopes the original used — that is Stage 36c's job. After that it
    // must not drift at all, or the tool cannot be used iteratively.
    let mut prev = std::fs::read_to_string(&ripped).unwrap();
    let mut settled = None;
    for i in 0..4 {
        let n = tmp.join(format!("r{}.nsf", i));
        let v = tmp.join(format!("r{}.vip", i));
        let out = viper().args(["compile"]).arg(tmp.join(if i == 0 { "ripped.vip".into() } else { format!("r{}.vip", i - 1) })).arg("--driver").arg(root().join("tests/fixtures/driver.bin")).arg("-o").arg(&n).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let out = viper().args(["rip"]).arg(&n).arg("-o").arg(&v).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let now = std::fs::read_to_string(&v).unwrap();
        if rows_of(&now) == rows_of(&prev) && settled.is_none() {
            settled = Some(i);
        }
        prev = now;
    }
    assert!(matches!(settled, Some(i) if i <= 1), "a rip must reach a fixed point, settled at {:?}", settled);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The note name in every cell of every phrase, in order.
///
/// Rows are placed by their step number, not by the order they appear: a
/// `.vip` omits rows that are entirely empty, so counting lines would silently
/// shorten a sparse phrase and misalign everything after it.
fn note_grid(vip: &str) -> Vec<[String; 5]> {
    let blank = || std::array::from_fn(|_| "---".to_string());
    let mut out: Vec<[String; 5]> = Vec::new();
    let mut base = 0usize;
    let mut in_phrase = false;
    for line in vip.lines() {
        if line.starts_with("@phrase") {
            base = out.len();
            out.resize_with(base + 16, blank);
            in_phrase = true;
            continue;
        }
        if line.starts_with('@') {
            in_phrase = false;
        }
        let t = line.trim_start();
        if !in_phrase || t.starts_with('#') || t.is_empty() {
            continue;
        }
        let mut f = t.split_whitespace();
        let Some(step) = f.next() else { continue };
        let Ok(step) = usize::from_str_radix(step, 16) else { continue };
        if step >= 16 {
            continue;
        }
        for (i, c) in f.take(5).enumerate() {
            out[base + step][i] = c.split(':').next().unwrap_or("---").to_string();
        }
    }
    out
}

/// Just the pattern rows of a `.vip`, for comparing two of them.
fn rows_of(vip: &str) -> Vec<String> {
    vip.lines().filter(|l| l.starts_with("  ") && l.trim_start().len() > 2).map(|l| l.trim_end().to_string()).collect()
}
