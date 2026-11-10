//! Identities are prepared during configuration validation, outside audio work.

use crate::{Res, config};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const FORK_REVISION: &str = env!("CAMILLA_PROTECTION_BUILD_REVISION");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    pub graph: [u64; 4],
    pub profile: [u64; 4],
}

// Match Python's finite float spelling used by camilla-config-json-v1. The
// shortest significant digits come from serde_json; only notation differs.
fn float_text(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .into();
    }
    let raw = serde_json::to_string(&value.abs()).unwrap();
    let (mantissa, exponent) = raw
        .split_once('e')
        .map_or((raw.as_str(), 0), |(m, e)| (m, e.parse::<i32>().unwrap()));
    let decimal = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let untrimmed = mantissa.replace('.', "");
    let leading = untrimmed.len() - untrimmed.trim_start_matches('0').len();
    let digits = untrimmed.trim_start_matches('0').trim_end_matches('0');
    let power = exponent + decimal - leading as i32 - 1;
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if !(-4..16).contains(&power) {
        let tail = if digits.len() == 1 {
            String::new()
        } else {
            format!(".{}", &digits[1..])
        };
        format!(
            "{sign}{}{tail}e{}{power:02}",
            &digits[..1],
            if power >= 0 { "+" } else { "-" },
            power = power.abs()
        )
    } else if power < 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-power - 1) as usize))
    } else {
        let split = power as usize + 1;
        if split >= digits.len() {
            format!("{sign}{digits}{}.0", "0".repeat(split - digits.len()))
        } else {
            format!("{sign}{}.{}", &digits[..split], &digits[split..])
        }
    }
}

fn canonical(value: &Value, result: &mut String, depth: usize) -> Res<()> {
    if depth > 64 {
        return Err(config::ConfigError::new("Config identity nesting limit").into());
    }
    match value {
        Value::Object(items) => {
            result.push('{');
            let mut keys: Vec<_> = items.keys().collect();
            keys.sort();
            for (index, key) in keys.iter().enumerate() {
                if index != 0 {
                    result.push(',');
                }
                result.push_str(&serde_json::to_string(key)?);
                result.push(':');
                canonical(&items[*key], result, depth + 1)?;
            }
            result.push('}');
        }
        Value::Array(items) => {
            result.push('[');
            for (index, item) in items.iter().enumerate() {
                if index != 0 {
                    result.push(',');
                }
                canonical(item, result, depth + 1)?;
            }
            result.push(']');
        }
        Value::Number(number) if number.is_f64() => {
            result.push_str(&float_text(number.as_f64().unwrap()))
        }
        _ => result.push_str(&serde_json::to_string(value)?),
    }
    if result.len() > 1024 * 1024 {
        return Err(config::ConfigError::new("Config identity size limit").into());
    }
    Ok(())
}

pub fn digest(value: &Value) -> Res<[u64; 4]> {
    let mut text = String::new();
    canonical(value, &mut text, 0)?;
    let bytes = Sha256::digest(text.as_bytes());
    Ok(std::array::from_fn(|i| {
        u64::from_be_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
    }))
}

pub fn hex(words: [u64; 4]) -> String {
    words.iter().map(|word| format!("{word:016x}")).collect()
}

fn profile(parameters: &super::Parameters, policy: &super::policy::Policy) -> Value {
    serde_json::json!({
        "algorithm": "protection-audio-time-v1",
        "parameters": parameters,
        "qualification_frames": policy.qualification_frames,
        "enabled": {"rms": parameters.max_tweeter_rms_db.is_some(),
            "short_power": parameters.max_tweeter_avg_power_db.is_some(),
            "long_power": parameters.max_tweeter_long_avg_power_db.is_some(),
            "tone": parameters.tone_min_rms_db.is_some(), "dc_frozen": true},
        "short_prearm_factor": 2.5,
        "qualification_rule": "candidate-baseline-max-windows-short-veto-v1",
    })
}

