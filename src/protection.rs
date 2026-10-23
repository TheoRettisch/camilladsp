//! Final candidate-sample admission for the protection fork.
//!
//! This owner is deliberately separate from the rebuildable processing graph:
//! graph changes cannot clear a trip. Only stamped guard output may enter ALSA.

use crate::audiochunk::AudioChunk;
use crate::{PrcFmt, Res, config};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const CHANNELS: usize = 24;
pub const FRAMES: usize = 256;
pub const SAMPLE_RATE: usize = 48_000;

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
            let before = self.sequence.load(Ordering::SeqCst);
            if before & 1 != 0 {
                continue;
            }
            let values = std::array::from_fn(|i| self.values[i].load(Ordering::SeqCst));
            if before == self.sequence.load(Ordering::SeqCst) {
                return Some((before, values));
            }
        }
        None
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

#[derive(Debug, Default)]
pub struct Shared {
    epoch: AtomicU64,
    // epoch, processed frames, interval begin/end, trip generation/reason/channel/frame/time
    guard_record: AtomicRecord<9>,
}

impl Shared {
    /// Read-only projection; publication and epoch retirement belong to Guard.
    pub fn guard_snapshot(&self) -> Option<(u64, [u64; 9])> {
        self.guard_record.read()
    }
}

/// The guard alone constructs stamps, after assessment and whole-chunk masking.
#[derive(Debug)]
pub struct Stamp {
    shared: Arc<Shared>,
    epoch: u64,
    pub frame_begin: u64,
    pub frame_end: u64,
    pub source_begin_ns: u64,
    pub source_end_ns: u64,
}

