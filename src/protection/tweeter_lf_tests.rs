use super::*;
use lifecycle_tests::{prearm, zero};
use serde_json::{Value, json};
use tests::{chunk, configuration};

fn policy() -> Value {
    json!({"kind":"tweeter_lf", "targets":["tw_l"], "expected_highpass_hz":3000.0,
           "limit_dbfs":-24.0, "energy_blocks":8, "hold_blocks":6})
}

fn prepare(policy: Value) -> Res<config::PreparedConfig> {
    let mut graph = serde_json::to_value(configuration())?;
    graph["processors"]["guard"]["parameters"]["policies"] = json!({"hf_bass": policy});
    config::validate_config(serde_json::from_value(graph)?, None)
}

fn guard(conf: &config::PreparedConfig) -> Guard {
    let mut g = Guard::new(Arc::new(Shared::default()));
    assert!(g.install(conf));
    prearm(&mut g, 1);
    g
}

fn candidate() -> AudioChunk {
    let mut audio = chunk();
    audio.waveforms[0].fill(0.0);
    audio
}

#[test]
fn admission_binds_crossover_absolute_power_window_persistence_and_target() {
    let reference = identity::export(&prepare(policy()).unwrap()).unwrap();
    assert_eq!(
        reference["profile"]["qualification_frames"],
        FRAMES * (8 + 8 + 6 - 1)
    );
    for (key, value) in [
        ("expected_highpass_hz", json!(600.0)),
        ("expected_highpass_hz", json!(12000.0)),
        ("limit_dbfs", json!(-12.0)),
        ("energy_blocks", json!(32)),
        ("hold_blocks", json!(64)),
        ("targets", json!(["tw_r"])),
    ] {
        let mut p = policy();
        p[key] = value;
        let changed = identity::export(&prepare(p).unwrap()).unwrap();
        for hash in ["graph_sha256", "profile_sha256"] {
            assert_ne!(changed[hash], reference[hash], "{key}");
        }
    }
    for (key, value) in [
        ("expected_highpass_hz", json!(599.9)),
        ("expected_highpass_hz", json!(12000.1)),
        ("limit_dbfs", json!(0.0)),
        ("limit_dbfs", json!(-4000.0)),
        ("energy_blocks", json!(0)),
        ("energy_blocks", json!(33)),
        ("energy_blocks", json!(1.5)),
        ("hold_blocks", json!(0)),
        ("hold_blocks", json!(65)),
        ("hold_blocks", json!(true)),
        ("targets", json!(["mid_l"])),
        ("targets", json!(["sub_a", "tw_l"])),
    ] {
        let mut p = policy();
        p[key] = value;
        assert!(prepare(p).is_err(), "{key}");
    }
}

// Both raw blocks have finite total power. Filter memory from the first
// burst pushes the second block's filtered sum of squares beyond f64::MAX.
fn numeric_trip(conf: &config::PreparedConfig) -> Value {
    let mut g = guard(conf);
    let _playback = playback::Playback::new(g.shared.clone());
    for _ in 0..=conf.guard().plan.qualification_frames / FRAMES as u64 {
        g.process(&mut candidate());
    }
    let amplitude = (f64::MAX / 129.0).sqrt();
    let mut first = candidate();
    first.waveforms[0][128..].fill(amplitude);
    g.process(&mut first);
    assert!(g.trip().is_none());
    let mut second = candidate();
    second.waveforms[0][..128].fill(amplitude);
    g.process(&mut second);
    let trip = g.trip().unwrap();
    assert_eq!(trip.reason, Reason::Numeric);
    assert_eq!(trip.policy_id.text(), Some("hf_bass"));
    assert_eq!(trip.destination_id.text(), Some("tw_l"));
    assert!(zero(&second));
    wire::tests::status(&g.shared)
}

