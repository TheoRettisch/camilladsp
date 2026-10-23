//! Version-1 protection operations. Socket handlers copy fixed atomic records;
//! only the processing owner applies queued control commands.

use super::{
    Shared,
    control::{self, Command, Expected, Mode, Operation},
    identity,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs, os::unix::fs::MetadataExt, sync::atomic::Ordering};

pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX: u64 = i64::MAX as u64;

#[derive(Debug)]
pub(super) struct Host {
    process: String,
    boot: String,
    namespace: u64,
}

impl Host {
    pub fn load() -> Option<Self> {
        let process = fs::read_to_string("/proc/sys/kernel/random/uuid")
            .ok()?
            .trim()
            .to_owned();
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()?
            .trim()
            .to_owned();
        uuid(&process)?;
        uuid(&boot)?;
        let namespace = fs::metadata("/proc/self/ns/time").ok()?.ino();
        (namespace > 0 && namespace <= MAX).then_some(Self {
            process,
            boot,
            namespace,
        })
    }
}

fn uuid(text: &str) -> Option<u128> {
    if text.len() != 36 || ![8, 13, 18, 23].iter().all(|i| text.as_bytes()[*i] == b'-') {
        return None;
    }
    let compact = text.replace('-', "");
    if compact.len() != 32
        || !compact
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    u128::from_str_radix(&compact, 16).ok()
}

