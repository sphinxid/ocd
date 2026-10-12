//! Serde helpers used by the generated types.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A JSON object with insertion order preserved (`{type: object}` schemas).
pub type JsonObject = serde_json::Map<String, serde_json::Value>;

/// Define a zero-sized marker type for a single-value string enum (a discriminator such as
/// `"type": "text"` or `"_tag": "SessionNotFoundError"`). It serializes to the literal and only
/// deserializes from it, which is what lets `#[serde(untagged)]` unions pick the right variant.
#[macro_export]
macro_rules! lit_str {
    ($name:ident, $value:literal) => {
        #[doc = concat!("The literal string `", $value, "`.")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name;

        impl $name {
            /// The literal value.
            pub const VALUE: &'static str = $value;
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str($value)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = <::std::borrow::Cow<'de, str> as ::serde::Deserialize>::deserialize(deserializer)?;
                if s == $value {
                    Ok($name)
                } else {
                    Err(<D::Error as ::serde::de::Error>::custom(format!(
                        "expected {:?}, got {:?}",
                        $value, s
                    )))
                }
            }
        }
    };
}

/// Marker type for a single-value boolean enum (`{type: boolean, enum: [false]}`).
#[macro_export]
macro_rules! lit_bool {
    ($name:ident, $value:literal) => {
        #[doc = concat!("The literal boolean `", stringify!($value), "`.")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name;

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_bool($value)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let b = <bool as ::serde::Deserialize>::deserialize(deserializer)?;
                if b == $value {
                    Ok($name)
                } else {
                    Err(<D::Error as ::serde::de::Error>::custom(format!("expected {}", $value)))
                }
            }
        }
    };
}

/// Marker type for a single-value integer enum (e.g. `version: 1`).
#[macro_export]
macro_rules! lit_int {
    ($name:ident, $value:literal) => {
        #[doc = concat!("The literal integer `", stringify!($value), "`.")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name;

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_i64($value)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let n = <f64 as ::serde::Deserialize>::deserialize(deserializer)?;
                if n == $value as f64 {
                    Ok($name)
                } else {
                    Err(<D::Error as ::serde::de::Error>::custom(format!("expected {}", $value)))
                }
            }
        }
    };
}

/// Serialize an `f64` the way `JSON.stringify` does: integral values without a fraction.
pub fn ser_f64<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    write_number(*value, serializer)
}

/// `Option<f64>` variant of [`ser_f64`].
pub fn ser_opt_f64<S: Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => write_number(*v, serializer),
        None => serializer.serialize_none(),
    }
}

/// `Vec<f64>` variant of [`ser_f64`].
pub fn ser_vec_f64<S: Serializer>(value: &[f64], serializer: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(value.len()))?;
    for v in value {
        seq.serialize_element(&JsNumber(*v))?;
    }
    seq.end()
}

/// `Option<Vec<f64>>` variant of [`ser_f64`].
pub fn ser_opt_vec_f64<S: Serializer>(value: &Option<Vec<f64>>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => ser_vec_f64(v, serializer),
        None => serializer.serialize_none(),
    }
}

fn write_number<S: Serializer>(v: f64, serializer: S) -> Result<S::Ok, S::Error> {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
        serializer.serialize_i64(v as i64)
    } else if v.is_finite() {
        serializer.serialize_f64(v)
    } else {
        // JSON.stringify writes non-finite numbers as null.
        serializer.serialize_none()
    }
}

/// A number that may be non-finite. Effect encodes such numbers as the strings `"Infinity"`,
/// `"-Infinity"` or `"NaN"` (`anyOf: [number, enum[...]]` in the OpenAPI). Finite integral values
/// are written without a fraction, like `JSON.stringify`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct JsNumber(pub f64);

impl From<f64> for JsNumber {
    fn from(v: f64) -> Self {
        JsNumber(v)
    }
}

impl From<i64> for JsNumber {
    fn from(v: i64) -> Self {
        JsNumber(v as f64)
    }
}

impl From<JsNumber> for f64 {
    fn from(v: JsNumber) -> Self {
        v.0
    }
}

impl Serialize for JsNumber {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let v = self.0;
        if v.is_nan() {
            serializer.serialize_str("NaN")
        } else if v == f64::INFINITY {
            serializer.serialize_str("Infinity")
        } else if v == f64::NEG_INFINITY {
            serializer.serialize_str("-Infinity")
        } else {
            write_number(v, serializer)
        }
    }
}

impl<'de> Deserialize<'de> for JsNumber {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Num(f64),
            Str(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Num(n) => Ok(JsNumber(n)),
            Raw::Str(s) => match s.as_str() {
                "Infinity" => Ok(JsNumber(f64::INFINITY)),
                "-Infinity" => Ok(JsNumber(f64::NEG_INFINITY)),
                "NaN" => Ok(JsNumber(f64::NAN)),
                other => Err(serde::de::Error::custom(format!("expected a number, got {other:?}"))),
            },
        }
    }
}

