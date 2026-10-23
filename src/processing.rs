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

// Continue servicing the sole control mailbox even if playback stops draining.
// A reset can retire this pending chunk; the backend must check its stamp.
fn send_output(
    protection: &mut crate::protection::Guard,
    shared: &crate::protection::Shared,
    tx: &crossbeam_channel::Sender<AudioMessage>,
    message: AudioMessage,
) -> bool {
    loop {
        crossbeam_channel::select! {
            send(tx, message) -> result => return result.is_ok(),
            recv(shared.commands()) -> command => {
                protection.control_only(command.expect("owned control mailbox disconnected"));
            }
        }
    }
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
        loop {
            let audio = crossbeam_channel::select! {
                recv(rx_cap) -> audio => audio,
                recv(shared.commands()) -> command => {
                    protection.control_only(command.expect("owned control mailbox disconnected"));
                    continue;
                }
            };
            match audio {
                Ok(AudioMessage::Audio(mut chunk)) => {
                    //trace!("AudioMessage::Audio received");
                    chunk = pipeline.process_chunk(chunk);
                    let command = shared.commands().try_recv().ok();
                    protection.process_with_command(&mut chunk, command);
                    let msg = AudioMessage::Audio(chunk);
                    if !send_output(&mut protection, &shared, &tx_pb, msg) {
                        info!("Playback thread has already stopped.");
                        break;
                    }
                }
                Ok(AudioMessage::EndOfStream) => {
                    trace!("AudioMessage::EndOfStream received");
                    let msg = AudioMessage::EndOfStream;
                    if !send_output(&mut protection, &shared, &tx_pb, msg) {
                        info!("Playback thread has already stopped.");
                    }
                    break;
                }
                Ok(AudioMessage::Pause) => {
                    trace!("AudioMessage::Pause received");
                    protection.pause();
                    let msg = AudioMessage::Pause;
                    if !send_output(&mut protection, &shared, &tx_pb, msg) {
                        info!("Playback thread has already stopped.");
                        break;
                    }
                }
                Err(err) => {
                    error!("Message channel error: {err}");
                    let msg = AudioMessage::EndOfStream;
                    if !send_output(&mut protection, &shared, &tx_pb, msg) {
                        info!("Playback thread has already stopped.");
                    }
                    break;
                }
            }
            if let Ok((diff, new_config)) = rx_pipeconf.try_recv() {
                trace!("Message received on config channel");
                if let Err(err) = protection.install(&new_config) {
                    error!("Replacement protection admission rejected: {err}");
                    let _ = tx_pb.send(AudioMessage::EndOfStream);
                    break;
                }
                match diff {
                    config::ConfigChange::Pipeline | config::ConfigChange::MixerParameters => {
                        debug!("Rebuilding pipeline.");
                        let new_pipeline =
                            pipeline::Pipeline::from_config(new_config, processing_params.clone());
                        pipeline = new_pipeline;
                    }
                    config::ConfigChange::FilterParameters {
                        filters,
                        mixers,
                        processors,
                    } => {
                        debug!("Updating parameters of filters: {filters:?}, mixers: {mixers:?}.");
                        pipeline.update_parameters(new_config, &filters, &mixers, &processors);
                    }
                    config::ConfigChange::Devices => {
                        let msg = AudioMessage::EndOfStream;
                        tx_pb.send(msg).unwrap();
                        break;
                    }
                    _ => {}
                };
            };
        }
        protection.pause();
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
        let mut updated = conf.clone();
        updated.title = Some("updated title".to_string());
        tx_conf
            .send((config::config_diff(&conf, &updated), updated))
            .unwrap();
        let mut unsafe_input = chunk();
        unsafe_input.waveforms[0][7] = 1.0;
        tx_cap.send(AudioMessage::Audio(unsafe_input)).unwrap();
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
