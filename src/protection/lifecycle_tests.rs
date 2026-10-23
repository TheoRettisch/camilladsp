use super::*;
use crate::ProcessingParameters;
use crate::filters::basicfilters::{AppliedVolume, Volume};
use crate::pipeline::Pipeline;
use control::{Expected, Result as ControlResult};
use tests::{chunk, configuration};

fn setup(muted: bool, ramp_ms: f32) -> (Arc<ProcessingParameters>, Pipeline, Guard) {
    let params = Arc::new(ProcessingParameters::new(&[0.0; 5], &[muted; 5]));
    let mut conf = configuration();
    conf.devices.volume_ramp_time = Some(ramp_ms);
    let pipeline = Pipeline::from_config(conf.clone(), params.clone());
    let mut guard = Guard::new(params.protection.clone());
    guard.install(&conf).unwrap();
    (params, pipeline, guard)
}

fn command(guard: &Guard, operation: Operation, qualification_id: u64) -> Command {
    Command {
        id: guard.control.command_high_water + 1,
        expected: guard.expected(),
        owner: guard.control.owner.unwrap_or(7),
        parent_reset_generation: guard.control.parent_reset_generation
            + u64::from(operation == Operation::Reset),
        qualification_id,
        operation,
    }
}

fn prearm(guard: &mut Guard, qualification_id: u64) {
    let command = command(guard, Operation::Context(Mode::Prearm), qualification_id);
    guard.control_only(command);
    assert_eq!(
        guard.control.last_command.unwrap().result,
        ControlResult::Applied
    );
}

fn process(pipeline: &mut Pipeline, guard: &mut Guard) -> AudioChunk {
    let mut candidate = pipeline.process_chunk(chunk());
    guard.process(&mut candidate);
    candidate
}

fn zero(chunk: &AudioChunk) -> bool {
    chunk.waveforms.iter().flatten().all(|s| *s == 0.0)
}

#[test]
fn production_without_context_never_activates_after_a_full_window() {
    let (_, mut pipeline, mut guard) = setup(false, 0.0);
    for _ in 0..2 * QUALIFICATION_FRAMES / FRAMES as u64 {
        assert!(zero(&process(&mut pipeline, &mut guard)));
        assert_eq!(guard.state(), State::Inactive);
        assert_eq!(guard.qualified_frames, 0);
    }
    assert!(guard.control.owner.is_none());
}

fn qualify(pipeline: &mut Pipeline, guard: &mut Guard) {
    assert!(zero(&process(pipeline, guard))); // Candidate baseline is not duration.
    assert_eq!(guard.qualified_frames, 0);
    for _ in 1..QUALIFICATION_FRAMES / FRAMES as u64 {
        assert!(zero(&process(pipeline, guard)));
        assert!(!guard.active);
    }
    assert!(!zero(&process(pipeline, guard)));
    assert_eq!(guard.qualified_frames, QUALIFICATION_FRAMES);
    assert_eq!(guard.state(), State::Active);
}

#[test]
fn muted_start_and_full_opening_ramp_cannot_qualify() {
    let (params, mut pipeline, mut guard) = setup(true, 400.0);
    prearm(&mut guard, 1);
    for _ in 0..400 {
        assert!(zero(&process(&mut pipeline, &mut guard)));
        assert!(!guard.candidate_observable);
        assert_eq!(guard.qualified_frames, 0);
    }
    params.set_mute(0, false);
    assert!(zero(&process(&mut pipeline, &mut guard)));
    assert!(guard.attempt_required);
    let stale = command(&guard, Operation::Context(Mode::Prearm), 1);
    guard.control_only(stale);
    assert_eq!(
        guard.control.last_command.unwrap().result,
        ControlResult::StateMismatch
    );
    prearm(&mut guard, 2);
    for _ in 1..75 {
        assert!(zero(&process(&mut pipeline, &mut guard)));
        assert!(!guard.candidate_observable); // Includes the final opening chunk.
        assert_eq!(guard.qualified_frames, 0);
    }
    qualify(&mut pipeline, &mut guard);
    assert!(!params.is_mute(0));
}