#[test]
fn window_trip_and_filtered_overflow_publish_targeted_wire_evidence() {
    let conf = prepare(policy()).unwrap();
    let mut g = guard(&conf);
    let _playback = playback::Playback::new(g.shared.clone());
    for i in 1..=(8 + 6 - 1) {
        let mut audio = candidate();
        audio.waveforms[0].fill(0.5);
        g.process(&mut audio);
        assert_eq!(g.trip().is_some(), i == 13);
        assert_eq!(g.detector.prearm_veto, i >= 8);
    }
    let lf = wire::tests::status(&g.shared);
    assert_eq!(lf["trip"]["reason"], "tweeter_lf");
    assert_eq!(lf["trip"]["frame"], FRAMES * 13);
    let numeric = numeric_trip(&conf);
    if let Ok(path) = std::env::var("TWEETER_LF_REPLAY") {
        let slice = json!({"export":identity::export(&conf).unwrap(), "statuses":[lf,numeric]});
        std::fs::write(path, serde_json::to_string_pretty(&slice).unwrap() + "\n").unwrap();
    }
}

// Independent bilinear-transform reference in direct form I. The production
// filter uses RBJ coefficients and direct form II transposed delay state.
struct Reference {
    k: f64,
    delays: [[f64; 4]; 2],
    powers: Vec<f64>,
}

impl Reference {
    fn new(corner: f64) -> Self {
        Self {
            k: (std::f64::consts::PI * corner / SAMPLE_RATE as f64).tan(),
            delays: [[0.0; 4]; 2],
            powers: Vec::new(),
        }
    }

    fn push(&mut self, samples: &[f64], width: usize) -> Option<f64> {
        let k2 = self.k * self.k;
        let damping = std::f64::consts::SQRT_2 * self.k;
        let mut sum = 0.0;
        for &sample in samples {
            let mut x = sample;
            for delays in &mut self.delays {
                let [x1, x2, y1, y2] = *delays;
                let y = (k2 * (x + 2.0 * x1 + x2) + 2.0 * (1.0 - k2) * y1
                    - (1.0 - damping + k2) * y2)
                    / (1.0 + damping + k2);
                *delays = [x, x1, y, y1];
                x = y;
            }
            sum += x * x;
        }
        self.powers.push(sum / FRAMES as f64);
        (self.powers.len() >= width)
            .then(|| self.powers[self.powers.len() - width..].iter().sum::<f64>() / width as f64)
    }
}

fn qualify_and_arm(g: &mut Guard, frames: u64) {
    for _ in 0..=frames / FRAMES as u64 {
        g.process(&mut candidate());
    }
    let arm = lifecycle_tests::command(g, Operation::Context(Mode::Armed), 1);
    g.process_with_command(&mut candidate(), Some(arm));
    assert_eq!(
        g.control.last_command.unwrap().result,
        control::Result::Applied
    );
}

