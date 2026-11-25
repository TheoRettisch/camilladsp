// Scale binary64 by 2^512 to avoid hardware subnormal assists. Values on
// this grid are exact multiples of 2^-562, the lifted least binary64 subnormal.
const SIGN: u64 = 1 << 63;
const FRACTION: u64 = (1 << 52) - 1;
const SHIFT: u64 = 512 << 52;
const GRID_BOUNDARY: f64 = f64::from_bits(513 << 52); // 2^-510

fn magnitude(x: f64) -> u64 {
    x.to_bits() & !SIGN
}

pub(super) fn operand(x: f64) -> bool {
    magnitude(x) <= 1279 << 52 // at most 2^256
}

#[inline]
pub(super) fn up(x: f64) -> f64 {
    if magnitude(x) >= 1 << 52 {
        f64::from_bits(x.to_bits() + SHIFT)
    } else {
        let units = magnitude(x) as f64;
        (units * f64::from_bits(461 << 52)).copysign(x) // 2^-562
    }
}

#[inline]
pub(super) fn down(x: f64) -> f64 {
    let bits = x.to_bits();
    let exponent = magnitude(x) >> 52;
    if exponent > 512 {
        f64::from_bits(bits - SHIFT)
    } else if exponent == 0 {
        x // signed zero
    } else {
        debug_assert!(exponent >= 461);
        let units = ((1 << 52) | (bits & FRACTION)) >> (513 - exponent);
        f64::from_bits((bits & SIGN) | units)
    }
}

#[inline]
pub(super) fn mul(coefficient: f64, lifted: f64) -> f64 {
    // For a finite binary64 with biased exponent E, |x| < 2^(E-1022),
    // including subnormals. On the lifted grid half a unit is 2^-563.
    // Very small coefficients can otherwise underflow even in these coordinates.
    let abs = magnitude(coefficient);
    let (coefficient, lifted) = if abs < (563 << 52) {
        // below 2^-460
        let exponent = abs >> 52;
        if abs == 0 || exponent + (magnitude(lifted) >> 52) <= 1481 {
            return f64::from_bits((coefficient.to_bits() ^ lifted.to_bits()) & SIGN);
        }
        if exponent == 0 {
            // The test above implies |lifted| >= 2^459. Transfer a factor
            // of 2^512 between operands so both are normal.
            (up(coefficient), down(lifted))
        } else {
            (coefficient, lifted)
        }
    } else {
        (coefficient, lifted)
    };
    let product = coefficient * lifted;
    if product.abs() >= GRID_BOUNDARY {
        product
    } else {
        // Adding this power of two rounds directly to the original subnormal
        // grid. A fused product avoids double rounding at the half-unit ties.
        let biased = coefficient.abs().mul_add(lifted.abs(), GRID_BOUNDARY);
        (biased - GRID_BOUNDARY).copysign(product)
    }
}

/// One binary64 product, including gradual underflow, for unrestricted operands.
#[inline]
pub(super) fn product(a: f64, b: f64) -> f64 {
    let ea = magnitude(a) >> 52;
    let eb = magnitude(b) >> 52;
    if ea == 2047 || eb == 2047 || (ea != 0 && eb != 0 && ea + eb >= 1024) {
        return a * b;
    }
    // |x| < 2^(E-1022), even for E=0. Below half a least subnormal
    // the result is signed zero. Otherwise lift the smaller operand: the
    // unscaled product is tiny, or one operand is subnormal and the other
    // is at most MAX, so the scaled product cannot overflow.
    if ea + eb <= 969 {
        return f64::from_bits((a.to_bits() ^ b.to_bits()) & SIGN);
    }
    let (small, large) = if ea <= eb { (a, b) } else { (b, a) };
    down(mul(large, up(small)))
}

#[inline]
pub(super) fn square(x: f64) -> f64 {
    let abs = magnitude(x);
    if abs >= 512 << 52 {
        x * x // At or above 2^-511 a finite square cannot underflow.
    } else if abs <= 485 << 52 {
        0.0 // At or below 2^-538 the square is below half a least subnormal.
    } else {
        product(x, x)
    }
}

#[cfg(test)]
mod tests {
    use super::{down, mul, product, square, up};
    use std::hint::black_box;

    #[test]
    fn unrestricted_products_match_native_at_underflow_and_overflow() {
        let least = f64::from_bits(1);
        let operands = [
            0.0,
            least,
            f64::MIN_POSITIVE.next_down(),
            f64::MIN_POSITIVE,
            2.0_f64.powi(-538),
            2.0_f64.powi(-537),
            0.5,
            0.5_f64.next_up(),
            2.0_f64.powi(52),
            2.0_f64.powi(145),
            2.0_f64.powi(300),
            f64::MAX,
        ];
        for a in operands {
            for b in operands {
                let a = black_box(a);
                let b = black_box(-b);
                assert_eq!(product(a, b).to_bits(), (a * b).to_bits());
            }
        }
        assert!(product(f64::INFINITY, -0.0).is_nan());
    }

    #[test]
    fn squares_round_on_both_sides_of_underflow_boundaries() {
        let small = 2.0_f64.powi(-538);
        for edge in [small, small * std::f64::consts::SQRT_2, 2.0_f64.powi(-511)] {
            for x in [edge.next_down(), edge, edge.next_up()] {
                for x in [black_box(x), black_box(-x)] {
                    assert_eq!(square(x).to_bits(), (x * x).to_bits());
                }
            }
        }
    }

    #[test]
    fn tiny_product_rounds_once_on_either_side_of_zero() {
        // In least-subnormal units this exact magnitude is just below a
        // half-integer. Rounding the scaled product before adding the bias
        // gives the halfway value and the opposite ties-to-even decision.
        let coefficient = black_box(f64::from_bits(0x3fdf_ffff_ffff_fff8));
        let x = black_box(f64::from_bits(0x0008_0000_0000_0001));
        for sample in [x, -x] {
            let reference = coefficient * sample;
            assert_eq!(reference.abs().to_bits(), (1 << 50) - 1);
            assert_eq!(
                down(mul(coefficient, up(sample))).to_bits(),
                reference.to_bits()
            );
        }
    }

    #[test]
    fn small_coefficient_rounds_once_and_preserves_the_zero_sign() {
        let least = f64::from_bits(1);
        // The last coefficient and last two samples straddle the exponent
        // cutoff for a definitely zero product. The larger pair rounds to one unit.
        for coefficient in [
            least,
            -least,
            f64::MIN_POSITIVE.next_down(),
            f64::from_bits((10 << 52) - 1),
        ] {
            for input in [
                0.0_f64,
                0.5_f64.next_down(),
                0.5,
                0.5_f64.next_up(),
                2.0,
                f64::from_bits((961 << 52) - 1),
                f64::from_bits((962 << 52) - 1),
            ] {
                for x in [input, -input] {
                    let native = black_box(coefficient) * black_box(x);
                    assert_eq!(down(mul(coefficient, up(x))).to_bits(), native.to_bits());
                }
            }
        }
    }
}
