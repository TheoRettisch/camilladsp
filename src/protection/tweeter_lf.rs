//! Measurement-only LR4 low-pass on the final tweeter candidate.
use super::{FRAMES, Reason, SAMPLE_RATE, lifted, policy::Assessment};
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
    if !c.is_stable() || !c.b0.is_finite() || !c.b1.is_finite() || c.b0.to_bits() != c.b2.to_bits()
    {
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
        let mut x = sample;
        for [s1, s2] in &mut self.stages {
            if [x, *s1, *s2].into_iter().all(lifted::operand) {
                // Keep the additions on the lifted grid as well. Admitted
                // low-pass coefficients have magnitude below two, leaving
                // ample exponent range for both sums and the output feedback.
                let (mut a, mut b) = (lifted::up(*s1), lifted::up(*s2));
                let out = coefficients.step_with(lifted::up(x), &mut a, &mut b, lifted::mul);
                *s1 = lifted::down(a);
                *s2 = lifted::down(b);
                x = lifted::down(out);
            } else {
                x = coefficients.step_with(x, s1, s2, lifted::product);
            }
        }
        x
    }

    // Advance the bounded prefix in lifted coordinates. A large input can
    // switch coordinates at any sample without repeating its preceding work.
    fn lifted_prefix(&mut self, c: &BiquadCoefficients, samples: &[f64]) -> (usize, f64) {
        if !self.stages.iter().flatten().copied().all(lifted::operand) {
            return (0, 0.0);
        }
        let mut stages = self.stages.map(|s| s.map(lifted::up));
        let mut count = 0;
        let mut sum = 0.0;
        for &sample in samples {
            if !lifted::operand(sample) {
                break;
            }
            let mut x = lifted::up(sample);
            for [s1, s2] in &mut stages {
                // The admitted low-pass has identical b0 and b2.
                let feed = lifted::mul(c.b0, x);
                let y = *s1 + feed;
                *s1 = *s2 + lifted::mul(c.b1, x) - lifted::mul(c.a1, y);
                *s2 = feed - lifted::mul(c.a2, y);
                x = y;
            }
            if !x.is_finite() {
                return (0, 0.0);
            }
            sum += lifted::square(lifted::down(x));
            count += 1;
        }
        if !stages.iter().flatten().all(|x| x.is_finite()) {
            return (0, 0.0);
        }
        self.stages = stages.map(|s| s.map(lifted::down));
        (count, sum)
    }

    pub(super) fn block_power(&mut self, c: &BiquadCoefficients, samples: &[f64]) -> Option<f64> {
        let mut next = *self;
        let (used, mut sum) = next.lifted_prefix(c, samples);
        for &sample in &samples[used..] {
            sum += lifted::square(next.step(c, sample));
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
    fn tiny_samples_and_decaying_delays_match_native_through_block_cleanup() {
        for hp in [600.0, 3000.0, 12000.0] {
            let c = coefficients(hp).unwrap();
            let mut filter = Cascade::default();
            let mut reference = [[0.0; 2]; 2];
            for amplitude in [1e-153, 1e-159, 1e-307, f64::from_bits(17), 0.0, 0.25, 0.5] {
                let mut samples: [f64; FRAMES] =
                    std::array::from_fn(|i| if i % 7 < 3 { amplitude } else { -amplitude });
                if amplitude == 0.25 {
                    // Fall back at the last sample, then start the next block
                    // with large delays. Both must commit the native history.
                    samples[FRAMES - 1] = 2.0_f64.powi(300);
                }
                let mut sum = 0.0;
                let mut per_sample = filter;
                for &x in &samples {
                    let mut y = x;
                    for [s1, s2] in &mut reference {
                        y = c.step(y, s1, s2);
                    }
                    assert_eq!(per_sample.step(&c, x).to_bits(), y.to_bits());
                    assert_eq!(
                        per_sample.stages.map(|s| s.map(f64::to_bits)),
                        reference.map(|s| s.map(f64::to_bits))
                    );
                    sum += y * y;
                }
                for delay in reference.iter_mut().flatten() {
                    if delay.is_subnormal() {
                        *delay = 0.0;
                    }
                }
                let power = filter.block_power(&c, &samples).unwrap();
                assert_eq!(power.to_bits(), (sum / FRAMES as f64).to_bits());
                assert_eq!(
                    filter.stages.map(|s| s.map(f64::to_bits)),
                    reference.map(|s| s.map(f64::to_bits))
                );
            }
        }
    }

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
