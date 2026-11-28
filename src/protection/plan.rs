//! Closed graph schema and off-thread compilation for the final audio guard.
use super::{CHANNELS, FRAMES, Reason, identity, policy::Policy};
use crate::{Res, config::ConfigError};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{Error, MapAccess, Visitor},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    marker::PhantomData,
};

fn invalid(message: &str) -> Box<dyn std::error::Error> {
    ConfigError::new(message).into()
}

// Serde's ordinary BTreeMap deserializer silently overwrites duplicate keys.
fn unique<'de, D, V>(deserializer: D) -> Result<BTreeMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct Unique<V>(PhantomData<V>);
    impl<'de, V: Deserialize<'de>> Visitor<'de> for Unique<V> {
        type Value = BTreeMap<String, V>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a mapping with unique protection IDs")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                if !Id::valid(&key) || entries.contains_key(&key) || entries.len() == 216 {
                    return Err(A::Error::custom(
                        "invalid, duplicate or excess protection ID",
                    ));
                }
                entries.insert(key, map.next_value()?);
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_map(Unique(PhantomData))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Id(pub(super) [u8; 32]);
impl Id {
    pub(super) fn valid(s: &str) -> bool {
        (1..=31).contains(&s.len())
            && s.as_bytes()[0].is_ascii_lowercase()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }
    pub(super) fn new(s: &str) -> Res<Self> {
        if !Self::valid(s) {
            return Err(invalid("invalid protection ID"));
        }
        let mut id = Self::default();
        id.0[..s.len()].copy_from_slice(s.as_bytes());
        Ok(id)
    }
    pub fn text(&self) -> Option<&str> {
        let len = self.0.iter().position(|b| *b == 0)?;
        (len > 0)
            .then(|| std::str::from_utf8(&self.0[..len]).ok())
            .flatten()
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    pub routing_sha256: String,
    pub activity: Activity,
    #[serde(deserialize_with = "unique")]
    pub signals: BTreeMap<String, Signal>,
    #[serde(deserialize_with = "unique")]
    pub destinations: BTreeMap<String, String>,
    #[serde(deserialize_with = "unique")]
    pub policies: BTreeMap<String, Instance>,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Activity {
    pub silence_dbfs: f64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Tweeter,
    Midwoofer,
    Sub,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    pub lane: usize,
    pub role: Role,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Instance {
    Peak {
        targets: Vec<String>,
        limit_dbfs: f64,
    },
    Rms {
        targets: Vec<String>,
        limit_dbfs: f64,
        hold_s: f64,
    },
    ShortPower {
        targets: Vec<String>,
        limit_dbfs: f64,
        tau_s: f64,
    },
    LongPower {
        targets: Vec<String>,
        limit_dbfs: f64,
        tau_s: f64,
    },
    CrestTone {
        targets: Vec<String>,
        min_rms_dbfs: f64,
        max_crest_db: f64,
        hold_s: f64,
    },
    Excursion {
        targets: Vec<String>,
        model: Box<super::excursion::Pack>,
    },
    TweeterLf {
        targets: Vec<String>,
        expected_highpass_hz: f64,
        limit_dbfs: f64,
        energy_blocks: u8,
        hold_blocks: u8,
    },
    SignedDc {
        targets: Vec<String>,
        limit_fs: f64,
        window_blocks: u8,
        same_sign_windows: u8,
    },
    Frozen {
        targets: Vec<String>,
        floor_dbfs: f64,
        crest_tolerance_db: f64,
        quantum_db: f64,
        hold_s: f64,
    },
}
impl Instance {
    fn descriptor(&self) -> (&[String], Reason) {
        match self {
            Self::Excursion { targets, .. } => (targets, Reason::Excursion),
            Self::TweeterLf { targets, .. } => (targets, Reason::TweeterLf),
            Self::SignedDc { targets, .. } => (targets, Reason::PositiveDc),
            Self::Peak { targets, .. } => (targets, Reason::Peak),
            Self::Rms { targets, .. } => (targets, Reason::Rms),
            Self::ShortPower { targets, .. } => (targets, Reason::ShortPower),
            Self::LongPower { targets, .. } => (targets, Reason::LongPower),
            Self::CrestTone { targets, .. } => (targets, Reason::Tone),
            Self::Frozen { targets, .. } => (targets, Reason::DcFrozen),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Destination {
    pub id: Id,
    pub lane: usize,
    pub policies: [Id; Reason::Excursion as usize + 1],
    pub model: Option<super::excursion::Model>,
    pub peak: Option<f64>,
    pub policy: Policy,
}
#[derive(Clone, Debug)]
pub(super) struct Plan {
    pub destinations: [Destination; CHANNELS],
    pub count: usize,
    pub model_count: usize,
    pub peak_floor: [f64; CHANNELS],
    pub qualification_frames: u64,
    pub silence_power: f64,
    pub binding: [u64; 4],
}
fn power(db: f64) -> Res<f64> {
    let value = 10f64.powf(db / 10.0);
    if db.is_finite() && db < 0.0 && value > 0.0 {
        Ok(value)
    } else {
        Err(invalid("invalid protection level"))
    }
}
fn positive(value: f64) -> Res<f64> {
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(invalid("invalid positive protection parameter"))
    }
}
fn frames(seconds: f64, sample_rate: usize) -> Res<u64> {
    let count = (positive(seconds)? * sample_rate as f64).ceil();
    if count >= i64::MAX as f64 {
        return Err(invalid("protection duration overflow"));
    }
    Ok(count as u64)
}
fn alpha(tau: f64, sample_rate: usize) -> Res<f64> {
    frames(tau, sample_rate)?;
    positive(-(-(FRAMES as f64 / sample_rate as f64) / tau).exp_m1())
}

impl Parameters {
    pub(super) fn compile(&self, sample_rate: usize) -> Res<(Plan, Value)> {
        if self.routing_sha256.len() != 64
            || !self
                .routing_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !(1..=CHANNELS).contains(&self.signals.len())
            || !(1..=CHANNELS).contains(&self.destinations.len())
        {
            return Err(invalid(
                "invalid protection routing digest or inventory size",
            ));
        }
        let mut lanes = BTreeSet::new();
        for (name, signal) in &self.signals {
            Id::new(name)?;
            if signal.lane >= CHANNELS
                || !lanes.insert(signal.lane)
                || !self.destinations.values().any(|s| s == name)
            {
                return Err(invalid(
                    "unused signal, duplicate or out-of-range protection lane",
                ));
            }
        }
        let mut plan = Plan {
            destinations: [Destination::default(); CHANNELS],
            count: self.destinations.len(),
            model_count: 0,
            peak_floor: [f64::INFINITY; CHANNELS],
            qualification_frames: FRAMES as u64,
            silence_power: power(self.activity.silence_dbfs)?,
            binding: [0; 4],
        };
        let mut bindings = BTreeMap::new();
        let mut lf_sources = BTreeMap::new();
        let mut resolved = Vec::new();
        let mut algorithms = BTreeMap::new();
        for (index, (id, source)) in self.destinations.iter().enumerate() {
            let signal = self
                .signals
                .get(source)
                .ok_or_else(|| invalid("unknown destination signal"))?;
            let dest = &mut plan.destinations[index];
            dest.id = Id::new(id)?;
            dest.lane = signal.lane;
            let mut model_binding = None;
            let mut peak_db = 0.0;
            let mut ceiling = f64::NEG_INFINITY;
            let (mut short, mut long) = (None, None);
            for (name, instance) in &self.policies {
                let (targets, reason) = instance.descriptor();
                if targets.is_empty()
                    || targets.len() > CHANNELS
                    || targets.windows(2).any(|w| w[0] >= w[1])
                    || targets.iter().any(|s| !self.destinations.contains_key(s))
                {
                    return Err(invalid(
                        "empty, unsorted, repeated or unknown policy target",
                    ));
                }
                if targets.binary_search(id).is_err() {
                    continue;
                }
                let slot = &mut dest.policies[reason.policy_slot()];
                if *slot != Id::default() {
                    return Err(invalid("overlapping kind and destination"));
                }
                *slot = Id::new(name)?;
                let p = &mut dest.policy;
                let q = match *instance {
                    Instance::Peak { limit_dbfs, .. } => {
                        if !(-120.0..=0.0).contains(&limit_dbfs) {
                            return Err(invalid("invalid peak threshold"));
                        }
                        peak_db = limit_dbfs;
                        let limit = 10f64.powf(peak_db / 20.0);
                        dest.peak = Some(limit);
                        plan.peak_floor[dest.lane] = plan.peak_floor[dest.lane].min(limit);
                        0
                    }
                    Instance::Rms {
                        limit_dbfs, hold_s, ..
                    } => {
                        ceiling = ceiling.max(limit_dbfs);
                        p.rms_limit = Some(power(limit_dbfs)?);
                        p.rms_frames = frames(hold_s, sample_rate)?;
                        p.rms_frames
                    }
                    Instance::ShortPower {
                        limit_dbfs, tau_s, ..
                    } => {
                        ceiling = ceiling.max(limit_dbfs);
                        p.short_limit = Some(power(limit_dbfs)?);
                        p.short_alpha = alpha(tau_s, sample_rate)?;
                        p.prearm_alpha = alpha(tau_s / 2.5, sample_rate)?;
                        short = Some((limit_dbfs, tau_s));
                        frames(2.0 * tau_s, sample_rate)?
                    }
                    Instance::LongPower {
                        limit_dbfs, tau_s, ..
                    } => {
                        ceiling = ceiling.max(limit_dbfs);
                        p.long_limit = Some(power(limit_dbfs)?);
                        p.long_alpha = alpha(tau_s, sample_rate)?;
                        long = Some((limit_dbfs, tau_s));
                        0
                    }
                    Instance::CrestTone {
                        min_rms_dbfs,
                        max_crest_db,
                        hold_s,
                        ..
                    } => {
                        if signal.role != Role::Tweeter {
                            return Err(invalid("crest tone requires tweeter"));
                        }
                        ceiling = ceiling.max(min_rms_dbfs);
                        p.tone_limit = Some(power(min_rms_dbfs)?);
                        p.tone_crest = positive(max_crest_db)?;
                        p.tone_frames = frames(hold_s, sample_rate)?;
                        p.tone_frames
                    }
                    Instance::Excursion { ref model, .. } => {
                        if signal.role == Role::Tweeter || targets.len() != 1 {
                            return Err(invalid(
                                "excursion requires one sub or midwoofer destination",
                            ));
                        }
                        dest.model = Some(model.compile(id, plan.model_count, sample_rate)?);
                        plan.model_count += 1;
                        model_binding = Some(model);
                        FRAMES as u64 * u64::from(model.settle_blocks)
                    }
                    Instance::TweeterLf {
                        expected_highpass_hz,
                        limit_dbfs,
                        energy_blocks,
                        hold_blocks,
                        ..
                    } => {
                        if signal.role != Role::Tweeter
                            || !(1..=32).contains(&energy_blocks)
                            || !(1..=64).contains(&hold_blocks)
                        {
                            return Err(invalid(
                                "invalid tweeter LF role, integration or persistence",
                            ));
                        }
                        // Identical filters on a shared lane advance once per candidate.
                        let source = *lf_sources
                            .entry((signal.lane, expected_highpass_hz.to_bits()))
                            .or_insert(index);
                        p.tweeter_lf = Some(super::tweeter_lf::TweeterLf {
                            coefficients: super::tweeter_lf::coefficients(
                                expected_highpass_hz,
                                sample_rate,
                            )?,
                            source,
                            limit_power: power(limit_dbfs)?,
                            energy_blocks,
                            hold_blocks,
                        });
                        FRAMES as u64 * (8 + u64::from(energy_blocks) + u64::from(hold_blocks) - 1)
                    }
                    Instance::SignedDc {
                        limit_fs,
                        window_blocks,
                        same_sign_windows,
                        ..
                    } => {
                        if !(64..=192).contains(&window_blocks)
                            || !(2..=8).contains(&same_sign_windows)
                            || !(limit_fs > 0.0 && limit_fs <= 1.0)
                        {
                            return Err(invalid("invalid signed DC window, persistence or limit"));
                        }
                        p.signed_dc = Some(super::policy::SignedDc {
                            limit_fs,
                            window_blocks,
                            same_sign_windows,
                        });
                        // All factors are bounded above before widening.
                        FRAMES as u64
                            * u64::from(window_blocks)
                            * (1 + u64::from(same_sign_windows))
                    }
                    Instance::Frozen {
                        floor_dbfs,
                        crest_tolerance_db,
                        quantum_db,
                        hold_s,
                        ..
                    } => {
                        ceiling = ceiling.max(floor_dbfs);
                        p.freeze_floor = Some(power(floor_dbfs)?);
                        p.dc_tolerance = positive(crest_tolerance_db)?;
                        p.epsilon = positive(quantum_db)?;
                        p.dc_frames = frames(hold_s, sample_rate)?;
                        p.dc_frames
                    }
                };
                if plan.qualification_frames < q {
                    plan.qualification_frames = q;
                }
                let kind = reason.kind();
                algorithms.insert(kind, 1);
                resolved.push((name, kind, id, signal.lane, signal.role));
            }
            if ceiling >= peak_db
                || long.is_some_and(|(db, tau)| short.is_none_or(|(s, t)| db >= s || tau <= t))
            {
                return Err(invalid(
                    "incompatible destination thresholds or power time constants",
                ));
            }
            bindings.insert(
                id,
                json!([
                    signal.role,
                    signal.lane,
                    short.map(|v| v.1),
                    long.map(|v| v.1),
                    model_binding
                ]),
            );
        }
        plan.binding = identity::digest(&json!([sample_rate, self.routing_sha256, bindings]))?;
        resolved.sort_by_key(|&(name, kind, id, _, _)| (name, kind, id));
        let targets:Vec<_>=resolved.into_iter().map(|(policy,kind,destination,lane,role)|
            json!({ "policy":policy, "kind":kind, "destination":destination, "lane":lane, "role":role })).collect();
        let mut profile = json!({"parameters":self, "algorithms":algorithms, "resolved_targets":targets,
            "qualification_frames":plan.qualification_frames, "qualification_rule":"candidate-baseline-max-windows-v2",
            "activity_rule":"all-forwarded-power-v1", "precedence":"channel-sample-kind-id-v2", "short_prearm_factor":2.5});
        // Protocol 2's original profile encodes the fixed 48 kHz contract.
        // Bind the new rate explicitly even for a peak-only or block-count policy.
        if sample_rate == super::RATE_44100 {
            profile["geometry"] = json!({
                "sample_rate": sample_rate, "chunk_frames": FRAMES, "playback_channels": CHANNELS,
                "period_frames": super::PERIOD_44100, "buffer_frames": super::BUFFER_44100,
                "target_level": super::TARGET_44100
            });
        }
        Ok((plan, profile))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config,
        protection::{Guard, Shared, tests::chunk},
    };
    use std::sync::Arc;
    fn parity() -> config::PreparedConfig {
        config::load_validate_config("testdata/protection-parity.yml").unwrap()
    }
    fn changed(
        p: &config::PreparedConfig,
        edit: impl FnOnce(&mut Value),
    ) -> Res<config::PreparedConfig> {
        let mut graph = serde_json::to_value(p)?;
        edit(&mut graph["processors"]["protection"]["parameters"]);
        config::validate_config(serde_json::from_value(graph)?, None)
    }

    #[test]
    fn native_rate_binds_guard_timing_and_live_identity_to_the_open_pipeline() {
        let legacy = parity();
        let mut graph = legacy.config().clone();
        graph.devices.samplerate = super::super::RATE_44100;
        let native = config::validate_config(graph.clone(), None).unwrap();
        let export = identity::export(&native).unwrap();
        assert_eq!(export["profile"]["qualification_frames"], 88_200);
        graph.devices.target_level = Some(256);
        assert!(config::validate_config(graph, None).is_err());

        let shared = Arc::new(Shared::default());
        let mut guard = Guard::new(shared.clone());
        assert!(guard.install(&native));
        let _playback = crate::protection::playback::Playback::new(shared.clone());
        let live = crate::protection::wire::tests::status(&shared);
        assert_eq!(live["identity"]["sample_rate"], 44_100);
        for key in ["graph_sha256", "profile_sha256"] {
            assert_eq!(live["identity"][key], export[key]);
        }
        assert!(shared.check_binding(&legacy).is_err());

        // Identical frame counts must still distinguish the effective policies.
        let peak_only = |params: &mut Value| {
            params["policies"]
                .as_object_mut()
                .unwrap()
                .retain(|_, p| p["kind"] == "peak");
        };
        let a = changed(&legacy, peak_only).unwrap();
        let b = changed(&native, peak_only).unwrap();
        assert_eq!(
            a.guard().plan.qualification_frames,
            b.guard().plan.qualification_frames
        );
        assert_ne!(a.identity().profile, b.identity().profile);
    }

    #[test]
    fn traced_and_split_targets_reach_the_guard_and_offline_export() {
        let parity = parity();
        let split = config::load_validate_config("testdata/protection-split.yml").unwrap();
        assert_eq!(parity.guard().plan.qualification_frames, 96000);
        let shared = changed(&parity, |p| {
            for (id, limit) in [("sub_a", -3.0), ("sub_b", -9.0)] {
                p["policies"][id] = json!({"kind":"peak","targets":[id],"limit_dbfs":limit});
            }
        })
        .unwrap();
        for (graph, lane, value, id, name) in [
            (&shared, 4, 0.5, "sub_b", "sub_b"),
            (&split, 5, 0.9, "sub_b", "bass_b_peak"),
        ] {
            let mut guard = Guard::new(Arc::new(Shared::default()));
            assert!(guard.install(graph));
            let export = identity::export(graph).unwrap();
            assert_eq!(
                identity::hex(
                    guard
                        .shared
                        .guard_snapshot()
                        .unwrap()
                        .record
                        .identity
                        .profile
                ),
                export["profile_sha256"]
            );
            let mut audio = chunk();
            audio.waveforms[lane][127] = value;
            guard.process(&mut audio);
            let trip = guard.trip().unwrap();
            assert_eq!(
                (
                    trip.channel,
                    trip.frame,
                    trip.destination_id.text(),
                    trip.policy_id.text()
                ),
                (Some(lane), 127, Some(id), Some(name))
            );
            assert!(audio.waveforms.iter().flatten().all(|s| *s == 0.0));
        }
        let mut guard = Guard::new(Arc::new(Shared::default()));
        guard.install(&split);
        for _ in 0..94 {
            let mut audio = chunk();
            audio.waveforms[3].fill(0.5);
            audio.waveforms[4].fill(1.0);
            guard.process(&mut audio);
        }
        let trip = guard.trip().unwrap();
        assert_eq!(
            (trip.reason, trip.frame, trip.destination_id.text()),
            (Reason::Rms, 24064, Some("mid_r"))
        );
    }

    #[test]
    fn shared_lane_peak_ties_use_policy_then_destination_and_untargeted_overflow_trips() {
        let graph = changed(&parity(), |p| {
            p["policies"]["z_peak"] = json!({"kind":"peak","targets":["sub_a"],"limit_dbfs":-3.0});
            p["policies"]["a_peak"] = json!({"kind":"peak","targets":["sub_b"],"limit_dbfs":-6.0});
        })
        .unwrap();
        let mut guard = Guard::new(Arc::new(Shared::default()));
        guard.install(&graph);
        let mut audio = chunk();
        audio.waveforms[4][0] = 0.9;
        guard.process(&mut audio);
        let trip = guard.trip().unwrap();
        assert_eq!(
            (trip.policy_id.text(), trip.destination_id.text()),
            (Some("a_peak"), Some("sub_b"))
        );

        let graph = changed(&parity(), |p| {
            p["policies"]["both"] =
                json!({"kind":"peak","targets":["sub_a","sub_b"],"limit_dbfs":-3.0});
        })
        .unwrap();
        let mut guard = Guard::new(Arc::new(Shared::default()));
        guard.install(&graph);
        let mut audio = chunk();
        audio.waveforms[4][0] = 0.9;
        guard.process(&mut audio);
        assert_eq!(guard.trip().unwrap().destination_id.text(), Some("sub_a"));

        let mut guard = Guard::new(Arc::new(Shared::default()));
        guard.install(&graph);
        let mut audio = chunk();
        audio.waveforms[23][7] = f64::MAX;
        guard.process(&mut audio);
        let trip = guard.trip().unwrap();
        assert_eq!(
            (trip.reason, trip.channel, trip.frame, trip.policy_id.text()),
            (Reason::Numeric, Some(23), FRAMES as u64, None)
        );
        assert!(
            audio
                .waveforms
                .iter()
                .flatten()
                .all(|sample| *sample == 0.0)
        );
    }

    #[test]
    fn invalid_target_plans_are_rejected_before_install() {
        let p = parity();
        for (path, value) in [
            ("/signals/high_l/lane", json!(24)),
            ("/signals/high_l/lane", json!(1)),
            ("/destinations/tw_l", json!("missing")),
            ("/policies/hf_peak/targets", json!([])),
            ("/policies/hf_peak/targets", json!(["tw_l", "tw_l"])),
            ("/policies/hf_peak/targets", json!(["tw_r", "tw_l"])),
            ("/policies/hf_peak/limit_dbfs", json!(true)),
            ("/policies/hf_freeze/hold_s", json!(1e25)),
            ("/policies/hf_peak", Value::Null),
            ("/policies/hf_peak/kind", json!("excursion")),
        ] {
            assert!(
                changed(&p, |params| *params.pointer_mut(path).unwrap() = value).is_err(),
                "{path}"
            );
        }
        assert!(
            changed(&p, |p| p["policies"]["second"] =
                p["policies"]["hf_peak"].clone())
            .is_err()
        );
        assert!(changed(&p,|p|p["policies"]["bad"]=json!({"kind":"crest_tone","targets":["sub_a"],"min_rms_dbfs":-12.0,"max_crest_db":1.0,"hold_s":1.0})).is_err());
        let y = include_str!("../../testdata/protection-parity.yml");
        let duplicate = y.replace(
            "        high_l:",
            "        high_l: {role: sub, lane: 4}\n        high_l:",
        );
        assert_ne!(y, duplicate);
        assert!(yaml_serde::from_str::<config::Configuration>(&duplicate).is_err());
    }

    #[test]
    fn power_binding_and_retained_trip_survive_replacement() {
        let start=changed(&parity(),|p|p["policies"]["thermal"]=json!({"kind":"short_power","targets":["sub_a"],"limit_dbfs":-3.0,"tau_s":0.010})).unwrap();
        let tighter = changed(&start, |p| {
            p["policies"]["thermal"]["limit_dbfs"] = json!(-11.0)
        })
        .unwrap();
        let mut guard = Guard::new(Arc::new(Shared::default()));
        guard.install(&start);
        let mut input = chunk();
        input.waveforms[4].fill(0.4);
        guard.process(&mut input);
        let stamp = input.protection.unwrap();
        for edit in [0, 1, 2, 3] {
            let incompatible = changed(&start, |p| match edit {
                0 => p["policies"]["thermal"]["tau_s"] = json!(0.020),
                1 => {
                    p["policies"].as_object_mut().unwrap().remove("thermal");
                }
                2 => p["signals"]["bass"]["lane"] = json!(5),
                _ => p["routing_sha256"] = json!("2".repeat(64)),
            })
            .unwrap();
            assert!(guard.shared.check_binding(&incompatible).is_err());
            assert!(!guard.install(&incompatible));
            assert!(stamp.is_current());
        }
        assert!(guard.install(&tighter));
        assert!(!stamp.is_current());
        let mut quiet = chunk();
        quiet.waveforms[4].fill(0.0);
        guard.process(&mut quiet);
        let trip = guard.trip().unwrap();
        assert_eq!(trip.reason, Reason::ShortPower);
        assert_eq!(trip.origin_profile, tighter.identity().profile);
        assert!(guard.install(&start));
        assert_eq!(
            guard.shared.guard_snapshot().unwrap().record.trip,
            Some(trip)
        );
    }
}
