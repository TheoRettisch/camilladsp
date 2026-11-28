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

use std::sync::Arc;

use ringbuf::LocalRb;
use ringbuf::storage::Heap;
use ringbuf::traits::*;

use crate::audiochunk::AudioChunk;
use crate::config;
use crate::filters::Filter;
use crate::filters::biquad::{Biquad, BiquadCoefficients};

use crate::NewValue;
use crate::PrcFmt;
use crate::ProcessingParameters;
use crate::Res;
use crate::utils::decibels::gain_from_value;

#[derive(Clone, Debug)]
pub struct Gain {
    pub name: String,
    pub gain: PrcFmt,
}

pub struct Delay {
    pub name: String,
    samplerate: usize,
    queue: Option<LocalRb<Heap<PrcFmt>>>,
    biquad: Option<Biquad>,
}

/// State actually applied by the built-in main Volume to one complete chunk.
/// A later control read cannot describe samples already processed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedVolume {
    pub(crate) muted: bool,
    pub(crate) opening: bool,
}

pub struct Volume {
    pub name: String,
    ramptime_in_chunks: usize,
    current_volume: PrcFmt,
    target_volume: f32,
    target_linear_gain: PrcFmt,
    mute: bool,
    opening: bool,
    ramp_start: PrcFmt,
    ramp_end: PrcFmt,
    // First chunk of the current ramp segment, numbered from 1 like ramp_step.
    ramp_start_step: usize,
    ramp_step: usize,
    samplerate: usize,
    chunksize: usize,
    processing_params: Arc<ProcessingParameters>,
    fader: usize,
    volume_limit: f32,
}

impl Volume {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &str,
        ramp_time_ms: f32,
        limit: f32,
        current_volume: f32,
        mute: bool,
        chunksize: usize,
        samplerate: usize,
        processing_params: Arc<ProcessingParameters>,
        fader: usize,
    ) -> Self {
        let name = name.to_string();
        let ramptime_in_chunks =
            (ramp_time_ms / (1000.0 * chunksize as f32 / samplerate as f32)).round() as usize;
        let current_volume_with_mute = if mute { -100.0 } else { current_volume };
        let target_linear_gain = if mute {
            0.0
        } else {
            let tempgain: PrcFmt = 10.0;
            tempgain.powf(current_volume as PrcFmt / 20.0)
        };
        Self {
            name,
            ramptime_in_chunks,
            current_volume: current_volume_with_mute as PrcFmt,
            ramp_start: current_volume as PrcFmt,
            ramp_end: current_volume_with_mute as PrcFmt,
            ramp_start_step: 1,
            target_volume: current_volume,
            target_linear_gain,
            mute,
            opening: false,
            ramp_step: 0,
            samplerate,
            chunksize,
            processing_params,
            fader,
            volume_limit: limit,
        }
    }

    pub fn from_config(
        name: &str,
        conf: config::VolumeParameters,
        chunksize: usize,
        samplerate: usize,
        processing_params: Arc<ProcessingParameters>,
    ) -> Self {
        let fader = conf.fader as usize;
        let current_volume = processing_params.current_volume(fader);
        let mute = processing_params.is_mute(fader);
        Self::new(
            name,
            conf.ramp_time(),
            conf.limit(),
            current_volume,
            mute,
            chunksize,
            samplerate,
            processing_params,
            fader,
        )
    }

    /// First sample level and dB change per chunk for the active segment.
    fn ramp_chunk_db(&self) -> (PrcFmt, PrcFmt) {
        let chunks = self.ramptime_in_chunks - self.ramp_start_step + 1;
        let change = (self.ramp_end - self.ramp_start) / chunks as PrcFmt;
        let start = self.ramp_start + change * (self.ramp_step - self.ramp_start_step) as PrcFmt;
        (start, change)
    }

    fn make_ramp(&self) -> Vec<PrcFmt> {
        let (start, change) = self.ramp_chunk_db();
        let stepsize = change / self.chunksize as PrcFmt;
        (0..self.chunksize)
            .map(|val| (PrcFmt::coerce(10.0)).powf((start + val as PrcFmt * stepsize) / 20.0))
            .collect()
    }

    fn prepare_processing(&mut self) {
        let shared_vol = self.processing_params.target_volume(self.fader);
        let shared_mute = self.processing_params.is_mute(self.fader);

        // are we above the set limit?
        let target_volume = shared_vol.min(self.volume_limit);

        // A level edit under mute only sets the destination for the next unmute.
        // Let an ongoing fade to silence finish on its original schedule.
        if self.mute && shared_mute {
            self.target_volume = target_volume;
            return;
        }

        // Volume setting changed
        if (target_volume - self.target_volume).abs() > 0.01 || self.mute != shared_mute {
            if self.mute != shared_mute {
                self.opening = !shared_mute && self.ramptime_in_chunks > 0;
            }
            if self.ramptime_in_chunks > 0 {
                trace!(
                    "starting ramp: {} -> {}, mute: {}",
                    self.current_volume, target_volume, shared_mute
                );
                self.ramp_start = self.current_volume;
                self.ramp_end = if shared_mute {
                    self.volume_limit.min(-100.0)
                } else {
                    target_volume
                } as PrcFmt;
                self.ramp_start_step = 1;
                self.ramp_step = 1;
            } else {
                trace!(
                    "switch volume without ramp: {} -> {}, mute: {}",
                    self.current_volume, target_volume, shared_mute
                );
                self.current_volume = if shared_mute {
                    0.0
                } else {
                    target_volume as PrcFmt
                };
                self.ramp_step = 0;
            }
            self.target_volume = target_volume;
            self.target_linear_gain = if shared_mute {
                0.0
            } else {
                let tempgain: PrcFmt = 10.0;
                tempgain.powf(target_volume as PrcFmt / 20.0)
            };
            self.mute = shared_mute;
        }
    }

    pub fn process_chunk(&mut self, chunk: &mut AudioChunk) -> AppliedVolume {
        self.prepare_processing();
        // Capture before processing: the last ramp chunk is still masked even
        // though ramp_step becomes zero while processing it.
        let applied = AppliedVolume {
            muted: self.mute,
            opening: self.opening,
        };

        // Not in a ramp
        if self.ramp_step == 0 {
            xtrace!("Vol: applying linear gain {}", self.target_linear_gain);
            for waveform in chunk.waveforms.iter_mut() {
                for item in waveform.iter_mut() {
                    *item *= self.target_linear_gain;
                }
            }
        }
        // Ramping
        else if self.ramp_step <= self.ramptime_in_chunks {
            trace!("Vol: ramp step {}", self.ramp_step);
            let ramp = self.make_ramp();
            self.ramp_step += 1;
            if self.ramp_step > self.ramptime_in_chunks {
                // Last step of ramp
                self.ramp_step = 0;
            }
            for waveform in chunk.waveforms.iter_mut() {
                for (item, stepgain) in waveform.iter_mut().zip(ramp.iter()) {
                    *item *= *stepgain;
                }
            }
            self.current_volume = 20.0 * ramp.last().unwrap().log10();
        }

        // Update shared current volume
        self.processing_params
            .set_current_volume(self.fader, self.current_volume as f32);
        if self.ramp_step == 0 {
            self.opening = false;
        }
        applied
    }
}