/// Format a finite `f64` exactly like ECMAScript `Number.prototype.toString` (which `JSON.stringify` uses):
/// integral values without a fraction, decimal notation for 1e-7 < |x| < 1e21, otherwise `1.5e+21` / `1e-7`.
/// Non-finite values become `null`, `-0` becomes `0`.
pub fn js_number_string(x: f64) -> String {
    if !x.is_finite() {
        return "null".into();
    }
    if x == 0.0 {
        return "0".into();
    }
    let mut buf = ryu::Buffer::new();
    let shortest = buf.format_finite(x.abs());
    // Split ryu's output into significant digits and a decimal exponent `n` with value = 0.d1d2.. * 10^n.
    let (mantissa, exp) = match shortest.find('e') {
        Some(i) => (&shortest[..i], shortest[i + 1..].parse::<i32>().unwrap_or(0)),
        None => (shortest, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let frac_part = if frac_part == "0" { "" } else { frac_part };
    let mut digits: String = format!("{int_part}{frac_part}");
    let mut n = int_part.len() as i32 + exp;
    let lead = digits.len() - digits.trim_start_matches('0').len();
    digits.drain(..lead);
    n -= lead as i32;
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let k = digits.len() as i32;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        if k == 1 { format!("{digits}e{sign}{}", e.abs()) } else { format!("{}.{}e{sign}{}", &digits[..1], &digits[1..], e.abs()) }
    };
    if x < 0.0 { format!("-{body}") } else { body }
}

/// A `serde_json` formatter that writes floating-point numbers like `JSON.stringify` (see
/// [`js_number_string`]). Every JSON body and SSE frame the server sends goes through it, so `f64`
/// fields (costs, ratios, timestamps computed as floats) match upstream byte for byte.
#[derive(Clone, Copy, Default)]
pub struct JsFormatter;

impl serde_json::ser::Formatter for JsFormatter {
    fn write_f64<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, value: f64) -> std::io::Result<()> {
        writer.write_all(js_number_string(value).as_bytes())
    }

    fn write_f32<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, value: f32) -> std::io::Result<()> {
        writer.write_all(js_number_string(value as f64).as_bytes())
    }
}

/// `serde_json::to_vec` with [`JsFormatter`].
pub fn to_js_vec<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(128);
    let mut ser = serde_json::Serializer::with_formatter(&mut out, JsFormatter);
    value.serialize(&mut ser)?;
    Ok(out)
}

/// `serde_json::to_string` with [`JsFormatter`].
pub fn to_js_string<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    // JsFormatter only writes ASCII number text, serde_json writes valid UTF-8 for the rest.
    to_js_vec(value).map(|v| String::from_utf8(v).expect("serde_json output is UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct T {
        #[serde(serialize_with = "ser_f64")]
        a: f64,
        #[serde(serialize_with = "ser_opt_f64")]
        b: Option<f64>,
    }

    #[test]
    fn integral_numbers_have_no_fraction() {
        let s = serde_json::to_string(&T { a: 1760000000000.0, b: Some(0.5) }).unwrap();
        assert_eq!(s, r#"{"a":1760000000000,"b":0.5}"#);
    }

    #[test]
    fn js_number_formatting_matches_ecmascript() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-3.0, "-3"),
            (0.5, "0.5"),
            (0.1 + 0.2, "0.30000000000000004"),
            (123.456, "123.456"),
            (1760000000000.0, "1760000000000"),
            (1e16, "10000000000000000"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (1e-6, "0.000001"),
            (1.5e-6, "0.0000015"),
            (1e-7, "1e-7"),
            (-2.5e-8, "-2.5e-8"),
            (0.00001, "0.00001"),
            (f64::NAN, "null"),
            (f64::INFINITY, "null"),
        ];
        for (x, want) in cases {
            assert_eq!(js_number_string(*x), *want, "{x:?}");
        }
        let v = serde_json::json!({"cost": 0.0, "n": 3, "f": 0.25, "list": [1.0, 2.5]});
        assert_eq!(to_js_string(&v).unwrap(), r#"{"cost":0,"n":3,"f":0.25,"list":[1,2.5]}"#);
    }

    #[test]
    fn js_number_round_trip() {
        assert_eq!(serde_json::to_string(&JsNumber(f64::INFINITY)).unwrap(), "\"Infinity\"");
        assert_eq!(serde_json::to_string(&JsNumber(3.0)).unwrap(), "3");
        let n: JsNumber = serde_json::from_str("\"-Infinity\"").unwrap();
        assert_eq!(n.0, f64::NEG_INFINITY);
        let n: JsNumber = serde_json::from_str("2.5").unwrap();
        assert_eq!(n.0, 2.5);
        assert!(serde_json::from_str::<JsNumber>("\"x\"").is_err());
    }
}
