//! Identities are prepared during configuration validation, outside audio work.

use crate::{Res, config};
use serde_json::Value;
use sha2::{Digest, Sha256};

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

pub fn prepare(conf: &config::Configuration) -> Res<Identity> {
    let parameters = super::admission(conf)?;
    // GetConfig uses this same YAML serialization. Round-trip it before
    // canonicalization so f32 config scalars have the same text semantics.
    let graph: Value = yaml_serde::from_str(&yaml_serde::to_string(conf)?)?;
    let profile = serde_json::json!({
        "algorithm": "protection-p2-instantaneous-v1",
        "parameters": parameters,
        "qualification_frames": super::QUALIFICATION_FRAMES,
        "demand_threshold_db": -70.0,
        "activation_inhibited": true,
    });
    Ok(Identity {
        graph: digest(&graph)?,
        profile: digest(&profile)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut conf = super::super::tests::configuration();
        let old = prepare(&conf).unwrap();
        conf.title = Some("other".into());
        let renamed = prepare(&conf).unwrap();
        assert_ne!(old.graph, renamed.graph);
        assert_eq!(old.profile, renamed.profile);
        assert_eq!(
            digest(&serde_json::json!({"a": 1, "b": 2})).unwrap(),
            digest(&serde_json::from_str("{\"b\":2,\"a\":1}").unwrap()).unwrap()
        );
    }
}
