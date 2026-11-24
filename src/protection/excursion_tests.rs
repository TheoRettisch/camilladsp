use super::lifecycle_tests::{prearm, zero};
use super::*;
use serde_json::{Value, json};

fn model() -> Value {
    serde_json::from_str(include_str!("../../testdata/excursion-model.json")).unwrap()
}
fn prepare(models: Vec<(&str, Value)>) -> Res<config::PreparedConfig> {
    let mut graph = serde_json::to_value(tests::configuration())?;
    let mut policies = json!({});
    for (id, m) in models {
        policies[id] = json!({"kind":"excursion","targets":[m["destination_id"]],"model":m});
    }
    graph["processors"]["guard"]["parameters"]["policies"] = policies;
    config::validate_config(serde_json::from_value(graph)?, None)
}
fn guard(conf: &config::PreparedConfig) -> Guard {
    let mut g = Guard::new(Arc::new(Shared::default()));
    assert!(g.install(conf));
    prearm(&mut g, 1);
    g
}
fn audio(x: f64) -> AudioChunk {
    let mut chunk = tests::chunk();
    chunk.waveforms[4].fill(x);
    chunk
}

// Independent scalar difference equation, obtained by substituting
// s=96000(1-q)/(1+q) in the continuous displacement transfer.
#[derive(Clone, Copy, Default)]
struct Reference {
    u: [f64; 4],
    z: [f64; 3],
}
impl Reference {
    fn step(&mut self, u: f64) -> f64 {
        self.step_in_box(u, 0.02)
    }

    fn step_in_box(&mut self, u: f64, volume: f64) -> f64 {
        let k = 1000. + 1.2 * 343. * 343. * 0.03 * 0.03 / volume;
        let a = 4. * k;
        let b = (4. + 0.0005 * k + 100.) * 96000.;
        let c = (4. * 0.05 + 0.0005) * 96000_f64.powi(2);
        let d = 0.0005 * 0.05 * 96000_f64.powi(3);
        let den = [
            a + b + c + d,
            3. * a + b - c - 3. * d,
            3. * a - b - c + 3. * d,
            a - b + c - d,
        ];
        self.u.rotate_right(1);
        self.u[0] = u;
        let next = (10000. * (self.u[0] + 3. * self.u[1] + 3. * self.u[2] + self.u[3])
            - den[1] * self.z[0]
            - den[2] * self.z[1]
            - den[3] * self.z[2])
            / den[0];
        self.z.rotate_right(1);
        self.z[0] = next;
        next
    }
}

#[test]
fn delayed_enable_crosses_after_forwarding_and_all_chunk_previews_settle() {
    let conf = prepare(vec![("travel", model())]).unwrap();
    let mut g = guard(&conf);
    let (mut digital, mut forwarded, mut terminal) = (
        Reference::default(),
        Reference::default(),
        Reference::default(),
    );
    let (mut max_digital, mut max_terminal, mut peak_frame) = (0_f64, 0_f64, 0);
    for start in (0..131072).step_by(FRAMES) {
        let mut chunk = audio(0.);
        let mut preview = forwarded;
        for (offset, x) in chunk.waveforms[4].iter_mut().enumerate() {
            let n = start + offset;
            *x = (n as f64 / 48000.).min(1.) * (std::f64::consts::TAU * n as f64 / 600.).cos();
            let u = 10. * *x;
            max_digital = max_digital
                .max(digital.step(u).abs())
                .max(preview.step(u).abs());
            let committed = forwarded.step(if n >= 96000 { u } else { 0. });
            max_digital = max_digital.max(committed.abs());
            let z = terminal.step(if n >= 120450 { u } else { 0. }).abs();
            if z > max_terminal {
                max_terminal = z;
                peak_frame = n;
            }
        }
        let original = chunk.waveforms.clone();
        g.process(&mut chunk);
        assert!(g.trip().is_none());
        if start >= 96000 {
            assert_eq!(chunk.waveforms, original);
        } else {
            assert!(zero(&chunk));
        }
    }
    assert!((max_digital + 0.05 - 2.0784).abs() < 0.0001);
    assert!((max_terminal - 2.1309).abs() < 0.0001);
    assert_eq!(peak_frame, 120450 + 315);
}

#[test]
fn first_qualifying_chunk_from_forwarded_silence_trips_after_pause() {
    let mut m = model();
    m["limit_mm_one_way"] = json!(2.4);
    let conf = prepare(vec![("travel", m)]).unwrap();
    let mut g = guard(&conf);
    for _ in 0..=375 {
        g.process(&mut audio(0.1));
    }
    assert!(g.qualification.is_active());
    g.pause();
    let base = g.stream.frames;
    assert!(!g.qualification.is_active());
    prearm(&mut g, 2);
    let mut candidate = Reference::default();
    for _ in 0..375 {
        let mut a = audio(-0.4);
        g.process(&mut a);
        assert!(zero(&a) && !g.qualification.is_active());
        for _ in 0..FRAMES {
            candidate.step(-4.);
        }
    }
    let mut preview = Reference::default();
    let expected = (0..FRAMES)
        .find(|_| {
            assert!(candidate.step(10.).abs() + 0.05 < 2.4);
            preview.step(10.).abs() + 0.05 >= 2.4
        })
        .unwrap();
    let mut a = audio(1.);
    g.process(&mut a);
    assert!(zero(&a));
    let trip = g.trip().unwrap();
    assert_eq!(
        (trip.reason, trip.frame),
        (Reason::Excursion, base + 96000 + expected as u64)
    );
}