#[test]
fn admitted_ordinary_mute_and_volume_ramps_keep_native_samples() {
    for sign in [-1.0, 1.0] {
        let (params, mut pipeline, mut guard) = setup(false, 400.0);
        prearm(&mut guard, 1);
        qualify(&mut pipeline, &mut guard);
        let mut reference = Volume::new(
            "reference",
            400.0,
            0.0,
            0.0,
            false,
            FRAMES,
            SAMPLE_RATE,
            params.clone(),
            0,
        );
        for block in 0..180 {
            if block == 2 {
                params.set_target_volume(0, -6.0);
            }
            if block == 10 {
                params.set_mute(0, true);
            }
            if block == 30 {
                params.set_target_volume(0, -12.0);
            }
            let mut input = chunk();
            for waveform in &mut input.waveforms {
                waveform.fill(sign * 0.1);
            }
            let mut expected = AudioChunk::new(input.waveforms.clone(), 0.1, -0.1, FRAMES, FRAMES);
            reference.process_chunk(&mut expected);
            let mut actual = pipeline.process_chunk(input);
            guard.process(&mut actual);
            assert_eq!(actual.waveforms, expected.waveforms, "block {block}");
            assert!(guard.active);
            if block < 10 {
                assert!(guard.candidate_observable);
            } else {
                assert!(!guard.candidate_observable);
            }
        }
        params.set_mute(0, false);
        assert!(zero(&process(&mut pipeline, &mut guard)));
        assert!(!guard.active);
        assert!(guard.attempt_required);
    }
}

#[test]
fn applied_chunk_state_wins_over_later_control_flags_and_survives_mixer() {
    let (params, mut pipeline, mut guard) = setup(false, 0.0);
    prearm(&mut guard, 1);
    let mut candidate = pipeline.process_chunk(chunk());
    params.set_mute(0, true);
    guard.process(&mut candidate);
    assert!(guard.candidate_observable);
    let mut candidate = pipeline.process_chunk(chunk());
    params.set_mute(0, false);
    guard.process(&mut candidate);
    assert!(!guard.candidate_observable);
    assert!(zero(&candidate));
    assert_eq!(guard.qualified_frames, 0);

    let mut conf = serde_json::to_value(configuration()).unwrap();
    conf["mixers"] = serde_json::json!({"identity": {
        "channels": {"in": 24, "out": 24},
        "mapping": (0..24).map(|c| serde_json::json!({
            "dest": c, "sources": [{"channel": c, "gain": 0}]
        })).collect::<Vec<_>>()
    }});
    conf["pipeline"]
        .as_array_mut()
        .unwrap()
        .insert(0, serde_json::json!({"type": "Mixer", "name": "identity"}));
    let mut pipeline = Pipeline::from_config(serde_json::from_value(conf).unwrap(), params);
    let mixed = pipeline.process_chunk(chunk());
    assert_eq!(
        mixed.main_volume,
        Some(AppliedVolume {
            muted: false,
            opening: false
        })
    );
    assert_eq!(mixed.waveforms, chunk().waveforms);
}

#[test]
fn reversing_opening_cancels_credit_and_requires_another_attempt() {
    let (params, mut pipeline, mut guard) = setup(true, 400.0);
    prearm(&mut guard, 1);
    process(&mut pipeline, &mut guard);
    params.set_mute(0, false);
    process(&mut pipeline, &mut guard);
    prearm(&mut guard, 2);
    for _ in 0..20 {
        assert!(zero(&process(&mut pipeline, &mut guard)));
    }
    params.set_mute(0, true);
    assert!(zero(&process(&mut pipeline, &mut guard)));
    assert!(guard.attempt_required);
    params.set_mute(0, false);
    assert!(zero(&process(&mut pipeline, &mut guard)));
    prearm(&mut guard, 3);
    params.set_target_volume(0, -6.0); // Restart opening ramp at the current gain.
    for _ in 0..75 {
        assert!(zero(&process(&mut pipeline, &mut guard)));
        assert!(!guard.candidate_observable);
    }
    qualify(&mut pipeline, &mut guard);
}