impl Filter for Volume {
    fn name(&self) -> &str {
        &self.name
    }

    fn process_waveform(&mut self, waveform: &mut [PrcFmt]) -> Res<()> {
        self.prepare_processing();

        // Not in a ramp
        if self.ramp_step == 0 {
            for item in waveform.iter_mut() {
                *item *= self.target_linear_gain;
            }
        }
        // Ramping
        else if self.ramp_step <= self.ramptime_in_chunks {
            trace!("ramp step {}", self.ramp_step);
            let ramp = self.make_ramp();
            self.ramp_step += 1;
            if self.ramp_step > self.ramptime_in_chunks {
                // Last step of ramp
                self.ramp_step = 0;
            }
            for (item, stepgain) in waveform.iter_mut().zip(ramp.iter()) {
                *item *= *stepgain;
            }
            self.current_volume = 20.0 * ramp.last().unwrap().log10();
        }

        // Update shared current volume
        self.processing_params
            .set_current_volume(self.fader, self.current_volume as f32);
        Ok(())
    }

    fn update_parameters(&mut self, conf: config::Filter) {
        if let config::Filter::Volume {
            parameters: conf, ..
        } = conf
        {
            self.ramptime_in_chunks = (conf.ramp_time()
                / (1000.0 * self.chunksize as f32 / self.samplerate as f32))
                .round() as usize;
            self.fader = conf.fader as usize;
            let limit = conf.limit();
            if self.ramp_step > self.ramptime_in_chunks {
                // A shorter configured ramp may already have reached its deadline.
                self.ramp_step = 0;
                self.current_volume = self.ramp_end;
            } else if self.mute && self.ramp_step > 0 && limit < self.volume_limit {
                let ceiling = limit as PrcFmt;
                let (next_db, _) = self.ramp_chunk_db();
                if next_db > ceiling || self.ramp_end > ceiling {
                    // Cap the remaining fade from its next sample, keeping the end chunk.
                    self.ramp_start = next_db.min(ceiling);
                    self.ramp_end = self.ramp_end.min(self.ramp_start);
                    self.ramp_start_step = self.ramp_step;
                }
            }
            self.volume_limit = limit;
            if (self.volume_limit as PrcFmt) < self.current_volume {
                self.current_volume = self.volume_limit as PrcFmt;
            }
        } else {
            // This should never happen unless there is a bug somewhere else
            panic!("Invalid config change!");
        }
    }
}