fn uuid_text(value: u128) -> String {
    let hex = format!("{value:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Epoch {
    process: String,
    pipeline: u64,
    backend: u64,
    reset: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expectation {
    epoch: Epoch,
    trip_generation: u64,
    control_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
enum WireMode {
    Prearm,
    Armed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    command_id: u64,
    expected: Expectation,
    owner: String,
    parent_reset_generation: u64,
    qualification_id: u64,
    mode: WireMode,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reset {
    command_id: u64,
    expected: Expectation,
    owner: String,
    parent_reset_generation: u64,
    qualification_id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
enum Request {
    SetProtectionContext(Context),
    ResetProtection(Reset),
}

fn error(op: &str, code: &str) -> String {
    json!({op: {"result": "Error", "code": code}}).to_string()
}

fn epoch(host: &Host, pipeline: u64, backend: u64, reset: u64) -> Value {
    json!({"process": host.process, "pipeline": pipeline, "backend": backend, "reset": reset})
}

fn receipt_result(result: control::Result) -> &'static str {
    match result {
        control::Result::Applied => "applied",
        control::Result::Invalid => "invalid",
        control::Result::StateMismatch => "state_mismatch",
        control::Result::NotQualified => "not_qualified",
    }
}

impl Shared {
    fn status_value(&self) -> Result<Value, &'static str> {
        let host = self.host.as_ref().ok_or("unavailable")?;
        for _ in 0..3 {
            let Some(guard) = self.guard_snapshot_once() else {
                continue;
            };
            let Some((sequence, p)) = self.playback_record.read_once() else {
                continue;
            };
            if sequence == 0
                || p[0] != guard.pipeline
                || p[1] != guard.backend
                || self.guard_record.sequence.load(Ordering::SeqCst) != guard.sequence
                || self.playback_record.sequence.load(Ordering::SeqCst) != sequence
                || self.epoch.load(Ordering::SeqCst) != guard.pipeline
                || self.backend.load(Ordering::SeqCst) != guard.backend
                || self.reset.load(Ordering::SeqCst) != guard.reset
            {
                continue;
            }
            let now = super::monotonic_ns();
            let current_reset = p[2] == guard.reset;
            let mut telemetry_valid = current_reset && p[9] != 0 && p[10] >= guard.frame_floor;
            let receipt = guard.control.last_command.map(|r| json!({
                "command_id": r.command_id, "result": receipt_result(r.result),
                "applied_epoch": epoch(host, r.pipeline, r.backend, r.reset),
                "control_revision": r.revision, "parent_reset_generation": r.parent_reset_generation,
                "qualification_id": r.qualification_id,
            }));
            let reason = guard.trip.map(|trip| match trip.reason {
                super::Reason::Peak => "peak",
                super::Reason::Nonfinite => "nonfinite",
                super::Reason::Coverage => "coverage",
                super::Reason::Rms => "rms",
                super::Reason::ShortPower => "short_power",
                super::Reason::LongPower => "long_power",
                super::Reason::Tone => "tone",
                super::Reason::DcFrozen => "dc_frozen",
            });
            let state = match guard.state {
                control::State::Inactive => "inactive",
                control::State::Qualifying => "qualifying",
                control::State::Active => "active",
                control::State::Tripped => "tripped",
            };
            let mut powers: Vec<f64> = p[16..40].iter().map(|bits| f64::from_bits(*bits)).collect();
            if powers
                .iter()
                .any(|power| !power.is_finite() || *power < 0.0)
            {
                telemetry_valid = false;
                powers.fill(0.0);
            }
            return Ok(json!({
                "protocol": 1,
                "clock": {"name": "CLOCK_MONOTONIC", "boot_id": host.boot, "time_namespace_inode": host.namespace},
                "snapshot_at_ns": now,
                "epoch": epoch(host, guard.pipeline, guard.backend, guard.reset),
                "epoch_frame_floor": guard.frame_floor,
                "identity": {"fork_revision": git_version::git_version!(args = ["--always", "--abbrev=40"], fallback = "unavailable"),
                    "graph_sha256": identity::hex(guard.identity.graph), "profile_sha256": identity::hex(guard.identity.profile),
                    "backend": "alsa-hw-ring-v1", "sample_rate": super::SAMPLE_RATE, "chunk_frames": super::FRAMES,
                    "output_channels": (0..super::CHANNELS).collect::<Vec<_>>(), "tweeter_channels": [0, 1]},
                "guard": {"state": state, "final": guard.final_guard, "coverage_valid": guard.final_guard && guard.processing_valid,
                    "candidate_observable": guard.candidate_observable, "short_power_veto": guard.short_power_veto,
                    "qualified_frames": guard.qualified_frames},
                "trip": {"generation": guard.trip_generation, "latched": guard.trip.is_some(), "reason": reason,
                    "channel": guard.trip.and_then(|t| t.channel), "frame": guard.trip.map(|t| t.frame), "at_ns": guard.trip.map(|t| t.at_ns)},
                "control": {"owner": guard.control.owner.map(uuid_text), "revision": guard.control.revision,
                    "parent_reset_generation": guard.control.parent_reset_generation, "qualification_id": guard.control.qualification_id,
                    "mode": if guard.control.mode == Mode::Armed {"armed"} else {"prearm"}, "last_command": receipt},
                "processing": {"valid": guard.processing_valid, "sequence": guard.sequence, "frames": guard.frames,
                    "source_begin_ns": if guard.processing_valid { guard.source_begin_ns } else {0},
                    "source_end_ns": if guard.processing_valid { guard.source_end_ns } else {0}},
                "playback": {"valid": current_reset && p[5] != 0 && p[4] > guard.frame_floor, "sequence": sequence,
                    "frames": p[4], "submitted_frames": p[3], "source_begin_ns": if current_reset {p[6]} else {0},
                    "source_end_ns": if current_reset {p[7]} else {0}},
                "telemetry": {"valid": telemetry_valid, "frame_begin": p[10], "frame_end": p[11],
                    "source_begin_ns": p[12], "source_end_ns": p[13], "write_begin_ns": p[14], "write_end_ns": p[15],
                    "present": if telemetry_valid {(0..super::CHANNELS).collect::<Vec<_>>()} else {Vec::new()}, "rms_power": powers},
                "activity": {"valid": guard.processing_valid, "generation": guard.activity_generation,
                    "through_frame": guard.frames, "silent_frames": guard.silent_frames, "last_active_end_frame": guard.last_active_end_frame},
                "conversion": {"valid": p[40] != 0, "clipped_samples": p[41], "checked_at_ns": p[42]},
            }));
        }
        Err("incoherent")
    }

    pub fn protection_reply(&self, text: &str) -> Option<String> {
        // Probe only the first root string/key; the strict typed parse below
        // retains duplicate-field rejection before any mutation can be queued.
        let trimmed = text.trim_start();
        let first = trimmed.strip_prefix('{').unwrap_or(trimmed).trim_start();
        let key = serde_json::Deserializer::from_str(first)
            .into_iter::<String>()
            .next()?
            .ok()?;
        if ![
            "GetProtectionStatus",
            "SetProtectionContext",
            "ResetProtection",
        ]
        .contains(&key.as_str())
        {
            return None;
        }
        let code = if key == "GetProtectionStatus" {
            "unsupported"
        } else {
            "invalid"
        };
        if text.len() > MAX_MESSAGE_BYTES {
            return Some(error(&key, code));
        }
        if key == "GetProtectionStatus" {
            if !matches!(
                serde_json::from_str::<String>(text).as_deref(),
                Ok("GetProtectionStatus")
            ) {
                return Some(error(&key, "unsupported"));
            }
            return Some(match self.status_value() {
                Ok(status) => json!({key: {"result": "Ok", "value": status}}).to_string(),
                Err(code) => error(&key, code),
            });
        }
        let request = match serde_json::from_str::<Request>(text) {
            Ok(request) => request,
            Err(_) => return Some(error(&key, "invalid")),
        };
        let (id, expected, owner, parent_reset_generation, qualification_id, operation) =
            match request {
                Request::SetProtectionContext(c) => (
                    c.command_id,
                    c.expected,
                    c.owner,
                    c.parent_reset_generation,
                    c.qualification_id,
                    Operation::Context(match c.mode {
                        WireMode::Prearm => Mode::Prearm,
                        WireMode::Armed => Mode::Armed,
                    }),
                ),
                Request::ResetProtection(c) => (
                    c.command_id,
                    c.expected,
                    c.owner,
                    c.parent_reset_generation,
                    c.qualification_id,
                    Operation::Reset,
                ),
            };
        let Some(owner) = uuid(&owner) else {
            return Some(error(&key, "invalid"));
        };
        let e = expected.epoch;
        if uuid(&e.process).is_none()
            || id == 0
            || owner == 0
            || [
                id,
                parent_reset_generation,
                qualification_id,
                e.pipeline,
                e.backend,
                e.reset,
                expected.trip_generation,
                expected.control_revision,
            ]
            .iter()
            .any(|v| *v > MAX)
        {
            return Some(error(&key, "invalid"));
        }
        if self
            .host
            .as_ref()
            .is_none_or(|host| host.process != e.process)
        {
            return Some(error(&key, "state_mismatch"));
        }
        let command = Command {
            id,
            owner,
            parent_reset_generation,
            qualification_id,
            operation,
            expected: Expected {
                pipeline: e.pipeline,
                backend: e.backend,
                reset: e.reset,
                trip_generation: expected.trip_generation,
                revision: expected.control_revision,
            },
        };
        Some(match self.submit(command) {
            Ok(()) => {
                json!({key: {"result": "Ok", "value": {"command_id": id, "disposition": "queued"}}})
                    .to_string()
            }
            Err(_) => error(&key, "busy"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protection::{
        Guard,
        playback::Playback,
        tests::{chunk, configuration},
    };
    use std::sync::Arc;

    fn setup() -> (Arc<Shared>, Guard, Playback) {
        let shared = Arc::new(Shared::default());
        let mut guard = Guard::new(shared.clone());
        let mut conf = configuration();
        crate::config::validate_config(&mut conf, None).unwrap();
        guard.install(&conf).unwrap();
        let playback = Playback::new(shared.clone());
        (shared, guard, playback)
    }

    fn status(shared: &Shared) -> Value {
        let text = shared.protection_reply("\"GetProtectionStatus\"").unwrap();
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply["GetProtectionStatus"]["result"], "Ok", "{text}");
        reply["GetProtectionStatus"]["value"].clone()
    }

    fn request(status: &Value, id: u64, reset: bool) -> String {
        let mut command = json!({"command_id": id,
            "expected": {"epoch": status["epoch"], "trip_generation": status["trip"]["generation"],
                "control_revision": status["control"]["revision"]},
            "owner": "00000000-0000-0000-0000-000000000007", "parent_reset_generation": u64::from(reset),
            "qualification_id": id});
        if !reset {
            command["mode"] = json!("prearm");
        }
        json!({if reset {"ResetProtection"} else {"SetProtectionContext"}: command}).to_string()
    }

    #[test]
    fn offline_frame_to_wire_slice_for_parent_replay() {
        let (shared, mut guard, mut playback) = setup();
        // Compile-time test-only bypass. Production constructor always inhibits.
        guard.activation_inhibited = false;
        let mut replies = Vec::new();
        let capture = |replies: &mut Vec<Value>| {
            replies.push(
                serde_json::from_str(&shared.protection_reply("\"GetProtectionStatus\"").unwrap())
                    .unwrap(),
            );
        };
        capture(&mut replies);
        let prearm = request(&status(&shared), 1, false);
        shared.protection_reply(&prearm).unwrap();
        guard.control_only(shared.commands().try_recv().unwrap());
        let play = |audio: &crate::audiochunk::AudioChunk, playback: &mut Playback| {
            let stamp = audio.protection.as_ref().unwrap();
            assert!(playback.accepts(stamp));
            playback.converted(0);
            let begin = super::super::monotonic_ns();
            playback.written(256, 256).unwrap();
            playback
                .observe(1024, 1024, begin, super::super::monotonic_ns())
                .unwrap();
            playback.completed(stamp, begin, super::super::monotonic_ns());
        };
        for index in 0..=super::super::QUALIFICATION_FRAMES / 256 {
            let mut audio = chunk();
            guard.process(&mut audio);
            if index < super::super::QUALIFICATION_FRAMES / 256 {
                assert!(audio.waveforms.iter().flatten().all(|v| *v == 0.0));
            } else {
                assert!(
                    audio
                        .waveforms
                        .iter()
                        .flatten()
                        .all(|v| *v == 0.1 || *v == 0.0)
                );
            }
            play(&audio, &mut playback);
        }
        capture(&mut replies);
        let mut armed: Value = serde_json::from_str(&request(&status(&shared), 2, false)).unwrap();
        armed["SetProtectionContext"]["mode"] = json!("armed");
        armed["SetProtectionContext"]["qualification_id"] = json!(1);
        shared.protection_reply(&armed.to_string()).unwrap();
        let mut audio = chunk();
        guard.process_with_command(&mut audio, Some(shared.commands().try_recv().unwrap()));
        assert!(
            audio
                .waveforms
                .iter()
                .flatten()
                .all(|v| *v == 0.1 || *v == 0.0)
        );
        play(&audio, &mut playback);
        capture(&mut replies);
        for index in 0..3 {
            let mut audio = chunk();
            if index == 0 {
                audio.waveforms[1][17] = 1.0;
            }
            guard.process(&mut audio);
            assert!(audio.waveforms.iter().flatten().all(|v| *v == 0.0));
            play(&audio, &mut playback);
        }
        capture(&mut replies); // The faulting chunk's reply was skipped.
        capture(&mut replies);
        guard.pause();
        let reset = request(&status(&shared), 3, true);
        shared.protection_reply(&reset).unwrap();
        guard.control_only(shared.commands().try_recv().unwrap());
        capture(&mut replies);
        assert_eq!(
            replies[5]["GetProtectionStatus"]["value"]["control"]["last_command"]["result"],
            "applied"
        );
        if let Ok(path) = std::env::var("P2_SLICE_FIXTURE") {
            let config = yaml_serde::to_string(&configuration()).unwrap();
            fs::write(
                path,
                serde_json::to_string_pretty(&json!({
                    "kind": "offline-test-only-ring-model", "config_yaml": config,
                    "requests": [prearm, armed.to_string(), reset], "replies": replies,
                }))
                .unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn actual_wire_commands_reset_while_paused_and_reject_duplicates() {
        let (shared, mut guard, _playback) = setup();
        for (id, reset) in [(1, false), (2, true)] {
            let text = request(&status(&shared), id, reset);
            let duplicate =
                text.replacen("\"command_id\":", "\"command_id\":99,\"command_id\":", 1);
            assert!(
                shared
                    .protection_reply(&duplicate)
                    .unwrap()
                    .contains("invalid")
            );
            assert!(shared.commands().is_empty());
            assert!(shared.protection_reply(&text).unwrap().contains("queued"));
            guard.control_only(shared.commands().try_recv().unwrap());
            let result = status(&shared);
            assert_eq!(result["control"]["last_command"]["result"], "applied");
            assert_eq!(result["control"]["last_command"]["command_id"], id);
            assert_eq!(result["processing"]["frames"], 0);
            assert_eq!(result["playback"]["frames"], 0);
            assert_eq!(result["processing"]["valid"], false);
            assert_eq!(result["playback"]["valid"], false);
            assert_eq!(result["epoch"]["reset"], u64::from(reset));
        }
    }

    #[test]
    fn skipped_replies_preserve_trip_and_fully_written_telemetry() {
        let (shared, mut guard, mut playback) = setup();
        for index in 0..3 {
            let mut audio = chunk();
            if index == 1 {
                audio.waveforms[1][255] = 1.0;
            }
            guard.process(&mut audio);
            assert!(audio.waveforms.iter().flatten().all(|v| *v == 0.0));
            let stamp = audio.protection.as_ref().unwrap();
            assert!(playback.accepts(stamp));
            playback.converted(0);
            playback.written(256, 256).unwrap();
            let begin = super::super::monotonic_ns();
            playback
                .observe(1024, 1024, begin, super::super::monotonic_ns())
                .unwrap();
            playback.completed(stamp, begin, super::super::monotonic_ns());
        }
        let result = status(&shared);
        assert_eq!(result["trip"]["latched"], true);
        assert_eq!(result["trip"]["reason"], "peak");
        assert_eq!(result["trip"]["frame"], 511);
        assert_eq!(result["playback"]["frames"], 768);
        assert_eq!(result["telemetry"]["frame_begin"], 512);
        assert_eq!(result["telemetry"]["valid"], true);
        assert_eq!(
            result["identity"]["graph_sha256"],
            identity::hex(guard.identity.unwrap().graph)
        );
        let repeated = status(&shared);
        for key in [
            "trip",
            "processing",
            "playback",
            "telemetry",
            "activity",
            "conversion",
        ] {
            assert_eq!(repeated[key], result[key]);
        }
        if let Ok(path) = std::env::var("P2_WIRE_FIXTURE") {
            fs::write(
                path,
                shared.protection_reply("\"GetProtectionStatus\"").unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn fixed_production_inhibit_survives_real_context_and_full_qualification() {
        let (shared, mut guard, _playback) = setup();
        let text = request(&status(&shared), 1, false);
        assert!(shared.protection_reply(&text).unwrap().contains("queued"));
        guard.control_only(shared.commands().try_recv().unwrap());
        for _ in 0..2 * super::super::QUALIFICATION_FRAMES / 256 {
            let mut audio = chunk();
            guard.process(&mut audio);
            assert!(audio.waveforms.iter().flatten().all(|v| *v == 0.0));
        }
        assert!(guard.qualified_frames >= super::super::QUALIFICATION_FRAMES);
        assert_eq!(status(&shared)["guard"]["state"], "qualifying");
        let mut command: Value =
            serde_json::from_str(&request(&status(&shared), 2, false)).unwrap();
        command["SetProtectionContext"]["mode"] = json!("armed");
        command["SetProtectionContext"]["qualification_id"] = json!(1);
        assert!(
            shared
                .protection_reply(&command.to_string())
                .unwrap()
                .contains("queued")
        );
        guard.process_with_command(&mut chunk(), Some(shared.commands().try_recv().unwrap()));
        assert_eq!(
            status(&shared)["control"]["last_command"]["result"],
            "not_qualified"
        );
    }

    #[test]
    fn wire_rejects_wrong_process_unknown_fields_and_boolean_counters() {
        let (shared, guard, _playback) = setup();
        let base: Value = serde_json::from_str(&request(&status(&shared), 1, false)).unwrap();
        for field in ["command_id", "qualification_id", "parent_reset_generation"] {
            let mut request = base.clone();
            request["SetProtectionContext"][field] = json!(true);
            assert!(
                shared
                    .protection_reply(&request.to_string())
                    .unwrap()
                    .contains("invalid")
            );
        }
        let mut request = base.clone();
        request["SetProtectionContext"]["extra"] = json!(0);
        assert!(
            shared
                .protection_reply(&request.to_string())
                .unwrap()
                .contains("invalid")
        );
        request = base;
        request["SetProtectionContext"]["expected"]["epoch"]["process"] =
            json!("00000000-0000-0000-0000-000000000000");
        assert!(
            shared
                .protection_reply(&request.to_string())
                .unwrap()
                .contains("state_mismatch")
        );
        assert!(shared.commands().is_empty());
        assert_eq!(guard.control.revision, 0);
    }
}
