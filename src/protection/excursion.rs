//! Sealed single-driver voltage-to-displacement estimate. Compilation is off-thread.
use super::{FRAMES, Reason, lifted, plan::Id};
use crate::{Res, config::ConfigError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Driver {
    re_ohm: f64,
    le_h: f64,
    bl_n_per_a: f64,
    mms_kg: f64,
    rms_ns_per_m: f64,
    cms_m_per_n: f64,
    sd_m2: f64,
    vb_m3: f64,
    rho_kg_per_m3: f64,
    c_m_per_s: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pack {
    #[serde(rename = "type")]
    model_type: String,
    registration: String,
    destination_id: String,
    loading: String,
    sample_rate_hz: u32,
    si: Driver,
    pub(super) settle_blocks: u16,
    volts_peak_per_fs: f64,
    terminal_peak_bound_v: f64,
    limit_mm_one_way: f64,
    error_margin_mm: f64,
    validated_error_mm: f64,
    transition_error_mm: f64,
    calibration_sha256: String,
    coefficient_words: [String; 12],
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Model {
    pub index: usize,
    native_coefficients: bool,
    lifted_state_limit: u64,
    matrix: [[f64; 3]; 3],
    input: [f64; 3],
    gain: f64,
    voltage_limit: f64,
    limit_mm: f64,
    margin_mm: f64,
}

impl Pack {
    pub(super) fn compile(&self, dest: &str, index: usize, sample_rate: usize) -> Res<Model> {
        let s = &self.si;
        if self.model_type != "sealed_linear_v1"
            || self.loading != "sealed_independent_single_coil"
            || !matches!(self.registration.as_str(), "physical" | "synthetic")
            || self.sample_rate_hz as usize != sample_rate
            || self.destination_id != dest
            || !Id::valid(dest)
            || !(1..=1875).contains(&self.settle_blocks)
            || self.calibration_sha256.len() != 64
            || !self
                .calibration_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || ![
                s.re_ohm,
                s.le_h,
                s.bl_n_per_a,
                s.mms_kg,
                s.rms_ns_per_m,
                s.cms_m_per_n,
                s.sd_m2,
                s.vb_m3,
                s.rho_kg_per_m3,
                s.c_m_per_s,
                self.volts_peak_per_fs,
                self.terminal_peak_bound_v,
                self.limit_mm_one_way,
                self.error_margin_mm,
            ]
            .iter()
            .all(|x| x.is_finite() && *x > 0.)
            || s.sd_m2 * (self.limit_mm_one_way / 1000.) >= s.vb_m3
            || !(self.validated_error_mm >= 0.
                && self.transition_error_mm >= 0.
                && self.validated_error_mm < self.error_margin_mm
                && self.error_margin_mm < self.limit_mm_one_way)
        {
            return Err(ConfigError::new("invalid sealed excursion pack").into());
        }

        let t = 0.5 / sample_rate as f64;
        let spring = 1. / s.cms_m_per_n
            + s.rho_kg_per_m3 * s.c_m_per_s * s.c_m_per_s * s.sd_m2 * s.sd_m2 / s.vb_m3;
        let (a, b) = (1. + t * s.re_ohm / s.le_h, t * s.bl_n_per_a / s.le_h);
        let (d, e, f) = (
            t * s.bl_n_per_a / s.mms_kg,
            1. + t * s.rms_ns_per_m / s.mms_kg,
            t * spring / s.mms_kg,
        );
        let pivot = e + f * t + d * b / a;
        let solve = |x, y, z| {
            let v = (y + d * (x / a) - f * z) / pivot;
            [(x - b * v) / a, v, z + t * v]
        };
        let cols = [solve(1., 0., 0.), solve(0., 1., 0.), solve(0., 0., 1.)];
        let inv_norm = (0..3)
            .map(|i| cols.iter().map(|col| col[i].abs()).sum::<f64>())
            .fold(0., f64::max);
        let condition = (a + b).max(d + e + f).max(1. + t) * inv_norm;
        let matrix: [[f64; 3]; 3] =
            std::array::from_fn(|i| std::array::from_fn(|j| 2. * cols[j][i] - f64::from(i == j)));
        let input = solve(t / s.le_h, 0., 0.);
        let mut admitted = [0.; 12];
        for (index, word) in self.coefficient_words.iter().enumerate() {
            if word.len() != 16
                || !word
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(ConfigError::new("invalid excursion coefficient word").into());
            }
            admitted[index] = f64::from_bits(u64::from_str_radix(word, 16)?);
        }
        if !condition.is_finite()
            || condition > 1e8
            || matrix
                .iter()
                .flatten()
                .chain(&input)
                .zip(admitted)
                .any(|(x, word)| !x.is_finite() || x.to_bits() != word.to_bits())
        {
            return Err(ConfigError::new("excursion coefficients differ from SI model").into());
        }
        let matrix = std::array::from_fn(|i| std::array::from_fn(|j| admitted[3 * i + j]));
        let input = [admitted[9], admitted[10], admitted[11]];
        // The verified words round-trip exactly. Enclose the infinite unknown
        // input-history tail for these rounded coefficients at the admitted N.
        let tail = history_tail(matrix, input, usize::from(self.settle_blocks) * FRAMES)
            .ok_or_else(|| ConfigError::new("unverified excursion poles or settling"))?;
        let budget = ((2. * self.terminal_peak_bound_v * tail).next_up()
            + self.transition_error_mm)
            .next_up()
            + self.validated_error_mm;
        if !budget.is_finite() || budget.next_up() > self.error_margin_mm {
            return Err(ConfigError::new("insufficient excursion settling/error budget").into());
        }
        // Each row has three state products and one product of a voltage pair.
        // Bound their lifted sum by 5 * 2^(498+512), leaving room for the
        // final factor of 1000. Large coefficients can still advance tiny states.
        let max_coefficient = admitted.into_iter().map(f64::abs).fold(1.0, f64::max);
        let lifted_state_limit = (2.0_f64.powi(498) / max_coefficient)
            .min(2.0_f64.powi(256))
            .to_bits();
        Ok(Model {
            index,
            native_coefficients: admitted.into_iter().all(native_operand),
            lifted_state_limit,
            matrix,
            input,
            gain: self.volts_peak_per_fs,
            voltage_limit: self.terminal_peak_bound_v,
            limit_mm: self.limit_mm_one_way,
            margin_mm: self.error_margin_mm,
        })
    }
}

// Directed interval products keep cancellation in powers of A and enclose
// every rounding error in the stability/settling certificate.
#[derive(Clone, Copy, Default)]
struct Interval(f64, f64);
impl Interval {
    fn exact(x: f64) -> Self {
        Self(x, x)
    }
    fn add(self, b: Self) -> Self {
        Self((self.0 + b.0).next_down(), (self.1 + b.1).next_up())
    }
    fn mul(self, b: Self) -> Self {
        let terms = [self.0 * b.0, self.0 * b.1, self.1 * b.0, self.1 * b.1];
        if terms.iter().any(|x| !x.is_finite()) {
            return Self(f64::NEG_INFINITY, f64::INFINITY);
        }
        Self(
            terms.into_iter().fold(f64::INFINITY, f64::min).next_down(),
            terms
                .into_iter()
                .fold(f64::NEG_INFINITY, f64::max)
                .next_up(),
        )
    }
    fn abs(self) -> f64 {
        self.0.abs().max(self.1.abs())
    }
}
type Matrix = [[Interval; 3]; 3];
fn mul(a: Matrix, b: Matrix) -> Matrix {
    std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            (0..3).fold(Interval::exact(0.), |v, k| v.add(a[i][k].mul(b[k][j])))
        })
    })
}
fn row_norm(row: &[Interval; 3]) -> f64 {
    row.iter().fold(0., |a, x| (a + x.abs()).next_up())
}
fn history_tail(a: [[f64; 3]; 3], b: [f64; 3], frames: usize) -> Option<f64> {
    let units = [1., 1., 1024.];
    let mut power: Matrix = std::array::from_fn(|i| {
        std::array::from_fn(|j| Interval::exact(a[i][j]).mul(Interval::exact(units[i] / units[j])))
    });
    let mut prefix: Matrix =
        std::array::from_fn(|i| std::array::from_fn(|j| Interval::exact(f64::from(i == j))));
    let input: Matrix = std::array::from_fn(|i| {
        [
            Interval::exact(b[i]).mul(Interval::exact(units[i])),
            Interval::default(),
            Interval::default(),
        ]
    });
    let mut state = mul(power, input);
    for i in 0..3 {
        state[i][0] = state[i][0].add(input[i][0]);
    }
    let mut n = frames - 1;
    let mut sum_bound = None;
    for _ in 0..20 {
        if sum_bound.is_none() {
            let norm = power.iter().map(row_norm).fold(0., f64::max);
            if !norm.is_finite() {
                return None;
            }
            if norm < 0.5 {
                sum_bound = Some((row_norm(&prefix[2]) / (1. - norm).next_down()).next_up());
            } else {
                let extra = mul(prefix, power.map(|row| row.map(|v| Interval(0., v.abs()))));
                prefix =
                    std::array::from_fn(|i| std::array::from_fn(|j| prefix[i][j].add(extra[i][j])));
            }
        }
        if n & 1 == 1 {
            state = mul(power, state);
        }
        n >>= 1;
        if n == 0
            && let Some(factor) = sum_bound
        {
            let state_norm = state.iter().map(|v| v[0].abs()).fold(0., f64::max);
            let tail = ((factor * state_norm).next_up() * (1000. / 1024.)).next_up();
            return tail.is_finite().then_some(tail);
        }
        power = mul(power, power);
    }
    None
}

