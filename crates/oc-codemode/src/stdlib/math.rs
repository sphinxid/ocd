//! `Math`: constants, the numeric methods, `random`, and `sumPrecise`. Port of `stdlib/math.ts`. Upstream delegates
//! to the host's `Math`, so each method reproduces JavaScriptCore's computation: the transcendental functions call the
//! platform libm like JavaScriptCore does, and `pow`, `hypot`, `round`, and `sumPrecise` follow its own algorithms.

use crate::interp::coerce::{with_primitives, Hint};
use crate::interp::errors::{type_error, JsResult};
use crate::interp::function::{arg, constants, method, methods, Method, NResult};
use crate::interp::iteration::{self, preserve_consumer_error};
use crate::interp::Rt;
use crate::number::{to_int32, to_uint32};
use crate::object::Obj;
use crate::stdlib::number::to_number;
use crate::value::Value;

mod libm {
    unsafe extern "C" {
        pub safe fn acos(x: f64) -> f64;
        pub safe fn acosh(x: f64) -> f64;
        pub safe fn asin(x: f64) -> f64;
        pub safe fn asinh(x: f64) -> f64;
        pub safe fn atan(x: f64) -> f64;
        pub safe fn atan2(y: f64, x: f64) -> f64;
        pub safe fn atanh(x: f64) -> f64;
        pub safe fn cbrt(x: f64) -> f64;
        pub safe fn cos(x: f64) -> f64;
        pub safe fn cosh(x: f64) -> f64;
        pub safe fn sin(x: f64) -> f64;
        pub safe fn sinh(x: f64) -> f64;
        pub safe fn tan(x: f64) -> f64;
        pub safe fn tanh(x: f64) -> f64;
        pub safe fn log(x: f64) -> f64;
        pub safe fn log2(x: f64) -> f64;
        pub safe fn log10(x: f64) -> f64;
        pub safe fn log1p(x: f64) -> f64;
        pub safe fn exp(x: f64) -> f64;
        pub safe fn expm1(x: f64) -> f64;
        pub safe fn pow(x: f64, y: f64) -> f64;
        pub safe fn hypot(x: f64, y: f64) -> f64;
    }
}

/// Math.round: the nearest integer, ties toward +Infinity, keeping -0 for inputs in [-0.5, -0].
pub fn round(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    if x > 0.0 && x < 0.5 {
        return 0.0;
    }
    if x < 0.0 && x >= -0.5 {
        return -0.0;
    }
    let floor = x.floor();
    if x - floor >= 0.5 { floor + 1.0 } else { floor }
}

pub fn sign(x: f64) -> f64 {
    if x.is_nan() || x == 0.0 { x } else { x.signum() }
}

/// Math.pow as JavaScriptCore computes it: the ECMAScript special cases, `sqrt` for exponents ±0.5, repeated
/// squaring for small non-negative integer exponents, and C `pow` otherwise.
pub fn pow(base: f64, exponent: f64) -> f64 {
    if exponent.is_nan() {
        return f64::NAN;
    }
    if exponent == 0.0 {
        return 1.0;
    }
    let magnitude = base.abs();
    if magnitude == 1.0 && exponent.is_infinite() {
        return f64::NAN;
    }
    if exponent == 0.5 {
        if magnitude == 0.0 {
            return 0.0;
        }
        if magnitude == f64::INFINITY {
            return f64::INFINITY;
        }
        return base.sqrt();
    }
    if exponent == -0.5 {
        if magnitude == 0.0 {
            return f64::INFINITY;
        }
        if magnitude == f64::INFINITY {
            return 0.0;
        }
        return 1.0 / base.sqrt();
    }
    if exponent.fract() == 0.0 && (0.0..=MAX_INTEGER_POW_EXPONENT).contains(&exponent) {
        let mut n = exponent as u32;
        let mut x = base;
        let mut result = 1.0;
        while n != 0 {
            if n & 1 == 1 {
                result *= x;
            }
            x *= x;
            n >>= 1;
        }
        return result;
    }
    libm::pow(base, exponent)
}

const MAX_INTEGER_POW_EXPONENT: f64 = 1000.0;

