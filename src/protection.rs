//! Final candidate-sample admission for the protection fork.
//!
//! This owner is deliberately separate from the rebuildable processing graph:
//! graph changes cannot clear a trip. Only stamped guard output may enter ALSA.

use crate::audiochunk::AudioChunk;
use crate::{PrcFmt, Res, config};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(any(test, feature = "protection-test-capture"))]
mod capture;
pub mod control;
pub mod identity;
#[cfg(test)]
mod lifecycle_tests;
pub mod playback;
mod policy;
mod record;
mod state;
pub mod wire;
use control::{Command, Control, Mode, Operation};
use record::AtomicRecord;
pub use record::GuardRecord;
use state::{
    CandidateObservation, DetectorState, FaultState, OutputActivity, Qualification, StreamState,
};

/// Fixed protection profile. Configuration fields assert these mappings;
/// they do not select or remap channels.
pub const OUTPUT_CHANNELS: [usize; 24] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
];
pub const TWEETER_CHANNELS: [usize; 2] = [0, 1];
pub const CHANNELS: usize = OUTPUT_CHANNELS.len();
pub const FRAMES: usize = 256;
pub const SAMPLE_RATE: usize = 48_000;
// Default DC window; parameter-dependent qualification is compiled in Policy.
pub const QUALIFICATION_FRAMES: u64 = 2 * SAMPLE_RATE as u64;

fn bump(value: u64) -> u64 {
    assert!(value < i64::MAX as u64, "Protection counter exhausted");
    value + 1
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    /// Must equal OUTPUT_CHANNELS, in order; a fixed-profile assertion.
    pub output_channels: [usize; CHANNELS],
    /// Must equal TWEETER_CHANNELS, in order; a fixed-profile assertion.
    pub tweeter_channels: [usize; TWEETER_CHANNELS.len()],
    pub max_allowed_peak_db: f64,
    pub dc_freeze_seconds: f64,
    pub dc_freeze_peak_rms_tolerance_db: f64,
    pub level_freshness_epsilon_db: f64,
    pub max_tweeter_rms_db: Option<f64>,
    pub tweeter_rms_fault_seconds: f64,
    pub max_tweeter_avg_power_db: Option<f64>,
    pub tweeter_avg_power_tau_seconds: f64,
    pub max_tweeter_long_avg_power_db: Option<f64>,
    pub tweeter_long_avg_power_tau_seconds: f64,
    pub tone_min_rms_db: Option<f64>,
    pub tone_crest_db: f64,
    pub tone_fault_seconds: f64,
    pub silence_threshold_db: f64,
}

impl Parameters {
    pub fn validate(&self) -> Res<()> {
        if self.output_channels != OUTPUT_CHANNELS
            || self.tweeter_channels != TWEETER_CHANNELS
            || !self.max_allowed_peak_db.is_finite()
            || self.max_allowed_peak_db > 0.0
            || self.max_allowed_peak_db < -120.0
        {
            return Err(
                config::ConfigError::new("Invalid protection channel map or peak limit").into(),
            );
        }
        policy::validate(self)?;
        Ok(())
    }
}

/// Validate before constructing or updating any executable graph. The declared
/// final marker is compiled as the processing owner's final Guard operation.
fn admission(conf: &config::Configuration) -> Res<Parameters> {
    if cfg!(feature = "32bit") || cfg!(feature = "threaded-alsa") {
        return Err(config::ConfigError::new(
            "Protection requires double precision and direct ALSA",
        )
        .into());
    }
    if conf.devices.samplerate != SAMPLE_RATE
        || conf.devices.chunksize != FRAMES
        || conf.devices.playback.channels() != CHANNELS
    {
        return Err(config::ConfigError::new(
            "Protection requires 48000 Hz, 256 frames and 24 outputs",
        )
        .into());
    }
    #[cfg(target_os = "linux")]
    let backend_valid = matches!(&conf.devices.playback,
        config::PlaybackDevice::Alsa { device, format, .. }
        if device.starts_with("hw:") && *format == Some(config::AlsaSampleFormat::S24_3_LE));
    #[cfg(not(target_os = "linux"))]
    let backend_valid = false;
    if !backend_valid {
        return Err(config::ConfigError::new(
            "Protection requires direct hw: ALSA S24_3_LE playback",
        )
        .into());
    }
    let pipeline = conf.pipeline.as_deref().unwrap_or_default();
    let mut found = None;
    for (index, step) in pipeline.iter().enumerate() {
        if let config::PipelineStep::Processor(step) = step
            && let Some(config::Processor::Protection { parameters, .. }) =
                conf.processors.as_ref().and_then(|p| p.get(&step.name))
        {
            if found.is_some() || step.is_bypassed() || index + 1 != pipeline.len() {
                return Err(config::ConfigError::new(
                    "Protection must occur exactly once, last and not bypassed",
                )
                .into());
            }
            parameters.validate()?;
            found = Some(parameters.clone());
        }
    }
    found.ok_or_else(|| {
        config::ConfigError::new("Required final Protection processor is missing").into()
    })
}

