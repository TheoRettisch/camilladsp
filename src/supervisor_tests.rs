//! In-memory supervisor lifecycle checks; no audio device or GPIO is opened.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn configuration() -> config::PreparedConfig {
    let raw = serde_json::json!({
        "devices": {"samplerate": 48000, "chunksize": 256,
            "capture": {"type": "SignalGenerator", "channels": 24,
                "signal": {"type": "Sine", "freq": 1000, "level": -20}},
            "playback": {"type": "Alsa", "channels": 24, "device": "hw:fixture,0", "format": "S24_3_LE"}},
        "processors": {"guard": {"type": "Protection", "parameters": {
            "output_channels": camillalib::protection::OUTPUT_CHANNELS,
            "tweeter_channels": camillalib::protection::TWEETER_CHANNELS,
            "max_allowed_peak_db": -0.5, "dc_freeze_seconds": 2.0,
            "dc_freeze_peak_rms_tolerance_db": 0.25, "level_freshness_epsilon_db": 0.01,
            "max_tweeter_rms_db": null, "tweeter_rms_fault_seconds": 5.0,
            "max_tweeter_avg_power_db": null, "tweeter_avg_power_tau_seconds": 5.0,
            "max_tweeter_long_avg_power_db": null, "tweeter_long_avg_power_tau_seconds": 90.0,
            "tone_min_rms_db": null, "tone_crest_db": 6.0,
            "tone_fault_seconds": 5.0, "silence_threshold_db": -90.0}}},
        "pipeline": [{"type": "Processor", "name": "guard"}]
    });
    config::validate_config(serde_json::from_value(raw).unwrap(), None).unwrap()
}

fn status_structs() -> StatusStructs {
    StatusStructs {
        capture: Arc::new(RwLock::new(CaptureStatus {
            measured_samplerate: 0,
            update_interval: 1000,
            signal_range: 0.0,
            rate_adjust: 0.0,
            state: ProcessingState::Running,
            signal_rms: countertimer::ValueHistory::new(1, 24),
            signal_peak: countertimer::ValueHistory::new(1, 24),
            used_channels: Vec::new(),
        })),
        playback: Arc::new(RwLock::new(PlaybackStatus {
            buffer_level: 0,
            clipped_samples: 0,
            update_interval: 1000,
            signal_rms: countertimer::ValueHistory::new(1, 24),
            signal_peak: countertimer::ValueHistory::new(1, 24),
        })),
        processing: Arc::new(ProcessingParameters::default()),
        status: Arc::new(RwLock::new(ProcessingStatus {
            stop_reason: StopReason::None,
        })),
    }
}