/// Math.hypot as JavaScriptCore computes it: Infinity wins over NaN; two arguments use C `hypot`, three libstdc++'s
/// `std::hypot(x, y, z)` (`max * sqrt(sum((x / max)^2))`), and more a running fused multiply-add over the largest
/// magnitude seen so far.
pub fn hypot(values: &[f64]) -> f64 {
    if values.iter().any(|v| v.is_infinite()) {
        return f64::INFINITY;
    }
    if values.iter().any(|v| v.is_nan()) {
        return f64::NAN;
    }
    match values {
        [] => 0.0,
        [x] => x.abs(),
        [x, y] => libm::hypot(*x, *y),
        [_, _, _] => {
            let max = values.iter().fold(0f64, |m, v| m.max(v.abs()));
            if max == 0.0 {
                return 0.0;
            }
            let sum = values.iter().fold(0f64, |sum, v| sum + (v.abs() / max) * (v.abs() / max));
            max * sum.sqrt()
        }
        _ => {
            let mut max = 0f64;
            let mut sum = 0f64;
            for v in values {
                let a = v.abs();
                if max < a {
                    let scaled = max / a;
                    sum = sum.mul_add(scaled * scaled, 1.0);
                    max = a;
                } else if max != 0.0 {
                    let scaled = a / max;
                    sum = scaled.mul_add(scaled, sum);
                }
            }
            if max == 0.0 {
                return 0.0;
            }
            sum.sqrt() * max
        }
    }
}

pub fn max(values: &[f64]) -> f64 {
    let mut result = f64::NEG_INFINITY;
    for &v in values {
        if v.is_nan() {
            result = f64::NAN;
        } else if !result.is_nan() && (v > result || (v == 0.0 && result == 0.0 && result.is_sign_negative())) {
            result = v;
        }
    }
    result
}

pub fn min(values: &[f64]) -> f64 {
    let mut result = f64::INFINITY;
    for &v in values {
        if v.is_nan() {
            result = f64::NAN;
        } else if !result.is_nan() && (v < result || (v == 0.0 && result == 0.0 && v.is_sign_negative())) {
            result = v;
        }
    }
    result
}

/// Math.f16round: round to the nearest IEEE 754 binary16 value (ties to even), directly from the double.
pub fn f16round(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let a = x.abs();
    // Below 2^-14 binary16 is subnormal with a fixed quantum of 2^-24; above, 10 fraction bits.
    let exponent = (((a.to_bits() >> 52) & 0x7ff) as i32 - 1023).max(-14);
    let quantum = 2f64.powi(exponent - 10);
    let rounded = (a / quantum).round_ties_even() * quantum;
    let result = if rounded > 65504.0 { f64::INFINITY } else { rounded };
    if x < 0.0 { -result } else { result }
}

pub fn unary(name: &str, x: f64) -> f64 {
    match name {
        "abs" => x.abs(),
        "acos" => libm::acos(x),
        "acosh" => libm::acosh(x),
        "asin" => libm::asin(x),
        "asinh" => libm::asinh(x),
        "atan" => libm::atan(x),
        "atanh" => libm::atanh(x),
        "floor" => x.floor(),
        "ceil" => x.ceil(),
        "round" => round(x),
        "trunc" => x.trunc(),
        "sign" => sign(x),
        "sqrt" => x.sqrt(),
        "cbrt" => libm::cbrt(x),
        "cos" => libm::cos(x),
        "cosh" => libm::cosh(x),
        "sin" => libm::sin(x),
        "sinh" => libm::sinh(x),
        "tan" => libm::tan(x),
        "tanh" => libm::tanh(x),
        "log" => libm::log(x),
        "log2" => libm::log2(x),
        "log10" => libm::log10(x),
        "log1p" => libm::log1p(x),
        "exp" => libm::exp(x),
        "expm1" => libm::expm1(x),
        "f16round" => f16round(x),
        "fround" => x as f32 as f64,
        "clz32" => to_uint32(x).leading_zeros() as f64,
        _ => unreachable!("unknown Math method {name}"),
    }
}

pub fn binary(name: &str, a: f64, b: f64) -> f64 {
    match name {
        "atan2" => libm::atan2(a, b),
        "pow" => pow(a, b),
        "imul" => to_int32(a).wrapping_mul(to_int32(b)) as f64,
        _ => unreachable!("unknown Math method {name}"),
    }
}