#[test]
fn hidden_fault_is_contained_on_unmute_and_cannot_arm() {
    for offset in [0, FRAMES - 1] {
        for sample in [
            1.0,
            -1.0,
            PrcFmt::NAN,
            PrcFmt::INFINITY,
            PrcFmt::NEG_INFINITY,
        ] {
            let (params, mut pipeline, mut guard) = setup(true, 400.0);
            prearm(&mut guard, 1);
            for _ in 0..400 {
                assert!(zero(&process(&mut pipeline, &mut guard)));
            }
            params.set_mute(0, false);
            let mut candidate = pipeline.process_chunk(chunk());
            candidate.waveforms[1][offset] = sample; // Actual final assessment boundary.
            let armed = command(&guard, Operation::Context(Mode::Armed), 1);
            guard.process_with_command(&mut candidate, Some(armed));
            assert!(zero(&candidate));
            let trip = guard.trip().unwrap();
            assert_eq!(trip.frame, 400 * FRAMES as u64 + offset as u64);
            assert_eq!(
                guard.control.last_command.unwrap().result,
                ControlResult::StateMismatch
            );
            for _ in 0..400 {
                assert!(zero(&process(&mut pipeline, &mut guard)));
            }
            assert_eq!(guard.trip(), Some(trip));
        }
    }
}

#[test]
fn reset_is_exact_preserves_frames_and_mute_and_retrip_advances_generation() {
    let (params, mut pipeline, mut guard) = setup(true, 0.0);
    prearm(&mut guard, 1);
    let mut candidate = pipeline.process_chunk(chunk());
    candidate.waveforms[0][0] = 1.0;
    guard.process(&mut candidate);
    let old_stamp = candidate.protection.take().unwrap();
    let old_trip = guard.trip().unwrap();
    let mut stale = command(&guard, Operation::Reset, 2);
    stale.expected.trip_generation -= 1;
    guard.control_only(stale);
    assert_eq!(guard.trip(), Some(old_trip));
    assert_eq!(guard.reset, 0);
    let reset = command(&guard, Operation::Reset, 2);
    guard.control_only(reset);
    assert!(params.is_mute(0));
    assert_eq!(guard.frames, FRAMES as u64);
    assert_eq!(guard.frame_floor, FRAMES as u64);
    assert_eq!(guard.reset, 1);
    assert!(!old_stamp.is_current());
    let snapshot = guard.shared.guard_snapshot().unwrap();
    assert!(!snapshot.processing_valid);
    assert_eq!((snapshot.source_begin_ns, snapshot.source_end_ns), (0, 0));
    assert_eq!(snapshot.trip_generation, old_trip.generation);
    assert!(snapshot.trip.is_none());
    guard.control_only(reset); // Replay cannot reset again.
    assert_eq!(guard.reset, 1);
    assert_eq!(
        guard.control.last_command.unwrap().result,
        ControlResult::StateMismatch
    );
    let mut candidate = pipeline.process_chunk(chunk());
    candidate.waveforms[0][FRAMES - 1] = 1.0;
    guard.process(&mut candidate);
    assert!(zero(&candidate));
    assert_eq!(guard.trip().unwrap().generation, old_trip.generation + 1);
    assert_eq!(guard.frames, 2 * FRAMES as u64);
}

#[test]
fn reset_receipt_and_same_chunk_new_trip_are_coherent() {
    let (_, mut pipeline, mut guard) = setup(false, 0.0);
    prearm(&mut guard, 1);
    let mut hazard = pipeline.process_chunk(chunk());
    hazard.waveforms[0][0] = 1.0;
    guard.process(&mut hazard);
    let reset = command(&guard, Operation::Reset, 2);
    let mut hazard = pipeline.process_chunk(chunk());
    hazard.waveforms[0][0] = 1.0;
    guard.process_with_command(&mut hazard, Some(reset));
    let snapshot = guard.shared.guard_snapshot().unwrap();
    assert_eq!(
        snapshot.control.last_command.unwrap().result,
        ControlResult::Applied
    );
    assert_eq!(snapshot.trip.unwrap().generation, 2);
    assert_eq!(snapshot.state, State::Tripped);
    assert!(zero(&hazard));
    assert_eq!(guard.shared.guard_snapshot(), Some(snapshot)); // Reads do not consume.
}