/// Compiled once during preparation, never paired with another raw graph.
#[derive(Clone, Debug)]
pub(crate) struct AdmittedGuard {
    parameters: Parameters,
    peak_limit: PrcFmt,
    policy: policy::Policy,
    identity: identity::Identity,
}

impl AdmittedGuard {
    pub(crate) fn new(conf: &config::Configuration) -> Res<Self> {
        let parameters = admission(conf)?;
        let peak_limit = 10.0_f64.powf(parameters.max_allowed_peak_db / 20.0) as PrcFmt;
        let policy = policy::Policy::new(&parameters);
        let identity = identity::prepare(conf, &parameters, &policy)?;
        Ok(Self {
            parameters,
            peak_limit,
            policy,
            identity,
        })
    }

    pub(crate) fn identity(&self) -> identity::Identity {
        self.identity
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum Reason {
    Peak = 1,
    Nonfinite = 2,
    Coverage = 3,
    Rms = 4,
    ShortPower = 5,
    LongPower = 6,
    Tone = 7,
    DcFrozen = 8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trip {
    pub generation: u64,
    pub reason: Reason,
    pub channel: Option<usize>,
    pub frame: u64,
    pub at_ns: u64,
}

#[derive(Debug)]
pub struct Shared {
    epoch: AtomicU64,
    backend: AtomicU64,
    reset: AtomicU64,
    guard_record: AtomicRecord<{ record::GUARD_WORDS }>,
    playback_record: AtomicRecord<{ record::PLAYBACK_WORDS }>,
    host: Option<wire::Host>,
    conversion_count: AtomicU64,
    #[cfg(any(test, feature = "protection-test-capture"))]
    pub(crate) capture: capture::Capture,
    backend_wakeup: crossbeam_channel::Sender<()>,
    backend_wakeup_rx: crossbeam_channel::Receiver<()>,
    commands: crossbeam_channel::Sender<Command>,
    command_rx: crossbeam_channel::Receiver<Command>,
}

impl Default for Shared {
    fn default() -> Self {
        let (commands, command_rx) = crossbeam_channel::bounded(1);
        let (backend_wakeup, backend_wakeup_rx) = crossbeam_channel::bounded(1);
        Self {
            epoch: AtomicU64::new(0),
            backend: AtomicU64::new(1),
            reset: AtomicU64::new(0),
            guard_record: AtomicRecord::default(),
            playback_record: AtomicRecord::default(),
            host: wire::Host::load(),
            conversion_count: AtomicU64::new(0),
            #[cfg(any(test, feature = "protection-test-capture"))]
            capture: capture::Capture::default(),
            backend_wakeup,
            backend_wakeup_rx,
            commands,
            command_rx,
        }
    }
}

/// Coherent processing-owner payload with its acquisition sequence. The
/// sequence is transport metadata, never a field encoded by the owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub sequence: u64,
    pub record: GuardRecord,
}

impl Shared {
    /// Internal bounded mailbox, not an externally admitted wire operation.
    pub fn submit(&self, command: Command) -> Result<(), crossbeam_channel::TrySendError<Command>> {
        self.commands.try_send(command)
    }

    pub(crate) fn commands(&self) -> &crossbeam_channel::Receiver<Command> {
        &self.command_rx
    }

    pub(crate) fn backend_wakeups(&self) -> &crossbeam_channel::Receiver<()> {
        &self.backend_wakeup_rx
    }

    /// Read-only projection; publication and epoch retirement belong to Guard.
    pub fn guard_snapshot(&self) -> Option<Snapshot> {
        (0..3).find_map(|_| self.guard_snapshot_once())
    }

    fn guard_snapshot_once(&self) -> Option<Snapshot> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        let backend = self.backend.load(Ordering::SeqCst);
        let reset = self.reset.load(Ordering::SeqCst);
        let (sequence, words) = self.guard_record.read_once()?;
        if sequence == 0 {
            return None;
        }
        let record = GuardRecord::from_words(&words)?;
        (record.pipeline == epoch
            && record.reset == reset
            && record.backend == backend
            && epoch == self.epoch.load(Ordering::SeqCst)
            && reset == self.reset.load(Ordering::SeqCst)
            && backend == self.backend.load(Ordering::SeqCst))
        .then_some(Snapshot { sequence, record })
    }
}

/// The guard alone constructs stamps, after assessment and whole-chunk masking.
#[derive(Debug)]
pub struct Stamp {
    shared: Arc<Shared>,
    epoch: u64,
    backend: u64,
    reset: u64,
    pub frame_begin: u64,
    pub frame_end: u64,
    pub source_begin_ns: u64,
    pub source_end_ns: u64,
    pub rms_power: [f64; CHANNELS],
}

impl Stamp {
    #[cfg(any(test, feature = "protection-test-capture"))]
    pub(crate) fn capture_conversion(&self, clipped: usize, pcm: &[u8], begin: u64, end: u64) {
        self.shared
            .capture
            .converted(self, clipped, pcm, begin, end);
    }

