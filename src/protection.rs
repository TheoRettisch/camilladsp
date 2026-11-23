//! Final candidate-sample admission for the protection fork.
//!
//! This owner is deliberately separate from the rebuildable processing graph:
//! graph changes cannot clear a trip. Only stamped guard output may enter ALSA.

use crate::audiochunk::AudioChunk;
use crate::{Res, config};
mod plan;
use plan::Plan;
pub use plan::{Id, Instance, Parameters};
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
#[cfg(test)]
mod signed_dc_tests;
mod state;
mod tweeter_lf;
#[cfg(test)]
mod tweeter_lf_tests;
pub mod wire;
use control::{Command, Control, Mode, Operation};
use record::AtomicRecord;
pub use record::GuardRecord;
use state::{
    CandidateObservation, DetectorState, FaultState, OutputActivity, Qualification, StreamState,
};

/// Fixed playback geometry for global safety coverage and forwarded telemetry.
pub const OUTPUT_CHANNELS: [usize; 24] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
];

pub const CHANNELS: usize = OUTPUT_CHANNELS.len();
pub const FRAMES: usize = 256;
pub const SAMPLE_RATE: usize = 48_000;
// Reviewed parity fixture window. Runtime admission uses the compiled Q.
#[cfg(test)]
pub const QUALIFICATION_FRAMES: u64 = 2 * SAMPLE_RATE as u64;

fn bump(value: u64) -> u64 {
    assert!(value < i64::MAX as u64, "Protection counter exhausted");
    value + 1
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
    plan: Box<Plan>,
    identity: identity::Identity,
    profile: serde_json::Value,
}

impl AdmittedGuard {
    pub(crate) fn new(conf: &config::Configuration) -> Res<Self> {
        let (plan, profile) = admission(conf)?.compile()?;
        let identity = identity::prepare(conf, &profile)?;
        Ok(Self {
            plan: Box::new(plan),
            profile,
            identity,
        })
    }
    pub(crate) fn binding(&self) -> [u64; 4] {
        self.plan.binding
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
    Numeric = 9,
    PositiveDc = 10,
    NegativeDc = 11,
    TweeterLf = 12,
}

impl Reason {
    // Encode DC polarity in the durable reason word so other faults can never
    // acquire a polarity. Both signs share one policy slot and tie rank.
    fn polarity(self) -> Option<i8> {
        match self {
            Self::PositiveDc => Some(1),
            Self::NegativeDc => Some(-1),
            _ => None,
        }
    }

    fn policy_slot(self) -> usize {
        match self {
            Self::NegativeDc => Self::PositiveDc as usize,
            _ => self as usize,
        }
    }