// Math.sumPrecise as the host computes it: WebKit's port of Radford Neal's xsum (WTF/PreciseSum.cpp), including that
// port's rounding of negative sums (an exactly representable negative total can come back one unit larger in
// magnitude). Arrays longer than 1000 elements use the large accumulator, like JavaScriptCore.

const MANTISSA_BITS: i64 = 52;
const EXP_MASK: i64 = (1 << 11) - 1;
const EXP_BIAS: i64 = (1 << 10) - 1;
const MANTISSA_MASK: i64 = (1 << MANTISSA_BITS) - 1;
const SIGN_MASK: i64 = i64::MIN;
const LOW_EXP_BITS: i64 = 5;
const LOW_EXP_MASK: i64 = (1 << LOW_EXP_BITS) - 1;
const SCHUNKS: usize = (1 << 6) + 3;
const LOW_MANTISSA_BITS: i64 = 1 << LOW_EXP_BITS;
const LOW_MANTISSA_MASK: i64 = (1 << LOW_MANTISSA_BITS) - 1;
const SMALL_CARRY_TERMS: i64 = (1 << (63 - MANTISSA_BITS)) - 1;
const LCOUNT_BITS: i64 = 64 - MANTISSA_BITS;
const LCHUNKS: usize = 1 << 12;
/// Above this many elements the host uses the large accumulator.
const PRECISE_SUM_THRESHOLD: usize = 1000;

#[derive(Clone)]
struct SmallAccumulator {
    chunk: [i64; SCHUNKS],
    adds_until_propagate: i64,
    inf: i64,
    nan: i64,
    size_count: usize,
    has_pos_number: bool,
}

impl SmallAccumulator {
    fn new() -> Self {
        SmallAccumulator {
            chunk: [0; SCHUNKS],
            adds_until_propagate: SMALL_CARRY_TERMS,
            inf: 0,
            nan: 0,
            size_count: 0,
            has_pos_number: false,
        }
    }

    fn add_inf_nan(&mut self, ivalue: i64) {
        let mantissa = ivalue & MANTISSA_MASK;
        if mantissa == 0 {
            if self.inf == 0 {
                self.inf = ivalue;
            } else if self.inf != ivalue {
                let f = f64::from_bits(ivalue as u64);
                self.inf = (f - f).to_bits() as i64;
            }
        } else if (self.nan & MANTISSA_MASK) <= mantissa {
            self.nan = ivalue & !SIGN_MASK;
        }
    }

