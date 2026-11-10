// CamillaDSP - A flexible tool for processing audio
// Copyright (C) 2026 Henrik Enquist
//
// This file is part of CamillaDSP.
//
// CamillaDSP is free software; you can redistribute it and/or modify it
// under the terms of either:
//
// a) the GNU General Public License version 3,
//    or
// b) the Mozilla Public License Version 2.0.
//
// You should have received copies of the GNU General Public License and the
// Mozilla Public License along with this program. If not, see
// <https://www.gnu.org/licenses/> and <https://www.mozilla.org/MPL/2.0/>.

use crate::ProcessingParameters;
use crate::audiodevice::*;
use crate::config;
use crate::pipeline;
use audio_thread_priority::{
    demote_current_thread_from_real_time, promote_current_thread_to_real_time,
};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

// Leave room for command transport and status delivery inside the unchanged
// 30 ms parent deadline. A healthy 48 kHz/256-frame candidate arrives every
// 5.333 ms; absence beyond this wait rejects only the pending arm request.
const ARM_CANDIDATE_WAIT: Duration = Duration::from_millis(10);

struct ProcessingControl {
    paused: bool,
    armed: Option<crate::protection::control::Command>,
    armed_deadline: Option<Instant>,
}

impl ProcessingControl {
    fn service(
        &mut self,
        protection: &mut crate::protection::Guard,
        command: crate::protection::control::Command,
    ) {
        use crate::protection::control::{Mode, Operation};
        if !self.paused && command.operation == Operation::Context(Mode::Armed) {
            // Between chunks is not a capture pause. Assess the next candidate
            // before applying armed, but do not wait for the capture backend
            // to report a stall (which can take longer than the parent deadline).
            self.armed = Some(command);
            self.armed_deadline = Some(Instant::now() + ARM_CANDIDATE_WAIT);
        } else {
            protection.control_only(command);
        }
    }

    fn arm_timeout(&self) -> crossbeam_channel::Receiver<Instant> {
        match self.armed_deadline {
            Some(deadline) => crossbeam_channel::at(deadline),
            None => crossbeam_channel::never(),
        }
    }

    fn take_armed(&mut self) -> Option<crate::protection::control::Command> {
        self.armed_deadline = None;
        self.armed.take()
    }

    fn expire_armed(&mut self, protection: &mut crate::protection::Guard) {
        if self
            .armed_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
            && let Some(command) = self.take_armed()
        {
            // No candidate, pause, progress or qualification is fabricated.
            protection.control_only(command);
        }
    }

    fn candidate_command(
        &mut self,
        protection: &mut crate::protection::Guard,
        shared: &crate::protection::Shared,
    ) -> Option<crate::protection::control::Command> {
        // A ready audio receive (or a slow pipeline) must not win a race with
        // the original arm deadline and apply a command after its wait expired.
        self.expire_armed(protection);
        self.take_armed()
            .or_else(|| shared.commands().try_recv().ok())
    }

    fn pause(&mut self, protection: &mut crate::protection::Guard) {
        self.paused = true;
        protection.pause();
        self.reject_armed(protection);
    }

    fn reject_armed(&mut self, protection: &mut crate::protection::Guard) {
        if let Some(command) = self.take_armed() {
            protection.control_only(command);
        }
    }
}

// Selection of a configuration event is the commit boundary. An audio event
// selected first still belongs to the old pipeline; no later candidate can.
// Resolve pending Armed before admission/reconstruction can block this owner.
// install() retires every old stamp, including the one pending output, which
// must retain its original stamp and its backpressure until playback takes it.
fn replace_pipeline(
    protection: &mut crate::protection::Guard,
    control: &mut ProcessingControl,
    pipeline: &mut pipeline::Pipeline,
    diff: config::ConfigChange,
    new_config: config::Configuration,
    processing_params: &Arc<ProcessingParameters>,
) -> bool {
    control.reject_armed(protection);
    if let Err(err) = protection.install(&new_config) {
        error!("Replacement protection admission rejected: {err}");
        return false;
    }
    match diff {
        config::ConfigChange::Pipeline | config::ConfigChange::MixerParameters => {
            debug!("Rebuilding pipeline.");
            *pipeline = pipeline::Pipeline::from_config(new_config, processing_params.clone());
        }
        config::ConfigChange::FilterParameters {
            filters,
            mixers,
            processors,
        } => {
            debug!("Updating parameters of filters: {filters:?}, mixers: {mixers:?}.");
            pipeline.update_parameters(new_config, &filters, &mixers, &processors);
        }
        config::ConfigChange::Devices => return false,
        config::ConfigChange::None => {}
    }
    true
}