#[test]
fn active_config_waits_for_installation_and_queued_diffs_follow_requests() {
    let initial = configuration();
    let mut raw = serde_json::to_value(&initial).unwrap();
    raw["title"] = serde_json::json!("first replacement");
    raw["filters"] = serde_json::json!({"gain": {"type": "Gain", "parameters": {"gain": -6.0}}});
    raw["pipeline"].as_array_mut().unwrap().insert(
        0,
        serde_json::json!({"type": "Filter", "channels": [0], "names": ["gain"]}),
    );
    let first =
        config::validate_config(serde_json::from_value(raw.clone()).unwrap(), None).unwrap();
    raw["filters"]["gain"]["parameters"]["gain"] = serde_json::json!(-12.0);
    let second = config::validate_config(serde_json::from_value(raw).unwrap(), None).unwrap();
    let active = Arc::new(Mutex::new(Some(initial.clone())));
    let shared = SharedConfigs {
        active: active.clone(),
        previous: Arc::new(Mutex::new(None)),
    };
    let (tx_ctrl, rx_ctrl) = crossbeam_channel::unbounded();
    let (_tx_status, rx_status) = crossbeam_channel::unbounded();
    let (tx_conf, rx_conf) = crossbeam_channel::unbounded();
    let (tx_stop, rx_stop) = crossbeam_channel::unbounded();
    let (tx_capture, rx_capture) = crossbeam_channel::unbounded();
    let threads = RunThreads {
        capture_command: tx_capture,
        processing_stop: tx_stop,
        barrier: Arc::new(Barrier::new(1)),
        starting: false,
        processing: thread::spawn(move || {
            rx_stop.recv().unwrap();
        }),
        capture: thread::spawn(move || {
            rx_capture.recv().unwrap();
        }),
        playback: thread::spawn(|| {}),
    };
    let supervisor = thread::spawn(move || {
        supervise_run(
            initial,
            shared,
            &status_structs(),
            rx_ctrl,
            rx_status,
            tx_conf,
            threads,
        )
        .unwrap()
    });
    tx_ctrl
        .send(ControllerMessage::ConfigChanged(Box::new(first.clone())))
        .unwrap();
    let first_update = rx_conf.recv_timeout(Duration::from_secs(2)).unwrap();
    // A second dequeued update proves the first queue operation has completed,
    // without depending on scheduling at the active-config assignment.
    tx_ctrl
        .send(ControllerMessage::ConfigChanged(Box::new(second.clone())))
        .unwrap();
    let second_update = rx_conf.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(active.lock().as_ref().unwrap().config().title, None);
    assert!(
        matches!(&second_update.diff, config::ConfigChange::FilterParameters { filters, .. }
        if filters == &["gain".to_string()])
    );

    for (update, expected) in [(first_update, first), (second_update, second)] {
        update.installed.send(update.config).unwrap();
        let start = std::time::Instant::now();
        while active.lock().as_ref().unwrap().config() != expected.config() {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "installation not published"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    tx_ctrl.send(ControllerMessage::Stop).unwrap();
    assert!(matches!(supervisor.join().unwrap(), ExitState::Restart));
    assert!(active.lock().is_none());
}

enum Trigger {
    Stop,
    Exit,
    Devices,
    Backend(StatusMessage),
    ControllerClosed,
    StatusClosed,
    ConfigClosed,
}

struct Finished(Arc<AtomicUsize>);
impl Drop for Finished {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn exercise(trigger: Trigger, starting: bool, panic_processing: bool, panic_capture: bool) {
    let (tx_result, rx_result) = crossbeam_channel::bounded(1);
    let supervisor = thread::spawn(move || {
        let conf = configuration();
        let statuses = status_structs();
        let active = Arc::new(Mutex::new(Some(conf.clone())));
        let previous = Arc::new(Mutex::new(None));
        let shared = SharedConfigs {
            active: active.clone(),
            previous: previous.clone(),
        };
        let (tx_ctrl, rx_ctrl) = crossbeam_channel::unbounded();
        let (tx_status, rx_status) = crossbeam_channel::unbounded();
        let (tx_conf, rx_conf) = crossbeam_channel::unbounded();
        let (tx_cap_command, rx_cap_command) = crossbeam_channel::unbounded();
        let (tx_stop, rx_stop) = crossbeam_channel::unbounded();
        let (tx_processed, rx_processed) = crossbeam_channel::bounded::<()>(0);
        let barrier = Arc::new(Barrier::new(if starting { 4 } else { 1 }));
        let finished = Arc::new(AtomicUsize::new(0));
        let proc_barrier = barrier.clone();
        let proc_finished = Finished(finished.clone());
        let params = statuses.processing.clone();
        let processing = thread::spawn(move || {
            let _finished = proc_finished;
            let _completion = tx_processed;
            let _owner = params.protection_owner.lock().unwrap();
            if starting {
                proc_barrier.wait();
            }
            rx_stop.recv().unwrap();
            assert!(!panic_processing, "simulated guard-owner panic");
        });
        let cap_barrier = barrier.clone();
        let cap_finished = Finished(finished.clone());
        let capture_processed = rx_processed.clone();
        let capture = thread::spawn(move || {
            let _finished = cap_finished;
            if starting {
                cap_barrier.wait();
            }
            assert!(matches!(
                rx_cap_command.recv().unwrap(),
                CommandMessage::Exit
            ));
            // Exercise cleanup failure after processing has closed its channels,
            // without poisoning the guard or depending on thread scheduling.
            assert!(capture_processed.recv().is_err());
            assert!(!panic_capture, "simulated capture cleanup panic");
        });
        let pb_barrier = barrier.clone();
        let pb_finished = Finished(finished.clone());
        let playback = thread::spawn(move || {
            let _finished = pb_finished;
            if starting {
                pb_barrier.wait();
            }
            // Model playback's channel receive ending when processing exits.
            assert!(rx_processed.recv().is_err());
        });
        let threads = RunThreads {
            capture_command: tx_cap_command,
            processing_stop: tx_stop,
            barrier,
            starting,
            processing,
            capture,
            playback,
        };
        let expect_exit = matches!(trigger, Trigger::Exit);
        let expect_error = ((panic_processing || panic_capture) && !expect_exit)
            || matches!(trigger, Trigger::ControllerClosed | Trigger::ConfigClosed);
        let clears_active = matches!(trigger, Trigger::Stop | Trigger::Backend(_));
        let mut tx_ctrl = Some(tx_ctrl);
        let mut tx_status = Some(tx_status);
        let mut rx_conf = Some(rx_conf);
        match trigger {
            Trigger::Stop => tx_ctrl
                .as_ref()
                .unwrap()
                .send(ControllerMessage::Stop)
                .unwrap(),
            Trigger::Exit => tx_ctrl
                .as_ref()
                .unwrap()
                .send(ControllerMessage::Exit)
                .unwrap(),
            Trigger::Devices | Trigger::ConfigClosed => {
                let mut raw = conf.config().clone();
                if matches!(trigger, Trigger::Devices) {
                    raw.devices.queuelimit = Some(raw.devices.queuelimit() + 1);
                } else {
                    let config::Processor::Protection { parameters, .. } =
                        raw.processors.as_mut().unwrap().get_mut("guard").unwrap()
                    else {
                        unreachable!()
                    };
                    parameters.max_allowed_peak_db = -1.0;
                    drop(rx_conf.take());
                }
                let changed = config::validate_config(raw, None).unwrap();
                tx_ctrl
                    .as_ref()
                    .unwrap()
                    .send(ControllerMessage::ConfigChanged(Box::new(changed)))
                    .unwrap();
            }
            Trigger::Backend(message) => tx_status.as_ref().unwrap().send(message).unwrap(),
            Trigger::ControllerClosed => drop(tx_ctrl.take()),
            Trigger::StatusClosed => drop(tx_status.take()),
        }
        let result = supervise_run(
            conf, shared, &statuses, rx_ctrl, rx_status, tx_conf, threads,
        );
        assert_eq!(result.is_err(), expect_error, "{result:?}");
        if let Ok(exit) = result {
            assert_eq!(
                matches!(exit, ExitState::ExitWithError),
                expect_exit && (panic_processing || panic_capture)
            );
            assert_eq!(
                matches!(exit, ExitState::Exit | ExitState::ExitWithError),
                expect_exit
            );
        }
        assert_eq!(
            finished.load(Ordering::SeqCst),
            3,
            "returned before all joins"
        );
        assert_eq!(
            statuses.processing.protection_owner.is_poisoned(),
            panic_processing
        );
        if !panic_processing {
            assert!(statuses.processing.protection_owner.try_lock().is_ok());
        }
        assert!(matches!(
            statuses.capture.read().state,
            ProcessingState::Inactive
        ));
        if clears_active {
            assert!(active.lock().is_none());
            assert!(previous.lock().is_some());
        }
        tx_result.send(()).unwrap();
    });
    rx_result
        .recv_timeout(Duration::from_secs(5))
        .expect("supervisor failed or stranded a thread");
    supervisor.join().unwrap();
}

#[test]
fn every_exit_stops_and_joins_all_owners() {
    for trigger in [
        Trigger::Stop,
        Trigger::Exit,
        Trigger::Devices,
        Trigger::Backend(StatusMessage::PlaybackError("fixture".into())),
        Trigger::Backend(StatusMessage::CaptureError("fixture".into())),
        Trigger::Backend(StatusMessage::PlaybackFormatChange(44100)),
        Trigger::Backend(StatusMessage::CaptureFormatChange(44100)),
        Trigger::Backend(StatusMessage::PlaybackDone),
        Trigger::ControllerClosed,
        Trigger::StatusClosed,
        Trigger::ConfigClosed,
    ] {
        exercise(trigger, false, false, false);
    }
}

#[test]
fn startup_errors_release_waiters_before_joining() {
    for message in [
        StatusMessage::PlaybackError("open failed".into()),
        StatusMessage::CaptureError("open failed".into()),
    ] {
        exercise(Trigger::Backend(message), true, false, false);
    }
}

#[test]
fn processing_panic_is_reported_after_other_threads_join_and_poison_survives() {
    exercise(Trigger::Stop, false, true, false);
}

#[test]
fn explicit_exit_survives_cleanup_panics() {
    exercise(Trigger::Exit, false, false, true);
    exercise(Trigger::Exit, false, true, false);
}