    /// Propagates carries; returns the index of the uppermost non-zero chunk (0 when the sum is zero).
    fn carry_propagate(&mut self) -> usize {
        let mut u = SCHUNKS as i64 - 1;
        while 0 <= u && self.chunk[u as usize] == 0 {
            if u == 0 {
                self.adds_until_propagate = SMALL_CARRY_TERMS - 1;
                return 0;
            }
            u -= 1;
        }
        let mut i: i64 = 0;
        let mut uix: i64 = -1;
        'outer: loop {
            let mut c;
            loop {
                c = self.chunk[i as usize];
                if c != 0 {
                    break;
                }
                i += 1;
                if i > u {
                    break;
                }
            }
            if i > u {
                break 'outer;
            }
            let chigh = c >> LOW_MANTISSA_BITS;
            if chigh == 0 {
                uix = i;
                i += 1;
                if i <= u {
                    continue 'outer;
                }
                break 'outer;
            }
            if u == i {
                if chigh == -1 {
                    uix = i;
                    break 'outer;
                }
                u = i + 1;
            }
            let clow = c & LOW_MANTISSA_MASK;
            if clow != 0 {
                uix = i;
            }
            self.chunk[i as usize] = clow;
            if i + 1 >= SCHUNKS as i64 {
                self.add_inf_nan((EXP_MASK << MANTISSA_BITS) | MANTISSA_MASK);
                u = i;
            } else {
                self.chunk[i as usize + 1] = self.chunk[i as usize + 1].wrapping_add(chigh);
            }
            i += 1;
            if i > u {
                break;
            }
        }
        if uix < 0 {
            self.adds_until_propagate = SMALL_CARRY_TERMS - 1;
            return 0;
        }
        while self.chunk[uix as usize] == -1 && uix > 0 {
            let below = uix as usize - 1;
            self.chunk[below] = self.chunk[below].wrapping_add(-(1i64 << LOW_MANTISSA_BITS));
            self.chunk[uix as usize] = 0;
            uix -= 1;
        }
        self.adds_until_propagate = SMALL_CARRY_TERMS - 1;
        uix as usize
    }

    fn add1_no_carry(&mut self, value: f64) {
        let ivalue = value.to_bits() as i64;
        let exp = (ivalue >> MANTISSA_BITS) & EXP_MASK;
        let mut mantissa = ivalue & MANTISSA_MASK;
        let high_exp = (exp >> LOW_EXP_BITS) as usize;
        let mut low_exp = exp & LOW_EXP_MASK;
        if exp == 0 {
            if mantissa == 0 {
                return;
            }
            low_exp = 1;
        } else if exp == EXP_MASK {
            self.add_inf_nan(ivalue);
            return;
        } else {
            mantissa |= 1 << MANTISSA_BITS;
        }
        let low = (((mantissa as u64) << low_exp) as i64) & LOW_MANTISSA_MASK;
        let high = mantissa >> (LOW_MANTISSA_BITS - low_exp);
        if ivalue < 0 {
            self.chunk[high_exp] = self.chunk[high_exp].wrapping_sub(low);
            self.chunk[high_exp + 1] = self.chunk[high_exp + 1].wrapping_sub(high);
        } else {
            self.chunk[high_exp] = self.chunk[high_exp].wrapping_add(low);
            self.chunk[high_exp + 1] = self.chunk[high_exp + 1].wrapping_add(high);
        }
    }

    fn count_value(&mut self, value: f64) {
        self.size_count += 1;
        self.has_pos_number = self.has_pos_number || !value.is_sign_negative();
    }

    fn add(&mut self, value: f64) {
        self.count_value(value);
        if self.adds_until_propagate == 0 {
            self.carry_propagate();
        }
        self.add1_no_carry(value);
        self.adds_until_propagate -= 1;
    }

    /// Rounds the accumulated sum to a double.
    fn compute(&mut self) -> f64 {
        if self.nan != 0 {
            return f64::from_bits(self.nan as u64);
        }
        if self.inf != 0 {
            return f64::from_bits(self.inf as u64);
        }
        if self.size_count == 0 {
            return -0.0;
        }
        let i = self.carry_propagate();
        let mut ivalue = self.chunk[i];
        if i <= 1 {
            if ivalue == 0 {
                return if self.has_pos_number { 0.0 } else { -0.0 };
            }
            if i == 0 {
                let mut intv = ivalue.wrapping_abs() >> 1;
                if ivalue < 0 {
                    intv |= SIGN_MASK;
                }
                return f64::from_bits(intv as u64);
            }
            let intv = ivalue.wrapping_mul(1i64 << (LOW_MANTISSA_BITS - 1)).wrapping_add(self.chunk[0] >> 1);
            if intv < 0 {
                if intv > -(1i64 << MANTISSA_BITS) {
                    return f64::from_bits(((-intv) | SIGN_MASK) as u64);
                }
            } else if (intv as u64) < 1u64 << MANTISSA_BITS {
                return f64::from_bits(intv as u64);
            }
        }
        let fltv = ivalue as f64;
        let mut intv = fltv.to_bits() as i64;
        let mut e = (intv >> MANTISSA_BITS) & EXP_MASK;
        let mut more = 2 + MANTISSA_BITS + EXP_BIAS - e;
        ivalue = ivalue.wrapping_mul(1i64.wrapping_shl(more as u32));
        let mut j = i as i64 - 1;
        let mut lower = self.chunk[j as usize];
        if more >= LOW_MANTISSA_BITS {
            more -= LOW_MANTISSA_BITS;
            ivalue = ivalue.wrapping_add(lower.wrapping_shl(more as u32));
            j -= 1;
            lower = if j < 0 { 0 } else { self.chunk[j as usize] };
        }
        ivalue = ivalue.wrapping_add(lower >> (LOW_MANTISSA_BITS - more));
        lower &= (1i64 << (LOW_MANTISSA_BITS - more)) - 1;

        let mut round_away = false;
        if 0 <= ivalue {
            intv = 0;
            if ivalue & 2 == 0 {
                round_away = false;
            } else if ivalue & 1 != 0 || ivalue & 4 != 0 {
                round_away = true;
            } else {
                if lower == 0 {
                    while j > 0 {
                        j -= 1;
                        if self.chunk[j as usize] != 0 {
                            lower = 1;
                            break;
                        }
                    }
                }
                if lower != 0 {
                    round_away = true;
                }
            }
        } else {
            if (ivalue.wrapping_neg()) & (1i64 << (MANTISSA_BITS + 2)) == 0 {
                // An `int` in the host: bit 31 sign-extends.
                let pos = ((1i64 << (LOW_MANTISSA_BITS - 1 - more)) as i32) as i64;
                ivalue = ivalue.wrapping_mul(2);
                if lower & pos != 0 {
                    ivalue += 1;
                    lower &= !pos;
                }
                e -= 1;
            }
            intv = SIGN_MASK;
            ivalue = ivalue.wrapping_neg();
            if ivalue & 3 == 3 {
                round_away = true;
            }
            if lower == 0 {
                while j > 0 {
                    j -= 1;
                    if self.chunk[j as usize] != 0 {
                        lower = 1;
                        break;
                    }
                }
            }
            if lower == 0 {
                round_away = true;
            }
        }
        if round_away {
            ivalue = ivalue.wrapping_add(4);
            if ivalue & (1i64 << (MANTISSA_BITS + 3)) != 0 {
                ivalue >>= 1;
                e += 1;
            }
        }
        ivalue >>= 2;
        e += ((i as i64) << LOW_EXP_BITS) - EXP_BIAS - MANTISSA_BITS;
        if e >= EXP_MASK {
            intv |= EXP_MASK << MANTISSA_BITS;
            return f64::from_bits(intv as u64);
        }
        intv = intv.wrapping_add((e << MANTISSA_BITS).wrapping_add(ivalue & MANTISSA_MASK));
        f64::from_bits(intv as u64)
    }
}

