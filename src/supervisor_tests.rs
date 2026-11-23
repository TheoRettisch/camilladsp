//! In-memory supervisor lifecycle checks; no audio device or GPIO is opened.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn configuration() -> config::PreparedConfig {
    let raw = serde_json::json!({
        "devices": {"samplerate": 48000, "chunksize": 256,
            "capture": {"type": "SignalGenerator", "channels": 24,
                "signal": {"type": "Sine", "freq": 1000, "level": -20}},
            "playback": {"type": "Alsa", "channels": 24, "device": "hw:nonexistent-camilladsp-test-card,0", "format": "S24_3_LE"}},
        "processors": {"guard": {"type": "Protection", "parameters": {"routing_sha256": "1111111111111111111111111111111111111111111111111111111111111111", "activity": {"silence_dbfs": -70.0}, "signals": {"high_l": {"lane": 0, "role": "tweeter"}, "high_r": {"lane": 1, "role": "tweeter"}, "door_l": {"lane": 2, "role": "midwoofer"}, "door_r": {"lane": 3, "role": "midwoofer"}, "bass": {"lane": 4, "role": "sub"}}, "destinations": {"tw_l": "high_l", "tw_r": "high_r", "mid_l": "door_l", "mid_r": "door_r", "sub_a": "bass", "sub_b": "bass"}, "policies": {"hf_peak": {"kind": "peak", "targets": ["tw_l", "tw_r"], "limit_dbfs": -0.5}, "hf_freeze": {"kind": "frozen", "targets": ["tw_l", "tw_r"], "floor_dbfs": -70.0, "crest_tolerance_db": 0.25, "quantum_db": 0.05, "hold_s": 2.0}}}}},
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
    let statuses = status_structs();
    let supervisor_statuses = statuses.clone();
    let supervisor_control = tx_ctrl.clone();
    // Both requests are already queued when the supervisor starts. Earlier
    // requests must not be dropped: later partial edits may depend on them.
    tx_ctrl
        .send(ControllerMessage::ConfigChanged(Box::new(first.clone())))
        .unwrap();
    tx_ctrl
        .send(ControllerMessage::ConfigChanged(Box::new(second.clone())))
        .unwrap();
    let supervisor = thread::spawn(move || {
        supervise_run(
            initial,
            shared,
            &supervisor_statuses,
            rx_ctrl,
            rx_status,
            tx_conf,
            threads,
        )
        .unwrap()
    });
    let first_update = rx_conf.recv_timeout(Duration::from_secs(2)).unwrap();
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
    #[cfg(feature = "websocket")]
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (state_change_notify, _state_changes) = crossbeam_channel::unbounded();
        socketserver::start_server(
            socketserver::ServerParameters {
                address: "127.0.0.1",
                port: port.into(),
                #[cfg(feature = "secure-websocket")]
                cert_file: None,
                #[cfg(feature = "secure-websocket")]
                cert_pass: None,
            },
            socketserver::SharedData {
                active_config: active.clone(),
                active_config_path: Arc::new(Mutex::new(None)),
                previous_config: Arc::new(Mutex::new(None)),
                command_sender: supervisor_control,
                capture_status: statuses.capture,
                playback_status: statuses.playback,
                processing_params: statuses.processing,
                processing_status: statuses.status,
                state_change_notify,
                state_file_path: None,
                unsaved_state_change: Arc::new(AtomicBool::new(false)),
            },
        );
        let start = std::time::Instant::now();
        let stream = loop {
            match std::net::TcpStream::connect(("127.0.0.1", port)) {
                Ok(stream) => break stream,
                Err(_) if start.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("WebSocket server did not start: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (mut websocket, _) =
            tungstenite::client(format!("ws://127.0.0.1:{port}"), stream).unwrap();
        let mut request = |command: serde_json::Value| -> serde_json::Value {
            websocket
                .send(tungstenite::Message::text(command.to_string()))
                .unwrap();
            serde_json::from_str(websocket.read().unwrap().to_text().unwrap()).unwrap()
        };

        // Hold every installation acknowledgement. Both mutation APIs must
        // compose with accepted requests while GetConfig stays installed.
        let mut expected = serde_json::to_value(active.lock().as_ref().unwrap()).unwrap();
        let mut updates = Vec::new();
        for (command, field, value) in [
            (
                serde_json::json!({"PatchConfig": {"title": "first patch"}}),
                "title",
                "first patch",
            ),
            (
                serde_json::json!({"PatchConfig": {"description": "second patch"}}),
                "description",
                "second patch",
            ),
            (
                serde_json::json!({"SetConfigValue": ["/title", "third edit"]}),
                "title",
                "third edit",
            ),
            (
                serde_json::json!({"SetConfigValue": ["/description", "fourth edit"]}),
                "description",
                "fourth edit",
            ),
        ] {
            let name = command.as_object().unwrap().keys().next().unwrap().clone();
            assert_eq!(request(command)[&name]["result"], "Ok");
            let update = rx_conf.recv_timeout(Duration::from_secs(2)).unwrap();
            expected[field] = serde_json::json!(value);
            assert_eq!(serde_json::to_value(&update.config).unwrap(), expected);
            updates.push(update);
            if updates.len() == 1 {
                for (command, error_kind) in [
                    (
                        serde_json::json!({"SetConfigValue": ["/missing", 1]}),
                        "InvalidRequestError",
                    ),
                    (
                        serde_json::json!({"PatchConfig": {"title": []}}),
                        "ConfigReadError",
                    ),
                    (
                        serde_json::json!({"PatchConfig": {"title": "rejected", "processors": null}}),
                        "ConfigValidationError",
                    ),
                ] {
                    let name = command.as_object().unwrap().keys().next().unwrap().clone();
                    assert!(request(command)[&name]["result"][error_kind].is_string());
                    assert!(rx_conf.is_empty(), "rejected edit reached processing");
                }
            }
            assert_eq!(
                request(serde_json::json!("GetConfigTitle"))["GetConfigTitle"]["value"],
                "first replacement"
            );
        }
        for update in updates {
            update.installed.send(update.config).unwrap();
        }
        let start = std::time::Instant::now();
        while serde_json::to_value(active.lock().as_ref().unwrap()).unwrap() != expected {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "partial updates not installed"
            );
            thread::sleep(Duration::from_millis(1));
        }
        websocket.close(None).unwrap();
    }
    #[cfg(not(feature = "websocket"))]
    drop(supervisor_control);
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
        let active = Arc::new(Mutex::new(if starting { None } else { Some(conf.clone()) }));
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
        let expect_restart_config = matches!(trigger, Trigger::Devices | Trigger::StatusClosed);
        let expect_error = ((panic_processing || panic_capture) && !expect_exit)
            || matches!(trigger, Trigger::ControllerClosed | Trigger::ConfigClosed);
        let had_active = !starting;
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
                    let camillalib::protection::Instance::Peak { limit_dbfs, .. } =
                        parameters.policies.get_mut("hf_peak").unwrap()
                    else {
                        unreachable!()
                    };
                    *limit_dbfs = -1.0;
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
                matches!(exit, ExitState::RestartWithConfig(_)),
                expect_restart_config
            );
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
        assert!(
            active.lock().is_none(),
            "retired run still advertised active"
        );
        assert_eq!(
            previous.lock().is_some(),
            had_active,
            "failed startup must not publish an uninstalled previous config"
        );
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
fn failed_playback_open_keeps_the_prior_trip_readable() {
    let config = configuration();
    let statuses = status_structs();
    let params = &statuses.processing;
    {
        let mut guard = params.protection_owner.lock().unwrap();
        guard.install(&config);
        let mut incomplete =
            camillalib::audiochunk::AudioChunk::new(Vec::new(), 0.0, 0.0, 256, 256);
        guard.process(&mut incomplete);
    }
    let shared = SharedConfigs {
        active: Arc::default(),
        previous: Arc::default(),
    };
    run(config, shared, statuses.clone(), crossbeam_channel::never()).unwrap();
    assert!(matches!(
        statuses.status.read().stop_reason,
        StopReason::PlaybackError(_)
    ));
    let reply: serde_json::Value =
        serde_json::from_str(&params.protection_reply("\"GetProtectionStatus\"").unwrap()).unwrap();
    let reply = &reply["GetProtectionStatus"];
    assert_eq!(reply["result"], "Ok", "{reply}");
    let status = &reply["value"];
    assert_eq!(status["trip"]["latched"], true);
    assert_eq!(status["trip"]["reason"], "coverage");
    for source in ["processing", "playback", "telemetry", "conversion"] {
        assert_eq!(status[source]["valid"], false);
    }
    assert_eq!(status["playback"]["submitted_frames"], 0);
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

#[test]
fn startup_publication_waits_for_installation_and_rejects_poisoned_owner() {
    for poisoned in [false, true] {
        let config = configuration();
        let active = Arc::new(Mutex::new(None));
        let shared = SharedConfigs {
            active: active.clone(),
            previous: Arc::new(Mutex::new(None)),
        };
        let statuses = status_structs();
        if poisoned {
            let params = statuses.processing.clone();
            assert!(
                thread::spawn(move || {
                    let _owner = params.protection_owner.lock().unwrap();
                    panic!("failed prior processing owner");
                })
                .join()
                .is_err()
            );
        }
        let (capture_command, capture_commands) = crossbeam_channel::unbounded();
        let (processing_stop, processing_stops) = crossbeam_channel::unbounded();
        let barrier = Arc::new(Barrier::new(2));
        let processing_barrier = barrier.clone();
        let (install, installation) = crossbeam_channel::bounded(1);
        let (ready, readiness) = crossbeam_channel::bounded(1);
        let processing_active = active.clone();
        let (before_install, before_installation) = crossbeam_channel::bounded(1);
        let threads = RunThreads {
            capture_command,
            processing_stop,
            barrier,
            starting: true,
            processing: thread::spawn(move || {
                // Model delayed installation, then acknowledge through the
                // actual production startup barrier. No audio is required.
                installation.recv().unwrap();
                before_install
                    .send(processing_active.lock().is_none())
                    .unwrap();
                processing_barrier.wait();
                processing_stops.recv().unwrap();
            }),
            capture: thread::spawn(move || {
                capture_commands.recv().unwrap();
            }),
            playback: thread::spawn(|| {}),
        };
        let (finish, finished) = crossbeam_channel::bounded(1);
        let expected = config.config().clone();
        let supervisor = thread::spawn(move || {
            let mut threads = threads;
            let result = threads.publish_startup(&config, &shared, &statuses);
            ready.send(result.is_ok()).unwrap();
            finished.recv().unwrap();
            threads.shutdown().unwrap();
        });
        assert!(active.lock().is_none());
        install.send(()).unwrap();
        assert_eq!(
            readiness.recv_timeout(Duration::from_secs(2)).unwrap(),
            !poisoned
        );
        let observed = active.lock().as_ref().map(|c| c.config().clone());
        finish.send(()).unwrap();
        supervisor.join().unwrap();
        assert!(
            before_installation.recv().unwrap(),
            "configuration published before installation"
        );
        assert_eq!(observed, if poisoned { None } else { Some(expected) });
    }
}

#[test]
fn mixer_changes_restart_only_when_the_capture_mask_changes() {
    let mut raw = serde_json::to_value(configuration()).unwrap();
    raw["mixers"] = serde_json::json!({"sparse": {
        "channels": {"in": 24, "out": 24},
        "mapping": [{"dest": 0, "sources": [{"channel": 0}]}]
    }});
    raw["pipeline"]
        .as_array_mut()
        .unwrap()
        .insert(0, serde_json::json!({"type": "Mixer", "name": "sparse"}));
    let prepare = |value: &serde_json::Value| {
        config::validate_config(serde_json::from_value(value.clone()).unwrap(), None).unwrap()
    };
    let sparse = prepare(&raw);
    raw["mixers"]["sparse"]["mapping"][0]["sources"][0]["gain"] = serde_json::json!(-6);
    let gain = prepare(&raw);
    assert!(matches!(
        config::config_diff(sparse.config(), gain.config()),
        config::ConfigChange::MixerParameters
    ));

    raw["mixers"]["sparse"]["mapping"][0]["sources"][0]["channel"] = serde_json::json!(1);
    let rerouted = prepare(&raw);
    raw["pipeline"][0]["bypassed"] = serde_json::json!(true);
    let bypassed = prepare(&raw);
    assert!(matches!(
        config::config_diff(gain.config(), rerouted.config()),
        config::ConfigChange::Devices
    ));
    assert_restart_waits_for_old_workers(rerouted, bypassed);
}

#[test]
fn device_reload_must_not_publish_active_before_old_workers_exit() {
    let initial = configuration();
    let mut raw = initial.config().clone();
    raw.devices.queuelimit = Some(raw.devices.queuelimit() + 1);
    let replacement = config::validate_config(raw, None).unwrap();
    assert_restart_waits_for_old_workers(initial, replacement);
}

fn assert_restart_waits_for_old_workers(
    initial: config::PreparedConfig,
    replacement: config::PreparedConfig,
) {
    assert!(matches!(
        config::config_diff(initial.config(), replacement.config()),
        config::ConfigChange::Devices
    ));
    let active = Arc::new(Mutex::new(Some(initial.clone())));
    let shared = SharedConfigs {
        active: active.clone(),
        previous: Arc::new(Mutex::new(None)),
    };
    let (tx_ctrl, rx_ctrl) = crossbeam_channel::unbounded();
    let rx_pending = rx_ctrl.clone();
    let (_tx_status, rx_status) = crossbeam_channel::unbounded();
    let (tx_conf, rx_conf) = crossbeam_channel::unbounded();
    let (tx_stop, rx_stop) = crossbeam_channel::unbounded();
    let (tx_capture, rx_capture) = crossbeam_channel::unbounded();
    let (tx_stopping, rx_stopping) = crossbeam_channel::bounded(1);
    let (tx_finish, rx_finish) = crossbeam_channel::bounded(1);
    let threads = RunThreads {
        capture_command: tx_capture,
        processing_stop: tx_stop,
        barrier: Arc::new(Barrier::new(1)),
        starting: false,
        processing: thread::spawn(move || {
            rx_stop.recv().unwrap();
            tx_stopping.send(()).unwrap();
            // Deliberately hold the old run before join completes. No new
            // processing owner or installation acknowledgement can exist.
            rx_finish.recv_timeout(Duration::from_secs(2)).unwrap();
        }),
        capture: thread::spawn(move || {
            rx_capture.recv().unwrap();
        }),
        playback: thread::spawn(|| {}),
    };
    let expected_old = initial.config().clone();
    let expected_new = replacement.config().clone();
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
        .send(ControllerMessage::ConfigChanged(Box::new(replacement)))
        .unwrap();
    rx_stopping.recv_timeout(Duration::from_secs(2)).unwrap();
    let observed = active.lock().as_ref().map(|c| c.config().clone());
    // A partial request queued while the old run joins must compose on the
    // replacement, even though there is no public active config yet.
    let (reply, response) = crossbeam_channel::bounded(1);
    tx_ctrl
        .send(ControllerMessage::MutateConfig {
            mutation: config::ConfigMutation::Patch(
                serde_json::json!({"title": "queued after device change"}),
            ),
            reply,
        })
        .unwrap();
    // Always finish and join, including when the qualification assertion fails.
    tx_finish.send(()).unwrap();
    let ExitState::RestartWithConfig(next) = supervisor.join().unwrap() else {
        panic!("replacement request was lost during shutdown");
    };
    assert!(
        rx_conf.is_empty(),
        "old run received an incompatible live update"
    );
    assert_eq!(next.config(), &expected_new);
    assert!(
        active.lock().is_none(),
        "retired configuration is still public"
    );
    let selected = next_configuration(
        &rx_pending,
        Some(*next),
        false,
        &camillalib::protection::Shared::default(),
    )
    .unwrap();
    assert!(response.recv().unwrap().is_ok());
    assert_eq!(
        selected.config().devices.queuelimit(),
        expected_new.devices.queuelimit()
    );
    assert_eq!(
        selected.config().title.as_deref(),
        Some("queued after device change")
    );
    assert!(
        active.lock().is_none(),
        "pending partial edit was published"
    );
    assert!(
        observed.is_none() || observed.as_ref() == Some(&expected_old),
        "replacement advertised active while the old run is still shutting down"
    );
}
