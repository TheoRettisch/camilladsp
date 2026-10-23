//! Final candidate-sample admission for the protection fork.
//!
//! This owner is deliberately separate from the rebuildable processing graph:
//! graph changes cannot clear a trip. Only stamped guard output may enter ALSA.

use crate::audiochunk::AudioChunk;
use crate::{PrcFmt, Res, config};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod control;
pub mod identity;
#[cfg(test)]
mod lifecycle_tests;
pub mod playback;
pub mod wire;
use control::{Command, Control, Mode, Operation, Receipt, State};

pub const CHANNELS: usize = 24;
pub const FRAMES: usize = 256;
pub const SAMPLE_RATE: usize = 48_000;
// Initial policy's local window. Full sustained-policy implementation remains
// required before production activation can be enabled.
pub const QUALIFICATION_FRAMES: u64 = 2 * SAMPLE_RATE as u64;

fn bump(value: u64) -> u64 {
    assert!(value < i64::MAX as u64, "Protection counter exhausted");
    value + 1
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    pub output_channels: [usize; CHANNELS],
    pub tweeter_channels: [usize; 2],
    pub max_allowed_peak_db: f64,
}

impl Parameters {
    pub fn validate(&self) -> Res<()> {
        if self.output_channels != std::array::from_fn(|i| i)
            || self.tweeter_channels != [0, 1]
            || !self.max_allowed_peak_db.is_finite()
            || self.max_allowed_peak_db > 0.0
            || self.max_allowed_peak_db < -120.0
        {
            return Err(
                config::ConfigError::new("Invalid protection channel map or peak limit").into(),
            );
        }
        Ok(())
    }
}