pub(super) fn prepare(
    conf: &config::Configuration,
    parameters: &super::Parameters,
    policy: &super::policy::Policy,
) -> Res<Identity> {
    // GetConfig uses this same YAML serialization. Round-trip it before
    // canonicalization so f32 config scalars have the same text semantics.
    let graph: Value = yaml_serde::from_str(&yaml_serde::to_string(conf)?)?;
    let profile = profile(parameters, policy);
    Ok(Identity {
        graph: digest(&graph)?,
        profile: digest(&profile)?,
    })
}

/// Offline export of the same normalized graph and policy used by status.
/// This does not instantiate an audio backend or read a running DSP.
pub fn export(conf: &config::PreparedConfig) -> Res<Value> {
    let admitted = conf.guard();
    let identity = admitted.identity;
    Ok(serde_json::json!({
        "schema_version": 1,
        "protocol": 1,
        "backend": "alsa-hw-ring-v1",
        "fork_revision": FORK_REVISION,
        "graph_sha256": hex(identity.graph),
        "profile_sha256": hex(identity.profile),
        "graph_yaml": yaml_serde::to_string(conf)?,
        "profile": profile(&admitted.parameters, &admitted.policy),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_export_matches_status_and_round_trips_without_identity_drift() {
        let conf = super::super::tests::prepared_configuration();
        let exported = export(&conf).unwrap();
        let round_trip: config::Configuration =
            yaml_serde::from_str(exported["graph_yaml"].as_str().unwrap()).unwrap();
        // Prepared metadata is not serialized and cannot affect raw equality.
        assert_eq!(conf.config(), &round_trip);
        assert_eq!(
            serde_json::to_value(&conf).unwrap(),
            serde_json::to_value(&round_trip).unwrap()
        );
        let round_trip = config::validate_config(round_trip, None).unwrap();
        let identity = round_trip.identity();
        assert_eq!(exported["graph_sha256"], hex(identity.graph));
        assert_eq!(exported["profile_sha256"], hex(identity.profile));
        assert_eq!(exported["profile"]["short_prearm_factor"], 2.5);
        assert_eq!(exported["fork_revision"], FORK_REVISION);
        assert_eq!(FORK_REVISION.len(), 40);
        assert_eq!(export(&round_trip).unwrap(), exported);
        let shared = std::sync::Arc::new(super::super::Shared::default());
        let mut guard = super::super::Guard::new(shared.clone());
        guard.install(&round_trip);
        let status = shared.guard_snapshot().unwrap().record;
        assert!(status.final_guard);
        assert_eq!(status.identity, identity);
    }

    #[test]
    fn canonical_float_notation_matches_identity_contract() {
        for (number, text) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1e-7, "1e-07"),
            (1e-4, "0.0001"),
            (1e-5, "1e-05"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (-12.5, "-12.5"),
            (1.234e20, "1.234e+20"),
        ] {
            assert_eq!(float_text(number), text);
        }
    }

    #[test]
    fn identity_changes_with_graph_or_profile_and_preserves_key_order_independence() {
        let original = super::super::tests::prepared_configuration();
        let original_export = export(&original).unwrap();
        let mut edited = original.config().clone();
        edited.title = Some("other".into());
        let renamed = config::validate_config(edited, None).unwrap();
        assert_ne!(original.identity().graph, renamed.identity().graph);
        assert_eq!(original.identity().profile, renamed.identity().profile);
        let mut edited = renamed.into_config();
        let config::Processor::Protection { parameters, .. } = edited
            .processors
            .as_mut()
            .unwrap()
            .get_mut("guard")
            .unwrap()
        else {
            unreachable!()
        };
        parameters.max_allowed_peak_db = -12.0;
        let changed = config::validate_config(edited, None).unwrap();
        assert_ne!(original.identity().graph, changed.identity().graph);
        assert_ne!(original.identity().profile, changed.identity().profile);
        assert_eq!(export(&original).unwrap(), original_export);
        // A changed admitted threshold must reach the guard, along with its
        // new identity. Reusing either old policy or old identity is unsafe.
        let mut guard =
            super::super::Guard::new(std::sync::Arc::new(super::super::Shared::default()));
        let mut candidate = super::super::tests::chunk();
        candidate.waveforms[0][0] = 0.3;
        guard.install(&original);
        guard.process(&mut candidate);
        assert!(guard.trip().is_none());
        guard.install(&changed);
        let mut candidate = super::super::tests::chunk();
        candidate.waveforms[0][0] = 0.3;
        guard.process(&mut candidate);
        assert_eq!(guard.trip().unwrap().reason, super::super::Reason::Peak);
        assert!(
            candidate
                .waveforms
                .iter()
                .flatten()
                .all(|sample| *sample == 0.0)
        );
        assert_eq!(
            guard.shared.guard_snapshot().unwrap().record.identity,
            changed.identity()
        );
        assert_eq!(
            digest(&serde_json::json!({"a": 1, "b": 2})).unwrap(),
            digest(&serde_json::from_str("{\"b\":2,\"a\":1}").unwrap()).unwrap()
        );
    }

    #[test]
    fn preparation_binds_identity_after_overrides_tokens_and_relative_paths() {
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(std::env::temp_dir().join(format!(
            "camilladsp-prepared-{}-{}",
            std::process::id(),
            super::super::monotonic_ns()
        )));
        std::fs::create_dir(&fixture.0).unwrap();
        let config_path = fixture.0.join("config.yml");
        let wav_path = fixture.0.join("capture.wav");
        let coefficient_path = fixture.0.join("coeff-48000-24.txt");
        let mut wav = std::fs::File::create(&wav_path).unwrap();
        crate::utils::wavtools::write_wav_header(
            &mut wav,
            24,
            config::BinarySampleFormat::S16_LE,
            48000,
        )
        .unwrap();
        std::fs::write(&coefficient_path, "1.0\n0.0\n").unwrap();
        let mut raw = serde_json::to_value(super::super::tests::configuration()).unwrap();
        // WAV metadata supplies local overrides without mutating global CLI
        // overrides or racing the other configuration tests.
        raw["devices"]["samplerate"] = serde_json::json!(24000);
        raw["devices"]["chunksize"] = serde_json::json!(128);
        raw["devices"]["capture"] = serde_json::json!({"type": "WavFile", "filename": wav_path});
        raw["filters"] = serde_json::json!({"fir-48000": {"type": "Conv", "parameters": {
            "type": "Raw", "filename": "coeff-$samplerate$-$channels$.txt", "format": "TEXT"
        }}});
        raw["pipeline"].as_array_mut().unwrap().insert(
            0,
            serde_json::json!({"type": "Filter", "names": ["fir-$samplerate$"]}),
        );
        let raw: config::Configuration = serde_json::from_value(raw).unwrap();
        std::fs::write(&config_path, yaml_serde::to_string(&raw).unwrap()).unwrap();
        let prepared = config::load_validate_config(config_path.to_str().unwrap()).unwrap();
        let mut expected = serde_json::to_value(raw).unwrap();
        expected["devices"]["samplerate"] = serde_json::json!(48000);
        expected["devices"]["chunksize"] = serde_json::json!(256);
        expected["filters"]["fir-48000"]["parameters"]["filename"] =
            serde_json::json!(coefficient_path);
        expected["pipeline"][0]["names"][0] = serde_json::json!("fir-48000");
        let expected: config::Configuration = serde_json::from_value(expected).unwrap();
        assert_eq!(prepared.config(), &expected);
        let expected_graph: Value =
            yaml_serde::from_str(&yaml_serde::to_string(&expected).unwrap()).unwrap();
        assert_eq!(prepared.identity().graph, digest(&expected_graph).unwrap());
        let exported = export(&prepared).unwrap();
        assert_eq!(
            prepared.identity().profile,
            digest(&exported["profile"]).unwrap()
        );
    }
}