    pub fn kind(self) -> &'static str {
        match self {
            Self::Peak => "peak",
            Self::Nonfinite => "nonfinite",
            Self::Coverage => "coverage",
            Self::Rms => "rms",
            Self::ShortPower => "short_power",
            Self::LongPower => "long_power",
            Self::Tone => "crest_tone",
            Self::DcFrozen => "frozen",
            Self::Numeric => "numeric",
            Self::TweeterLf => "tweeter_lf",
            Self::PositiveDc | Self::NegativeDc => "signed_dc",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trip {
    pub generation: u64,
    pub reason: Reason,
    pub origin_profile: [u64; 4],
    pub policy_id: Id,
    pub destination_id: Id,
    pub channel: Option<usize>,
    pub frame: u64,
    pub at_ns: u64,
}

#[derive(Debug)]
pub struct Shared {
    binding: std::sync::OnceLock<[u64; 4]>,
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
    pipeline_wakeup: crossbeam_channel::Sender<()>,
    pipeline_wakeup_rx: crossbeam_channel::Receiver<()>,
    commands: parking_lot::Mutex<Option<crossbeam_channel::Sender<Command>>>,
}

impl Default for Shared {
    fn default() -> Self {
        let (backend_wakeup, backend_wakeup_rx) = crossbeam_channel::bounded(1);
        let (pipeline_wakeup, pipeline_wakeup_rx) = crossbeam_channel::bounded(1);
        Self {
            binding: Default::default(),
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
            pipeline_wakeup,
            pipeline_wakeup_rx,
            commands: parking_lot::Mutex::new(None),
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
    pub fn check_binding(&self, conf: &config::PreparedConfig) -> Res<()> {
        if self
            .binding
            .get()
            .is_some_and(|b| *b != conf.guard().binding())
        {
            return Err(config::ConfigError::new(
                "Protection destination memory binding requires a cold start",
            )
            .into());
        }
        Ok(())
    }
    pub fn bind_run(&self, conf: &config::PreparedConfig) -> Res<()> {
        self.binding.get_or_init(|| conf.guard().binding());
        self.check_binding(conf)
    }

    /// Internal bounded mailbox, not an externally admitted wire operation.
    pub fn submit(&self, command: Command) -> Result<(), crossbeam_channel::TrySendError<Command>> {
        self.commands
            .lock()
            .as_ref()
            .ok_or(crossbeam_channel::TrySendError::Disconnected(command))?
            .try_send(command)
    }

    pub(crate) fn open_commands(&self) -> crossbeam_channel::Receiver<Command> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        *self.commands.lock() = Some(tx);
        rx
    }

    pub(crate) fn close_commands(&self) {
        self.commands.lock().take();
    }

    pub(crate) fn backend_wakeups(&self) -> &crossbeam_channel::Receiver<()> {
        &self.backend_wakeup_rx
    }

    pub(crate) fn pipeline_wakeups(&self) -> &crossbeam_channel::Receiver<()> {
        &self.pipeline_wakeup_rx
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
    configuration: Option<RuntimePlan>,
    stream: StreamState,
    detector: DetectorState,
    fault: FaultState,
    control: Control,
    qualification: Qualification,
    candidate: CandidateObservation,
    activity: OutputActivity,
}

#[derive(Debug)]
struct RuntimePlan {
    policy: Plan,
    identity: identity::Identity,
}

/// Facts about the original candidate; no sample copies or retained evidence.
#[derive(Clone, Copy)]
struct CandidateAssessment {
    shape_valid: bool,
    finite: bool,
    peaks: [f64; CHANNELS],
    powers: [f64; CHANNELS],
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

    pub fn install(&mut self, conf: &config::PreparedConfig) -> bool {
        if self.shared.bind_run(conf).is_err() {
            return false;
        }
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
        self.configuration = Some(RuntimePlan {
            policy: (*conf.guard().plan).clone(),
            identity: conf.identity(),
        });
        self.publish();
        // Playback must retire the old PCM generation even when capture sends
        // no more audio or Pause messages. Coalesce without blocking this owner.
        let _ = self.shared.pipeline_wakeup.try_send(());
        true
    }

    pub fn trip(&self) -> Option<Trip> {
        self.fault.trip
    }

    fn latch(&mut self, reason: Reason, channel: Option<usize>, frame: u64) {
        self.latch_target(reason, channel, frame, Id::default(), Id::default());
    }

    fn latch_target(
        &mut self,
        reason: Reason,
        channel: Option<usize>,
        frame: u64,
        policy_id: Id,
        destination_id: Id,
    ) {
        let origin = self
            .configuration
            .as_ref()
            .map(|c| c.identity.profile)
            .unwrap_or_default();
        self.fault
            .latch(reason, channel, frame, origin, policy_id, destination_id);
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
            prearm_veto: self.detector.prearm_veto,
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
        let masked =
            !self.qualification.is_active() || self.fault.trip.is_some() || !assessment.shape_valid;
        if masked {
            for waveform in &mut chunk.waveforms {
                waveform.fill(0.0);
            }
            chunk.maxval = 0.0;
            chunk.minval = 0.0;
        }
        // Activity and RMS describe the forwarded chunk, after containment.
        let forwarded = self.account_forwarded_output(
            if masked {
                [0.0; CHANNELS]
            } else {
                assessment.powers
            },
            assessment.shape_valid,
        );
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
        let mut peaks = [0.0_f64; CHANNELS];
        let mut powers = [0.0; CHANNELS];
        // Assess available instantaneous evidence before a missing-channel fault.
        for (channel, waveform) in chunk.waveforms.iter().take(CHANNELS).enumerate() {
            for (offset, sample) in waveform.iter().take(FRAMES).enumerate() {
                peaks[channel] = peaks[channel].max(sample.abs());
                powers[channel] += sample * sample;
                let frame = self.stream.frames + offset as u64;
                if !sample.is_finite() {
                    finite = false;
                    self.latch(Reason::Nonfinite, Some(channel), frame);
                } else if let Some(config) = &self.configuration
                    && self.fault.trip.is_none()
                    && sample.abs() >= config.policy.peak_floor[channel]
                {
                    let selected = config.policy.destinations[..config.policy.count]
                        .iter()
                        .filter(|d| d.lane == channel && d.peak.is_some_and(|l| sample.abs() >= l))
                        .map(|d| (d.policies[Reason::Peak as usize], d.id))
                        .min();
                    if let Some((policy, dest)) = selected {
                        self.latch_target(Reason::Peak, Some(channel), frame, policy, dest);
                    }
                }
            }
        }
        for power in &mut powers {
            *power /= FRAMES as f64;
        }
        if !shape_valid {
            self.latch(Reason::Coverage, None, self.stream.frames);
        }
        CandidateAssessment {
            shape_valid,
            finite,
            peaks,
            powers,
        }
    }

    /// Detector history and qualification use candidates, never masked output.
    fn assess_policy_and_qualify(&mut self, chunk: &AudioChunk, assessment: CandidateAssessment) {
        let CandidateAssessment {
            shape_valid,
            finite,
            peaks,
            powers,
        } = assessment;
        self.observe_main_volume(chunk, shape_valid);
        self.detector.prearm_veto = false;
        if shape_valid && finite {
            let plan = &self.configuration.as_ref().unwrap().policy;
            let mut first = None;
            let mut lf_powers = [None; CHANNELS];
            for (lane, q) in powers.into_iter().enumerate() {
                if !q.is_finite() {
                    let key = (0, lane, 0, Id::default(), Id::default());
                    if first.is_none_or(|(old, _)| key < old) {
                        first = Some((key, Reason::Numeric));
                    }
                    continue;
                }
                let peak = peaks[lane];
                let mut signed_sum = None;
                for (i, d) in plan.destinations[..plan.count]
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| d.lane == lane)
                {
                    let history = &mut self.detector.history[i];
                    let prearm = self.control.mode == Mode::Prearm;
                    let mut result =
                        d.policy
                            .assess(history, peak, q, prearm, self.candidate.observable);
                    if let Some(dc) = d.policy.signed_dc {
                        let sum = signed_sum.get_or_insert_with(|| {
                            policy::CompensatedSum::block(&chunk.waveforms[lane])
                        });
                        let assessment = dc.observe(&mut history.signed_dc, *sum, prearm);
                        result.reason = result.reason.or(assessment.reason);
                        result.veto |= assessment.veto;
                    }
                    self.detector.prearm_veto |= result.veto;
                    // Frozen-signature quantization can produce a numeric fault.
                    let existing = result.reason.map(|reason| {
                        let kind = if reason == Reason::Numeric {
                            Reason::DcFrozen
                        } else {
                            reason
                        };
                        (reason, kind)
                    });
                    let lf_reason = d.policy.tweeter_lf.and_then(|lf| {
                        let power = lf_powers[lf.source].get_or_insert_with(|| {
                            self.detector.lf_filters[lf.source]
                                .block_power(&lf.coefficients, &chunk.waveforms[lane])
                        });
                        let assessment = lf.observe(&mut history.lf_window, *power, prearm);
                        self.detector.prearm_veto |= assessment.veto;
                        assessment.reason
                    });
                    for (reason, kind) in [existing, lf_reason.map(|r| (r, Reason::TweeterLf))]
                        .into_iter()
                        .flatten()
                    {
                        let numeric = reason == Reason::Numeric;
                        let policy = d.policies[kind.policy_slot()];
                        let key = (
                            if numeric { 0 } else { 1 },
                            lane,
                            if numeric { 1 } else { reason.policy_slot() },
                            policy,
                            d.id,
                        );
                        if first.is_none_or(|(old, _)| key < old) {
                            first = Some((key, reason));
                        }
                    }
                }
            }
            if let Some(((_, lane, _, policy, dest), reason)) = first {
                self.latch_target(
                    reason,
                    Some(lane),
                    self.stream.frames + FRAMES as u64,
                    policy,
                    dest,
                );
            }
        } else {
            self.detector.break_continuity();
        }
        if self.detector.prearm_veto {
            // A veto restarts a full window, including after local admission
            // while the parent's independent checks are still qualifying.
            self.qualification.restart_window();
        } else {
            self.qualify();
        }
    }

    fn account_forwarded_output(
        &mut self,
        rms_power: [f64; CHANNELS],
        shape_valid: bool,
    ) -> ForwardedOutput {
        let frame_begin = self.stream.frames;
        if shape_valid {
            self.stream.frames = self.stream.frames.checked_add(FRAMES as u64).unwrap();
            assert!(self.stream.frames <= i64::MAX as u64);
        }
        if shape_valid {
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
    use crate::PrcFmt;

    pub(crate) fn parameters() -> Parameters {
        let fixture: config::Configuration =
            yaml_serde::from_str(include_str!("../testdata/protection-parity.yml")).unwrap();
        admission(&fixture).unwrap()
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
                input.waveforms[channel][offset] =
                    guard.configuration.as_ref().unwrap().policy.peak_floor[channel];
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
            if serde_json::from_value(value)
                .is_ok_and(|conf| config::validate_config(conf, None).is_ok())
            {
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