impl Gain {
    /// A simple filter providing gain in dB, and can also invert the signal.
    pub fn new(name: &str, gain_value: PrcFmt, inverted: bool, mute: bool, linear: bool) -> Self {
        let name = name.to_string();
        let gain = gain_from_value(gain_value, linear, inverted, mute);
        Gain { name, gain }
    }

    pub fn from_config(name: &str, conf: config::GainParameters) -> Self {
        let gain = conf.gain;
        let inverted = conf.is_inverted();
        let mute = conf.is_mute();
        let linear = conf.scale() == config::GainScale::Linear;
        Gain::new(name, gain, inverted, mute, linear)
    }

    pub fn process_single(&self, value: PrcFmt) -> PrcFmt {
        value * self.gain
    }
}

impl Filter for Gain {
    fn name(&self) -> &str {
        &self.name
    }

    fn process_waveform(&mut self, waveform: &mut [PrcFmt]) -> Res<()> {
        for item in waveform.iter_mut() {
            *item *= self.gain;
        }
        Ok(())
    }

    fn update_parameters(&mut self, conf: config::Filter) {
        if let config::Filter::Gain {
            parameters: conf, ..
        } = conf
        {
            let gain_value = conf.gain;
            let inverted = conf.is_inverted();
            let mute = conf.is_mute();
            let linear = conf.scale() == config::GainScale::Linear;
            let gain = gain_from_value(gain_value, linear, inverted, mute);
            self.gain = gain;
        } else {
            // This should never happen unless there is a bug somewhere else
            panic!("Invalid config change!");
        }
    }
}

fn build_subsample_biquad(delay: PrcFmt, samplerate: usize) -> (usize, Option<Biquad>) {
    // delay is less than 0.1 samples, ignore
    if delay < 0.1 {
        debug!("Delay too small, ignoring");
        return (0, None);
    }
    // delay is less than 1.1 samples, use first order allpass
    if delay < 1.1 {
        let coeff = (1.0 - delay) / (1.0 + delay);
        debug!("Using first order allpass for delay of {delay:.2} samples");
        // 1st order Thiran allpass
        let bqcoeffs = BiquadCoefficients::new(coeff, 0.0, coeff, 1.0, 0.0);
        trace!("Coefficients: {bqcoeffs:?}");
        return (0, Some(Biquad::new("subsample", samplerate, bqcoeffs)));
    }

    // delay is large enough to use a second order allpass
    let mut samples = delay.floor();
    let mut fraction = delay - samples;
    // adjust fraction and samples to keep fraction between 1.1 and 2.1
    samples -= 1.0;
    fraction += 1.0;
    if fraction < 1.1 {
        samples -= 1.0;
        fraction += 1.0;
    }
    // 2nd order Thiran allpass
    debug!("Using second order allpass for delay of {samples} + {fraction:.2} samples");
    let coeff1 = 2.0 * (2.0 - fraction) / (1.0 + fraction);
    let coeff2 = (2.0 - fraction) / (2.0 + fraction) * (1.0 - fraction) / (1.0 + fraction);
    let bqcoeffs = BiquadCoefficients::new(coeff1, coeff2, coeff2, coeff1, 1.0);
    trace!("Coefficients: {bqcoeffs:?}");
    (
        samples as usize,
        Some(Biquad::new("subsample", samplerate, bqcoeffs)),
    )
}

