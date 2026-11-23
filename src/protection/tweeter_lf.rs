//! Measurement-only LR4 low-pass on the final tweeter candidate.
use super::{FRAMES, Reason, SAMPLE_RATE, policy::Assessment};
use crate::{Res, config, filters::biquad::BiquadCoefficients};

pub(super) fn coefficients(expected_highpass_hz: f64) -> Res<BiquadCoefficients> {
    if !(600.0..=12000.0).contains(&expected_highpass_hz) {
        return Err(
            config::ConfigError::new("invalid expected tweeter high-pass frequency").into(),
        );
    }
    let c = BiquadCoefficients::from_config(
        SAMPLE_RATE,
        config::BiquadParameters::Lowpass {
            freq: expected_highpass_hz / 3.0,
            q: std::f64::consts::FRAC_1_SQRT_2,
        },
    );
    validate_section(c)
}

fn validate_section(c: BiquadCoefficients) -> Res<BiquadCoefficients> {
    if !c.is_stable() || ![c.b0, c.b1, c.b2].into_iter().all(f64::is_finite) {
        return Err(config::ConfigError::new("invalid tweeter LF measurement filter").into());
    }
    Ok(c)
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Cascade {
    stages: [[f64; 2]; 2],
}

impl Cascade {
    fn step(&mut self, coefficients: &BiquadCoefficients, sample: f64) -> f64 {
        let [first, second] = &mut self.stages;
        let [s1, s2] = first;
        let y = coefficients.step(sample, s1, s2);
        let [s1, s2] = second;
        coefficients.step(y, s1, s2)
    }

    pub(super) fn block_power(&mut self, c: &BiquadCoefficients, samples: &[f64]) -> Option<f64> {
        let mut next = *self;
        let mut sum = 0.0;
        for &sample in samples {
            let y = next.step(c, sample);
            sum += y * y;
        }
        // Non-finite outputs propagate into sum. Check the terminal delays too,
        // including the last sample, before committing any part of this block.
        if !sum.is_finite() || !next.stages.iter().flatten().all(|v| v.is_finite()) {
            return None;
        }
        for delay in next.stages.iter_mut().flatten() {
            if delay.is_subnormal() {
                *delay = 0.0;
            }
        }
        *self = next;
        Some(sum / FRAMES as f64)
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TweeterLf {
    pub coefficients: BiquadCoefficients,
    pub source: usize,
    pub limit_power: f64,
    pub energy_blocks: u8,
    pub hold_blocks: u8,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct EnergyWindow {
    powers: [f64; 32],
    next: usize,
    count: usize,
    hits: u8,
}

impl TweeterLf {
    pub(super) fn observe(
        &self,
        window: &mut EnergyWindow,
        power: Option<f64>,
        prearm: bool,
    ) -> Assessment {
        let Some(power) = power else {
            return Assessment {
                reason: Some(Reason::Numeric),
                veto: false,
            };
        };
        let width = usize::from(self.energy_blocks);
        window.powers[window.next] = power;
        window.next = (window.next + 1) % width;
        window.count = (window.count + 1).min(width);
        // Each power is at most f64::MAX / 256 and there are at most 32 entries.
        let hit = window.count == width
            && window.powers[..width].iter().sum::<f64>() / width as f64 >= self.limit_power;
        window.hits = if hit {
            (window.hits + 1).min(self.hold_blocks)
        } else {
            0
        };
        Assessment {
            reason: (window.hits == self.hold_blocks).then_some(Reason::TweeterLf),
            veto: prearm && hit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex64;
    use std::f64::consts::{PI, SQRT_2, TAU};

    #[test]
    fn impulse_response_matches_prewarped_butterworth_pair_across_allowed_corners() {
        let fs = SAMPLE_RATE as f64;
        for hp in (600..=12000).step_by(100) {
            let corner = f64::from(hp) / 3.0;
            let c = coefficients(f64::from(hp)).unwrap();
            let mut filter = Cascade::default();
            let impulse: Vec<_> = (0..8 * FRAMES)
                .map(|i| filter.step(&c, f64::from(i == 0)))
                .collect();
            for freq in [
                0.0,
                corner / 8.0,
                corner / 2.0,
                corner,
                corner * 1.5,
                corner * 3.0,
                18000.0,
                23000.0,
            ] {
                let phase = Complex64::from_polar(1.0, -TAU * freq / fs);
                let measured = impulse
                    .iter()
                    .rev()
                    .fold(Complex64::default(), |acc, &x| acc * phase + x);
                // Bilinear transform of two analog Butterworth sections, including phase.
                let u = (PI * freq / fs).tan() / (PI * corner / fs).tan();
                let den = Complex64::new(1.0 - u * u, SQRT_2 * u);
                let expected = 1.0 / (den * den);
                assert!((measured - expected).norm() < 2e-11, "hp {hp}, freq {freq}");
            }
            // Eight blocks cover the slowest admitted corner after a unit step.
            let mut step = Cascade::default();
            for _ in 0..8 * FRAMES {
                step.step(&c, 1.0);
            }
            assert!((step.step(&c, 1.0) - 1.0).abs() < 2e-11);
        }
        let mut unstable = coefficients(600.0).unwrap();
        unstable.a2 = 1.0;
        assert!(validate_section(unstable).is_err());
        let mut nonfinite = coefficients(12000.0).unwrap();
        nonfinite.b1 = f64::NAN;
        assert!(validate_section(nonfinite).is_err());
    }
}