#[test]
fn equality_domain_and_shared_lane_trips_survive_skipped_polls_and_rebinding() {
    let mut m = model();
    let bd = f64::from_bits(
        u64::from_str_radix(m["coefficient_words"][11].as_str().unwrap(), 16).unwrap(),
    );
    let equality = 0.05 + 1000. * (bd * 10.);
    for (limit, crosses) in [(equality, true), (equality.next_up(), false)] {
        m["limit_mm_one_way"] = json!(limit);
        let conf = prepare(vec![("travel", m.clone())]).unwrap();
        let mut g = guard(&conf);
        let mut a = audio(0.);
        a.waveforms[4][255] = 1.;
        g.process(&mut a);
        assert_eq!(
            g.trip().map(|t| (t.reason, t.frame)),
            crosses.then_some((Reason::Excursion, 255))
        );
    }
    let mut sibling: Value =
        serde_json::from_str(include_str!("../../testdata/excursion-sub-b.json")).unwrap();
    let conf = prepare(vec![("travel", model()), ("other", sibling.clone())]).unwrap();
    sibling["volts_peak_per_fs"] = json!(19.0);
    let changed = prepare(vec![("travel", model()), ("other", sibling)]).unwrap();
    let mut g = guard(&conf);
    let _playback = playback::Playback::new(g.shared.clone());
    let mut a = audio(0.);
    a.waveforms[4][37] = 0.6;
    g.process(&mut a);
    assert!(zero(&a));
    let trip = g.trip().unwrap();
    assert_eq!(
        (
            trip.reason,
            trip.destination_id.text(),
            trip.policy_id.text(),
            trip.frame
        ),
        (Reason::ModelDomain, Some("sub_b"), Some("other"), 37)
    );
    assert!(!g.install(&changed));
    for _ in 0..10 {
        g.process(&mut audio(0.));
    }
    assert_eq!(g.trip(), Some(trip));
    let status = wire::tests::status(&g.shared);
    assert_eq!(status["trip"]["reason"], "model_domain");
    let mut travel = guard(&conf);
    let _travel_playback = playback::Playback::new(travel.shared.clone());
    let mut reference = Reference::default();
    let mut other_load = Reference::default();
    let crossed = (0..FRAMES)
        .find(|_| {
            assert!(other_load.step(4.5).abs() + 0.05 < 2.1);
            reference.step_in_box(9., 0.024).abs() + 0.05 >= 2.1
        })
        .unwrap();
    let mut burst = audio(0.45);
    travel.process(&mut burst);
    let trip = travel.trip().unwrap();
    assert!(zero(&burst));
    assert_eq!(
        (trip.reason, trip.destination_id.text(), trip.frame),
        (Reason::Excursion, Some("sub_b"), crossed as u64)
    );
    for _ in 0..8 {
        travel.process(&mut audio(0.));
    }
    if let Ok(path) = std::env::var("EXCURSION_REPLAY") {
        std::fs::write(
            path,
            json!({"export":identity::export(&conf).unwrap(),"statuses":[status,wire::tests::status(&travel.shared)]}).to_string(),
        )
        .unwrap();
    }
}

#[test]
fn refuse_incompatible_role_wrong_words_and_insufficient_settling() {
    for (key, v) in [
        ("settle_blocks", json!(1)),
        ("destination_id", json!("tw_l")),
        ("sample_rate_hz", json!(96000)),
        ("limit_mm_one_way", json!(700.)),
        ("loading", json!("coupled")),
    ] {
        let mut m = model();
        m[key] = v;
        assert!(prepare(vec![("travel", m)]).is_err());
    }
    let mut m = model();
    m["coefficient_words"][1] = json!("bfd8999a57138cd7");
    assert!(prepare(vec![("travel", m)]).is_err());
}

#[test]
fn tiny_gain_preserves_the_first_excursion_crossing() {
    let mut m = model();
    let gain = 6.0e-306_f64;
    let sample = 1.0 / 1024.0;
    let margin = 1.0e-315_f64;
    let bd = f64::from_bits(
        u64::from_str_radix(m["coefficient_words"][11].as_str().unwrap(), 16).unwrap(),
    );
    let volts = gain * sample;
    let displacement = 1000.0 * (bd * volts);
    assert!(volts.is_subnormal() && displacement.is_subnormal());
    let edge = displacement + margin;
    m["volts_peak_per_fs"] = json!(gain);
    m["terminal_peak_bound_v"] = json!(volts);
    m["validated_error_mm"] = json!(0.0);
    m["error_margin_mm"] = json!(margin);
    for (limit, crosses) in [(edge, true), (edge.next_up(), false)] {
        m["limit_mm_one_way"] = json!(limit);
        let conf = prepare(vec![("travel", m.clone())]).unwrap();
        for sign in [-1.0, 1.0] {
            let mut g = guard(&conf);
            let mut a = audio(0.0);
            a.waveforms[4][FRAMES - 1] = sign * sample;
            g.process(&mut a);
            assert_eq!(
                g.trip().map(|t| (t.reason, t.frame)),
                crosses.then_some((Reason::Excursion, FRAMES as u64 - 1))
            );
        }
    }
}
