use super::*;
use control::Result as ControlResult;
use lifecycle_tests::{command, prearm, zero};
use serde_json::{Value, json};
use std::f64::consts::{PI, TAU};
use tests::{chunk, configuration};

const W: usize = 64;
const K: usize = 3;
const LIMIT: f64 = 0.125;

fn policies() -> Value {
    json!({"bias":{"kind":"signed_dc","targets":["sub_a"],"limit_fs": LIMIT,
                  "window_blocks":W,"same_sign_windows":K}})
}

fn prepare(policies: Value) -> Res<config::PreparedConfig> {
    let mut graph = serde_json::to_value(configuration())?;
    graph["processors"]["guard"]["parameters"]["policies"] = policies;
    config::validate_config(serde_json::from_value(graph)?, None)
}

fn guard(policies: Value) -> Guard {
    let mut guard = Guard::new(Arc::new(Shared::default()));
    assert!(guard.install(&prepare(policies).unwrap()));
    prearm(&mut guard, 1);
    guard
}

fn candidate(bias: f64) -> AudioChunk {
    let mut audio = chunk();
    audio.waveforms[4].fill(bias);
    audio
}

#[test]
fn signed_dc_admission_binds_both_hashes_and_alignment_budget() {
    let reference = identity::export(&prepare(policies()).unwrap()).unwrap();
    assert_eq!(
        reference["profile"]["qualification_frames"],
        FRAMES * W * (K + 1)
    );
    for (key, value) in [
        ("window_blocks", json!(192)),
        ("same_sign_windows", json!(8)),
        ("limit_fs", json!(1.0)),
        ("targets", json!(["sub_b"])),
        ("targets", json!(["mid_l"])),
        ("targets", json!(["tw_l"])),
    ] {
        let mut p = policies();
        p["bias"][key] = value;
        let changed = identity::export(&prepare(p).unwrap()).unwrap();
        for digest in ["graph_sha256", "profile_sha256"] {
            assert_ne!(reference[digest], changed[digest], "{key}");
        }
    }
    for (key, value) in [
        ("window_blocks", json!(63)),
        ("window_blocks", json!(193)),
        ("window_blocks", json!(64.0)),
        ("same_sign_windows", json!(1)),
        ("same_sign_windows", json!(9)),
        ("same_sign_windows", json!(true)),
        ("limit_fs", json!(0.0)),
        ("limit_fs", json!(1.01)),
    ] {
        let mut p = policies();
        p["bias"][key] = value;
        assert!(prepare(p).is_err(), "{key}");
    }
}

