//! Fixed-size sample statistics and audio-time policy, owned by the guard.

use super::{FRAMES, Reason};
#[cfg(test)]
use super::{Parameters, SAMPLE_RATE};
#[cfg(test)]
use crate::config;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct History {
    pub short_power: f64,
    pub long_power: f64,
    rms_frames: u64,
    tone_frames: u64,
    dc_frames: u64,
    signature: Option<(f64, f64)>,
    pub(super) signed_dc: DcWindow,
    pub(super) lf_window: super::tweeter_lf::EnergyWindow,
}

impl History {
    pub fn break_continuity(&mut self) {
        self.rms_frames = 0;
        self.tone_frames = 0;
        self.dc_frames = 0;
        self.signature = None;
        self.signed_dc = DcWindow::default();
        self.lf_window = Default::default();
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Policy {
    pub(super) signed_dc: Option<SignedDc>,
    pub(super) tweeter_lf: Option<super::tweeter_lf::TweeterLf>,
    pub(super) rms_limit: Option<f64>,
    pub(super) short_limit: Option<f64>,
    pub(super) long_limit: Option<f64>,
    pub(super) tone_limit: Option<f64>,
    pub(super) rms_frames: u64,
    pub(super) tone_frames: u64,
    pub(super) dc_frames: u64,
    pub(super) short_alpha: f64,
    pub(super) prearm_alpha: f64,
    pub(super) long_alpha: f64,
    pub(super) tone_crest: f64,
    pub(super) dc_tolerance: f64,
    pub(super) epsilon: f64,
    pub(super) freeze_floor: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CompensatedSum {
    sum: f64,
    correction: f64,
}

impl CompensatedSum {
    fn add(&mut self, x: f64) {
        // Neumaier summation retains small residuals even across cancellation.
        let next = self.sum + x;
        self.correction += if self.sum.abs() >= x.abs() {
            (self.sum - next) + x
        } else {
            (x - next) + self.sum
        };
        self.sum = next;
    }

    pub fn block(samples: &[f64]) -> Self {
        let mut total = Self::default();
        for &x in samples {
            total.add(x);
        }
        total
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SignedDc {
    pub limit_fs: f64,
    pub window_blocks: u8,
    pub same_sign_windows: u8,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DcWindow {
    total: CompensatedSum,
    blocks: u8,
    // Sign and length of the last run of completed, above-limit means.
    run: i8,
}

impl SignedDc {
    pub fn observe(
        &self,
        window: &mut DcWindow,
        block: CompensatedSum,
        prearm: bool,
    ) -> Assessment {
        // The caller has checked finite lane sum-of-squares. This bounds every
        // sample by sqrt(f64::MAX), far below overflow even over 192 blocks.
        window.total.add(block.sum);
        window.total.add(block.correction);
        window.blocks += 1;
        let mut reason = None;
        if window.blocks == self.window_blocks {
            let frames = (FRAMES * usize::from(window.blocks)) as f64;
            // Normalize the high/low pair after cancellation. Compare the signed
            // sum with ±limit*N using a fused difference: rounding the mean (or
            // the scaled threshold) first can miss equality at non-power-of-two N.
            let mut total = CompensatedSum::default();
            total.add(window.total.sum);
            total.add(window.total.correction);
            let above = (-self.limit_fs).mul_add(frames, total.sum) + total.correction >= 0.0;
            let below = self.limit_fs.mul_add(frames, total.sum) + total.correction <= 0.0;
            window.total = CompensatedSum::default();
            window.blocks = 0;
            let k = self.same_sign_windows as i8;
            window.run = if above {
                (window.run.max(0) + 1).min(k)
            } else if below {
                (window.run.min(0) - 1).max(-k)
            } else {
                0
            };
            if window.run == k {
                reason = Some(Reason::PositiveDc);
            } else if window.run == -k {
                reason = Some(Reason::NegativeDc);
            }
        }
        Assessment {
            reason,
            veto: prearm && window.run != 0,
        }
    }
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
    pub fn assess(
        &self,
        h: &mut History,
        peak: f64,
        q: f64,
        prearm: bool,
        observable: bool,
    ) -> Assessment {
        if !q.is_finite() {
            return Assessment {
                reason: Some(Reason::Numeric),
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
            let retained = h.short_power;
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
            if retained >= limit || e >= limit {
                reason = reason.or(Some(Reason::ShortPower));
            }
        }
        if let Some(limit) = self.long_limit {
            let retained = h.long_power;
            let next = update_power(&mut h.long_power, q, self.long_alpha, observable);
            if retained >= limit || next >= limit {
                reason = reason.or(Some(Reason::LongPower));
            }
        }
        let levels = (q > 0.0
            && (self.tone_limit.is_some_and(|l| q >= l)
                || self.freeze_floor.is_some_and(|l| q > l)))
        .then(|| (20.0 * peak.log10(), 10.0 * q.log10()));
        let (peak_db, rms_db) = levels.unwrap_or((0.0, 0.0));
        let crest = peak_db - rms_db;
        if consecutive(
            &mut h.tone_frames,
            self.tone_limit.is_some_and(|limit| q >= limit) && crest <= self.tone_crest,
            self.tone_frames,
        ) {
            reason = reason.or(Some(Reason::Tone));
        }
        let signature = if self.freeze_floor.is_some_and(|l| q > l && peak * peak > l)
            && crest.abs() <= self.dc_tolerance
        {
            Some((
                (peak_db / self.epsilon).round_ties_even(),
                (rms_db / self.epsilon).round_ties_even(),
            ))
        } else {
            None
        };
        if signature.is_some_and(|(a, b)| !a.is_finite() || !b.is_finite()) {
            return Assessment {
                reason: Some(Reason::Numeric),
                veto: false,
            };
        }
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

    use serde_json::{Value, json};
    fn fixture() -> Value {
        serde_json::to_value(parameters()).unwrap()
    }
    fn destination_policy(p: &Value) -> Policy {
        let (plan, _) = serde_json::from_value::<Parameters>(p.clone())
            .unwrap()
            .compile()
            .unwrap();
        plan.destinations
            .iter()
            .find(|d| d.id.text() == Some("tw_l"))
            .unwrap()
            .policy
    }
    fn rms(db: f64, hold: f64) -> Value {
        json!({"kind":"rms","targets":["tw_l","tw_r"],"limit_dbfs":db,"hold_s":hold})
    }
    fn crest_policy(db: f64, hold: f64, crest: f64) -> Value {
        json!({"kind":"crest_tone","targets":["tw_l","tw_r"],"min_rms_dbfs":db,"hold_s":hold,"max_crest_db":crest})
    }
    fn power(kind: &str, db: f64, tau: f64) -> Value {
        json!({"kind":kind,"targets":["tw_l","tw_r"],"limit_dbfs":db,"tau_s":tau})
    }
    fn setup(p: Value) -> Guard {
        let mut conf = serde_json::to_value(configuration()).unwrap();
        conf["processors"]["guard"]["parameters"] = serde_json::to_value(p).unwrap();
        let conf = serde_json::from_value(conf).unwrap();
        let mut guard = Guard::new(Arc::new(Shared::default()));
        guard.install(&config::validate_config(conf, None).unwrap());
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
                    let mut p = fixture();
                    if tone {
                        p["policies"]["tone"] = crest_policy(threshold, 0.01001, 4.5);
                    } else {
                        p["policies"]["rms"] = rms(threshold, 0.01001);
                    }
                    let mut g = setup(p);
                    // Use exactly the compiled limit at equality; samples have
                    // constant power and remain below the instantaneous ceiling.
                    let q = 10.0_f64.powf(db / 10.0);
                    let mut audio = input(channel, q, true);
                    g.process(&mut audio);
                    assert!(g.fault.trip.is_none());
                    g.process(&mut input(channel, q, true));
                    if offset < 0.0 {
                        assert!(g.fault.trip.is_none());
                    } else {
                        let trip = g.fault.trip.unwrap();
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
        let mut p = fixture();
        p["policies"]["rms"] = rms(-20.0, 0.010);
        let mut g = setup(p);
        for _ in 0..5 {
            g.process(&mut input(1, 0.02, false));
            g.process(&mut input(1, 0.001, false));
            assert!(g.fault.trip.is_none());
        }
        g.process(&mut input(1, 0.02, false));
        g.pause();
        g.process(&mut input(1, 0.02, false));
        assert!(g.fault.trip.is_none());
        g.process(&mut input(1, 0.02, false));
        assert_eq!(g.fault.trip.unwrap().reason, Reason::Rms);
    }

    #[test]
    fn crest_and_dc_tolerance_boundaries_are_inclusive() {
        let crest = 20.0 * 0.5_f64.log10() - 10.0 * 0.125_f64.log10();
        for tone in [false, true] {
            for offset in [-0.01, 0.0, 0.01] {
                let mut p = fixture();
                p["policies"]["hf_freeze"]["hold_s"] = json!(FRAMES as f64 / SAMPLE_RATE as f64);
                if tone {
                    p["policies"]["tone"] =
                        crest_policy(-20.0, FRAMES as f64 / SAMPLE_RATE as f64, crest + offset);
                } else {
                    p["policies"]["hf_freeze"]["crest_tolerance_db"] = json!(crest + offset);
                }
                let policy = destination_policy(&p);
                let mut h = History::default();
                let mut result = policy.assess(&mut h, 0.5, 0.125, true, true);
                if !tone {
                    assert!(result.reason.is_none()); // Signature baseline.
                    result = policy.assess(&mut h, 0.5, 0.125, true, true);
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
        let mut p = fixture();
        p["policies"]["rms"] = rms(-20.0, 0.010);
        let mut g = setup(p);
        for _ in 0..2 {
            let mut audio = input(1, 0.02, false);
            audio.main_volume.as_mut().unwrap().muted = true;
            g.process(&mut audio);
            assert!(!g.candidate.observable && !g.qualification.is_active());
            assert!(audio.waveforms.iter().flatten().all(|x| *x == 0.0));
        }
        assert_eq!(g.fault.trip.unwrap().reason, Reason::Rms);
    }

    #[test]
    fn dc_signature_baseline_quantization_change_and_silence() {
        let mut p = fixture();
        p["policies"]["hf_freeze"]["hold_s"] = json!(0.01001);
        let mut g = setup(p);
        for _ in 0..10 {
            g.process(&mut input(0, 0.0, true));
            assert!(g.fault.trip.is_none());
        }
        g.process(&mut input(0, 0.01, true)); // baseline
        g.process(&mut input(0, 0.01 * 10.0_f64.powf(0.01 / 10.0), true)); // same quantized pair
        assert!(g.fault.trip.is_none());
        g.process(&mut input(0, 0.02, true)); // changed signature
        g.process(&mut input(0, 0.02, true));
        assert!(g.fault.trip.is_none());
        let mut last = input(0, 0.02, true);
        g.process(&mut last);
        assert_eq!(g.fault.trip.unwrap().reason, Reason::DcFrozen);
        assert!(last.waveforms.iter().flatten().all(|x| *x == 0.0));
        assert_eq!((2.5_f64).round_ties_even(), 2.0);
        assert_eq!((3.5_f64).round_ties_even(), 4.0);
    }

    #[test]
    fn short_power_audio_time_matches_closed_form_and_long_is_not_accelerated() {
        let mut p = fixture();
        p["policies"]["short"] = power("short_power", -10.0, 5.0);
        p["policies"]["long"] = power("long_power", -20.0, 90.0);
        let policy = destination_policy(&p);
        assert_eq!(
            serde_json::from_value::<Parameters>(p)
                .unwrap()
                .compile()
                .unwrap()
                .0
                .qualification_frames,
            480_000
        ); // not 180 seconds
        for prearm in [false, true] {
            let mut h = History::default();
            for _ in 0..1000 {
                policy.assess(&mut h, 0.1, 0.01, prearm, true);
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
        let mut p = fixture();
        p["policies"]["short"] = power("short_power", -10.0, 5.0);
        p["policies"]["long"] = power("long_power", -20.0, 90.0);
        let mut policy = destination_policy(&p);
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
                let a = policy.assess(&mut h, 0.25, q, true, true);
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
            let mut p = fixture();
            p["policies"]["short"] = power("short_power", -20.0, 5.0);
            let mut g = setup(p);
            let q = 10.0_f64.powf((-20.0 + overload) / 10.0);
            for _ in 0..=480_000 / 256 {
                let mut audio = input(1, q, false);
                g.process(&mut audio);
                assert!(!g.qualification.is_active() && g.detector.prearm_veto);
                assert!(audio.waveforms.iter().flatten().all(|x| *x == 0.0));
            }
            if overload == 0.01 {
                assert!(g.fault.trip.is_none());
                let heat = g.detector.history[5].short_power;
                context(&mut g, Operation::Context(Mode::Prearm), 2);
                assert_eq!(g.detector.history[5].short_power, heat);
                for _ in 0..480_000 / 256 {
                    g.process(&mut input(1, 0.0, false));
                    assert!(!g.qualification.is_active());
                }
                g.process(&mut input(1, 0.0, false));
                assert!(g.qualification.is_active());
            } else {
                assert_eq!(g.fault.trip.unwrap().reason, Reason::ShortPower);
            }
        }
    }

    #[test]
    fn masked_audio_and_pause_never_cool_and_only_reset_clears_heat() {
        let mut p = fixture();
        p["policies"]["short"] = power("short_power", -10.0, 5.0);
        p["policies"]["long"] = power("long_power", -20.0, 90.0);
        let mut g = setup(p.clone());
        for _ in 0..10 {
            g.process(&mut input(0, 0.005, false));
        }
        let heat = (
            g.detector.history[4].short_power,
            g.detector.history[4].long_power,
        );
        for _ in 0..100 {
            let mut masked = input(0, 0.0, false);
            masked.main_volume.as_mut().unwrap().muted = true;
            g.process(&mut masked);
        }
        g.pause();
        context(&mut g, Operation::Context(Mode::Prearm), 2);
        assert_eq!(
            heat,
            (
                g.detector.history[4].short_power,
                g.detector.history[4].long_power
            )
        );
        let mut next = serde_json::to_value(configuration()).unwrap();
        next["processors"]["guard"]["parameters"] = p;
        assert!(g.install(
            &config::validate_config(serde_json::from_value(next).unwrap(), None).unwrap()
        ));
        assert_eq!(
            heat,
            (
                g.detector.history[4].short_power,
                g.detector.history[4].long_power
            )
        );
        g.shared
            .backend
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        g.backend_wakeup();
        assert_eq!(
            heat,
            (
                g.detector.history[4].short_power,
                g.detector.history[4].long_power
            )
        );
        context(&mut g, Operation::Context(Mode::Prearm), 3);
        context(&mut g, Operation::Reset, 4);
        assert_eq!(
            (
                g.detector.history[4].short_power,
                g.detector.history[4].long_power
            ),
            (0.0, 0.0)
        );
    }

    #[test]
    fn post_hold_context_preserves_heat_and_restarts_complete_window() {
        let mut p = fixture();
        p["policies"]["short"] = power("short_power", -10.0, 5.0);
        p["policies"]["short"]["tau_s"] = json!(0.02);
        p["policies"]["hf_freeze"]["hold_s"] = json!(0.01);
        let mut g = setup(p);
        context(&mut g, Operation::Reset, 2);
        for _ in 0..10 {
            g.process(&mut input(0, 0.01, false));
        }
        assert!(g.qualification.is_active());
        let heat = g.detector.history[4].short_power;
        context(&mut g, Operation::Context(Mode::Prearm), 3);
        assert!(!g.qualification.is_active());
        assert_eq!(g.qualification.frames(), 0);
        assert_eq!(g.detector.history[4].short_power, heat);
        for _ in 0..8 {
            g.process(&mut input(0, 0.001, false));
            assert!(!g.qualification.is_active());
        }
        g.process(&mut input(0, 0.001, false));
        assert!(g.qualification.is_active());
    }

    #[test]
    fn prearm_veto_revokes_local_active_and_rejects_same_chunk_armed_command() {
        let mut p = fixture();
        p["policies"]["short"] = power("short_power", -20.0, 5.0);
        p["policies"]["short"]["tau_s"] = json!(0.02);
        p["policies"]["hf_freeze"]["hold_s"] = json!(0.01);
        let mut g = setup(p);
        for _ in 0..10 {
            g.process(&mut input(0, 0.0, false));
        }
        assert!(g.qualification.is_active());
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
        assert!(g.detector.prearm_veto && !g.qualification.is_active());
    }

    #[test]
    fn invalid_profile_relationships_and_all_policy_identity_changes() {
        let mut p = fixture();
        p["policies"]["long"] = power("long_power", -30.0, 90.0);
        let valid = |p: &Value| {
            serde_json::from_value::<Parameters>(p.clone()).is_ok_and(|p| p.compile().is_ok())
        };
        assert!(!valid(&p));
        p["policies"]["short"] = power("short_power", -20.0, 5.0);
        assert!(valid(&p));
        p["policies"]["long"]["tau_s"] = json!(5.0);
        assert!(!valid(&p));
        p["policies"]["hf_freeze"]["hold_s"] = Value::Null;
        assert!(!valid(&p));
    }
}