impl Delay {
    /// Creates a delay filter with delay in samples
    pub fn new(name: &str, samplerate: usize, delay: PrcFmt, subsample: bool) -> Self {
        let name = name.to_string();

        let (integerdelay, biquad) = if subsample {
            let (samples, bq) = build_subsample_biquad(delay, samplerate);
            debug!(
                "Building delay filter '{}' with delay {} + {:.2} samples",
                name,
                samples,
                delay - samples as PrcFmt
            );
            (samples, bq)
        } else {
            let samples = delay.round() as usize;
            debug!("Building delay filter '{name}' with delay {samples} samples");
            (samples, None)
        };

        let queue = if integerdelay > 0 {
            let mut q = LocalRb::new(integerdelay);
            for _ in 0..integerdelay {
                let _ = q.try_push(0.0);
            }
            Some(q)
        } else {
            None
        };

        Self {
            name,
            samplerate,
            queue,
            biquad,
        }
    }

    pub fn from_config(name: &str, samplerate: usize, conf: config::DelayParameters) -> Self {
        let delay_samples = match conf.unit() {
            config::TimeUnit::Microseconds => conf.delay / 1000000.0 * (samplerate as PrcFmt),
            config::TimeUnit::Milliseconds => conf.delay / 1000.0 * (samplerate as PrcFmt),
            config::TimeUnit::Millimetres => conf.delay / 1000.0 * (samplerate as PrcFmt) / 343.0,
            config::TimeUnit::Samples => conf.delay,
        };

        Self::new(name, samplerate, delay_samples, conf.subsample())
    }

    pub fn process_single(&mut self, input: PrcFmt) -> PrcFmt {
        let mut value = if let Some(q) = &mut self.queue {
            q.push_overwrite(input).unwrap()
        } else {
            input
        };
        if let Some(bq) = &mut self.biquad {
            value = bq.process_single(value);
        }
        value
    }
}

impl Filter for Delay {
    fn name(&self) -> &str {
        &self.name
    }

    fn process_waveform(&mut self, waveform: &mut [PrcFmt]) -> Res<()> {
        if let Some(q) = &mut self.queue {
            for item in waveform.iter_mut() {
                // this returns the item that was popped while pushing
                *item = q.push_overwrite(*item).unwrap();
            }
        }
        if let Some(bq) = &mut self.biquad {
            bq.process_waveform(waveform)?;
        }
        Ok(())
    }

    fn update_parameters(&mut self, conf: config::Filter) {
        if let config::Filter::Delay { parameters, .. } = conf {
            *self = Self::from_config(&self.name, self.samplerate, parameters);
        } else {
            // This should never happen unless there is a bug somewhere else
            unreachable!("Invalid config change!");
        }
    }
}

/// Validate a Loudness config.
pub fn validate_delay_config(conf: &config::DelayParameters) -> Res<()> {
    if conf.delay < 0.0 {
        return Err(config::ConfigError::new("Delay cannot be negative").into());
    }
    Ok(())
}

/// Validate a Volume config.
pub fn validate_volume_config(conf: &config::VolumeParameters) -> Res<()> {
    if conf.ramp_time() < 0.0 {
        return Err(config::ConfigError::new("Ramp time cannot be negative").into());
    }
    Ok(())
}