struct LargeAccumulator {
    chunk: Vec<u64>,
    count: Vec<i16>,
    chunks_used: Vec<u64>,
    used_used: u64,
    sacc: SmallAccumulator,
}

impl LargeAccumulator {
    fn new() -> Self {
        LargeAccumulator {
            chunk: vec![0; LCHUNKS],
            count: vec![-1; LCHUNKS],
            chunks_used: vec![0; LCHUNKS / 64],
            used_used: 0,
            sacc: SmallAccumulator::new(),
        }
    }

    fn add_lchunk_to_small(&mut self, ix: usize) {
        let count = self.count[ix] as i64;
        if count >= 0 {
            if self.sacc.adds_until_propagate == 0 {
                self.sacc.carry_propagate();
            }
            let mut chunk = self.chunk[ix];
            if count > 0 {
                chunk = chunk.wrapping_add(((count * ix as i64) as u64) << MANTISSA_BITS);
            }
            let exp = ix as i64 & EXP_MASK;
            let mut low_exp = exp & LOW_EXP_MASK;
            let mut high_exp = (exp >> LOW_EXP_BITS) as usize;
            if exp == 0 {
                low_exp = 1;
                high_exp = 0;
            }
            let low_chunk = (chunk << low_exp) & LOW_MANTISSA_MASK as u64;
            let mut mid_chunk = chunk >> (LOW_MANTISSA_BITS - low_exp);
            if exp != 0 {
                mid_chunk = mid_chunk.wrapping_add(
                    (((1i64 << LCOUNT_BITS) - count) as u64) << (MANTISSA_BITS - LOW_MANTISSA_BITS + low_exp),
                );
            }
            let high_chunk = mid_chunk >> LOW_MANTISSA_BITS;
            mid_chunk &= LOW_MANTISSA_MASK as u64;
            let s = &mut self.sacc.chunk;
            if ix & (1 << 11) != 0 {
                s[high_exp] = s[high_exp].wrapping_sub(low_chunk as i64);
                s[high_exp + 1] = s[high_exp + 1].wrapping_sub(mid_chunk as i64);
                s[high_exp + 2] = s[high_exp + 2].wrapping_sub(high_chunk as i64);
            } else {
                s[high_exp] = s[high_exp].wrapping_add(low_chunk as i64);
                s[high_exp + 1] = s[high_exp + 1].wrapping_add(mid_chunk as i64);
                s[high_exp + 2] = s[high_exp + 2].wrapping_add(high_chunk as i64);
            }
            self.sacc.adds_until_propagate -= 1;
        }
        self.chunk[ix] = 0;
        self.count[ix] = 1 << LCOUNT_BITS;
        self.chunks_used[ix >> 6] |= 1u64 << (ix & 0x3f);
        self.used_used |= 1u64 << (ix >> 6);
    }

