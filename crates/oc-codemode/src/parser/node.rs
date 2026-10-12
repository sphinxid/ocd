//! Parser-internal node shapes and numeric helpers.

use crate::ast::{Expr, Loc, Pat};

/// acorn's `forInit` argument: `false`, `true`, or `"await"` (the head of a `for await` loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InFor {
    No,
    Yes,
    Await,
}

impl InFor {
    pub(super) fn is_set(self) -> bool {
        self != InFor::No
    }

    pub(super) fn from_bool(b: bool) -> InFor {
        if b { InFor::Yes } else { InFor::No }
    }
}

/// An element of a list that may become an arrow function's parameter list: a parenthesized expression list
/// (which may end in a rest binding) or the argument list of `async(...)`.
#[derive(Debug)]
pub(super) enum ArrowParam {
    Expr(Expr),
    /// A spread argument of `async(...)`.
    Spread {
        argument: Expr,
        loc: Loc,
    },
    /// `...binding` in a parenthesized list (already a pattern).
    Rest(Pat),
}

/// A class element, reduced to what acorn's class checks look at.
pub(super) struct ClassElement {
    pub is_method: bool,
    pub kind: &'static str,
    pub is_static: bool,
    pub start: usize,
    /// The key of a method or field (`None` for static blocks).
    pub key: Option<crate::ast::PropKey>,
}

/// Multiplies the little-endian base-2^32 number `n` by `mul` and adds `add`.
fn mul_add(n: &mut Vec<u32>, mul: u32, add: u32) {
    let mut carry = add as u64;
    for limb in n.iter_mut() {
        let v = (*limb as u64) * (mul as u64) + carry;
        *limb = v as u32;
        carry = v >> 32;
    }
    if carry > 0 {
        n.push(carry as u32);
    }
}

/// Divides `n` by `div` in place and returns the remainder.
fn div_rem(n: &mut Vec<u32>, div: u32) -> u32 {
    let mut rem = 0u64;
    for limb in n.iter_mut().rev() {
        let v = (rem << 32) | *limb as u64;
        *limb = (v / div as u64) as u32;
        rem = v % div as u64;
    }
    while n.last() == Some(&0) {
        n.pop();
    }
    rem as u32
}

/// Converts digits (most significant first) in `radix` to a decimal string, like `BigInt(...).toString()`.
pub(super) fn big_digits_to_decimal(digits: &[u32], radix: u32) -> String {
    let mut n: Vec<u32> = Vec::new();
    for &d in digits {
        mul_add(&mut n, radix, d);
    }
    while n.last() == Some(&0) {
        n.pop();
    }
    if n.is_empty() {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while !n.is_empty() {
        let r = div_rem(&mut n, 1_000_000_000);
        if n.is_empty() {
            out.push(r.to_string());
        } else {
            out.push(format!("{r:09}"));
        }
    }
    out.reverse();
    out.concat()
}

/// The correctly rounded `f64` value of digits (most significant first) in `radix` (`parseInt(str, radix)`).
pub(super) fn f64_from_radix_digits(digits: &[u32], radix: u32) -> f64 {
    big_digits_to_decimal(digits, radix).parse::<f64>().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_conversion() {
        assert_eq!(big_digits_to_decimal(&[0], 16), "0");
        assert_eq!(big_digits_to_decimal(&[1, 15], 16), "31");
        let ff: Vec<u32> = vec![15; 32];
        assert_eq!(big_digits_to_decimal(&ff, 16), "340282366920938463463374607431768211455");
        assert_eq!(f64_from_radix_digits(&[7, 7, 7], 8), 511.0);
    }
}
