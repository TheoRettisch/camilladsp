//! Fixed-size sample statistics and audio-time policy, owned by the guard.

use super::{FRAMES, Parameters, Reason, SAMPLE_RATE};
use crate::{PrcFmt, Res, config};

fn frames(seconds: f64) -> u64 {
    (seconds * SAMPLE_RATE as f64).ceil() as u64
}

pub(super) fn validate(p: &Parameters) -> Res<()> {
    let durations = [
        p.dc_freeze_seconds,
        p.tweeter_rms_fault_seconds,
        p.tweeter_avg_power_tau_seconds * 2.0,
        p.tweeter_long_avg_power_tau_seconds,
        p.tone_fault_seconds,
    ];
    let limits = [
        p.max_tweeter_rms_db,
        p.max_tweeter_avg_power_db,
        p.max_tweeter_long_avg_power_db,
        p.tone_min_rms_db,
        Some(p.silence_threshold_db),
    ];
    if durations
        .iter()
        .any(|x| !x.is_finite() || *x <= 0.0 || *x * SAMPLE_RATE as f64 >= i64::MAX as f64)
        || [
            p.dc_freeze_peak_rms_tolerance_db,
            p.level_freshness_epsilon_db,
            p.tone_crest_db,
        ]
        .iter()
        .any(|x| !x.is_finite() || *x <= 0.0)
        || limits.iter().flatten().any(|x| {
            !x.is_finite() || *x >= p.max_allowed_peak_db || 10.0_f64.powf(*x / 10.0) == 0.0
        })
        || p.max_tweeter_long_avg_power_db.is_some_and(|long| {
            p.max_tweeter_avg_power_db.is_none_or(|short| long >= short)
                || p.tweeter_long_avg_power_tau_seconds <= p.tweeter_avg_power_tau_seconds
        })
    {
        return Err(
            config::ConfigError::new("Invalid protection policy thresholds or windows").into(),
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct History {
    pub short_power: f64,
    pub long_power: f64,
    rms_frames: u64,
    tone_frames: u64,
    dc_frames: u64,
    signature: Option<(f64, f64)>,
}

impl History {
    pub fn break_continuity(&mut self) {
        self.rms_frames = 0;
        self.tone_frames = 0;
        self.dc_frames = 0;
        self.signature = None;
    }
}

#[derive(Debug, Default)]
pub(super) struct Policy {
    rms_limit: Option<f64>,
    short_limit: Option<f64>,
    long_limit: Option<f64>,
    tone_limit: Option<f64>,
    rms_frames: u64,
    tone_frames: u64,
    dc_frames: u64,
    short_alpha: f64,
    prearm_alpha: f64,
    long_alpha: f64,
    tone_crest: f64,
    dc_tolerance: f64,
    epsilon: f64,
    pub qualification_frames: u64,
    pub silence_power: f64,
}

pub(super) struct Assessment {
    pub reason: Option<Reason>,
    pub veto: bool,
}

fn consecutive(total: &mut u64, condition: bool, limit: u64) -> bool {
    *total = if condition {
        total.saturating_add(FRAMES as u64).min(limit)
    } else {
        0
    };
    condition && *total >= limit
}

fn update_power(prior: &mut f64, q: f64, alpha: f64, observable: bool) -> f64 {
    let next = *prior + alpha * (q - *prior);
    *prior = if observable { next } else { prior.max(next) };
    *prior
}

impl Policy {
    pub fn new(p: &Parameters) -> Self {
        let power = |db: f64| 10.0_f64.powf(db / 10.0);
        let alpha = |tau: f64| -(-(FRAMES as f64 / SAMPLE_RATE as f64) / tau).exp_m1();
        let mut qualification = p.dc_freeze_seconds;
        if p.max_tweeter_rms_db.is_some() {
            qualification = qualification.max(p.tweeter_rms_fault_seconds);
        }
        if p.max_tweeter_avg_power_db.is_some() {
            qualification = qualification.max(2.0 * p.tweeter_avg_power_tau_seconds);
        }
        if p.tone_min_rms_db.is_some() {
            qualification = qualification.max(p.tone_fault_seconds);
        }
        Self {
            rms_limit: p.max_tweeter_rms_db.map(power),
            short_limit: p.max_tweeter_avg_power_db.map(power),
            long_limit: p.max_tweeter_long_avg_power_db.map(power),
            tone_limit: p.tone_min_rms_db.map(power),
            rms_frames: frames(p.tweeter_rms_fault_seconds),
            tone_frames: frames(p.tone_fault_seconds),
            dc_frames: frames(p.dc_freeze_seconds),
            short_alpha: alpha(p.tweeter_avg_power_tau_seconds),
            prearm_alpha: alpha(p.tweeter_avg_power_tau_seconds / 2.5),
            long_alpha: alpha(p.tweeter_long_avg_power_tau_seconds),
            tone_crest: p.tone_crest_db,
            dc_tolerance: p.dc_freeze_peak_rms_tolerance_db,
            epsilon: p.level_freshness_epsilon_db,
            qualification_frames: frames(qualification),
            silence_power: power(p.silence_threshold_db),
        }
    }

    pub fn assess(
        &self,
        h: &mut History,
        samples: &[PrcFmt],
        prearm: bool,
        observable: bool,
    ) -> Assessment {
        let peak = samples.iter().fold(0.0_f64, |p, x| p.max(x.abs() as f64));
        let q = samples
            .iter()
            .map(|x| (*x as f64) * (*x as f64))
            .sum::<f64>()
            / FRAMES as f64;
        if !q.is_finite() {
            return Assessment {
                reason: Some(Reason::Nonfinite),
                veto: false,
            };
        }
        let mut reason = None;
        if consecutive(
            &mut h.rms_frames,
            self.rms_limit.is_some_and(|limit| q >= limit),
            self.rms_frames,
        ) {
            reason = Some(Reason::Rms);
        }
        if let Some(limit) = self.short_limit {
            let e = update_power(
                &mut h.short_power,
                q,
                if prearm {
                    self.prearm_alpha
                } else {
                    self.short_alpha
                },
                observable,
            );
            if e >= limit {
                reason = reason.or(Some(Reason::ShortPower));
            }
        }
        if let Some(limit) = self.long_limit {
            if update_power(&mut h.long_power, q, self.long_alpha, observable) >= limit {
                reason = reason.or(Some(Reason::LongPower));
            }
        }
        let peak_db = 20.0 * peak.log10();
        let rms_db = 10.0 * q.log10();
        let crest = peak_db - rms_db;
        if consecutive(
            &mut h.tone_frames,
            self.tone_limit.is_some_and(|limit| q >= limit) && crest <= self.tone_crest,
            self.tone_frames,
        ) {
            reason = reason.or(Some(Reason::Tone));
        }
        let signature = if q > self.silence_power
            && peak * peak > self.silence_power
            && crest.abs() <= self.dc_tolerance
        {
            Some((
                (peak_db / self.epsilon).round_ties_even(),
                (rms_db / self.epsilon).round_ties_even(),
            ))
        } else {
            None
        };
        if consecutive(
            &mut h.dc_frames,
            signature.is_some() && signature == h.signature,
            self.dc_frames,
        ) {
            reason = reason.or(Some(Reason::DcFrozen));
        }
        h.signature = signature;
        Assessment {
            reason,
            veto: prearm && self.short_limit.is_some_and(|limit| q > limit),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protection::{
        Guard, Shared,
        control::{Command, Mode, Operation, Result as ControlResult},
        tests::{chunk, configuration, parameters},
    };
    use std::sync::Arc;

    fn setup(p: Parameters) -> Guard {
        let mut conf = serde_json::to_value(configuration()).unwrap();
        conf["processors"]["guard"]["parameters"] = serde_json::to_value(p).unwrap();
        let mut conf = serde_json::from_value(conf).unwrap();
        crate::config::validate_config(&mut conf, None).unwrap();
        let mut guard = Guard::new(Arc::new(Shared::default()));
        guard.install(&conf).unwrap();
        guard.activation_inhibited = false;
        context(&mut guard, Operation::Context(Mode::Prearm), 1);
        guard
    }

    fn context(g: &mut Guard, operation: Operation, qualification_id: u64) {
        let command = Command {
            id: g.control.command_high_water + 1,
            expected: g.expected(),
            owner: 7,
            parent_reset_generation: g.control.parent_reset_generation
                + u64::from(operation == Operation::Reset),
            qualification_id,
            operation,
        };
        g.control_only(command);
        assert_eq!(
            g.control.last_command.unwrap().result,
            ControlResult::Applied
        );
    }

    fn input(channel: usize, q: f64, dc: bool) -> crate::audiochunk::AudioChunk {
        let mut audio = chunk();
        for w in &mut audio.waveforms {
            w.fill(0.0);
        }
        for (i, x) in audio.waveforms[channel].iter_mut().enumerate() {
            *x = if dc {
                q.sqrt()
            } else if i % 2 == 0 {
                (2.0 * q).sqrt()
            } else {
                0.0
            };
        }
        audio
    }

    #[test]
    fn rms_and_tone_threshold_duration_and_each_channel() {
        for channel in 0..2 {
            for tone in [false, true] {
                let threshold = 10.0 * 0.0625_f64.log10();
                for offset in [-0.01, 0.0, 0.01] {
                    let db = threshold + offset;
                    let mut p = parameters();
                    if tone {
                        p.tone_min_rms_db = Some(threshold);
                        p.tone_fault_seconds = 0.01001;
                    } else {
                        p.max_tweeter_rms_db = Some(threshold);
                        p.tweeter_rms_fault_seconds = 0.01001;
                    }
                    let mut g = setup(p);
                    // Use exactly the compiled limit at equality; samples have
                    // constant power and remain below the instantaneous ceiling.
                    let q = 10.0_f64.powf(db / 10.0);
                    let mut audio = input(channel, q, true);
                    g.process(&mut audio);
                    assert!(g.trip.is_none());
                    g.process(&mut input(channel, q, true));
                    if offset < 0.0 {
                        assert!(g.trip.is_none());
                    } else {
                        let trip = g.trip.unwrap();
                        assert_eq!(trip.channel, Some(channel));
                        assert_eq!(trip.reason, if tone { Reason::Tone } else { Reason::Rms });
                        assert_eq!(trip.frame, 512);
                    }
                }
            }
        }
    }

    #[test]
    fn consecutive_policies_reset_on_below_limit_and_pause() {
        let mut p = parameters();
        p.max_tweeter_rms_db = Some(-20.0);
        p.tweeter_rms_fault_seconds = 0.010;
        let mut g = setup(p);
        for _ in 0..5 {
            g.process(&mut input(1, 0.02, false));
            g.process(&mut input(1, 0.001, false));
            assert!(g.trip.is_none());
        }
        g.process(&mut input(1, 0.02, false));
        g.pause();
        g.process(&mut input(1, 0.02, false));
        assert!(g.trip.is_none());
        g.process(&mut input(1, 0.02, false));
        assert_eq!(g.trip.unwrap().reason, Reason::Rms);
    }

    #[test]
    fn crest_and_dc_tolerance_boundaries_are_inclusive() {
        let samples = input(0, 0.125, false).waveforms.remove(0);
        let crest = 20.0 * 0.5_f64.log10() - 10.0 * 0.125_f64.log10();
        for tone in [false, true] {
            for offset in [-0.01, 0.0, 0.01] {
                let mut p = parameters();
                p.dc_freeze_seconds = FRAMES as f64 / SAMPLE_RATE as f64;
                if tone {
                    p.tone_min_rms_db = Some(-20.0);
                    p.tone_fault_seconds = p.dc_freeze_seconds;
                    p.tone_crest_db = crest + offset;
                } else {
                    p.dc_freeze_peak_rms_tolerance_db = crest + offset;
                }
                let policy = Policy::new(&p);
                let mut h = History::default();
                let mut result = policy.assess(&mut h, &samples, true, true);
                if !tone {
                    assert!(result.reason.is_none()); // Signature baseline.
                    result = policy.assess(&mut h, &samples, true, true);
                }
                assert_eq!(
                    result.reason,
                    if offset < 0.0 {
                        None
                    } else {
                        Some(if tone { Reason::Tone } else { Reason::DcFrozen })
                    }
                );
            }
        }
    }

    #[test]
    fn sustained_candidate_hazard_trips_even_when_mute_blocks_observability() {
        let mut p = parameters();
        p.max_tweeter_rms_db = Some(-20.0);
        p.tweeter_rms_fault_seconds = 0.010;
        let mut g = setup(p);
        for _ in 0..2 {
            let mut audio = input(1, 0.02, false);
            audio.main_volume.as_mut().unwrap().muted = true;
            g.process(&mut audio);
            assert!(!g.candidate_observable && !g.active);
            assert!(audio.waveforms.iter().flatten().all(|x| *x == 0.0));
        }
        assert_eq!(g.trip.unwrap().reason, Reason::Rms);
    }

    #[test]
    fn dc_signature_baseline_quantization_change_and_silence() {
        let mut p = parameters();
        p.dc_freeze_seconds = 0.01001;
        let mut g = setup(p);
        for _ in 0..10 {
            g.process(&mut input(0, 0.0, true));
            assert!(g.trip.is_none());
        }
        g.process(&mut input(0, 0.01, true)); // baseline
        g.process(&mut input(0, 0.01 * 10.0_f64.powf(0.01 / 10.0), true)); // same quantized pair
        assert!(g.trip.is_none());
        g.process(&mut input(0, 0.02, true)); // changed signature
        g.process(&mut input(0, 0.02, true));
        assert!(g.trip.is_none());
        let mut last = input(0, 0.02, true);
        g.process(&mut last);
        assert_eq!(g.trip.unwrap().reason, Reason::DcFrozen);
        assert!(last.waveforms.iter().flatten().all(|x| *x == 0.0));
        assert_eq!((2.5_f64).round_ties_even(), 2.0);
        assert_eq!((3.5_f64).round_ties_even(), 4.0);
    }

    #[test]
    fn short_power_audio_time_matches_closed_form_and_long_is_not_accelerated() {
        let mut p = parameters();
        p.max_tweeter_avg_power_db = Some(-10.0);
        p.max_tweeter_long_avg_power_db = Some(-20.0);
        let policy = Policy::new(&p);
        assert_eq!(policy.qualification_frames, 480_000); // not 180 seconds
        for prearm in [false, true] {
            let mut h = History::default();
            for _ in 0..1000 {
                policy.assess(&mut h, &[0.1; FRAMES], prearm, true);
            }
            let dt = 1000.0 * FRAMES as f64 / SAMPLE_RATE as f64;
            assert!(
                (h.short_power
                    - 0.01 * (1.0 - (-dt / (5.0 / if prearm { 2.5 } else { 1.0 })).exp()))
                .abs()
                    < 1e-14
            );
            assert!((h.long_power - 0.01 * (1.0 - (-dt / 90.0).exp())).abs() < 1e-14);
        }
    }

    #[test]
    fn power_thresholds_are_inclusive_and_veto_is_strict() {
        let mut p = parameters();
        p.max_tweeter_avg_power_db = Some(-10.0);
        p.max_tweeter_long_avg_power_db = Some(-20.0);
        let mut policy = Policy::new(&p);
        // Exact fixed point: update arithmetic adds zero at equality.
        let q = 0.0625;
        for long in [false, true] {
            policy.short_limit = Some(if long { 1.0 } else { q });
            policy.long_limit = Some(if long { q } else { 1.0 });
            for offset in [-1e-6, 0.0, 1e-6] {
                let mut h = History {
                    short_power: q,
                    long_power: q,
                    ..History::default()
                };
                if long {
                    policy.long_limit = Some(q + offset);
                } else {
                    policy.short_limit = Some(q + offset);
                }
                let a = policy.assess(&mut h, &[0.25; FRAMES], true, true);
                assert_eq!(
                    a.reason,
                    if offset <= 0.0 {
                        Some(if long {
                            Reason::LongPower
                        } else {
                            Reason::ShortPower
                        })
                    } else {
                        None
                    }
                );
                assert_eq!(a.veto, !long && offset < 0.0);
            }
        }
    }

    #[test]
    fn marginal_overload_veto_survives_skipped_reads_and_requires_full_window() {
        for overload in [0.01, 0.05] {
            let mut p = parameters();
            p.max_tweeter_avg_power_db = Some(-20.0);
            let mut g = setup(p);
            let q = 10.0_f64.powf((-20.0 + overload) / 10.0);
            for _ in 0..=480_000 / 256 {
                let mut audio = input(1, q, false);
                g.process(&mut audio);
                assert!(!g.active && g.short_power_veto);
                assert!(audio.waveforms.iter().flatten().all(|x| *x == 0.0));
            }
            if overload == 0.01 {
                assert!(g.trip.is_none());
                let heat = g.history[1].short_power;
                context(&mut g, Operation::Context(Mode::Prearm), 2);
                assert_eq!(g.history[1].short_power, heat);
                for _ in 0..480_000 / 256 {
                    g.process(&mut input(1, 0.0, false));
                    assert!(!g.active);
                }
                g.process(&mut input(1, 0.0, false));
                assert!(g.active);
            } else {
                assert_eq!(g.trip.unwrap().reason, Reason::ShortPower);
            }
        }
    }

    #[test]
    fn masked_audio_and_pause_never_cool_and_only_reset_clears_heat() {
        let mut p = parameters();
        p.max_tweeter_avg_power_db = Some(-10.0);
        p.max_tweeter_long_avg_power_db = Some(-20.0);
        let mut g = setup(p);
        for _ in 0..10 {
            g.process(&mut input(0, 0.005, false));
        }
        let heat = (g.history[0].short_power, g.history[0].long_power);
        for _ in 0..100 {
            let mut masked = input(0, 0.0, false);
            masked.main_volume.as_mut().unwrap().muted = true;
            g.process(&mut masked);
        }
        g.pause();
        context(&mut g, Operation::Context(Mode::Prearm), 2);
        assert_eq!(heat, (g.history[0].short_power, g.history[0].long_power));
        g.install(&configuration()).unwrap();
        assert_eq!(heat, (g.history[0].short_power, g.history[0].long_power));
        g.shared
            .backend
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        g.backend_wakeup();
        assert_eq!(heat, (g.history[0].short_power, g.history[0].long_power));
        context(&mut g, Operation::Context(Mode::Prearm), 3);
        context(&mut g, Operation::Reset, 4);
        assert_eq!(
            (g.history[0].short_power, g.history[0].long_power),
            (0.0, 0.0)
        );
    }

    #[test]
    fn post_hold_context_preserves_heat_and_restarts_complete_window() {
        let mut p = parameters();
        p.max_tweeter_avg_power_db = Some(-10.0);
        p.tweeter_avg_power_tau_seconds = 0.02;
        p.dc_freeze_seconds = 0.01;
        let mut g = setup(p);
        context(&mut g, Operation::Reset, 2);
        for _ in 0..10 {
            g.process(&mut input(0, 0.01, false));
        }
        assert!(g.active);
        let heat = g.history[0].short_power;
        context(&mut g, Operation::Context(Mode::Prearm), 3);
        assert!(!g.active);
        assert_eq!(g.qualified_frames, 0);
        assert_eq!(g.history[0].short_power, heat);
        for _ in 0..8 {
            g.process(&mut input(0, 0.001, false));
            assert!(!g.active);
        }
        g.process(&mut input(0, 0.001, false));
        assert!(g.active);
    }

    #[test]
    fn prearm_veto_revokes_local_active_and_rejects_same_chunk_armed_command() {
        let mut p = parameters();
        p.max_tweeter_avg_power_db = Some(-20.0);
        p.tweeter_avg_power_tau_seconds = 0.02;
        p.dc_freeze_seconds = 0.01;
        let mut g = setup(p);
        for _ in 0..10 {
            g.process(&mut input(0, 0.0, false));
        }
        assert!(g.active);
        let armed = Command {
            id: 2,
            expected: g.expected(),
            owner: 7,
            parent_reset_generation: 0,
            qualification_id: 1,
            operation: Operation::Context(Mode::Armed),
        };
        let mut audio = input(0, 0.01001, false);
        g.process_with_command(&mut audio, Some(armed));
        assert_eq!(
            g.control.last_command.unwrap().result,
            ControlResult::NotQualified
        );
        assert!(g.short_power_veto && !g.active);
    }

    #[test]
    fn invalid_profile_relationships_and_all_policy_identity_changes() {
        let mut p = parameters();
        p.max_tweeter_long_avg_power_db = Some(-30.0);
        assert!(p.validate().is_err());
        p.max_tweeter_avg_power_db = Some(-20.0);
        assert!(p.validate().is_ok());
        p.tweeter_long_avg_power_tau_seconds = 5.0;
        assert!(p.validate().is_err());
        p = parameters();
        p.dc_freeze_seconds = f64::NAN;
        assert!(p.validate().is_err());
        let original = parameters();
        let base = setup(original.clone()).identity.unwrap().profile;
        let value = serde_json::to_value(&original).unwrap();
        for (key, old) in value.as_object().unwrap() {
            if key.ends_with("channels") {
                continue;
            }
            let mut changed = value.clone();
            changed[key] = if old.is_null() {
                serde_json::json!(-30.0)
            } else {
                serde_json::json!(old.as_f64().unwrap() * 1.01)
            };
            if key == "max_tweeter_long_avg_power_db" {
                changed["max_tweeter_avg_power_db"] = serde_json::json!(-20.0);
            }
            let g = setup(serde_json::from_value(changed).unwrap());
            assert_ne!(g.identity.unwrap().profile, base, "{key}");
        }
    }
}