    fn add(&mut self, value: f64) {
        let uintv = value.to_bits();
        let ix = (uintv >> MANTISSA_BITS) as usize;
        let count = self.count[ix] - 1;
        self.sacc.count_value(value);
        if count < 0 {
            if ix as i64 & EXP_MASK == EXP_MASK {
                self.sacc.add_inf_nan(uintv as i64);
            } else {
                self.add_lchunk_to_small(ix);
                self.count[ix] -= 1;
                self.chunk[ix] = self.chunk[ix].wrapping_add(uintv);
            }
        } else {
            self.count[ix] = count;
            self.chunk[ix] = self.chunk[ix].wrapping_add(uintv);
        }
    }

    fn transfer_to_small(&mut self) {
        let size = self.chunks_used.len();
        let mut p = 0usize;
        let mut uu = self.used_used;
        if uu & 0xffff_ffff == 0 {
            uu >>= 32;
            p += 32;
        }
        if uu & 0xffff == 0 {
            uu >>= 16;
            p += 16;
        }
        if uu & 0xff == 0 {
            p += 8;
        }
        loop {
            let mut u;
            loop {
                u = self.chunks_used[p];
                if u != 0 {
                    break;
                }
                p += 1;
                if p == size {
                    return;
                }
            }
            let mut ix = p << 6;
            if u & 0xffff_ffff == 0 {
                u >>= 32;
                ix += 32;
            }
            if u & 0xffff == 0 {
                u >>= 16;
                ix += 16;
            }
            if u & 0xff == 0 {
                u >>= 8;
                ix += 8;
            }
            loop {
                if self.count[ix] >= 0 {
                    self.add_lchunk_to_small(ix);
                }
                ix += 1;
                u >>= 1;
                if u == 0 {
                    break;
                }
            }
            p += 1;
            if p >= size {
                break;
            }
        }
    }

    fn compute(&mut self) -> f64 {
        self.transfer_to_small();
        let mut small = self.sacc.clone();
        small.compute()
    }
}

/// Math.sumPrecise over already-collected numbers, as the host computes it.
pub fn sum_precise(values: &[f64]) -> f64 {
    if values.len() > PRECISE_SUM_THRESHOLD {
        let mut sum = LargeAccumulator::new();
        for &v in values {
            sum.add(v);
        }
        return sum.compute();
    }
    let mut sum = SmallAccumulator::new();
    for &v in values {
        sum.add(v);
    }
    sum.compute()
}

// Convert only the arguments a method consumes; like JS, extras are ignored (so built-ins work as callbacks
// receiving (element, index, array)).
fn unary_method(name: &'static str) -> Method {
    method(name, 1, move |rt, site, _, args| {
        with_primitives(rt, site, vec![Hint::Number], vec![arg(&args, 0)], move |p| {
            Ok(Value::Number(unary(name, to_number(&p[0])?)))
        })
    })
}

fn binary_method(name: &'static str) -> Method {
    method(name, 2, move |rt, site, _, args| {
        with_primitives(rt, site, vec![Hint::Number], vec![arg(&args, 0), arg(&args, 1)], move |p| {
            let a = to_number(&p[0])?;
            let b = to_number(&p[1])?;
            Ok(Value::Number(binary(name, a, b)))
        })
    })
}

fn variadic_method(name: &'static str, op: fn(&[f64]) -> f64) -> Method {
    method(name, 2, move |rt, site, _, args| {
        with_primitives(rt, site, vec![Hint::Number], args, move |p| {
            let values = p.iter().map(to_number).collect::<JsResult<Vec<f64>>>()?;
            Ok(Value::Number(op(&values)))
        })
    })
}