#[test]
fn crossover_transients_and_equal_rms_misrouting_follow_filtered_power_reference() {
    use crate::filters::{Filter, biquadcombo::BiquadCombo};
    use std::f64::consts::TAU;
    let conf = prepare(policy()).unwrap();
    let mut g = guard(&conf);
    qualify_and_arm(&mut g, conf.guard().plan.qualification_frames);
    let mut reference = Reference::new(1000.0);
    let mut hp = BiquadCombo::from_config(
        "tweeter",
        SAMPLE_RATE,
        config::BiquadComboParameters::LinkwitzRileyHighpass {
            freq: 3000.0,
            order: 4,
        },
    );
    let limit = 10f64.powf(-24.0 / 10.0);
    let base = g.stream.frames;
    let mut hits = 0;
    let mut first_hit = None;
    let mut tripped = false;
    for block in 0..160 {
        let mut audio = candidate();
        audio.waveforms[23][0] = -0.0;
        for i in 0..FRAMES {
            let t = (block * FRAMES + i) as f64 / SAMPLE_RATE as f64;
            let bass = (TAU * 375.0 * t).sin();
            let treble = (TAU * 6000.0 * t).sin();
            audio.waveforms[4][i] = 0.9 * bass;
            audio.waveforms[0][i] = match block {
                0..=7 => 0.0,                         // Unrelated loud sub.
                8..=47 => 0.6 * bass + 0.35 * treble, // Pass through the intended crossover below.
                48 => {
                    if i < 80 {
                        0.36 * bass
                    } else {
                        0.0
                    }
                } // Isolated drum attack.
                49..=87 => 0.62 * treble,
                88..=119 => 0.087 * bass, // Just below the absolute LF threshold.
                _ => 0.105 * bass + (0.62_f64.powi(2) - 0.105_f64.powi(2)).sqrt() * treble,
            };
        }
        if (8..=47).contains(&block) {
            hp.process_waveform(&mut audio.waveforms[0]).unwrap();
        }
        if (49..=87).contains(&block) || block >= 120 {
            // Both tones contain an integer number of periods per block.
            let raw = audio.waveforms[0].iter().map(|x| x * x).sum::<f64>() / FRAMES as f64;
            assert!((raw - 0.62_f64.powi(2) / 2.0).abs() < 2e-13);
        }
        let energy = reference.push(&audio.waveforms[0], 8);
        if energy.is_some_and(|e| e >= limit) {
            hits += 1;
            first_hit.get_or_insert(block);
        } else {
            hits = 0;
        }
        let input = audio.waveforms.clone();
        g.process(&mut audio);
        if hits < 6 {
            assert!(g.trip().is_none(), "block {block}");
            assert!(
                input
                    .iter()
                    .flatten()
                    .zip(audio.waveforms.iter().flatten())
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "block {block}"
            );
            continue;
        }
        assert!(first_hit.unwrap() >= 120);
        assert_eq!(block, first_hit.unwrap() + 5);
        let trip = g.trip().unwrap();
        assert_eq!(
            (
                trip.reason,
                trip.policy_id.text(),
                trip.destination_id.text(),
                trip.channel
            ),
            (Reason::TweeterLf, Some("hf_bass"), Some("tw_l"), Some(0))
        );
        assert_eq!(trip.frame, base + ((block + 1) * FRAMES) as u64);
        assert!(zero(&audio));
        let mut pcm = vec![0xff; CHANNELS * FRAMES * 3];
        let (bytes, clipped) = crate::utils::conversions::chunk_to_buffer_rawbytes(
            audio,
            &mut pcm,
            &config::BinarySampleFormat::S24_3_LE,
        );
        assert_eq!((bytes, clipped), (pcm.len(), 0));
        assert!(pcm.iter().all(|b| *b == 0));
        let mut clean = candidate();
        g.process(&mut clean);
        assert_eq!(g.trip(), Some(trip));
        assert!(zero(&clean));
        tripped = true;
        break;
    }
    assert!(tripped);
}

#[test]
fn final_sample_impulse_resolves_adjacent_powers_at_inclusive_threshold() {
    let mut p = policy();
    p["energy_blocks"] = json!(1);
    p["hold_blocks"] = json!(1);
    p["limit_dbfs"] = json!(-159.0);
    let conf = prepare(p).unwrap();
    // On the last sample with zero history, y = b0 * (b0 * input).
    // Choose a representable impulse whose single-sample block power is
    // exactly the compiled threshold. Its neighbors straddle that power.
    let c = tweeter_lf::coefficients(3000.0, SAMPLE_RATE).unwrap();
    let limit = 10f64.powf(-159.0 / 10.0);
    let equal = (limit * FRAMES as f64).sqrt() / c.b0 / c.b0;
    for (pulse, order) in [
        (equal.next_down(), std::cmp::Ordering::Less),
        (equal, std::cmp::Ordering::Equal),
        (equal.next_up(), std::cmp::Ordering::Greater),
    ] {
        let y = c.b0 * (c.b0 * pulse);
        assert_eq!((y * y / FRAMES as f64).total_cmp(&limit), order);
        let mut g = guard(&conf);
        let mut audio = candidate();
        audio.waveforms[0][FRAMES - 1] = pulse;
        g.process(&mut audio);
        assert_eq!(
            g.trip().map(|t| (t.reason, t.frame)),
            (order != std::cmp::Ordering::Less).then_some((Reason::TweeterLf, FRAMES as u64))
        );
    }
}