/// Validate before constructing or updating any executable graph. The declared
/// final marker is compiled as the processing owner's final Guard operation.
pub fn admission(conf: &config::Configuration) -> Res<Parameters> {
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

/// Every element is atomic; the sequence counter does not excuse non-atomic
/// seqlock reads. SeqCst gives a simple auditable ordering for this small record.
#[derive(Debug)]
struct AtomicRecord<const N: usize> {
    sequence: AtomicU64,
    values: [AtomicU64; N],
}

impl<const N: usize> Default for AtomicRecord<N> {
    fn default() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            values: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl<const N: usize> AtomicRecord<N> {
    // There is exactly one audio owner per record. Readers never write or wait.
    fn publish(&self, values: [u64; N]) {
        let sequence = self.sequence.load(Ordering::SeqCst);
        assert!(
            sequence < i64::MAX as u64 - 2,
            "Protection sequence exhausted"
        );
        self.sequence.store(sequence + 1, Ordering::SeqCst);
        for (target, value) in self.values.iter().zip(values) {
            target.store(value, Ordering::SeqCst);
        }
        self.sequence.store(sequence + 2, Ordering::SeqCst);
    }

    fn read(&self) -> Option<(u64, [u64; N])> {
        for _ in 0..3 {
            if let Some(record) = self.read_once() {
                return Some(record);
            }
        }
        None
    }

    fn read_once(&self) -> Option<(u64, [u64; N])> {
        let before = self.sequence.load(Ordering::SeqCst);
        if before & 1 != 0 {
            return None;
        }
        let values = std::array::from_fn(|i| self.values[i].load(Ordering::SeqCst));
        (before == self.sequence.load(Ordering::SeqCst)).then_some((before, values))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum Reason {
    Peak = 1,
    Nonfinite = 2,
    Coverage = 3,
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
    guard_record: AtomicRecord<43>,
    playback_record: AtomicRecord<43>,
    host: Option<wire::Host>,
    conversion_count: AtomicU64,
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
            backend_wakeup,
            backend_wakeup_rx,
            commands,
            command_rx,
        }
    }
}

/// Coherent processing-owner record. Playback consumption has a separate writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub sequence: u64,
    pub pipeline: u64,
    pub backend: u64,
    pub reset: u64,
    pub frame_floor: u64,
    pub frames: u64,
    pub processing_valid: bool,
    pub source_begin_ns: u64,
    pub source_end_ns: u64,
    pub state: State,
    pub candidate_observable: bool,
    pub qualified_frames: u64,
    pub attempt_required: bool,
    pub trip_generation: u64,
    pub trip: Option<Trip>,
    pub control: Control,
    pub identity: identity::Identity,
    pub final_guard: bool,
    pub activity_generation: u64,
    pub silent_frames: u64,
    pub last_active_end_frame: u64,
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
        let (sequence, v) = self.guard_record.read_once()?;
        if sequence == 0
            || v[0] != epoch
            || v[9] != reset
            || v[29] != backend
            || epoch != self.epoch.load(Ordering::SeqCst)
            || reset != self.reset.load(Ordering::SeqCst)
            || backend != self.backend.load(Ordering::SeqCst)
        {
            return None;
        }
        let trip = match v[5] {
            0 => None,
            reason => Some(Trip {
                generation: v[4],
                reason: match reason {
                    1 => Reason::Peak,
                    2 => Reason::Nonfinite,
                    3 => Reason::Coverage,
                    _ => return None,
                },
                channel: v[6].checked_sub(1).map(|c| c as usize),
                frame: v[7],
                at_ns: v[8],
            }),
        };
        let result = match v[22] {
            0 => None,
            1 => Some(control::Result::Applied),
            2 => Some(control::Result::Invalid),
            3 => Some(control::Result::StateMismatch),
            4 => Some(control::Result::NotQualified),
            _ => return None,
        };
        let owner = (u128::from(v[15]) << 64) | u128::from(v[16]);
        Some(Snapshot {
            sequence,
            pipeline: v[0],
            backend: v[29],
            frames: v[1],
            source_begin_ns: v[2],
            source_end_ns: v[3],
            trip_generation: v[4],
            trip,
            reset: v[9],
            frame_floor: v[10],
            state: match v[11] {
                0 => State::Inactive,
                1 => State::Qualifying,
                2 => State::Active,
                3 => State::Tripped,
                _ => return None,
            },
            candidate_observable: v[12] != 0,
            qualified_frames: v[13],
            attempt_required: v[14] != 0,
            processing_valid: v[28] != 0,
            control: Control {
                owner: (owner != 0).then_some(owner),
                revision: v[17],
                parent_reset_generation: v[18],
                qualification_id: v[19],
                mode: if v[20] == 0 {
                    Mode::Prearm
                } else {
                    Mode::Armed
                },
                last_command: result.map(|result| Receipt {
                    command_id: v[21],
                    result,
                    pipeline: v[23],
                    backend: v[30],
                    reset: v[24],
                    revision: v[25],
                    parent_reset_generation: v[26],
                    qualification_id: v[27],
                }),
                // Not exported: it is an owner-side replay fence, not status.
                command_high_water: 0,
            },
            identity: identity::Identity {
                graph: v[31..35].try_into().unwrap(),
                profile: v[35..39].try_into().unwrap(),
            },
            final_guard: v[39] != 0,
            activity_generation: v[40],
            silent_frames: v[41],
            last_active_end_frame: v[42],
        })
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

#[derive(Debug)]
pub struct Guard {
    pub(crate) shared: Arc<Shared>,
    epoch: u64,
    backend: u64,
    parameters: Option<Parameters>,
    peak_limit: PrcFmt,
    frames: u64,
    trip: Option<Trip>,
    trip_generation: u64,
    reset: u64,
    frame_floor: u64,
    processing_valid: bool,
    source_begin_ns: u64,
    source_end_ns: u64,
    control: Control,
    candidate_observable: bool,
    qualified_frames: u64,
    qualification_baseline: bool,
    attempt_required: bool,
    last_main_mute: Option<bool>,
    // No production caller supplies a context until wire/backend/policy and
    // transition acceptance are complete. Tests exercise this same lifecycle.
    pub(crate) active: bool,
    #[cfg(test)]
    activation_inhibited: bool,
    identity: Option<identity::Identity>,
    activity_generation: u64,
    silent_frames: u64,
    last_active_end_frame: u64,
}

impl Guard {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            backend: shared.backend.load(Ordering::SeqCst),
            shared,
            epoch: 0,
            parameters: None,
            peak_limit: 0.0,
            frames: 0,
            trip: None,
            trip_generation: 0,
            reset: 0,
            frame_floor: 0,
            processing_valid: false,
            source_begin_ns: 0,
            source_end_ns: 0,
            control: Control::default(),
            candidate_observable: false,
            qualified_frames: 0,
            qualification_baseline: false,
            attempt_required: true,
            last_main_mute: None,
            active: false,
            #[cfg(test)]
            activation_inhibited: true,
            identity: None,
            activity_generation: 0,
            silent_frames: 0,
            last_active_end_frame: 0,
        }
    }

    pub fn install(&mut self, conf: &config::Configuration) -> Res<()> {
        self.sync_backend();
        // Retire queued stamps even when the replacement is invalid.
        self.epoch = self
            .shared
            .epoch
            .fetch_add(1, Ordering::SeqCst)
            .checked_add(1)
            .unwrap();
        assert!(self.epoch <= i64::MAX as u64);
        self.parameters = None;
        self.identity = None;
        self.invalidate_activity();
        self.frames = 0;
        self.frame_floor = 0;
        self.processing_valid = false;
        self.source_begin_ns = 0;
        self.source_end_ns = 0;
        self.candidate_observable = false;
        self.last_main_mute = None;
        self.invalidate_qualification();
        self.publish();
        let parameters = admission(conf)?;
        self.peak_limit = 10.0_f64.powf(parameters.max_allowed_peak_db / 20.0) as PrcFmt;
        self.parameters = Some(parameters);
        self.identity = conf.protection_identity;
        self.publish();
        Ok(())
    }

    // Fixed build-time barrier until the separately reviewed activation gate.
    // Only the test build carries a bypass field; no production option exists.
    fn activation_inhibited(&self) -> bool {
        #[cfg(test)]
        {
            self.activation_inhibited
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    pub fn trip(&self) -> Option<Trip> {
        self.trip
    }

    fn latch(&mut self, reason: Reason, channel: Option<usize>, frame: u64) {
        if self.trip.is_none() {
            self.trip_generation = bump(self.trip_generation);
            self.trip = Some(Trip {
                generation: self.trip_generation,
                reason,
                channel,
                frame,
                at_ns: monotonic_ns(),
            });
        }
        self.invalidate_qualification();
    }

    fn publish(&self) {
        let (_, reason, channel, frame, at_ns) = self.trip.map_or((0, 0, 0, 0, 0), |t| {
            (
                t.generation,
                t.reason as u64,
                t.channel.map_or(0, |c| c as u64 + 1),
                t.frame,
                t.at_ns,
            )
        });
        let owner = self.control.owner.unwrap_or(0);
        let receipt = self.control.last_command;
        let identity = self.identity.unwrap_or_default();
        self.shared.guard_record.publish([
            self.epoch,
            self.frames,
            self.source_begin_ns,
            self.source_end_ns,
            self.trip_generation,
            reason,
            channel,
            frame,
            at_ns,
            self.reset,
            self.frame_floor,
            self.state() as u64,
            self.candidate_observable as u64,
            self.qualified_frames,
            self.attempt_required as u64,
            (owner >> 64) as u64,
            owner as u64,
            self.control.revision,
            self.control.parent_reset_generation,
            self.control.qualification_id,
            self.control.mode as u64,
            receipt.map_or(0, |r| r.command_id),
            receipt.map_or(0, |r| r.result as u64),
            receipt.map_or(0, |r| r.pipeline),
            receipt.map_or(0, |r| r.reset),
            receipt.map_or(0, |r| r.revision),
            receipt.map_or(0, |r| r.parent_reset_generation),
            receipt.map_or(0, |r| r.qualification_id),
            self.processing_valid as u64,
            self.backend,
            receipt.map_or(0, |r| r.backend),
            identity.graph[0],
            identity.graph[1],
            identity.graph[2],
            identity.graph[3],
            identity.profile[0],
            identity.profile[1],
            identity.profile[2],
            identity.profile[3],
            (self.parameters.is_some() && self.identity.is_some()) as u64,
            self.activity_generation,
            self.silent_frames,
            self.last_active_end_frame,
        ]);
    }

    pub fn process(&mut self, chunk: &mut AudioChunk) {
        self.process_with_command(chunk, None);
    }

    pub fn process_with_command(&mut self, chunk: &mut AudioChunk, command: Option<Command>) {
        self.sync_backend();
        let armed = command.filter(|c| c.operation == Operation::Context(Mode::Armed));
        if let Some(command) = command.filter(|c| c.operation != Operation::Context(Mode::Armed)) {
            self.apply_control(command, false);
        }
        chunk.protection = None;
        let begin = monotonic_ns();
        let shape_valid = self.parameters.is_some()
            && chunk.frames == FRAMES
            && chunk.valid_frames == FRAMES
            && chunk.channels == CHANNELS
            && chunk.waveforms.len() == CHANNELS
            && chunk.waveforms.iter().all(|w| w.len() == FRAMES);
        // Assess available instantaneous evidence before a missing-channel fault.
        for (channel, waveform) in chunk.waveforms.iter().take(CHANNELS).enumerate() {
            for (offset, sample) in waveform.iter().take(FRAMES).enumerate() {
                let reason = if !sample.is_finite() {
                    Some(Reason::Nonfinite)
                } else if channel < 2
                    && self.parameters.is_some()
                    && sample.abs() >= self.peak_limit
                {
                    Some(Reason::Peak)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    self.latch(reason, Some(channel), self.frames + offset as u64);
                }
            }
        }
        if !shape_valid {
            self.latch(Reason::Coverage, None, self.frames);
        }
        self.observe_main_volume(chunk, shape_valid);
        if let Some(command) = armed {
            // Current-chunk hazards and qualification precede armed receipt.
            self.apply_control(command, true);
        }
        if self.activation_inhibited() || !self.active || self.trip.is_some() || !shape_valid {
            for waveform in &mut chunk.waveforms {
                waveform.fill(0.0);
            }
            chunk.maxval = 0.0;
            chunk.minval = 0.0;
        }
        let frame_begin = self.frames;
        if shape_valid {
            self.frames = self.frames.checked_add(FRAMES as u64).unwrap();
            assert!(self.frames <= i64::MAX as u64);
        }
        let mut rms_power = [0.0; CHANNELS];
        if shape_valid {
            for (rms, waveform) in rms_power.iter_mut().zip(&chunk.waveforms) {
                *rms = waveform.iter().map(|x| x * x).sum::<PrcFmt>() as f64 / FRAMES as f64;
            }
            if rms_power.iter().any(|power| *power > 1e-7) {
                self.silent_frames = 0;
                self.last_active_end_frame = self.frames;
            } else {
                self.silent_frames = self.silent_frames.checked_add(FRAMES as u64).unwrap();
            }
        } else {
            self.invalidate_activity();
        }
        let end = monotonic_ns();
        self.processing_valid = shape_valid;
        self.source_begin_ns = if shape_valid { begin } else { 0 };
        self.source_end_ns = if shape_valid { end } else { 0 };
        self.publish();
        if shape_valid {
            chunk.protection = Some(Stamp {
                shared: self.shared.clone(),
                epoch: self.epoch,
                backend: self.backend,
                reset: self.reset,
                frame_begin,
                frame_end: self.frames,
                source_begin_ns: begin,
                source_end_ns: end,
                rms_power,
            });
        }
    }

    // Only the processing owner rebases protected frames. Playback retires its
    // atomic generation before any PCM recovery; old queued stamps then fail.
    fn sync_backend(&mut self) {
        let backend = self.shared.backend.load(Ordering::SeqCst);
        if backend != self.backend {
            self.backend = backend;
            self.frames = 0;
            self.frame_floor = 0;
            self.processing_valid = false;
            self.source_begin_ns = 0;
            self.source_end_ns = 0;
            self.candidate_observable = false;
            self.invalidate_activity();
            self.invalidate_qualification();
        }
    }

    pub fn backend_wakeup(&mut self) {
        self.sync_backend();
        self.publish();
    }

    fn invalidate_activity(&mut self) {
        self.activity_generation = bump(self.activity_generation);
        self.silent_frames = 0;
        self.last_active_end_frame = 0;
    }

    fn observe_main_volume(&mut self, chunk: &AudioChunk, shape_valid: bool) {
        self.candidate_observable =
            shape_valid && chunk.main_volume.is_some_and(|v| !v.muted && !v.opening);
        let Some(volume) = chunk.main_volume else {
            self.invalidate_qualification();
            return;
        };
        if self.last_main_mute == Some(true) && !volume.muted {
            // Must precede forwarding the first opening sample. A new prearm
            // attempt must be installed after this observed transition.
            self.invalidate_qualification();
        } else if self.last_main_mute == Some(false) && volume.muted && !self.active {
            self.invalidate_qualification();
        }
        self.last_main_mute = Some(volume.muted);
        if !self.candidate_observable {
            self.qualified_frames = 0;
            self.qualification_baseline = false;
            // An already active ordinary fade-out is still passed through.
            return;
        }
        if self.trip.is_some() || self.attempt_required || self.control.owner.is_none() {
            return;
        }
        if !self.active {
            if self.qualification_baseline {
                self.qualified_frames = self.qualified_frames.checked_add(FRAMES as u64).unwrap();
                if self.qualified_frames >= QUALIFICATION_FRAMES {
                    self.active = true;
                }
            } else {
                self.qualification_baseline = true;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn parameters() -> Parameters {
        Parameters {
            output_channels: std::array::from_fn(|i| i),
            tweeter_channels: [0, 1],
            max_allowed_peak_db: -0.5,
        }
    }

    fn guard() -> Guard {
        let mut guard = Guard::new(Arc::new(Shared::default()));
        guard.parameters = Some(parameters());
        guard.peak_limit = 10.0_f64.powf(-0.5 / 20.0) as PrcFmt;
        guard.active = true;
        guard.activation_inhibited = false;
        guard
    }

    pub(crate) fn chunk() -> AudioChunk {
        {
            let mut chunk =
                AudioChunk::new(vec![vec![0.1; FRAMES]; CHANNELS], 0.1, 0.1, FRAMES, FRAMES);
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
    fn invalid_replacement_retires_queue_without_clearing_trip() {
        let mut guard = Guard::new(Arc::new(Shared::default()));
        let conf = configuration();
        guard.install(&conf).unwrap();
        let mut input = chunk();
        input.waveforms[1][5] = 1.0;
        guard.process(&mut input);
        let stamp = input.protection.unwrap();
        let trip = guard.trip;
        let mut invalid = conf.clone();
        invalid.pipeline = None;
        assert!(guard.install(&invalid).is_err());
        assert!(!stamp.is_current());
        assert_eq!(guard.trip, trip);
        guard.install(&conf).unwrap();
        assert_eq!(guard.trip, trip);
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
                input.waveforms[channel][offset] = guard.peak_limit;
                guard.process(&mut input);
                assert!(input.waveforms.iter().flatten().all(|s| *s == 0.0));
                let trip = guard.trip.unwrap();
                assert_eq!(trip.reason, Reason::Peak);
                assert_eq!(trip.channel, Some(channel));
                assert_eq!(trip.frame, offset as u64);
                let mut safe = chunk();
                guard.process(&mut safe);
                assert!(safe.waveforms.iter().flatten().all(|s| *s == 0.0));
                assert_eq!(guard.trip, Some(trip));
                assert_eq!(guard.frames, 2 * FRAMES as u64);
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
                assert_eq!(guard.trip.unwrap().reason, Reason::Nonfinite);
                assert!(input.waveforms.iter().flatten().all(|s| *s == 0.0));
            }
        }
    }

    #[test]
    fn known_safe_samples_forward_exactly() {
        let mut guard = guard();
        let mut input = chunk();
        guard.process(&mut input);
        assert!(input.waveforms.iter().flatten().all(|s| *s == 0.1));
        assert!(guard.trip.is_none());
    }

    #[test]
    fn validate_config_enforces_complete_guard_admission() {
        let conf = configuration();
        config::validate_config(&mut conf.clone(), None).unwrap();
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
        let mut accepted = Vec::new();
        for (name, value) in invalid {
            let mut conf = serde_json::from_value(value).unwrap();
            if config::validate_config(&mut conf, None).is_ok() {
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
        assert_eq!(guard.trip.unwrap().reason, Reason::Coverage);
        assert_eq!(guard.frames, 0);
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
        assert_eq!(guard.trip.unwrap().reason, Reason::Peak);
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
    fn atomic_publication_is_bounded_and_does_not_consume_trip() {
        let mut guard = guard();
        let mut input = chunk();
        input.waveforms[0][0] = 1.0;
        guard.process(&mut input);
        let first = guard.shared.guard_record.read().unwrap();
        assert_eq!(first.1[5], Reason::Peak as u64);
        assert_eq!(guard.shared.guard_record.read(), Some(first));
        guard
            .shared
            .guard_record
            .sequence
            .store(first.0 + 1, Ordering::SeqCst);
        assert!(guard.shared.guard_record.read().is_none());
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
        guard.install(&conf).unwrap();
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
            assert!(!guard.active);
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
                    assert!(candidate.waveforms[0][FRAMES - 1] > 0.0);
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