#[derive(Clone, Copy, Debug, Default)]
struct Trajectory {
    s: [f64; 3],
    previous: f64,
}

// Zero or 2^-200 <= |x| <= 2^200. Every operand, including a cancelled
// voltage sum, is a multiple of 2^-252. Products and partial row sums are
// multiples of 2^-504, even after rounding. The row magnitude is at most
// 5 * 2^400 before conversion to mm, so every nonzero intermediate is normal.
#[inline]
fn native_operand(x: f64) -> bool {
    let magnitude = x.abs().to_bits();
    magnitude == 0 || ((823_u64 << 52)..=(1223_u64 << 52)).contains(&magnitude)
}

impl Trajectory {
    fn same_words(&self, other: &Self) -> bool {
        self.previous.to_bits() == other.previous.to_bits()
            && self.s.map(f64::to_bits) == other.s.map(f64::to_bits)
    }

    fn step(&mut self, m: &Model, u: f64) -> Option<f64> {
        if m.native_coefficients
            && native_operand(u)
            && native_operand(self.previous)
            && self.s.into_iter().all(native_operand)
        {
            let [i, v, z] = self.s;
            let pair = u + self.previous;
            self.s = std::array::from_fn(|k| {
                let [a, b, c] = m.matrix[k];
                a * i + b * v + c * z + m.input[k] * pair
            });
            self.previous = u;
            return Some(1000. * self.s[2]);
        }
        let fits = |x: f64| x.abs().to_bits() <= m.lifted_state_limit;
        if self.s.into_iter().all(fits) && fits(self.previous) && fits(u) {
            let [i, v, z] = self.s.map(lifted::up);
            let pair = lifted::up(u) + lifted::up(self.previous);
            let next: [f64; 3] = std::array::from_fn(|k| {
                let [a, b, c] = m.matrix[k];
                lifted::mul(a, i)
                    + lifted::mul(b, v)
                    + lifted::mul(c, z)
                    + lifted::mul(m.input[k], pair)
            });
            self.s = next.map(lifted::down);
            self.previous = u;
            return Some(lifted::down(lifted::mul(1000., next[2])));
        }
        let [i, v, z] = self.s;
        let pair = u + self.previous;
        let next = std::array::from_fn(|k| {
            let [a, b, c] = m.matrix[k];
            lifted::product(a, i)
                + lifted::product(b, v)
                + lifted::product(c, z)
                + lifted::product(m.input[k], pair)
        });
        let mm = 1000. * next[2];
        if !mm.is_finite() || !next.iter().all(|v| v.is_finite()) {
            return None;
        }
        self.s = next;
        self.previous = u;
        Some(mm)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct History {
    candidate: Trajectory,
    forwarded: Trajectory,
    preview: Trajectory,
}
impl Model {
    pub(super) fn assess(&self, history: &mut History, samples: &[f64]) -> Option<(Reason, usize)> {
        history.preview = history.forwarded;
        if history.candidate.same_words(&history.preview) {
            let result = self.assess_samples::<true>(history, samples);
            history.preview = history.candidate;
            result
        } else {
            self.assess_samples::<false>(history, samples)
        }
    }

    fn assess_samples<const SHARED: bool>(
        &self,
        history: &mut History,
        samples: &[f64],
    ) -> Option<(Reason, usize)> {
        let mut first = None;
        for (offset, x) in samples.iter().enumerate() {
            let u = lifted::product(self.gain, *x);
            let volts = u.to_bits() & !(1 << 63);
            if volts <= f64::INFINITY.to_bits() && volts > self.voltage_limit.to_bits() {
                first.get_or_insert((Reason::ModelDomain, offset));
                continue;
            }
            let candidate = history.candidate.step(self, u);
            let preview = if SHARED {
                candidate
            } else {
                history.preview.step(self, u)
            };
            let (Some(a), Some(b)) = (candidate, preview) else {
                return Some((Reason::Numeric, offset));
            };
            let peak = f64::from_bits((a.to_bits() & !(1 << 63)).max(b.to_bits() & !(1 << 63)));
            let travel = if lifted::operand(peak) && lifted::operand(self.margin_mm) {
                lifted::down(lifted::up(peak) + lifted::up(self.margin_mm))
            } else {
                peak + self.margin_mm
            };
            if travel.to_bits() >= self.limit_mm.to_bits() {
                first.get_or_insert((Reason::Excursion, offset));
            }
        }
        first
    }

    pub(super) fn commit(&self, history: &mut History, masked: bool) -> Option<usize> {
        if masked {
            for offset in 0..FRAMES {
                let before = history.forwarded;
                if history.forwarded.step(self, 0.).is_none() {
                    return Some(offset);
                }
                // A bit-identical state repeats exactly for the remaining zeros.
                if offset == 0 && before.same_words(&history.forwarded) {
                    break;
                }
            }
        } else {
            history.forwarded = history.preview;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_uses_its_own_previous_voltage_even_with_identical_state() {
        let pack: Pack =
            serde_json::from_str(include_str!("../../testdata/excursion-model.json")).unwrap();
        let mut model = pack
            .compile(&pack.destination_id, 0, crate::protection::SAMPLE_RATE)
            .unwrap();
        let mut history = History::default();
        history.forwarded.previous = 1.0;
        model.limit_mm = 1000.0 * model.input[2].abs() + model.margin_mm;
        assert_eq!(
            model.assess(&mut history, &[0.0]),
            Some((Reason::Excursion, 0))
        );
        assert_eq!(history.candidate.s[2].to_bits(), 0.0_f64.to_bits());
        assert_eq!(history.preview.s[2].to_bits(), model.input[2].to_bits());
    }
}