#[test]
fn reset_hold_credit_pause_and_old_armed_receipts_cannot_qualify_next_attempt() {
    let (_, mut pipeline, mut guard) = setup(false, 0.0);
    prearm(&mut guard, 1);
    let reset = command(&guard, Operation::Reset, 2);
    guard.control_only(reset);
    qualify(&mut pipeline, &mut guard); // Simulated healthy audio during parent hold.
    prearm(&mut guard, 3);
    assert!(!guard.active);
    assert_eq!(guard.qualified_frames, 0);
    qualify(&mut pipeline, &mut guard);
    let armed = command(&guard, Operation::Context(Mode::Armed), 3);
    let mut candidate = pipeline.process_chunk(chunk());
    guard.process_with_command(&mut candidate, Some(armed));
    assert_eq!(guard.control.mode, Mode::Armed);
    assert_eq!(
        guard.control.last_command.unwrap().result,
        ControlResult::Applied
    );
    let old_receipt = guard.control.last_command;
    let before = guard.shared.guard_snapshot().unwrap();
    guard.pause();
    let paused = guard.shared.guard_snapshot().unwrap();
    assert_eq!(paused.frames, before.frames);
    assert_eq!(paused.source_begin_ns, before.source_begin_ns);
    assert_eq!(paused.source_end_ns, before.source_end_ns);
    assert_eq!(paused.control.last_command, old_receipt);
    assert!(!paused.processing_valid);
    assert_eq!(paused.state, State::Inactive);
    let request = command(&guard, Operation::Context(Mode::Armed), 3);
    guard.control_only(request);
    assert_eq!(
        guard.control.last_command.unwrap().result,
        ControlResult::NotQualified
    );
    prearm(&mut guard, 4);
    assert_eq!(guard.qualified_frames, 0);
    qualify(&mut pipeline, &mut guard);
}

#[test]
fn replacement_and_owner_change_preserve_trip_and_reject_old_commands() {
    let (_, mut pipeline, mut guard) = setup(false, 0.0);
    prearm(&mut guard, 1);
    let old = command(&guard, Operation::Context(Mode::Armed), 1);
    let mut candidate = pipeline.process_chunk(chunk());
    candidate.waveforms[0][0] = 1.0;
    guard.process(&mut candidate);
    let trip = guard.trip();
    guard.install(&configuration()).unwrap();
    guard.control_only(old);
    assert_eq!(
        guard.control.last_command.unwrap().result,
        ControlResult::StateMismatch
    );
    assert_eq!(guard.trip(), trip);
    let mut takeover = command(&guard, Operation::Context(Mode::Prearm), 0);
    takeover.owner = 8;
    takeover.id = 1;
    guard.control_only(takeover);
    assert_eq!(guard.control.owner, Some(8));
    assert_eq!(guard.trip(), trip);
    guard.control_only(old);
    assert_eq!(
        guard.control.last_command.unwrap().result,
        ControlResult::StateMismatch
    );
    assert_eq!(guard.control.owner, Some(8));
    assert_eq!(guard.trip(), trip);
}

#[test]
fn missing_chunk_observation_cannot_reuse_active_state() {
    let (_, mut pipeline, mut guard) = setup(false, 0.0);
    prearm(&mut guard, 1);
    qualify(&mut pipeline, &mut guard);
    let mut candidate = chunk();
    candidate.main_volume = None;
    guard.process(&mut candidate);
    assert!(zero(&candidate));
    assert!(!guard.candidate_observable);
    assert!(guard.attempt_required);
}

#[test]
fn bounded_mailbox_rejects_a_second_pending_command() {
    let shared = Shared::default();
    let request = Command {
        id: 1,
        expected: Expected {
            pipeline: 0,
            reset: 0,
            trip_generation: 0,
            revision: 0,
        },
        owner: 1,
        parent_reset_generation: 0,
        qualification_id: 1,
        operation: Operation::Context(Mode::Prearm),
    };
    shared.submit(request).unwrap();
    assert!(matches!(
        shared.submit(request),
        Err(crossbeam_channel::TrySendError::Full(_))
    ));
    assert_eq!(shared.commands().try_recv().unwrap(), request);
}