pub fn run_processing(
    conf_proc: config::Configuration,
    barrier_proc: Arc<Barrier>,
    tx_pb: crossbeam_channel::Sender<AudioMessage>,
    rx_cap: crossbeam_channel::Receiver<AudioMessage>,
    rx_pipeconf: crossbeam_channel::Receiver<(config::ConfigChange, config::Configuration)>,
    processing_params: Arc<ProcessingParameters>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut protection = match processing_params.protection_owner.lock() {
            Ok(owner) => owner,
            Err(_) => {
                // Do not trust possibly interrupted safety state or strand the
                // supervisor/capture/playback participants in wait mode.
                error!("Protection owner poisoned; DSP process restart required");
                barrier_proc.wait();
                let _ = tx_pb.send(AudioMessage::EndOfStream);
                return;
            }
        };
        if let Err(err) = protection.install(&conf_proc) {
            error!("Protection admission rejected: {err}");
            barrier_proc.wait();
            let _ = tx_pb.send(AudioMessage::EndOfStream);
            return;
        }
        let chunksize = conf_proc.devices.chunksize;
        let samplerate = conf_proc.devices.samplerate;
        let multithreaded = conf_proc.devices.multithreaded();
        let nbr_threads = conf_proc.devices.worker_threads();
        let hw_threads = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or_default();
        if nbr_threads > hw_threads && multithreaded {
            warn!(
                "Requested {nbr_threads} worker threads. For optimal performance, this number should not \
                exceed the available CPU cores, which is {hw_threads}."
            );
        }
        if hw_threads == 1 && multithreaded {
            warn!(
                "This system only has one CPU core, multithreaded processing is not recommended."
            );
        }
        if nbr_threads == 1 && multithreaded {
            warn!(
                "Requested multithreaded processing with one worker thread. \
                   Performance can improve by adding more threads or disabling multithreading."
            );
        }
        let mut pipeline = pipeline::Pipeline::from_config(conf_proc, processing_params.clone());
        debug!("build filters, waiting to start processing loop");

        let thread_handle =
            match promote_current_thread_to_real_time(chunksize as u32, samplerate as u32) {
                Ok(h) => {
                    debug!("Processing thread has real-time priority.");
                    Some(h)
                }
                Err(err) => {
                    warn!("Processing thread could not get real time priority, error: {err}");
                    None
                }
            };

        // Initialize rayon thread pool
        if multithreaded {
            match rayon::ThreadPoolBuilder::new()
                .num_threads(nbr_threads)
                .build_global()
            {
                Ok(_) => {
                    debug!(
                        "Initialized global thread pool with {} workers",
                        rayon::current_num_threads()
                    );
                    rayon::broadcast(|_| {
                        match promote_current_thread_to_real_time(
                            chunksize as u32,
                            samplerate as u32,
                        ) {
                            Ok(_) => {
                                debug!(
                                    "Worker thread {} has real-time priority.",
                                    rayon::current_thread_index().unwrap_or_default()
                                );
                            }
                            Err(err) => {
                                warn!(
                                    "Worker thread {} could not get real time priority, error: {}",
                                    rayon::current_thread_index().unwrap_or_default(),
                                    err
                                );
                            }
                        };
                    });
                }
                Err(err) => {
                    warn!("Failed to build thread pool, error: {err}");
                }
            };
        }

        barrier_proc.wait();
        debug!("Processing loop starts now!");
        let shared = processing_params.protection.clone();
        let mut control = ProcessingControl {
            paused: true,
            armed: None,
            armed_deadline: None,
        };
        let no_commands = crossbeam_channel::never();
        let no_capture = crossbeam_channel::never();
        let no_config = crossbeam_channel::never();
        // A rendezvous channel with a live, unread receiver disables send.
        // Keeping these channels outside the pump avoids per-chunk allocation.
        let (no_output, _no_output_rx) = crossbeam_channel::bounded(0);
        let mut rx_pipeconf = rx_pipeconf;
        let mut pending = None;
        loop {
            control.expire_armed(&mut protection);
            let arm_timeout = control.arm_timeout();
            let commands = if control.armed.is_none() {
                shared.commands()
            } else {
                &no_commands
            };
            // Exactly two phases: receive input, or send one pending output.
            // Both service the same control/configuration events. Never consume
            // more capture while playback is applying backpressure.
            let capture = if pending.is_none() {
                &rx_cap
            } else {
                &no_capture
            };
            let output = if pending.is_some() {
                &tx_pb
            } else {
                &no_output
            };
            let ending = matches!(pending, Some(AudioMessage::EndOfStream));
            let configurations = if ending { &no_config } else { &rx_pipeconf };
            let audio = crossbeam_channel::select! {
                recv(capture) -> audio => audio,
                send(output, pending.take().expect("selected output without a pending message")) -> result => {
                    if result.is_err() {
                        info!("Playback thread has already stopped.");
                        break;
                    }
                    if ending {
                        break;
                    }
                    continue;
                }
                recv(configurations) -> update => {
                    match update {
                        Ok((diff, new_config)) => {
                            trace!("Message received on config channel");
                            if !replace_pipeline(
                                &mut protection, &mut control, &mut pipeline,
                                diff, new_config, &processing_params,
                            ) {
                                control.pause(&mut protection);
                                pending = Some(AudioMessage::EndOfStream);
                            }
                        }
                        Err(_) => {
                            // No further updates are possible. Retain the
                            // installed pipeline without selecting EOF forever.
                            rx_pipeconf = crossbeam_channel::never();
                        }
                    }
                    continue;
                }
                recv(arm_timeout) -> _ => {
                    control.expire_armed(&mut protection);
                    continue;
                }
                recv(shared.backend_wakeups()) -> _ => {
                    protection.backend_wakeup();
                    continue;
                }
                recv(commands) -> command => {
                    control.service(&mut protection, command.expect("owned control mailbox disconnected"));
                    continue;
                }
            };
            match audio {
                Ok(AudioMessage::Audio(mut chunk)) => {
                    control.paused = false;
                    //trace!("AudioMessage::Audio received");
                    chunk = pipeline.process_chunk(chunk);
                    let command = control.candidate_command(&mut protection, &shared);
                    protection.process_with_command(&mut chunk, command);
                    pending = Some(AudioMessage::Audio(chunk));
                }
                Ok(AudioMessage::EndOfStream) => {
                    trace!("AudioMessage::EndOfStream received");
                    control.pause(&mut protection);
                    pending = Some(AudioMessage::EndOfStream);
                }
                Ok(AudioMessage::Pause) => {
                    trace!("AudioMessage::Pause received");
                    control.pause(&mut protection);
                    pending = Some(AudioMessage::Pause);
                }
                Err(err) => {
                    error!("Message channel error: {err}");
                    control.pause(&mut protection);
                    pending = Some(AudioMessage::EndOfStream);
                }
            }
        }
        control.pause(&mut protection);
        processing_params.set_processing_load(0.0);
        processing_params.set_resampler_load(0.0);
        if let Some(h) = thread_handle {
            match demote_current_thread_from_real_time(h) {
                Ok(_) => {
                    debug!("Processing thread returned to normal priority.")
                }
                Err(_) => {
                    warn!("Could not bring the processing thread back to normal priority.")
                }
            };
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protection::tests::{chunk, configuration};
    use std::time::Duration;

    fn wait_receipt(shared: &crate::protection::Shared, id: u64) -> crate::protection::Snapshot {
        let start = std::time::Instant::now();
        loop {
            if let Some(snapshot) = shared.guard_snapshot()
                && snapshot
                    .control
                    .last_command
                    .is_some_and(|r| r.command_id == id)
            {
                eprintln!(
                    "local command {id} observed after {:?}; not target timing qualification",
                    start.elapsed()
                );
                return snapshot;
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "no terminal receipt"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn request(
        snapshot: crate::protection::Snapshot,
        id: u64,
        operation: crate::protection::control::Operation,
        qualification_id: u64,
    ) -> crate::protection::control::Command {
        use crate::protection::control::{Command, Expected, Operation};
        Command {
            id,
            expected: Expected {
                pipeline: snapshot.pipeline,
                backend: snapshot.backend,
                reset: snapshot.reset,
                trip_generation: snapshot.trip_generation,
                revision: snapshot.control.revision,
            },
            owner: 7,
            parent_reset_generation: snapshot.control.parent_reset_generation
                + u64::from(operation == Operation::Reset),
            qualification_id,
            operation,
        }
    }

    fn wait_snapshot(
        shared: &crate::protection::Shared,
        ready: impl Fn(crate::protection::Snapshot) -> bool,
    ) -> crate::protection::Snapshot {
        let start = Instant::now();
        loop {
            if let Some(snapshot) = shared.guard_snapshot()
                && ready(snapshot)
            {
                return snapshot;
            }
            // Host deadlock guard only, not target timing qualification.
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "processing stalled"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn configuration_with_gain(gain: f64) -> config::Configuration {
        let mut conf = serde_json::to_value(configuration()).unwrap();
        conf["filters"] = serde_json::json!({
            "gain": {"type": "Gain", "parameters": {"gain": gain, "scale": "linear"}}
        });
        conf["pipeline"].as_array_mut().unwrap().insert(
            0,
            serde_json::json!({"type": "Filter", "channels": [0], "names": ["gain"]}),
        );
        serde_json::from_value(conf).unwrap()
    }

    #[test]
    fn configuration_wakes_idle_capture_before_the_next_candidate() {
        // Cover both graph construction and in-place parameter changes. The
        // changed gain makes the next candidate unsafe, proving installation
        // of the actual pipeline rather than just publication of a new epoch.
        for initial in [configuration(), configuration_with_gain(1.0)] {
            let params = Arc::new(ProcessingParameters::default());
            let (tx_pb, rx_pb) = crossbeam_channel::bounded(1);
            let (tx_cap, rx_cap) = crossbeam_channel::bounded(1);
            let (tx_conf, rx_conf) = crossbeam_channel::bounded(1);
            let barrier = Arc::new(Barrier::new(2));
            let handle = run_processing(
                initial.clone(),
                barrier.clone(),
                tx_pb,
                rx_cap,
                rx_conf,
                params.clone(),
            );
            barrier.wait();
            let before = params.protection.guard_snapshot().unwrap();
            let updated = configuration_with_gain(20.0);
            tx_conf
                .send((config::config_diff(&initial, &updated), updated))
                .unwrap();
            let committed = wait_snapshot(&params.protection, |s| s.pipeline > before.pipeline);
            assert_eq!(committed.frames, 0);
            assert!(!committed.processing_valid);
            assert!(committed.attempt_required);
            assert!(rx_pb.is_empty());
            // A closed config source must be disabled, while audio and control
            // remain serviceable. It must not make the pump spin or exit.
            drop(tx_conf);
            tx_cap.send(AudioMessage::Audio(chunk())).unwrap();
            let AudioMessage::Audio(output) = rx_pb.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("missing updated output")
            };
            assert!(output.protection.unwrap().is_current());
            assert!(
                output
                    .waveforms
                    .iter()
                    .flatten()
                    .all(|sample| *sample == 0.0)
            );
            let trip = params.protection.guard_snapshot().unwrap().trip.unwrap();
            assert_eq!(trip.reason, crate::protection::Reason::Peak);
            assert_eq!(trip.channel, Some(0));
            tx_cap.send(AudioMessage::EndOfStream).unwrap();
            assert!(matches!(
                rx_pb.recv_timeout(Duration::from_secs(2)).unwrap(),
                AudioMessage::EndOfStream
            ));
            handle.join().unwrap();
        }
    }

    #[test]
    fn configuration_wakes_blocked_playback_without_consuming_more_capture() {
        for initial in [configuration(), configuration_with_gain(1.0)] {
            let params = Arc::new(ProcessingParameters::default());
            let (tx_pb, rx_pb) = crossbeam_channel::bounded(1);
            let (tx_cap, rx_cap) = crossbeam_channel::bounded(3);
            let capture_queue = rx_cap.clone();
            let (tx_conf, rx_conf) = crossbeam_channel::bounded(1);
            let barrier = Arc::new(Barrier::new(2));
            let handle = run_processing(
                initial.clone(),
                barrier.clone(),
                tx_pb,
                rx_cap,
                rx_conf,
                params.clone(),
            );
            barrier.wait();
            for _ in 0..3 {
                tx_cap.send(AudioMessage::Audio(chunk())).unwrap();
            }
            let before = wait_snapshot(&params.protection, |s| s.frames == 512);
            assert_eq!(rx_pb.len(), 1);
            assert_eq!(capture_queue.len(), 1);
            let updated = configuration_with_gain(20.0);
            tx_conf
                .send((config::config_diff(&initial, &updated), updated))
                .unwrap();
            let committed = wait_snapshot(&params.protection, |s| s.pipeline > before.pipeline);
            assert_eq!(committed.frames, 0);
            assert!(!committed.processing_valid);
            assert!(committed.attempt_required);
            assert_eq!(
                capture_queue.len(),
                1,
                "reload bypassed output backpressure"
            );
            // Both the queued and the one pending chunk retain their original
            // stamps. Neither may be relabelled with the replacement epoch.
            for _ in 0..2 {
                let AudioMessage::Audio(retired) =
                    rx_pb.recv_timeout(Duration::from_secs(2)).unwrap()
                else {
                    panic!("missing retired output")
                };
                assert!(!retired.protection.unwrap().is_current());
            }
            let AudioMessage::Audio(current) = rx_pb.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("missing updated output")
            };
            assert!(current.protection.unwrap().is_current());
            assert!(
                current
                    .waveforms
                    .iter()
                    .flatten()
                    .all(|sample| *sample == 0.0)
            );
            let trip = params.protection.guard_snapshot().unwrap().trip.unwrap();
            assert_eq!(trip.reason, crate::protection::Reason::Peak);
            assert_eq!(trip.channel, Some(0));
            tx_cap.send(AudioMessage::EndOfStream).unwrap();
            assert!(matches!(
                rx_pb.recv_timeout(Duration::from_secs(2)).unwrap(),
                AudioMessage::EndOfStream
            ));
            handle.join().unwrap();
        }
    }

    #[test]
    fn stalled_capture_rejects_arm_without_waiting_for_audio_or_pause() {
        use crate::protection::control::{Mode, Operation, Result as ControlResult};
        for pause in [false, true] {
            let params = Arc::new(ProcessingParameters::default());
            let (tx_pb, rx_pb) = crossbeam_channel::bounded(1);
            let (tx_cap, rx_cap) = crossbeam_channel::bounded(2);
            let (_tx_conf, rx_conf) = crossbeam_channel::bounded(1);
            let barrier = Arc::new(Barrier::new(2));
            let handle = run_processing(
                configuration(),
                barrier.clone(),
                tx_pb,
                rx_cap,
                rx_conf,
                params.clone(),
            );
            barrier.wait();
            let initial = params.protection.guard_snapshot().unwrap();
            params
                .protection
                .submit(request(initial, 1, Operation::Context(Mode::Prearm), 1))
                .unwrap();
            wait_receipt(&params.protection, 1);
            tx_cap.send(AudioMessage::Audio(chunk())).unwrap();
            assert!(matches!(
                rx_pb.recv_timeout(Duration::from_secs(2)).unwrap(),
                AudioMessage::Audio(_)
            ));
            let before = params.protection.guard_snapshot().unwrap();
            params
                .protection
                .submit(request(before, 2, Operation::Context(Mode::Armed), 1))
                .unwrap();
            // Neither a candidate nor a pause notification will arrive until
            // after the receipt. The old unbounded wait fails this regression.
            // The host wait is a deadlock guard, not target timing qualification.
            let rejected = wait_receipt(&params.protection, 2);
            assert_eq!(
                rejected.control.last_command.unwrap().result,
                ControlResult::NotQualified
            );
            assert_eq!(rejected.frames, before.frames);
            assert_eq!(rejected.qualified_frames, before.qualified_frames);
            assert_eq!(rejected.processing_valid, before.processing_valid);
            assert_eq!(rejected.source_begin_ns, before.source_begin_ns);
            assert_eq!(rejected.source_end_ns, before.source_end_ns);
            tx_cap
                .send(if pause {
                    AudioMessage::Pause
                } else {
                    AudioMessage::Audio(chunk())
                })
                .unwrap();
            rx_pb.recv_timeout(Duration::from_secs(2)).unwrap();
            let receipt = params.protection.guard_snapshot().unwrap();
            tx_cap.send(AudioMessage::EndOfStream).unwrap();
            rx_pb.recv_timeout(Duration::from_secs(2)).unwrap();
            handle.join().unwrap();

            assert_eq!(receipt.frames, before.frames + if pause { 0 } else { 256 });
            // One chunk after prearm cannot complete qualification;
            // candidate-time acceptance is tested by Guard.
            assert_eq!(
                receipt.control.last_command.unwrap().result,
                ControlResult::NotQualified
            );
        }
    }

    #[test]
    fn pending_arm_requires_a_candidate_before_its_original_deadline() {
        use crate::protection::{
            Guard, Shared,
            control::{Mode, Operation, Result as ControlResult},
        };
        for expired in [false, true] {
            let shared = Arc::new(Shared::default());
            let mut guard = Guard::new(shared.clone());
            guard.install(&configuration()).unwrap();
            let mut control = ProcessingControl {
                paused: false,
                armed: None,
                armed_deadline: None,
            };
            let prearm = request(
                shared.guard_snapshot().unwrap(),
                1,
                Operation::Context(Mode::Prearm),
                1,
            );
            control.service(&mut guard, prearm);
            for _ in 0..=375 {
                guard.process(&mut chunk());
            }
            assert_eq!(guard.state(), crate::protection::control::State::Active);
            let before = shared.guard_snapshot().unwrap();
            control.service(
                &mut guard,
                request(before, 2, Operation::Context(Mode::Armed), 1),
            );
            assert_eq!(
                shared
                    .guard_snapshot()
                    .unwrap()
                    .control
                    .last_command
                    .unwrap()
                    .command_id,
                1
            );
            // Exercise the boundary deterministically, including a candidate
            // already ready when the deadline expires. No host timing claim.
            control.armed_deadline = Some(if expired {
                Instant::now()
            } else {
                Instant::now() + Duration::from_secs(10)
            });
            let command = control.candidate_command(&mut guard, &shared);
            assert_eq!(command.is_none(), expired);
            guard.process_with_command(&mut chunk(), command);
            let after = shared.guard_snapshot().unwrap();
            assert_eq!(
                after.control.last_command.unwrap().result,
                if expired {
                    ControlResult::NotQualified
                } else {
                    ControlResult::Applied
                }
            );
            assert_eq!(
                after.control.mode,
                if expired { Mode::Prearm } else { Mode::Armed }
            );
            assert!(control.armed.is_none() && control.armed_deadline.is_none());
        }
    }

    #[test]
    fn configuration_commit_rejects_pending_arm_before_installation() {
        use crate::protection::control::{Mode, Operation, Result as ControlResult};
        for valid in [true, false] {
            let conf = configuration();
            let params = Arc::new(ProcessingParameters::default());
            let mut guard = params.protection_owner.lock().unwrap();
            guard.install(&conf).unwrap();
            let mut pipeline = pipeline::Pipeline::from_config(conf.clone(), params.clone());
            let mut control = ProcessingControl {
                paused: false,
                armed: None,
                armed_deadline: None,
            };
            control.service(
                &mut guard,
                request(
                    params.protection.guard_snapshot().unwrap(),
                    1,
                    Operation::Context(Mode::Prearm),
                    1,
                ),
            );
            let before = params.protection.guard_snapshot().unwrap();
            control.service(
                &mut guard,
                request(before, 2, Operation::Context(Mode::Armed), 1),
            );
            assert!(control.armed.is_some());
            let mut updated = conf;
            if !valid {
                updated.pipeline = None;
            }
            assert_eq!(
                replace_pipeline(
                    &mut guard,
                    &mut control,
                    &mut pipeline,
                    config::ConfigChange::Pipeline,
                    updated,
                    &params,
                ),
                valid
            );
            let after = params.protection.guard_snapshot().unwrap();
            let receipt = after.control.last_command.unwrap();
            assert_eq!(receipt.command_id, 2);
            assert_eq!(receipt.result, ControlResult::NotQualified);
            // Rejection after install would report the replacement epoch and
            // StateMismatch, and could be delayed by synchronous admission.
            assert_eq!(receipt.pipeline, before.pipeline);
            assert!(after.pipeline > before.pipeline);
            assert_eq!(after.frames, 0);
            assert!(!after.processing_valid);
            assert!(control.armed.is_none() && control.armed_deadline.is_none());
        }
    }

    #[test]
    fn control_wakeup_while_paused_needs_no_audio_or_second_owner() {
        use crate::protection::control::{Mode, Operation, Result as ControlResult};
        let params = Arc::new(ProcessingParameters::new(&[0.0; 5], &[true; 5]));
        let (tx_pb, rx_pb) = crossbeam_channel::bounded(1);
        let (tx_cap, rx_cap) = crossbeam_channel::bounded(2);
        let (_tx_conf, rx_conf) = crossbeam_channel::bounded(1);
        let barrier = Arc::new(Barrier::new(2));
        let handle = run_processing(
            configuration(),
            barrier.clone(),
            tx_pb,
            rx_cap,
            rx_conf,
            params.clone(),
        );
        barrier.wait();
        tx_cap.send(AudioMessage::Pause).unwrap();
        assert!(matches!(
            rx_pb.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioMessage::Pause
        ));
        let initial = params.protection.guard_snapshot().unwrap();
        params
            .protection
            .submit(request(initial, 1, Operation::Context(Mode::Prearm), 1))
            .unwrap();
        let prearm = wait_receipt(&params.protection, 1);
        assert_eq!(
            prearm.control.last_command.unwrap().result,
            ControlResult::Applied
        );
        params
            .protection
            .submit(request(prearm, 2, Operation::Context(Mode::Armed), 1))
            .unwrap();
        let armed = wait_receipt(&params.protection, 2);
        assert_eq!(
            armed.control.last_command.unwrap().result,
            ControlResult::NotQualified
        );
        params
            .protection
            .submit(request(armed, 3, Operation::Reset, 2))
            .unwrap();
        // This covers reset queued immediately before a further Pause as well.
        tx_cap.send(AudioMessage::Pause).unwrap();
        let reset = wait_receipt(&params.protection, 3);
        assert_eq!(
            reset.control.last_command.unwrap().result,
            ControlResult::Applied
        );
        assert_eq!(reset.reset, initial.reset + 1);
        assert!(params.is_mute(0));
        for snapshot in [prearm, armed, reset] {
            assert_eq!(snapshot.frames, initial.frames);
            assert_eq!(snapshot.qualified_frames, 0);
            assert!(!snapshot.processing_valid);
            assert_eq!((snapshot.source_begin_ns, snapshot.source_end_ns), (0, 0));
        }
        assert!(matches!(
            rx_pb.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioMessage::Pause
        ));
        tx_cap.send(AudioMessage::EndOfStream).unwrap();
        assert!(matches!(
            rx_pb.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioMessage::EndOfStream
        ));
        handle.join().unwrap();
    }

    #[test]
    fn blocked_playback_submission_cannot_block_control_receipt() {
        use crate::protection::control::{Mode, Operation, Result as ControlResult};
        let params = Arc::new(ProcessingParameters::default());
        let (tx_pb, rx_pb) = crossbeam_channel::bounded(1);
        let (tx_cap, rx_cap) = crossbeam_channel::bounded(3);
        let (_tx_conf, rx_conf) = crossbeam_channel::bounded(1);
        let barrier = Arc::new(Barrier::new(2));
        let handle = run_processing(
            configuration(),
            barrier.clone(),
            tx_pb,
            rx_cap,
            rx_conf,
            params.clone(),
        );
        barrier.wait();
        tx_cap.send(AudioMessage::Audio(chunk())).unwrap();
        tx_cap.send(AudioMessage::Audio(chunk())).unwrap();
        let start = std::time::Instant::now();
        let before = loop {
            let snapshot = params.protection.guard_snapshot().unwrap();
            if snapshot.frames == 512 {
                break snapshot;
            }
            assert!(start.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(1));
        };
        params
            .protection
            .submit(request(before, 1, Operation::Context(Mode::Prearm), 1))
            .unwrap();
        let receipt = wait_receipt(&params.protection, 1);
        assert_eq!(
            receipt.control.last_command.unwrap().result,
            ControlResult::Applied
        );
        assert_eq!(receipt.frames, before.frames);
        assert_eq!(receipt.source_begin_ns, before.source_begin_ns);
        assert_eq!(receipt.source_end_ns, before.source_end_ns);
        assert_eq!(receipt.qualified_frames, 0);
        params
            .protection
            .submit(request(receipt, 2, Operation::Context(Mode::Armed), 1))
            .unwrap();
        let rejected = wait_receipt(&params.protection, 2);
        assert_eq!(
            rejected.control.last_command.unwrap().result,
            ControlResult::NotQualified
        );
        assert_eq!(rejected.frames, receipt.frames);
        assert_eq!(rejected.source_begin_ns, receipt.source_begin_ns);
        assert_eq!(rejected.source_end_ns, receipt.source_end_ns);
        tx_cap.send(AudioMessage::EndOfStream).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                rx_pb.recv_timeout(Duration::from_secs(2)).unwrap(),
                AudioMessage::Audio(_)
            ));
        }
        assert!(matches!(
            rx_pb.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioMessage::EndOfStream
        ));
        handle.join().unwrap();
    }

    #[test]
    fn poisoned_owner_reaches_startup_barrier_and_stops_without_audio() {
        let params = Arc::new(ProcessingParameters::default());
        let poisoned = params.clone();
        assert!(
            thread::spawn(move || {
                let _owner = poisoned.protection_owner.lock().unwrap();
                panic!("simulated processing panic while owning the guard");
            })
            .join()
            .is_err()
        );
        let (tx_pb, rx_pb) = crossbeam_channel::unbounded();
        let (_tx_cap, rx_cap) = crossbeam_channel::unbounded();
        let (_tx_conf, rx_conf) = crossbeam_channel::unbounded();
        let barrier = Arc::new(Barrier::new(2));
        let handle = run_processing(
            configuration(),
            barrier.clone(),
            tx_pb,
            rx_cap,
            rx_conf,
            params.clone(),
        );
        let (tx_ready, rx_ready) = crossbeam_channel::bounded(1);
        let waiter = thread::spawn(move || {
            barrier.wait();
            tx_ready.send(()).unwrap();
        });
        rx_ready
            .recv_timeout(Duration::from_secs(2))
            .expect("processing stranded the startup barrier");
        assert!(matches!(
            rx_pb.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioMessage::EndOfStream
        ));
        handle.join().unwrap();
        waiter.join().unwrap();
        assert!(params.protection_owner.is_poisoned());
        assert!(rx_pb.try_recv().is_err());
    }

    #[test]
    fn running_processing_retires_queued_audio_and_preserves_trip_on_updates() {
        let conf = configuration();
        let params = Arc::new(ProcessingParameters::default());
        let (tx_pb, rx_pb) = crossbeam_channel::unbounded();
        let (tx_cap, rx_cap) = crossbeam_channel::unbounded();
        let (tx_conf, rx_conf) = crossbeam_channel::unbounded();
        let barrier = Arc::new(Barrier::new(2));
        let handle = run_processing(
            conf.clone(),
            barrier.clone(),
            tx_pb,
            rx_cap,
            rx_conf,
            params.clone(),
        );
        barrier.wait();
        let mut unsafe_input = chunk();
        unsafe_input.waveforms[0][7] = 1.0;
        tx_cap.send(AudioMessage::Audio(unsafe_input)).unwrap();
        let before = wait_snapshot(&params.protection, |s| s.frames == 256);
        let mut updated = conf.clone();
        updated.title = Some("updated title".to_string());
        tx_conf
            .send((config::config_diff(&conf, &updated), updated))
            .unwrap();
        wait_snapshot(&params.protection, |s| s.pipeline > before.pipeline);
        tx_cap.send(AudioMessage::Audio(chunk())).unwrap();
        tx_cap.send(AudioMessage::EndOfStream).unwrap();
        let AudioMessage::Audio(retired) = rx_pb.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("missing first output")
        };
        let AudioMessage::Audio(current) = rx_pb.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("missing replacement output")
        };
        assert!(matches!(
            rx_pb.recv_timeout(Duration::from_secs(2)).unwrap(),
            AudioMessage::EndOfStream
        ));
        handle.join().unwrap();
        assert!(!retired.protection.unwrap().is_current());
        assert!(current.protection.unwrap().is_current());
        assert!(
            current
                .waveforms
                .iter()
                .flatten()
                .all(|sample| *sample == 0.0)
        );
        assert_eq!(
            params
                .protection_owner
                .lock()
                .unwrap()
                .trip()
                .unwrap()
                .frame,
            7
        );
    }
}