impl Stamp {
    pub fn is_current(&self) -> bool {
        self.epoch == self.shared.epoch.load(Ordering::SeqCst)
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
    shared: Arc<Shared>,
    epoch: u64,
    parameters: Option<Parameters>,
    peak_limit: PrcFmt,
    frames: u64,
    trip: Option<Trip>,
    // P2's control owner will supply qualification. Incomplete integration is contained.
    pub(crate) active: bool,
}

impl Guard {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            epoch: 0,
            parameters: None,
            peak_limit: 0.0,
            frames: 0,
            trip: None,
            active: false,
        }
    }

    pub fn install(&mut self, conf: &config::Configuration) -> Res<()> {
        // Retire queued stamps even when the replacement is invalid.
        self.epoch = self
            .shared
            .epoch
            .fetch_add(1, Ordering::SeqCst)
            .checked_add(1)
            .unwrap();
        assert!(self.epoch <= i64::MAX as u64);
        self.parameters = None;
        self.frames = 0;
        self.active = false;
        self.publish(0, 0);
        let parameters = admission(conf)?;
        self.peak_limit = 10.0_f64.powf(parameters.max_allowed_peak_db / 20.0) as PrcFmt;
        self.parameters = Some(parameters);
        Ok(())
    }

    pub fn trip(&self) -> Option<Trip> {
        self.trip
    }

    fn latch(&mut self, reason: Reason, channel: Option<usize>, frame: u64) {
        if self.trip.is_none() {
            self.trip = Some(Trip {
                generation: 1,
                reason,
                channel,
                frame,
                at_ns: monotonic_ns(),
            });
        }
        self.active = false;
    }

    fn publish(&self, begin: u64, end: u64) {
        let (generation, reason, channel, frame, at_ns) = self.trip.map_or((0, 0, 0, 0, 0), |t| {
            (
                t.generation,
                t.reason as u64,
                t.channel.map_or(0, |c| c as u64 + 1),
                t.frame,
                t.at_ns,
            )
        });
        self.shared.guard_record.publish([
            self.epoch,
            self.frames,
            begin,
            end,
            generation,
            reason,
            channel,
            frame,
            at_ns,
        ]);
    }

    pub fn process(&mut self, chunk: &mut AudioChunk, main_mute: bool) {
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
        if main_mute || !self.active || self.trip.is_some() || !shape_valid {
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
        let end = monotonic_ns();
        self.publish(
            if shape_valid { begin } else { 0 },
            if shape_valid { end } else { 0 },
        );
        if shape_valid {
            chunk.protection = Some(Stamp {
                shared: self.shared.clone(),
                epoch: self.epoch,
                frame_begin,
                frame_end: self.frames,
                source_begin_ns: begin,
                source_end_ns: end,
            });
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
        guard
    }

    pub(crate) fn chunk() -> AudioChunk {
        AudioChunk::new(vec![vec![0.1; FRAMES]; CHANNELS], 0.1, 0.1, FRAMES, FRAMES)
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
        guard.process(&mut input, false);
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
        guard.process(&mut safe, false);
        assert!(safe.waveforms.iter().flatten().all(|s| *s == 0.0));
    }

    #[test]
    fn actual_pipeline_preserves_candidate_under_main_mute() {
        let conf = configuration();
        let params = Arc::new(crate::ProcessingParameters::new(&[0.0; 5], &[true; 5]));
        let mut pipeline = crate::pipeline::Pipeline::from_config(conf.clone(), params);
        let mut input = chunk();
        input.waveforms[0][5] = 1.0;
        let mut candidate = pipeline.process_chunk(input);
        assert_eq!(candidate.waveforms[0][5], 1.0);
        let mut guard = guard();
        guard.process(&mut candidate, true);
        assert_eq!(guard.trip.unwrap().reason, Reason::Peak);
        assert!(candidate.waveforms.iter().flatten().all(|s| *s == 0.0));
    }

    #[test]
    fn offending_chunk_and_following_safe_chunks_are_zero() {
        for channel in [0, 1] {
            for offset in [0, FRAMES - 1] {
                let mut guard = guard();
                let mut input = chunk();
                input.waveforms[channel][offset] = guard.peak_limit;
                guard.process(&mut input, false);
                assert!(input.waveforms.iter().flatten().all(|s| *s == 0.0));
                let trip = guard.trip.unwrap();
                assert_eq!(trip.reason, Reason::Peak);
                assert_eq!(trip.channel, Some(channel));
                assert_eq!(trip.frame, offset as u64);
                let mut safe = chunk();
                guard.process(&mut safe, false);
                assert!(safe.waveforms.iter().flatten().all(|s| *s == 0.0));
                assert_eq!(guard.trip, Some(trip));
                assert_eq!(guard.frames, 2 * FRAMES as u64);
                assert!(safe.protection.unwrap().is_current());
            }
        }
    }

    #[test]
    fn nonfinite_on_any_channel_is_contained_including_under_main_mute() {
        for channel in 0..CHANNELS {
            for sample in [PrcFmt::NAN, PrcFmt::INFINITY, PrcFmt::NEG_INFINITY] {
                let mut guard = guard();
                let mut input = chunk();
                input.waveforms[channel][FRAMES - 1] = sample;
                guard.process(&mut input, true);
                assert_eq!(guard.trip.unwrap().reason, Reason::Nonfinite);
                assert!(input.waveforms.iter().flatten().all(|s| *s == 0.0));
            }
        }
    }

    #[test]
    fn known_safe_samples_forward_exactly_and_main_mute_is_final() {
        let mut guard = guard();
        let mut input = chunk();
        guard.process(&mut input, false);
        assert!(input.waveforms.iter().flatten().all(|s| *s == 0.1));
        assert!(guard.trip.is_none());
        guard.process(&mut input, true);
        assert!(guard.trip.is_none());
        assert!(input.waveforms.iter().flatten().all(|s| *s == 0.0));
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
    fn loudness_main_fader_preserves_candidate_under_main_mute() {
        let mut conf = serde_json::to_value(configuration()).unwrap();
        conf["filters"] = serde_json::json!({"loudness": {"type": "Loudness",
            "parameters": {"reference_level": -20}}});
        conf["pipeline"].as_array_mut().unwrap().insert(
            0,
            serde_json::json!({"type": "Filter", "names": ["loudness"]}),
        );
        let mut conf: config::Configuration = serde_json::from_value(conf).unwrap();
        config::validate_config(&mut conf, None).unwrap();
        admission(&conf).unwrap();
        let mut candidates = Vec::new();
        for mute in [false, true] {
            let params = Arc::new(crate::ProcessingParameters::new(&[-30.0; 5], &[mute; 5]));
            let mut pipeline = crate::pipeline::Pipeline::from_config(conf.clone(), params);
            let input = AudioChunk::new(
                vec![vec![100.0; FRAMES]; CHANNELS],
                100.0,
                100.0,
                FRAMES,
                FRAMES,
            );
            candidates.push(pipeline.process_chunk(input));
        }
        assert_eq!(candidates[0].waveforms, candidates[1].waveforms);
        let mut guard = guard();
        guard.process(&mut candidates[1], true);
        assert_eq!(guard.trip.unwrap().reason, Reason::Peak);
        assert!(candidates[1].waveforms.iter().flatten().all(|s| *s == 0.0));
    }

    #[test]
    fn incomplete_coverage_is_not_silence_or_protected_progress() {
        let mut guard = guard();
        let mut input = chunk();
        input.waveforms[1].clear();
        guard.process(&mut input, false);
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
        guard.process(&mut input, false);
        assert_eq!(guard.trip.unwrap().reason, Reason::Peak);
        assert!(input.protection.is_none());
    }

    #[test]
    fn retired_stamps_cannot_be_submitted() {
        let mut guard = guard();
        let mut input = chunk();
        guard.process(&mut input, false);
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
        guard.process(&mut input, false);
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
}