#[test]
fn final_music_with_signed_offset_trips_on_oracle_window_and_contains_pcm() {
    let mut replies = Vec::new();
    let mut p = policies();
    p["freeze"] = json!({"kind":"frozen","targets":["sub_a"],"floor_dbfs":-60.0,
        "crest_tolerance_db":1.0,"quantum_db":0.001,"hold_s":0.02});
    let conf = prepare(p).unwrap();
    let export = identity::export(&conf).unwrap();
    for polarity in [-1, 1] {
        let mut g = Guard::new(Arc::new(Shared::default()));
        assert!(g.install(&conf));
        prearm(&mut g, 1);
        let _playback = playback::Playback::new(g.shared.clone());
        // A full aligned clean prelude leaves the guard active in PREARM.
        for b in 0..(K + 2) * W {
            g.process(&mut candidate(0.0));
            assert_eq!(g.qualification.is_active(), b >= (K + 1) * W);
        }
        assert!(g.qualification.is_active());
        let base = g.stream.frames;
        let origin = g.configuration.as_ref().unwrap().identity.profile;
        let mut window_sum = 0.0;
        let mut crossings = 0;
        for b in 0..(K + 1) * W {
            let mut audio = candidate(0.0);
            for (i, x) in audio.waveforms[4].iter_mut().enumerate() {
                let n = b * FRAMES + i;
                let t = n as f64 / SAMPLE_RATE as f64;
                let music = 0.08 * (TAU * 83.0 * t).sin() + 0.05 * (TAU * 131.0 * t).sin();
                let offset = if n < W * FRAMES - 119 {
                    0.0
                } else {
                    0.18 + 0.025 * (b / W) as f64
                };
                *x = music + f64::from(polarity) * offset;
                window_sum += *x;
            }
            let full = (b + 1) % W == 0;
            if full {
                let mean = window_sum / (W * FRAMES) as f64;
                if mean * f64::from(polarity) >= LIMIT {
                    crossings += 1;
                } else {
                    crossings = 0;
                }
                window_sum = 0.0;
            }
            let decisive = crossings == K;
            let first_veto = polarity < 0 && full && crossings == 1;
            let arm = (polarity > 0 && b == 0 || first_veto || polarity < 0 && decisive)
                .then(|| command(&g, Operation::Context(Mode::Armed), 1));
            if decisive {
                assert!(
                    g.shared
                        .protection_reply(r#"{"StartProtectionTestCapture":1}"#)
                        .unwrap()
                        .contains("Ok")
                );
            }
            let before = audio.waveforms.clone();
            g.process_with_command(&mut audio, arm);
            if first_veto {
                assert_eq!(
                    g.control.last_command.unwrap().result,
                    ControlResult::NotQualified
                );
            }
            if !decisive {
                assert!(g.trip().is_none());
                if polarity > 0 || crossings == 0 {
                    assert_eq!(audio.waveforms, before);
                } else {
                    assert!(g.detector.prearm_veto && zero(&audio));
                }
                continue;
            }
            assert_eq!(b + 1, (K + 1) * W);
            let status = wire::tests::status(&g.shared);
            let trip = &status["trip"];
            assert_eq!(trip["frame"], base + ((b + 1) * FRAMES) as u64);
            assert_eq!(trip["origin_profile_sha256"], identity::hex(origin));
            assert_eq!(trip["reason"], "signed_dc");
            assert_eq!(trip["polarity"], polarity);
            assert_eq!(trip["channel"], 4);
            assert_eq!(trip["destination_id"], "sub_a");
            assert_eq!(trip["policy_id"], "bias");
            if polarity < 0 {
                assert_eq!(
                    g.control.last_command.unwrap().result,
                    ControlResult::StateMismatch // The trip advanced the generation before ARM.
                );
            }
            assert!(zero(&audio));
            let mut pcm = vec![0xff; CHANNELS * FRAMES * 3];
            let stamp = audio.protection.take().unwrap();
            let begin = monotonic_ns();
            let (bytes, clipped) = crate::utils::conversions::chunk_to_buffer_rawbytes(
                audio,
                &mut pcm,
                &config::BinarySampleFormat::S24_3_LE,
            );
            assert_eq!((bytes, clipped), (pcm.len(), 0));
            assert!(pcm.iter().all(|b| *b == 0));
            g.shared
                .capture
                .converted(&stamp, clipped, &pcm, begin, monotonic_ns());
            let raw = g
                .shared
                .protection_reply(r#"{"StopProtectionTestCapture":1}"#)
                .unwrap();
            let envelope: Value = serde_json::from_str(&raw).unwrap();
            let capture = &envelope["StopProtectionTestCapture"]["value"];
            assert_eq!(capture["complete"], true);
            let captured = &capture["records"][0]["trip"];
            for field in [
                "generation",
                "origin_profile_sha256",
                "reason",
                "polarity",
                "destination_id",
                "policy_id",
                "channel",
                "frame",
                "at_ns",
            ] {
                assert_eq!(captured[field], trip[field], "{field}");
            }
            for _ in 0..2 {
                let mut clean = candidate(0.0);
                g.process(&mut clean);
                assert!(zero(&clean));
            }
            assert_eq!(wire::tests::status(&g.shared)["trip"], *trip);
            // The idle playback owner deliberately supplies no health evidence.
            replies.push(status);
        }
    }
    if let Ok(path) = std::env::var("SIGNED_DC_REPLAY") {
        let slice = json!({"export":export, "statuses":replies});
        std::fs::write(path, serde_json::to_string_pretty(&slice).unwrap() + "\n").unwrap();
    }
}

#[test]
fn full_windows_hold_veto_until_a_miss_and_restart_on_reversal_or_gap() {
    let mut g = guard(policies());
    // Finish two positive windows at equality, reverse on the third, and miss
    // by one representable step. A partial clean window cannot clear the veto.
    for (bias, veto) in [
        (LIMIT, true),
        (LIMIT, true),
        (-LIMIT, true),
        (-LIMIT.next_down(), false),
        (LIMIT, true),
        (0.0, false),
        (-LIMIT, true),
        (-LIMIT, true),
    ] {
        let prior = g.detector.prearm_veto;
        for b in 1..=W {
            g.process(&mut candidate(bias));
            assert_eq!(g.detector.prearm_veto, if b == W { veto } else { prior });
            assert!(g.trip().is_none());
        }
    }
    // Both a gap between windows and one inside a window retire all DC evidence.
    for partial in [0, W - 1] {
        for _ in 0..partial {
            g.process(&mut candidate(-LIMIT));
        }
        g.pause();
        for _ in 0..(K - 1) * W {
            g.process(&mut candidate(-LIMIT));
            assert!(g.trip().is_none());
        }
    }
    for _ in 1..W {
        g.process(&mut candidate(-LIMIT));
        assert!(g.trip().is_none());
    }
    g.process(&mut candidate(-LIMIT));
    assert_eq!(g.trip().unwrap().reason, Reason::NegativeDc);
}

#[test]
fn incomplete_means_and_masked_candidates_cannot_arm_but_visible_bias_still_trips() {
    let mut g = guard(policies());
    for _ in 2..W {
        g.process(&mut candidate(LIMIT));
    }
    let arm = command(&g, Operation::Context(Mode::Armed), 1);
    g.process_with_command(&mut candidate(LIMIT), Some(arm));
    assert_eq!(
        g.control.last_command.unwrap().result,
        ControlResult::NotQualified
    );
    assert!(!g.detector.prearm_veto && g.trip().is_none());
    g.pause();
    prearm(&mut g, 2);
    for b in 0..(2 * K + 1) * W {
        let biased = b >= (K + 1) * W;
        let mut audio = candidate(if biased { LIMIT } else { 0.0 });
        audio.main_volume.as_mut().unwrap().muted = true;
        let arm = command(&g, Operation::Context(Mode::Armed), 2);
        g.process_with_command(&mut audio, Some(arm));
        assert_ne!(
            g.control.last_command.unwrap().result,
            ControlResult::Applied
        );
        assert!(zero(&audio));
        assert_eq!(g.qualification.frames(), 0);
        assert_eq!(g.trip().is_some(), b + 1 == (2 * K + 1) * W);
    }
    assert_eq!(g.trip().unwrap().reason, Reason::PositiveDc);
}

#[test]
fn large_cancellation_residual_reaches_the_inclusive_threshold() {
    let mut g = guard(policies());
    for _ in 0..K * W {
        let mut audio = candidate(0.0);
        // Exact mean is 0.125. Naive summation loses every 0.5.
        for four in audio.waveforms[4].chunks_exact_mut(4) {
            four.copy_from_slice(&[1e16, 0.5, -1e16, 0.0]);
        }
        g.process(&mut audio);
    }
    assert_eq!(g.trip().unwrap().reason, Reason::PositiveDc);
}

#[test]
fn phase_swept_bass_stays_within_the_discrete_mean_bound() {
    let blocks = 96;
    let n = (blocks * FRAMES) as f64;
    for frequency in [20.0, 27.4, 63.5] {
        let omega = PI * frequency / SAMPLE_RATE as f64;
        let bound = (n * omega).sin().abs() / (n * omega.sin());
        assert!(bound < 0.05);
        for phase in [0.0, 0.4, 1.5, 2.8] {
            let mut p = policies();
            p["bias"]["window_blocks"] = json!(blocks);
            p["bias"]["limit_fs"] = json!(0.05);
            let mut g = guard(p);
            let mut sum = 0.0;
            for b in 0..(K + 2) * blocks {
                let mut audio = candidate(0.0);
                for (i, x) in audio.waveforms[4].iter_mut().enumerate() {
                    *x = (2.0 * omega * (b * FRAMES + i) as f64 + phase).sin();
                    sum += *x;
                }
                if (b + 1) % blocks == 0 {
                    assert!(sum.abs() / n <= bound + 1e-13);
                    sum = 0.0;
                }
                g.process(&mut audio);
                assert!(!g.detector.prearm_veto && g.trip().is_none());
            }
            assert!(g.qualification.is_active());
        }
    }
}

#[test]
fn frozen_zero_mean_and_upstream_dc_removed_by_the_crossover() {
    use crate::filters::{Filter, biquadcombo::BiquadCombo};
    let mut p = policies();
    p["bias"]["targets"] = json!(["tw_l"]);
    let mut dc = guard(p.clone());
    p["freeze"] = json!({"kind":"frozen","targets":["tw_l"],"floor_dbfs":-60.0,
                         "crest_tolerance_db":1.0,"quantum_db":0.001,"hold_s":0.02});
    let mut frozen = guard(p.clone());
    let mut crossed = guard(p);
    let mut hp = BiquadCombo::from_config(
        "tweeter",
        SAMPLE_RATE,
        config::BiquadComboParameters::LinkwitzRileyHighpass {
            freq: 3000.0,
            order: 4,
        },
    );
    for _ in 0..(K + 2) * W {
        for owner in [&mut dc, &mut frozen] {
            let mut audio = candidate(0.0);
            for pair in audio.waveforms[0].chunks_exact_mut(2) {
                pair.copy_from_slice(&[-0.5, 0.5]);
            }
            owner.process(&mut audio);
        }
        let mut audio = candidate(0.0);
        audio.waveforms[0].fill(0.5);
        hp.process_waveform(&mut audio.waveforms[0]).unwrap();
        assert!(audio.waveforms[0].iter().sum::<f64>().abs() / (FRAMES as f64) < LIMIT);
        crossed.process(&mut audio);
        assert!(!crossed.detector.prearm_veto && crossed.trip().is_none());
        assert!(!dc.detector.prearm_veto && dc.trip().is_none());
    }
    assert!(dc.qualification.is_active() && crossed.qualification.is_active());
    assert_eq!(frozen.trip().unwrap().reason, Reason::DcFrozen);
}

#[test]
fn shared_lane_keeps_independent_windows_and_signed_ties_use_policy_id() {
    // The last 64-block mean is negative while the overlapping 96-block
    // mean is positive. Both destinations reach their second hit at block 192.
    for (negative_id, expected) in [
        ("a_fast", Reason::NegativeDc),
        ("z_fast", Reason::PositiveDc),
    ] {
        let p = json!({
            negative_id: {"kind":"signed_dc","targets":["sub_a"],"limit_fs":0.125,"window_blocks":64,"same_sign_windows":2},
            "slow": {"kind":"signed_dc","targets":["sub_b"],"limit_fs":0.125,"window_blocks":96,"same_sign_windows":2},
        });
        let mut g = guard(p);
        for b in 0..192 {
            let bias = match b {
                0..64 => 0.9,
                64..96 => -0.95,
                96..128 => 0.65,
                _ => -0.125,
            };
            g.process(&mut candidate(bias));
            assert_eq!(g.trip().is_some(), b == 191);
        }
        let trip = g.trip().unwrap();
        assert_eq!(trip.reason, expected);
        assert_eq!(
            trip.destination_id.text(),
            Some(if expected == Reason::NegativeDc {
                "sub_a"
            } else {
                "sub_b"
            })
        );
    }
}

#[test]
fn equality_and_a_one_sample_miss_survive_nonbinary_window_and_subnormal_limit() {
    for limit in [0.99, f64::from_bits(1)] {
        for sign in [-1.0, 1.0] {
            let mut p = policies();
            p["bias"]["window_blocks"] = json!(65);
            p["bias"]["same_sign_windows"] = json!(2);
            p["bias"]["limit_fs"] = json!(limit);
            let mut g = guard(p);
            for b in 0..4 * 65 {
                let mut audio = candidate(sign * limit);
                // One sample one ULP nearer zero makes each of the first two
                // exact window means strictly below the threshold magnitude.
                if b == 0 || b == 65 {
                    audio.waveforms[4][0] = sign * limit.next_down();
                }
                g.process(&mut audio);
                assert_eq!(g.detector.prearm_veto, b + 1 >= 3 * 65);
                assert_eq!(g.trip().is_some(), b + 1 == 4 * 65);
            }
        }
    }
}