    pub fn is_stream_current(&self) -> bool {
        self.epoch == self.shared.epoch.load(Ordering::SeqCst)
            && self.backend == self.shared.backend.load(Ordering::SeqCst)
    }

    pub fn is_current(&self) -> bool {
        self.is_stream_current() && self.reset == self.shared.reset.load(Ordering::SeqCst)
    }
}

pub fn monotonic_ns() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let mut value = nix::libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // CLOCK_MONOTONIC is shared with the local watchdog; never serialize Instant.
        let result = unsafe { nix::libc::clock_gettime(nix::libc::CLOCK_MONOTONIC, &mut value) };
        assert_eq!(result, 0, "Protection monotonic clock failed");
        u64::try_from(value.tv_sec)
            .unwrap()
            .checked_mul(1_000_000_000)
            .unwrap()
            .checked_add(u64::try_from(value.tv_nsec).unwrap())
            .unwrap()
    }
    #[cfg(not(target_os = "linux"))]
    {
        panic!("Protection requires Linux CLOCK_MONOTONIC");
    }
}

/// Transition preservation contract:
/// - Install/backend change restart frames and floor; pause retains both.
/// - All three preserve the trip and power history, break detector continuity,
///   retire output activity and require a fresh qualification attempt.
/// - Accepted reset retains frames, advances the floor, clears trip/power and
///   begins a fresh prearm attempt; the lifetime trip generation survives.
#[derive(Debug)]
pub struct Guard {
    pub(crate) shared: Arc<Shared>,
    configuration: Option<AdmittedGuard>,
    stream: StreamState,
    detector: DetectorState,
    fault: FaultState,
    control: Control,
    qualification: Qualification,
    candidate: CandidateObservation,
    activity: OutputActivity,
}

/// Facts about the original candidate; no sample copies or retained evidence.
#[derive(Clone, Copy)]
struct CandidateAssessment {
    shape_valid: bool,
    finite: bool,
}

/// Accounting for the samples actually forwarded after whole-chunk containment.
struct ForwardedOutput {
    frame_begin: u64,
    rms_power: [f64; CHANNELS],
}

