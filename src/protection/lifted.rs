// The model uses this coordinate change only with the checked coefficient and
// operand ranges below. Each value in these coordinates is an exact multiple
// of 2^-562, the lifted least binary64 subnormal.
const SIGN: u64 = 1 << 63;
const FRACTION: u64 = (1 << 52) - 1;
const SHIFT: u64 = 512 << 52;
const GRID_BOUNDARY: f64 = f64::from_bits(513 << 52); // 2^-510

fn magnitude(x: f64) -> u64 {
    x.to_bits() & !SIGN
}

pub(super) fn coefficient(x: f64) -> bool {
    let bits = magnitude(x);
    bits == 0 || ((895 << 52)..=(1151 << 52)).contains(&bits) // 2^-128 .. 2^128
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

#[cfg(test)]
mod tests {
    use super::{down, mul, up};
    use std::hint::black_box;

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
}