pub fn math_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let math = Obj::ordinary(&b.object);
    constants(
        &math,
        vec![
            ("PI", Value::Number(std::f64::consts::PI)),
            ("E", Value::Number(std::f64::consts::E)),
            ("LN2", Value::Number(std::f64::consts::LN_2)),
            ("LN10", Value::Number(std::f64::consts::LN_10)),
            ("LOG2E", Value::Number(std::f64::consts::LOG2_E)),
            ("LOG10E", Value::Number(std::f64::consts::LOG10_E)),
            ("SQRT2", Value::Number(std::f64::consts::SQRT_2)),
            ("SQRT1_2", Value::Number(std::f64::consts::FRAC_1_SQRT_2)),
        ],
    );
    let mut table = vec![
        method("random", 0, |_, _, _, _| NResult::ok(rand::random::<f64>())),
        variadic_method("max", max),
        variadic_method("min", min),
        variadic_method("hypot", hypot),
    ];
    for name in ["abs", "acos", "acosh", "asin", "asinh", "atan"] {
        table.push(unary_method(name));
    }
    table.push(binary_method("atan2"));
    for name in ["atanh", "floor", "ceil", "round", "trunc", "sign", "sqrt", "cbrt"] {
        table.push(unary_method(name));
    }
    table.push(binary_method("pow"));
    for name in [
        "cos", "cosh", "sin", "sinh", "tan", "tanh", "log", "log2", "log10", "log1p", "exp", "expm1", "f16round",
        "fround", "clz32",
    ] {
        table.push(unary_method(name));
    }
    table.push(binary_method("imul"));
    table.push(method("sumPrecise", 1, |rt, site, _, args| {
        let rt = rt.clone();
        let source = arg(&args, 0);
        NResult::future(async move {
            let Some(cursor) = iteration::iterate(&rt, &source, None, site).await? else {
                return Err(type_error("Math.sumPrecise expects a synchronous iterable."));
            };
            let mut numbers = Vec::new();
            loop {
                let step = cursor.next().await?;
                if step.done {
                    return Ok(Value::Number(sum_precise(&numbers)));
                }
                let checked = match step.value {
                    Value::Number(n) => {
                        numbers.push(n);
                        Ok(())
                    }
                    _ => Err(type_error("Math.sumPrecise expects an iterable of numbers.")),
                };
                preserve_consumer_error(&cursor, checked).await?;
            }
        })
    }));
    methods(&b.function, &math, table);
    Value::Object(math)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_sums() {
        assert_eq!(sum_precise(&[1e30, 0.1, -1e30]), 0.1);
        assert_eq!(sum_precise(&[1e308, 1e308, 0.1, 0.1, 1e30, 0.1, -1e30, -1e308, -1e308]), 0.30000000000000004);
        assert_eq!(sum_precise(&[8.98846567431158e307, 8.988465674311579e307, -1.7976931348623157e308]), 9.9792015476736e291);
        assert_eq!(sum_precise(&[1.7976931348623157e308, 1.7976931348623157e308]), f64::INFINITY);
        assert!(sum_precise(&[]).is_sign_negative());
        assert!(!sum_precise(&[-0.0, 0.0]).is_sign_negative());
        assert!(!sum_precise(&[1.0, -1.0]).is_sign_negative());
        assert_eq!(sum_precise(&[5e-324, 5e-324]), 1e-323);
        // The host's xsum port rounds exact negative totals one unit away from zero.
        assert_eq!(sum_precise(&[-2.9926782846450805e-180]), -2.992678284645081e-180);
        assert_eq!(sum_precise(&[2.9926782846450805e-180]), 2.9926782846450805e-180);
        assert_eq!(f16round(1.337), 1.3369140625);
        assert_eq!(f16round(65520.0), f64::INFINITY);
        assert_eq!(f16round(65519.99), 65504.0);
        assert_eq!(f16round(2.9802322387695312e-8), 0.0);
        assert_eq!(round(-2.5), -2.0);
        assert!(round(-0.5).is_sign_negative());
    }
}