/// Validate a Gain config.
pub fn validate_gain_config(conf: &config::GainParameters) -> Res<()> {
    if conf.scale() == config::GainScale::Decibel {
        if conf.gain < -150.0 {
            return Err(config::ConfigError::new("Gain must be larger than -150 dB").into());
        } else if conf.gain > 150.0 {
            return Err(config::ConfigError::new("Gain must be less than +150 dB").into());
        }
    } else if conf.gain < -10.0 {
        return Err(config::ConfigError::new("Linear gain must be larger than -10.0").into());
    } else if conf.gain > 10.0 {
        return Err(config::ConfigError::new("Linear gain must be less than +10.0").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::PrcFmt;
    use crate::filters::Filter;
    use crate::filters::basicfilters::{Delay, Gain, Volume};
    use crate::{ProcessingParameters, config};
    use std::sync::Arc;

    fn is_close(left: PrcFmt, right: PrcFmt, maxdiff: PrcFmt) -> bool {
        println!("{left} - {right}");
        (left - right).abs() < maxdiff
    }

    fn compare_waveforms(left: Vec<PrcFmt>, right: Vec<PrcFmt>, maxdiff: PrcFmt) -> bool {
        for (val_l, val_r) in left.iter().zip(right.iter()) {
            if !is_close(*val_l, *val_r, maxdiff) {
                return false;
            }
        }
        true
    }

    #[test]
    fn aux_limit_reload_caps_the_mute_fade_until_its_original_deadline() {
        let parameters = |limit| config::VolumeParameters {
            fader: config::VolumeFader::Aux1,
            ramp_time: Some(100.0),
            limit: Some(limit),
        };
        let reload_limit = |volume: &mut Volume, limit| {
            volume.update_parameters(config::Filter::Volume {
                description: None,
                parameters: parameters(limit),
            });
        };
        let next_chunk = |volume: &mut Volume| {
            let mut samples = [1.0; 480]; // 10 ms at 48 kHz
            volume.process_waveform(&mut samples).unwrap();
            samples
        };

        for limit in [-40.0, -120.0] {
            let params = Arc::new(ProcessingParameters::default());
            let mut volume =
                Volume::from_config("aux", parameters(0.0), 480, 48_000, params.clone());
            params.set_mute(1, true);
            let first = next_chunk(&mut volume);
            let ceiling = 10.0_f64.powf(limit as f64 / 20.0);
            assert!(first[first.len() - 1] > ceiling);

            reload_limit(&mut volume, limit);
            let mut previous = ceiling;
            for block in 1..10 {
                if block == 5 {
                    reload_limit(&mut volume, 0.0); // Relaxing the ceiling must not lift the fade.
                }
                let samples = next_chunk(&mut volume);
                if block == 1 {
                    assert!(
                        (samples[0] - ceiling).abs() <= ceiling * 1e-12,
                        "first gain after reload: {}, expected {ceiling}",
                        samples[0]
                    );
                    if limit == -40.0 {
                        assert!(samples[samples.len() - 1] < samples[0]);
                    }
                }
                for gain in samples {
                    assert!(gain > 0.0 && gain <= previous);
                    previous = gain;
                }
            }
            assert_eq!(next_chunk(&mut volume), [0.0; 480]);

            params.set_mute(1, false);
            for _ in 0..10 {
                assert!(next_chunk(&mut volume)[0] < 1.0);
            }
            assert_eq!(next_chunk(&mut volume), [1.0; 480]);
        }
    }

    #[test]
    fn gain_invert() {
        let mut waveform = vec![-0.5, 0.0, 0.5];
        let waveform_inv = vec![0.5, 0.0, -0.5];
        let mut gain = Gain::new("test", 0.0, true, false, false);
        gain.process_waveform(&mut waveform).unwrap();
        assert_eq!(waveform, waveform_inv);
    }

    #[test]
    fn gain_ampl() {
        let mut waveform = vec![-0.5, 0.0, 0.5];
        let waveform_ampl = vec![-5.0, 0.0, 5.0];
        let mut gain = Gain::new("test", 20.0, false, false, false);
        gain.process_waveform(&mut waveform).unwrap();
        assert_eq!(waveform, waveform_ampl);
    }

    #[test]
    fn delay_small() {
        let mut waveform = vec![0.0, -0.5, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let waveform_delayed = vec![0.0, 0.0, 0.0, 0.0, -0.5, 1.0, 0.0, 0.0];
        let mut delay = Delay::new("test", 44100, 3.0, false);
        delay.process_waveform(&mut waveform).unwrap();
        assert_eq!(waveform, waveform_delayed);
    }

    #[test]
    fn delay_supersmall() {
        let mut waveform = vec![0.0, -0.5, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let waveform_delayed = waveform.clone();
        let mut delay = Delay::new("test", 44100, 0.1, false);
        delay.process_waveform(&mut waveform).unwrap();
        assert_eq!(waveform, waveform_delayed);
    }

    #[test]
    fn delay_large() {
        let mut waveform1 = vec![0.0, -0.5, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let mut waveform2 = vec![0.0; 8];
        let waveform_delayed = vec![0.0, 0.0, -0.5, 1.0, 0.0, 0.0, 0.0, 0.0];
        let mut delay = Delay::new("test", 44100, 9.0, false);
        delay.process_waveform(&mut waveform1).unwrap();
        delay.process_waveform(&mut waveform2).unwrap();
        assert_eq!(waveform1, vec![0.0; 8]);
        assert_eq!(waveform2, waveform_delayed);
    }

    #[test]
    fn delay_fraction() {
        let mut waveform = vec![0.0, -0.5, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let expected_waveform = vec![
            0.0,
            0.01051051051051051,
            -0.13446780113446782,
            -0.2476751025299573,
            1.0522122611990257,
            -0.23903133046978262,
            0.07523664949897024,
            -0.021743938066703532,
            0.006413537427714274,
            -0.001882310318672015,
        ];
        let mut delay = Delay::new("test", 44100, 1.7, true);
        delay.process_waveform(&mut waveform).unwrap();
        assert!(compare_waveforms(waveform, expected_waveform, 1.0e-6));
    }
}