#[test]
fn pause_discards_lf_history_and_masked_windows_still_veto_and_trip() {
    let mut p = policy();
    p["expected_highpass_hz"] = json!(600.0);
    p["energy_blocks"] = json!(4);
    p["hold_blocks"] = json!(3);
    p["limit_dbfs"] = json!(-50.0);
    let conf = prepare(p).unwrap();
    let mut g = guard(&conf);
    let q = conf.guard().plan.qualification_frames / FRAMES as u64;
    // Preserve local active PREARM to race ARM against the first real crossing.
    for _ in 0..=q {
        g.process(&mut candidate());
    }
    let mut bass = candidate();
    bass.waveforms[0].fill(0.5);
    let arm = lifecycle_tests::command(&g, Operation::Context(Mode::Armed), 1);
    g.process_with_command(&mut bass, Some(arm));
    assert!(g.detector.prearm_veto && zero(&bass));
    assert_eq!(
        g.control.last_command.unwrap().result,
        control::Result::NotQualified
    );
    let mut bass = candidate();
    bass.waveforms[0].fill(0.5);
    g.process(&mut bass); // One block short of a trip.
    assert!(g.trip().is_none());
    g.pause();
    prearm(&mut g, 2);
    for n in 0..=q {
        // A retained 200 Hz low-pass tail alone would cross this low limit.
        g.process(&mut candidate());
        assert!(!g.detector.prearm_veto && g.trip().is_none());
        assert_eq!(g.qualification.is_active(), n == q);
    }
    g.pause();
    prearm(&mut g, 3);
    for n in 1..=6 {
        let mut audio = candidate();
        audio.main_volume.as_mut().unwrap().muted = true;
        audio.waveforms[0].fill(0.5);
        g.process(&mut audio);
        assert!(zero(&audio));
        assert_eq!(g.qualification.frames(), 0);
        assert_eq!(g.detector.prearm_veto, n >= 4);
        assert_eq!(g.trip().is_some(), n == 6);
    }
}

#[test]
fn shared_tweeter_lane_keeps_separate_corners_limits_and_consecutive_counts() {
    let mut graph = serde_json::to_value(configuration()).unwrap();
    let params = &mut graph["processors"]["guard"]["parameters"];
    params["destinations"]["extra"] = json!("high_l");
    params["destinations"]["wide"] = json!("high_l");
    let mut wide = policy();
    wide["targets"] = json!(["wide"]);
    wide["expected_highpass_hz"] = json!(12000.0);
    wide["energy_blocks"] = json!(1);
    wide["hold_blocks"] = json!(3);
    let mut narrow = wide.clone();
    narrow["targets"] = json!(["tw_l"]);
    narrow["expected_highpass_hz"] = json!(600.0);
    let mut tolerant = wide.clone();
    tolerant["targets"] = json!(["extra"]);
    tolerant["limit_dbfs"] = json!(-3.0);
    params["policies"] = json!({"narrow":narrow, "sensitive":wide, "tolerant":tolerant});
    let conf = config::validate_config(serde_json::from_value(graph).unwrap(), None).unwrap();
    let mut g = guard(&conf);
    let mut reference = Reference::new(4000.0);
    let mut hits = 0;
    for block in 0..7 {
        let mut audio = candidate();
        if !matches!(block, 2 | 3) {
            for (i, x) in audio.waveforms[0].iter_mut().enumerate() {
                let phase = std::f64::consts::TAU * 1500.0 * (block * FRAMES + i) as f64
                    / SAMPLE_RATE as f64;
                *x = 0.3 * phase.sin();
            }
        }
        let hit = reference.push(&audio.waveforms[0], 1).unwrap() >= 10f64.powf(-24.0 / 10.0);
        hits = if hit { hits + 1 } else { 0 };
        g.process(&mut audio);
        assert_eq!(g.detector.prearm_veto, hit);
        assert_eq!(g.trip().is_some(), hits == 3);
    }
    let trip = g.trip().unwrap();
    assert_eq!(
        (
            trip.policy_id.text(),
            trip.destination_id.text(),
            trip.frame
        ),
        (Some("sensitive"), Some("wide"), 7 * FRAMES as u64)
    );
}