impl Guard {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            stream: StreamState {
                backend: shared.backend.load(Ordering::SeqCst),
                ..StreamState::default()
            },
            shared,
            configuration: None,
            detector: DetectorState::default(),
            fault: FaultState::default(),
            control: Control::default(),
            qualification: Qualification::default(),
            candidate: CandidateObservation::default(),
            activity: OutputActivity::default(),
        }
    }

    pub fn install(&mut self, conf: &config::PreparedConfig) {
        self.sync_backend();
        // Every admitted replacement retires queued stamps and qualification,
        // preserving the latched trip and accumulated power history.
        let epoch = self
            .shared
            .epoch
            .fetch_add(1, Ordering::SeqCst)
            .checked_add(1)
            .unwrap();
        assert!(epoch <= i64::MAX as u64);
        self.configuration = None;
        self.stream.new_pipeline(epoch);
        self.activity.interrupt();
        self.detector.break_continuity();
        self.candidate.new_configuration();
        self.qualification.require_attempt();
        self.publish();
        self.configuration = Some(conf.guard().clone());
        self.publish();
    }

    pub fn trip(&self) -> Option<Trip> {
        self.fault.trip
    }

    fn latch(&mut self, reason: Reason, channel: Option<usize>, frame: u64) {
        self.fault.latch(reason, channel, frame);
        self.qualification.require_attempt();
    }

    fn publish(&self) {
        let record = GuardRecord {
            pipeline: self.stream.epoch,
            backend: self.stream.backend,
            reset: self.stream.reset,
            frame_floor: self.stream.frame_floor,
            frames: self.stream.frames,
            processing_valid: self.stream.processing_valid,
            source_begin_ns: self.stream.source_begin_ns,
            source_end_ns: self.stream.source_end_ns,
            state: self.state(),
            candidate_observable: self.candidate.observable,
            qualified_frames: self.qualification.frames(),
            short_power_veto: self.detector.short_power_veto,
            attempt_required: self.qualification.attempt_required(),
            trip_generation: self.fault.generation,
            trip: self.fault.trip,
            control: self.control.snapshot(),
            identity: self
                .configuration
                .as_ref()
                .map(|c| c.identity)
                .unwrap_or_default(),
            final_guard: self.configuration.is_some(),
            activity_generation: self.activity.generation,
            silent_frames: self.activity.silent_frames,
            last_active_end_frame: self.activity.last_active_end_frame,
        };
        self.shared.guard_record.publish(record.to_words());
    }

    pub fn process(&mut self, chunk: &mut AudioChunk) {
        self.process_with_command(chunk, None);
    }

    pub fn process_with_command(&mut self, chunk: &mut AudioChunk, command: Option<Command>) {
        self.sync_backend();
        let armed = command.filter(|c| c.operation == Operation::Context(Mode::Armed));
        if let Some(command) = command.filter(|c| c.operation != Operation::Context(Mode::Armed)) {
            // Reset/Prearm precede assessment; this candidate may immediately retrip.
            self.apply_control(command, false);
        }
        #[cfg(any(test, feature = "protection-test-capture"))]
        let capture = self.shared.capture.guard(chunk, self.stream.frames);
        chunk.protection = None;
        let begin = monotonic_ns();
        let assessment = self.assess_candidate(chunk);
        self.assess_policy_and_qualify(chunk, assessment);
        if let Some(command) = armed {
            // Current-chunk hazards and qualification precede armed receipt.
            self.apply_control(command, true);
        }
        #[cfg(any(test, feature = "protection-test-capture"))]
        let decision_ns = monotonic_ns();
        if !self.qualification.is_active() || self.fault.trip.is_some() || !assessment.shape_valid {
            for waveform in &mut chunk.waveforms {
                waveform.fill(0.0);
            }
            chunk.maxval = 0.0;
            chunk.minval = 0.0;
        }
        // Activity and RMS describe the forwarded chunk, after containment.
        let forwarded = self.account_forwarded_output(chunk, assessment.shape_valid);
        let end = monotonic_ns();
        self.stream.processing_valid = assessment.shape_valid;
        self.stream.source_begin_ns = if assessment.shape_valid { begin } else { 0 };
        self.stream.source_end_ns = if assessment.shape_valid { end } else { 0 };
        self.publish();
        #[cfg(any(test, feature = "protection-test-capture"))]
        if let Some(capture) = capture {
            capture.guard(self, chunk, decision_ns, monotonic_ns());
        }
        if assessment.shape_valid {
            chunk.protection = Some(Stamp {
                shared: self.shared.clone(),
                epoch: self.stream.epoch,
                backend: self.stream.backend,
                reset: self.stream.reset,
                frame_begin: forwarded.frame_begin,
                frame_end: self.stream.frames,
                source_begin_ns: begin,
                source_end_ns: end,
                rms_power: forwarded.rms_power,
            });
        }
    }

    /// Inspect original candidate samples and latch at the first observed fault.
    /// Coverage failure must not hide available peak or nonfinite evidence.
    fn assess_candidate(&mut self, chunk: &AudioChunk) -> CandidateAssessment {
        let shape_valid = self.configuration.is_some()
            && chunk.frames == FRAMES
            && chunk.valid_frames == FRAMES
            && chunk.channels == CHANNELS
            && chunk.waveforms.len() == CHANNELS
            && chunk.waveforms.iter().all(|w| w.len() == FRAMES);
        let mut finite = true;
        // Assess available instantaneous evidence before a missing-channel fault.
        for (channel, waveform) in chunk.waveforms.iter().take(CHANNELS).enumerate() {
            for (offset, sample) in waveform.iter().take(FRAMES).enumerate() {
                let reason = if !sample.is_finite() {
                    finite = false;
                    Some(Reason::Nonfinite)
                } else if TWEETER_CHANNELS.contains(&channel)
                    && self
                        .configuration
                        .as_ref()
                        .is_some_and(|c| sample.abs() >= c.peak_limit)
                {
                    Some(Reason::Peak)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    self.latch(reason, Some(channel), self.stream.frames + offset as u64);
                }
            }
        }
        if !shape_valid {
            self.latch(Reason::Coverage, None, self.stream.frames);
        }
        CandidateAssessment {
            shape_valid,
            finite,
        }
    }

    /// Detector history and qualification use candidates, never masked output.
    fn assess_policy_and_qualify(&mut self, chunk: &AudioChunk, assessment: CandidateAssessment) {
        let CandidateAssessment {
            shape_valid,
            finite,
        } = assessment;
        self.observe_main_volume(chunk, shape_valid);
        self.detector.short_power_veto = false;
        if shape_valid && finite {
            for (history_index, channel) in TWEETER_CHANNELS.into_iter().enumerate() {
                let result = self.configuration.as_ref().unwrap().policy.assess(
                    &mut self.detector.history[history_index],
                    &chunk.waveforms[channel],
                    self.control.mode == Mode::Prearm,
                    self.candidate.observable,
                );
                self.detector.short_power_veto |= result.veto;
                if let Some(reason) = result.reason {
                    self.latch(reason, Some(channel), self.stream.frames + FRAMES as u64);
                }
            }
        } else {
            self.detector.break_continuity();
        }
        if self.detector.short_power_veto {
            // A veto restarts a full window, including after local admission
            // while the parent's independent checks are still qualifying.
            self.qualification.restart_window();
        } else {
            self.qualify();
        }
    }

    fn account_forwarded_output(
        &mut self,
        chunk: &AudioChunk,
        shape_valid: bool,
    ) -> ForwardedOutput {
        let frame_begin = self.stream.frames;
        if shape_valid {
            self.stream.frames = self.stream.frames.checked_add(FRAMES as u64).unwrap();
            assert!(self.stream.frames <= i64::MAX as u64);
        }
        let mut rms_power = [0.0; CHANNELS];
        if shape_valid {
            for (rms, waveform) in rms_power.iter_mut().zip(&chunk.waveforms) {
                *rms = waveform.iter().map(|x| x * x).sum::<PrcFmt>() / FRAMES as f64;
            }
            if rms_power
                .iter()
                .any(|power| *power > self.configuration.as_ref().unwrap().policy.silence_power)
            {
                self.activity.silent_frames = 0;
                self.activity.last_active_end_frame = self.stream.frames;
            } else {
                self.activity.silent_frames = self
                    .activity
                    .silent_frames
                    .checked_add(FRAMES as u64)
                    .unwrap();
            }
        } else {
            self.activity.interrupt();
            self.detector.break_continuity();
        }
        ForwardedOutput {
            frame_begin,
            rms_power,
        }
    }

    // Only the processing owner rebases protected frames. Playback retires its
    // atomic generation before any PCM recovery; old queued stamps then fail.
    fn sync_backend(&mut self) {
        let backend = self.shared.backend.load(Ordering::SeqCst);
        if backend != self.stream.backend {
            self.stream.new_backend(backend);
            self.candidate.interrupt();
            self.activity.interrupt();
            self.detector.break_continuity();
            self.qualification.require_attempt();
        }
    }

    pub fn backend_wakeup(&mut self) {
        self.sync_backend();
        self.publish();
    }

    fn observe_main_volume(&mut self, chunk: &AudioChunk, shape_valid: bool) {
        self.candidate.observable =
            shape_valid && chunk.main_volume.is_some_and(|v| !v.muted && !v.opening);
        let Some(volume) = chunk.main_volume else {
            self.qualification.require_attempt();
            return;
        };
        if self.candidate.last_main_mute == Some(true) && !volume.muted {
            // Must precede forwarding the first opening sample. A new prearm
            // attempt must be installed after this observed transition.
            self.qualification.require_attempt();
        } else if self.candidate.last_main_mute == Some(false)
            && volume.muted
            && !self.qualification.is_active()
        {
            self.qualification.require_attempt();
        }
        self.candidate.last_main_mute = Some(volume.muted);
        if !self.candidate.observable {
            // An already active ordinary fade-out is still passed through.
            self.qualification.masked_candidate();
        }
    }

    fn qualify(&mut self) {
        if !self.candidate.observable
            || self.fault.trip.is_some()
            || self.qualification.attempt_required()
            || self.control.owner.is_none()
        {
            return;
        }
        self.qualification.observe_candidate(
            self.configuration
                .as_ref()
                .unwrap()
                .policy
                .qualification_frames,
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn parameters() -> Parameters {
        Parameters {
            output_channels: OUTPUT_CHANNELS,
            tweeter_channels: TWEETER_CHANNELS,
            max_allowed_peak_db: -0.5,
            dc_freeze_seconds: 2.0,
            dc_freeze_peak_rms_tolerance_db: 0.25,
            level_freshness_epsilon_db: 0.05,
            max_tweeter_rms_db: None,
            tweeter_rms_fault_seconds: 5.0,
            max_tweeter_avg_power_db: None,
            tweeter_avg_power_tau_seconds: 5.0,
            max_tweeter_long_avg_power_db: None,
            tweeter_long_avg_power_tau_seconds: 90.0,
            tone_min_rms_db: None,
            tone_crest_db: 4.5,
            tone_fault_seconds: 5.0,
            silence_threshold_db: -70.0,
        }
    }

    fn guard() -> Guard {
        let mut guard = Guard::new(Arc::new(Shared::default()));
        guard.install(&prepared_configuration());
        // Sample-containment unit tests start from a locally admitted guard.
        guard.qualification = Qualification::Active { frames: 0 };
        guard
    }

    pub(crate) fn chunk() -> AudioChunk {
        {
            let mut chunk = AudioChunk::new(
                vec![
                    (0..FRAMES)
                        .map(|i| if i % 2 == 0 { 0.1 } else { 0.0 })
                        .collect();
                    CHANNELS
                ],
                0.1,
                0.0,
                FRAMES,
                FRAMES,
            );
            chunk.main_volume = Some(crate::filters::basicfilters::AppliedVolume {
                muted: false,
                opening: false,
            });
            chunk
        }
    }

    pub(crate) fn configuration() -> config::Configuration {
        serde_json::from_value(serde_json::json!({
            "devices": {"samplerate": 48000, "chunksize": 256, "volume_ramp_time": 0,
                "capture": {"type": "SignalGenerator", "channels": 24,
                    "signal": {"type": "Sine", "freq": 1000, "level": -20}},
                "playback": {"type": "Alsa", "channels": 24, "device": "hw:D624,0", "format": "S24_3_LE"}},
            "processors": {"guard": {"type": "Protection", "parameters": parameters()}},
            "pipeline": [{"type": "Processor", "name": "guard"}]
        })).unwrap()
    }

    pub(crate) fn prepared_configuration() -> config::PreparedConfig {
        config::validate_config(configuration(), None).unwrap()
    }

    #[test]
    fn missing_bypassed_nonfinal_and_repeated_guards_reject_admission() {
        let conf = configuration();
        admission(&conf).unwrap();
        let mut missing = conf.clone();
        missing.pipeline = None;
        assert!(admission(&missing).is_err());
        let mut bypassed = serde_json::to_value(&conf).unwrap();
        bypassed["pipeline"][0]["bypassed"] = serde_json::json!(true);
        let bypassed = serde_json::from_value(bypassed).unwrap();
        assert!(admission(&bypassed).is_err());
        let mut repeated = conf.clone();
        repeated
            .pipeline
            .as_mut()
            .unwrap()
            .push(conf.pipeline.as_ref().unwrap()[0].clone());
        assert!(admission(&repeated).is_err());
        let mut nonfinal = serde_json::to_value(conf).unwrap();
        nonfinal["filters"] =
            serde_json::json!({"gain": {"type": "Gain", "parameters": {"gain": 6}}});
        nonfinal["pipeline"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type": "Filter", "names": ["gain"]}));
        assert!(admission(&serde_json::from_value(nonfinal).unwrap()).is_err());
    }

    #[test]
    fn preparation_rejects_invalid_edits_and_install_retires_queue_without_clearing_trip() {
        let mut guard = Guard::new(Arc::new(Shared::default()));
        let conf = config::validate_config(configuration(), None).unwrap();
        guard.install(&conf);
        let mut input = chunk();
        input.waveforms[1][5] = 1.0;
        guard.process(&mut input);
        let stamp = input.protection.unwrap();
        let trip = guard.fault.trip;
        let mut invalid = conf.config().clone();
        invalid.pipeline = None;
        assert!(config::validate_config(invalid, None).is_err());
        // Rejected edits never reach the owner; the admitted graph stays bound.
        assert!(stamp.is_current());
        assert_eq!(guard.fault.trip, trip);
        guard.install(&conf);
        assert!(!stamp.is_current());
        assert_eq!(guard.fault.trip, trip);
        let mut safe = chunk();
        guard.process(&mut safe);
        assert!(safe.waveforms.iter().flatten().all(|s| *s == 0.0));
    }

    #[test]
    fn offending_chunk_and_following_safe_chunks_are_zero() {
        for channel in [0, 1] {
            for offset in [0, FRAMES - 1] {
                let mut guard = guard();
                let mut input = chunk();
                input.waveforms[channel][offset] = guard.configuration.as_ref().unwrap().peak_limit;
                guard.process(&mut input);
                assert!(input.waveforms.iter().flatten().all(|s| *s == 0.0));
                // A hazardous candidate must not appear as output power/activity.
                let stamp = input.protection.as_ref().unwrap();
                assert_eq!(stamp.rms_power, [0.0; CHANNELS]);
                let record = guard.shared.guard_snapshot().unwrap().record;
                assert_eq!(record.silent_frames, FRAMES as u64);
                assert_eq!(record.last_active_end_frame, 0);
                let trip = guard.fault.trip.unwrap();
                assert_eq!(trip.reason, Reason::Peak);
                assert_eq!(trip.channel, Some(channel));
                assert_eq!(trip.frame, offset as u64);
                let mut safe = chunk();
                guard.process(&mut safe);
                assert!(safe.waveforms.iter().flatten().all(|s| *s == 0.0));
                assert_eq!(guard.fault.trip, Some(trip));
                assert_eq!(guard.stream.frames, 2 * FRAMES as u64);
                assert!(safe.protection.unwrap().is_current());
            }
        }
    }

    #[test]
    fn nonfinite_at_guard_on_any_channel_is_contained() {
        for channel in 0..CHANNELS {
            for sample in [PrcFmt::NAN, PrcFmt::INFINITY, PrcFmt::NEG_INFINITY] {
                let mut guard = guard();
                let mut input = chunk();
                input.waveforms[channel][FRAMES - 1] = sample;
                guard.process(&mut input);
                assert_eq!(guard.fault.trip.unwrap().reason, Reason::Nonfinite);
                assert!(input.waveforms.iter().flatten().all(|s| *s == 0.0));
            }
        }
    }

    #[test]
    fn known_safe_samples_forward_exactly() {
        let mut guard = guard();
        let mut input = chunk();
        guard.process(&mut input);
        assert_eq!(input.waveforms, chunk().waveforms);
        assert!(guard.fault.trip.is_none());
        // Alternating 0.1/0.0 samples have mean square 0.005 on every channel.
        let stamp = input.protection.as_ref().unwrap();
        assert!(stamp.rms_power.iter().all(|p| (*p - 0.005).abs() < 1e-15));
        let record = guard.shared.guard_snapshot().unwrap().record;
        assert_eq!(record.silent_frames, 0);
        assert_eq!(record.last_active_end_frame, stamp.frame_end);
    }

    #[test]
    fn validate_config_enforces_complete_guard_admission() {
        let conf = configuration();
        config::validate_config(conf.clone(), None).unwrap();
        let base = serde_json::to_value(conf).unwrap();
        let mut invalid = Vec::new();
        let mut missing = base.clone();
        missing["pipeline"] = serde_json::json!([]);
        invalid.push(("missing", missing));
        let mut bypassed = base.clone();
        bypassed["pipeline"][0]["bypassed"] = serde_json::json!(true);
        invalid.push(("bypassed", bypassed));
        let mut repeated = base.clone();
        repeated["pipeline"]
            .as_array_mut()
            .unwrap()
            .push(base["pipeline"][0].clone());
        invalid.push(("repeated", repeated));
        for (name, step, definitions) in [
            (
                "trailing filter",
                serde_json::json!({"type": "Filter", "names": ["gain"]}),
                serde_json::json!({"filters": {"gain": {"type": "Gain", "parameters": {"gain": 6}}}}),
            ),
            (
                "trailing mixer",
                serde_json::json!({"type": "Mixer", "name": "mix"}),
                serde_json::json!({"mixers": {"mix": {"channels": {"in": 24, "out": 24}, "mapping": []}}}),
            ),
            (
                "trailing processor",
                serde_json::json!({"type": "Processor", "name": "comp"}),
                serde_json::json!({"processors": {"comp": {"type": "Compressor", "parameters": {
                    "channels": 24, "attack": 0.01, "release": 0.1, "threshold": -20,
                    "factor": 2, "makeup_gain": 0}}}}),
            ),
        ] {
            let mut nonfinal = base.clone();
            for (key, value) in definitions.as_object().unwrap() {
                if key == "processors" {
                    nonfinal[key]
                        .as_object_mut()
                        .unwrap()
                        .extend(value.as_object().unwrap().clone());
                } else {
                    nonfinal[key] = value.clone();
                }
            }
            nonfinal["pipeline"].as_array_mut().unwrap().push(step);
            invalid.push((name, nonfinal));
        }
        for (name, key, value) in [
            ("sample rate", "samplerate", 44100),
            ("chunk size", "chunksize", 128),
        ] {
            let mut wrong_shape = base.clone();
            wrong_shape["devices"][key] = serde_json::json!(value);
            invalid.push((name, wrong_shape));
        }
        let mut wrong_backend = base.clone();
        wrong_backend["devices"]["playback"]["device"] = serde_json::json!("plughw:D624,0");
        invalid.push(("backend", wrong_backend));
        let mut wrong_format = base.clone();
        wrong_format["devices"]["playback"]["format"] = serde_json::json!("S16_LE");
        invalid.push(("format", wrong_format));
        for (name, field, channels) in [
            (
                "reordered outputs",
                "output_channels",
                (0..24).rev().collect::<Vec<_>>(),
            ),
            ("remapped tweeters", "tweeter_channels", vec![2, 3]),
            ("reordered tweeters", "tweeter_channels", vec![1, 0]),
        ] {
            let mut wrong_map = base.clone();
            wrong_map["processors"]["guard"]["parameters"][field] = serde_json::json!(channels);
            invalid.push((name, wrong_map));
        }
        let mut accepted = Vec::new();
        for (name, value) in invalid {
            let conf = serde_json::from_value(value).unwrap();
            if config::validate_config(conf, None).is_ok() {
                accepted.push(name);
            }
        }
        assert!(accepted.is_empty(), "invalid graphs accepted: {accepted:?}");
    }

    #[test]
    fn incomplete_coverage_is_not_silence_or_protected_progress() {
        let mut guard = guard();
        let mut input = chunk();
        input.waveforms[1].clear();
        guard.process(&mut input);
        assert_eq!(guard.fault.trip.unwrap().reason, Reason::Coverage);
        assert_eq!(guard.stream.frames, 0);
        assert!(input.protection.is_none());
        assert!(input.waveforms.iter().flatten().all(|s| *s == 0.0));
    }

    #[test]
    fn available_peak_wins_over_missing_other_channel() {
        let mut guard = guard();
        let mut input = chunk();
        input.waveforms[0][2] = 1.0;
        input.waveforms[3].clear();
        guard.process(&mut input);
        assert_eq!(guard.fault.trip.unwrap().reason, Reason::Peak);
        assert!(input.protection.is_none());
    }

    #[test]
    fn retired_stamps_cannot_be_submitted() {
        let mut guard = guard();
        let mut input = chunk();
        guard.process(&mut input);
        let stamp = input.protection.unwrap();
        assert!(stamp.is_current());
        guard.shared.epoch.fetch_add(1, Ordering::SeqCst);
        assert!(!stamp.is_current());
    }

    #[test]
    fn reading_the_published_record_does_not_consume_the_trip() {
        let mut guard = guard();
        let mut input = chunk();
        input.waveforms[0][0] = 1.0;
        guard.process(&mut input);
        let first = guard.shared.guard_snapshot().unwrap();
        assert_eq!(first.record.trip, guard.fault.trip);
        assert_eq!(first.record.trip.unwrap().reason, Reason::Peak);
        assert_eq!(guard.shared.guard_snapshot(), Some(first));
    }

    #[test]
    fn ordinary_mute_ramps_and_interruptions_match_original_volume() {
        use crate::filters::basicfilters::Volume;
        for initial_mute in [false, true] {
            let mut conf = configuration();
            conf.devices.volume_ramp_time = Some(400.0);
            let params = Arc::new(crate::ProcessingParameters::new(
                &[0.0; 5],
                &[initial_mute; 5],
            ));
            let mut reference = Volume::new(
                "reference",
                400.0,
                0.0,
                0.0,
                initial_mute,
                FRAMES,
                SAMPLE_RATE,
                params.clone(),
                0,
            );
            let mut pipeline = crate::pipeline::Pipeline::from_config(conf, params.clone());
            for block in 0..420 {
                match block {
                    10 | 60 | 160 | 340 => params.set_mute(0, true),
                    30 | 140 | 180 => params.set_mute(0, false),
                    45 => params.set_target_volume(0, -6.0),
                    260 => params.set_target_volume(0, 0.0),
                    _ => (),
                }
                let mut expected = chunk();
                reference.process_chunk(&mut expected);
                let actual = pipeline.process_chunk(chunk());
                assert_eq!(
                    actual.waveforms, expected.waveforms,
                    "block {block}, initial mute {initial_mute}"
                );
                if block == 10 && !initial_mute {
                    assert!(actual.waveforms[0][0] > 0.09);
                    assert!(actual.waveforms[0][FRAMES - 1] < actual.waveforms[0][0]);
                }
            }
        }
    }

    #[test]
    fn mute_cannot_qualify_an_unintegrated_guard_or_hide_unmuted_fault() {
        let mut conf = configuration();
        conf.devices.volume_ramp_time = Some(400.0);
        let params = Arc::new(crate::ProcessingParameters::new(&[0.0; 5], &[true; 5]));
        let mut pipeline = crate::pipeline::Pipeline::from_config(conf.clone(), params.clone());
        let mut guard = Guard::new(params.protection.clone());
        guard.install(&config::validate_config(conf, None).unwrap());
        for block in 0..160 {
            if block == 80 {
                params.set_mute(0, false);
            }
            let mut input = chunk();
            input.waveforms[0].fill(2.0);
            let mut candidate = pipeline.process_chunk(input);
            if block < 80 {
                assert!(candidate.waveforms[0].iter().all(|s| *s == 0.0));
            }
            guard.process(&mut candidate);
            assert!(!guard.qualification.is_active());
            assert!(candidate.waveforms.iter().flatten().all(|s| *s == 0.0));
        }
        assert_eq!(guard.trip().unwrap().reason, Reason::Peak);
        assert!(!params.is_mute(0)); // The external request, not a guard/reset side effect.
    }

    #[test]
    fn final_candidate_faults_override_an_in_progress_user_fade() {
        for offset in [0, FRAMES - 1] {
            for channel in [0, 1] {
                for value in [
                    1.0,
                    -1.0,
                    PrcFmt::NAN,
                    PrcFmt::INFINITY,
                    PrcFmt::NEG_INFINITY,
                ] {
                    let mut conf = configuration();
                    conf.devices.volume_ramp_time = Some(400.0);
                    let params = Arc::new(crate::ProcessingParameters::new(&[0.0; 5], &[false; 5]));
                    let mut pipeline = crate::pipeline::Pipeline::from_config(conf, params.clone());
                    let mut guard = guard();
                    let mut previous = pipeline.process_chunk(chunk());
                    guard.process(&mut previous);
                    params.set_mute(0, true);
                    let mut candidate = pipeline.process_chunk(chunk());
                    assert!(candidate.waveforms[0][FRAMES - 2] > 0.0);
                    // Inject after processing so neither mute nor crossover can remove it.
                    candidate.waveforms[channel][offset] = value;
                    guard.process(&mut candidate);
                    assert!(candidate.waveforms.iter().flatten().all(|s| *s == 0.0));
                    let trip = guard.trip().unwrap();
                    assert_eq!(trip.frame, (FRAMES + offset) as u64);
                    assert_eq!(
                        trip.reason,
                        if value.is_finite() {
                            Reason::Peak
                        } else {
                            Reason::Nonfinite
                        }
                    );
                    let mut following = pipeline.process_chunk(chunk());
                    guard.process(&mut following);
                    assert!(following.waveforms.iter().flatten().all(|s| *s == 0.0));
                    assert_eq!(guard.trip(), Some(trip));
                }
            }
        }
    }
}
