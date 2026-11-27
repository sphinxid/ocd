//! Node's `Buffer` (2.3.1, #64365). Port of `stdlib/buffer.ts`.
//!
//! Upstream builds the global on the host's (Bun's) `Buffer`: a Buffer is a Uint8Array (a [`BytesData`] view marked
//! `node`) whose prototype is `Buffer.prototype`, itself inheriting from `Uint8Array.prototype`. Its own statics,
//! `toString`, `toJSON`, `slice`/`subarray` (views on the same memory) and `equals` are upstream's code; `compare`,
//! `indexOf`/`lastIndexOf`/`includes`, `write`, `fill`, `copy`, the `read*`/`write*` number methods and the swaps are
//! handed to the host Buffer as they are, so they follow Node's `lib/buffer.js` as Bun implements it, with its argument
//! checks and error texts (each error carries Node's `code`). Arguments reach those host methods as upstream passes
//! them: a Uint8Array as a Buffer over the same memory, a primitive as it is, any other object as a number.

use std::rc::Rc;

use crate::interp::coerce::{coerce_to_number, coerce_to_string};
use crate::interp::errors::{ErrorType, JsResult, Thrown, type_error};
use crate::interp::function::{Method, NResult, NativeFn, arg, constructor, method, methods, prototype_from, receiver};
use crate::interp::globals::create_error_value;
use crate::interp::{Rt, describe_value};
use crate::number;
use crate::object::{BytesData, HIDDEN, Obj, ObjKind, define, define_data, get_str, record, to_uint8};
use crate::stdlib::bytes::encode_base64;
use crate::value::{JsStr, Value};

/// Bun's `kMaxLength`, the bound Node's offset checks name.
const MAX_LENGTH: f64 = 4_294_967_296.0;

/// A text encoding a Buffer converts strings with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoding {
    Utf8,
    Hex,
    Base64,
    Base64Url,
    Latin1,
    Ascii,
    Utf16le,
}

impl Encoding {
    /// The encoding a name stands for, ignoring case (upstream's list of names).
    pub fn named(name: &str) -> Option<Encoding> {
        Some(match name.to_ascii_lowercase().as_str() {
            "utf8" | "utf-8" => Encoding::Utf8,
            "hex" => Encoding::Hex,
            "base64" => Encoding::Base64,
            "base64url" => Encoding::Base64Url,
            "latin1" | "binary" => Encoding::Latin1,
            "ascii" => Encoding::Ascii,
            "ucs2" | "ucs-2" | "utf16le" | "utf-16le" => Encoding::Utf16le,
            _ => return None,
        })
    }
}

/// A program error of `t` carrying Node's `code`, thrown.
fn fail(t: ErrorType, message: impl AsRef<str>, code: Option<&str>) -> Thrown {
    let rt = crate::interp::rt();
    let value = create_error_value(rt.builtins.error(t), Some(message.as_ref()));
    if let Some(code) = code {
        define_data(&value, "code", Value::str(code));
    }
    Thrown::Value(Value::Object(value))
}

fn unknown_encoding(name: &str) -> Thrown {
    fail(ErrorType::TypeError, format!("Unknown encoding: {name}"), Some("ERR_UNKNOWN_ENCODING"))
}

/// The encoding an argument names; `None` when it is `undefined`.
fn encoding(value: &Value) -> JsResult<Option<Encoding>> {
    match value {
        Value::Undefined => Ok(None),
        Value::String(s) => Encoding::named(&s.to_rust()).map(Some).ok_or_else(|| unknown_encoding(&s.to_rust())),
        other => Err(unknown_encoding(&coerce_to_string(other).to_rust())),
    }
}

fn is_encoding(value: &Value) -> bool {
    matches!(value, Value::String(s) if Encoding::named(&s.to_rust()).is_some())
}

// --- Text codecs (the host Buffer's) ---

/// A string's bytes in an encoding (`Buffer.from(string, encoding)`).
pub fn encode(units: &[u16], enc: Encoding) -> Vec<u8> {
    match enc {
        Encoding::Utf8 => utf8(units),
        Encoding::Latin1 | Encoding::Ascii => units.iter().map(|&u| u as u8).collect(),
        Encoding::Utf16le => units.iter().flat_map(|u| u.to_le_bytes()).collect(),
        Encoding::Hex => hex_decode(units, usize::MAX),
        Encoding::Base64 | Encoding::Base64Url => {
            let mut out = base64_decode(units);
            out.truncate(base64_decoded_size(units));
            out
        }
    }
}

/// UTF-8, a lone surrogate as U+FFFD.
fn utf8(units: &[u16]) -> Vec<u8> {
    String::from_utf16_lossy(units).into_bytes()
}

/// The UTF-8 length of each character, so writes can stop at a character boundary.
fn utf8_chars(units: &[u16]) -> Vec<Vec<u8>> {
    char::decode_utf16(units.iter().copied())
        .map(|c| {
            let c = c.unwrap_or('\u{FFFD}');
            let mut buffer = [0u8; 4];
            c.encode_utf8(&mut buffer).as_bytes().to_vec()
        })
        .collect()
}

fn hex_digit(u: u16) -> Option<u8> {
    char::from_u32(u as u32).and_then(|c| c.to_digit(16)).map(|d| d as u8)
}

/// Hex pairs up to the first that is not one (or `limit` bytes); an odd last digit is dropped.
fn hex_decode(units: &[u16], limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for pair in units.chunks_exact(2) {
        if out.len() >= limit {
            break;
        }
        match (hex_digit(pair[0]), hex_digit(pair[1])) {
            (Some(a), Some(b)) => out.push(a * 16 + b),
            _ => break,
        }
    }
    out
}

fn unbase64(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Node's forgiving base64 decoder (either alphabet): each character is read as its low byte, characters outside
/// the alphabet are skipped, and decoding stops at the first `=`.
fn base64_decode(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut group: [u8; 4] = [0; 4];
    let mut n = 0;
    for &u in units {
        let c = u as u8;
        let Some(d) = unbase64(c) else {
            if c == b'=' {
                break;
            }
            continue;
        };
        group[n] = d;
        n += 1;
        match n {
            2 => out.push((group[0] << 2) | (group[1] >> 4)),
            3 => out.push((group[1] << 4) | (group[2] >> 2)),
            4 => {
                out.push((group[2] << 6) | group[3]);
                n = 0;
            }
            _ => {}
        }
    }
    out
}

/// The byte length Node reserves for decoding base64 text.
fn base64_decoded_size(units: &[u16]) -> usize {
    let mut size = units.len();
    if size < 2 {
        return 0;
    }
    if units[size - 1] == b'=' as u16 {
        size -= 1;
        if units[size - 1] == b'=' as u16 {
            size -= 1;
        }
    }
    if size > 1 { (size / 4) * 3 + (size % 4).div_ceil(2) } else { 0 }
}

/// `Buffer.byteLength(string, encoding)`.
fn byte_length(units: &[u16], enc: Encoding) -> usize {
    match enc {
        Encoding::Utf8 => utf8(units).len(),
        Encoding::Latin1 | Encoding::Ascii => units.len(),
        Encoding::Utf16le => units.len() * 2,
        Encoding::Hex => units.len() / 2,
        Encoding::Base64 | Encoding::Base64Url => base64_decoded_size(units),
    }
}

/// Bytes as text in an encoding (`buffer.toString(encoding)`).
pub fn decode(bytes: &[u8], enc: Encoding) -> JsStr {
    match enc {
        Encoding::Utf8 => JsStr::from(String::from_utf8_lossy(bytes).as_ref()),
        Encoding::Latin1 => JsStr::from_units(bytes.iter().map(|&b| b as u16).collect()),
        Encoding::Ascii => JsStr::from_units(bytes.iter().map(|&b| (b & 0x7f) as u16).collect()),
        Encoding::Utf16le => JsStr::from_units(bytes.chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect()),
        Encoding::Hex => JsStr::from(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>().as_str()),
        Encoding::Base64 => JsStr::from(encode_base64(bytes).as_str()),
        Encoding::Base64Url => {
            let text: String = encode_base64(bytes)
                .chars()
                .filter(|&c| c != '=')
                .map(|c| match c {
                    '+' => '-',
                    '/' => '_',
                    c => c,
                })
                .collect();
            JsStr::from(text.as_str())
        }
    }
}

/// How `console.log` shows a Buffer: `<Buffer 68 69>`, at most 50 bytes.
pub fn inspect(bytes: &[u8]) -> String {
    let shown: Vec<String> = bytes.iter().take(50).map(|b| format!("{b:02x}")).collect();
    let more = if bytes.len() > 50 { format!(" ... {} more bytes", bytes.len() - 50) } else { String::new() };
    if shown.is_empty() { format!("<Buffer{more}>") } else { format!("<Buffer {}{more}>", shown.join(" ")) }
}

// --- Node's argument checks and error texts ---

/// A value as it reaches a host Buffer method (upstream `argument`).
#[derive(Clone)]
enum Arg {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(JsStr),
    Symbol(String),
    Bytes(BytesData),
}

fn argument(value: &Value) -> Arg {
    match value {
        Value::Undefined => Arg::Undefined,
        Value::Null => Arg::Null,
        Value::Bool(b) => Arg::Bool(*b),
        Value::Number(n) => Arg::Number(*n),
        Value::String(s) => Arg::String(s.clone()),
        Value::Symbol(s) => Arg::Symbol(s.description().to_string()),
        Value::Object(o) if o.bytes().is_some() => Arg::Bytes(o.bytes().unwrap().clone()),
        other => Arg::Number(coerce_to_number(other)),
    }
}

/// `+value`.
fn to_number(value: &Arg) -> JsResult<f64> {
    Ok(match value {
        Arg::Undefined => f64::NAN,
        Arg::Null => 0.0,
        Arg::Bool(b) => *b as u8 as f64,
        Arg::Number(n) => *n,
        Arg::String(s) => number::string_to_number(s.units()),
        Arg::Symbol(_) => return Err(type_error("Cannot convert a symbol to a number")),
        Arg::Bytes(b) => number::string_to_number(utf8_text(&b.to_vec()).units()),
    })
}

fn number_text(n: f64) -> String {
    if n == 0.0 && n.is_sign_negative() { "-0".into() } else { number::to_string(n) }
}

/// Node's `determineSpecificType`, for "Received ..." in a type error.
fn specific_type(value: &Arg) -> String {
    match value {
        Arg::Undefined => "undefined".into(),
        Arg::Null => "null".into(),
        Arg::Bool(b) => format!("type boolean ({b})"),
        Arg::Number(n) => format!("type number ({})", number::to_string(*n)),
        Arg::Symbol(d) => format!("type symbol ({d})"),
        Arg::Bytes(_) => "an instance of Buffer".into(),
        Arg::String(s) => {
            let mut text = s.to_rust();
            if s.units().len() > 28 {
                text = format!("{}...", String::from_utf16_lossy(&s.units()[..25]));
            }
            if text.contains('\'') { format!("type string ({})", crate::data::quote(&text)) } else { format!("type string ('{text}')") }
        }
    }
}

/// Node's numbers in "Received ...": integers past 2^32 with `_` separators.
fn received_number(n: f64) -> String {
    let text = number_text(n);
    if n.fract() != 0.0 || n.abs() <= 4_294_967_296.0 || !n.is_finite() {
        return text;
    }
    let (sign, digits) = text.split_at(if text.starts_with('-') { 1 } else { 0 });
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

fn received(value: &Arg) -> String {
    match value {
        Arg::Number(n) => received_number(*n),
        Arg::String(s) => format!("'{}'", s.to_rust()),
        other => specific_type(other).trim_start_matches("type ").to_string(),
    }
}

fn invalid_type(name: &str, expected: &str, value: &Arg) -> Thrown {
    fail(
        ErrorType::TypeError,
        format!("The \"{name}\" argument must be {expected}. Received {}", specific_type(value)),
        Some("ERR_INVALID_ARG_TYPE"),
    )
}

fn out_of_range(name: &str, range: &str, value: &Arg) -> Thrown {
    fail(
        ErrorType::RangeError,
        format!("The value of \"{name}\" is out of range. It must be {range}. Received {}", received(value)),
        Some("ERR_OUT_OF_RANGE"),
    )
}

/// `validateNumber(value, name)`.
fn number_arg(value: &Arg, name: &str) -> JsResult<f64> {
    match value {
        Arg::Number(n) => Ok(*n),
        other => Err(invalid_type(name, "of type number", other)),
    }
}

/// `validateInteger(value, name, min, max)` (Node's `validateOffset`).
fn integer_arg(value: &Arg, name: &str, min: f64, max: f64) -> JsResult<f64> {
    let n = number_arg(value, name)?;
    if !(n.is_finite() && n.fract() == 0.0) {
        return Err(out_of_range(name, "an integer", value));
    }
    if n < min || n > max {
        return Err(out_of_range(name, &format!(">= {} && <= {}", number_text(min), number_text(max)), value));
    }
    Ok(n)
}

/// Node's `boundsError`.
fn bounds_error(value: &Arg, length: f64, kind: Option<&str>) -> Thrown {
    let name = kind.unwrap_or("offset");
    let n = match value {
        Arg::Number(n) => *n,
        other => return invalid_type(name, "of type number", other),
    };
    if n.floor() != n {
        return out_of_range(name, "an integer", value);
    }
    if length < 0.0 {
        return fail(ErrorType::RangeError, "Attempt to access memory outside buffer bounds", Some("ERR_BUFFER_OUT_OF_BOUNDS"));
    }
    out_of_range(name, &format!(">= {} and <= {}", if kind.is_some() { 1 } else { 0 }, number_text(length)), value)
}

/// The byte index an offset names, or `None` when `buf[offset]` would be `undefined`.
fn index(offset: f64, len: usize) -> Option<usize> {
    (offset.fract() == 0.0 && offset >= 0.0 && offset < len as f64).then_some(offset as usize)
}

/// `checkBounds(buf, offset, byteLength)` (the last byte read or written is `offset + byteLength`).
fn check_bounds(offset: &Arg, len: usize, byte_length: usize) -> JsResult<usize> {
    let n = number_arg(offset, "offset")?;
    match (index(n, len), index(n + byte_length as f64, len)) {
        (Some(i), Some(_)) => Ok(i),
        _ => Err(bounds_error(offset, len as f64 - (byte_length as f64 + 1.0), None)),
    }
}

/// An optional offset defaulting to 0 (`offset = 0`).
fn offset_or_zero(value: Option<&Arg>) -> Arg {
    match value {
        None | Some(Arg::Undefined) => Arg::Number(0.0),
        Some(v) => v.clone(),
    }
}

fn arg_at(args: &[Arg], i: usize) -> Arg {
    args.get(i).cloned().unwrap_or(Arg::Undefined)
}

// --- The host methods ---

/// What a host method gives back: the receiver itself, or a value.
enum Out {
    This,
    Value(Value),
}

fn view(bytes: &BytesData) -> Vec<u8> {
    bytes.to_vec()
}

fn write_bytes(target: &BytesData, at: usize, bytes: &[u8]) {
    for (i, b) in bytes.iter().enumerate() {
        target.set(at + i, *b);
    }
}

fn read_number(name: &str, buf: &BytesData, args: &[Arg]) -> JsResult<Value> {
    let bytes = view(buf);
    let len = bytes.len();
    let (size, little, signed, float) = match name.trim_start_matches("read") {
        "UInt8" | "Uint8" => (1, true, false, false),
        "Int8" => (1, true, true, false),
        "UInt16LE" | "Uint16LE" => (2, true, false, false),
        "UInt16BE" | "Uint16BE" => (2, false, false, false),
        "Int16LE" => (2, true, true, false),
        "Int16BE" => (2, false, true, false),
        "UInt32LE" | "Uint32LE" => (4, true, false, false),
        "UInt32BE" | "Uint32BE" => (4, false, false, false),
        "Int32LE" => (4, true, true, false),
        "Int32BE" => (4, false, true, false),
        "FloatLE" => (4, true, false, true),
        "FloatBE" => (4, false, false, true),
        "DoubleLE" => (8, true, false, true),
        "DoubleBE" => (8, false, false, true),
        _ => return read_variable(name, &bytes, args),
    };
    let offset = offset_or_zero(args.first());
    let n = number_arg(&offset, "offset")?;
    let start = match (index(n, len), index(n + size as f64 - 1.0, len)) {
        (Some(i), Some(_)) => i,
        _ => return Err(bounds_error(&offset, len as f64 - size as f64, None)),
    };
    let mut chunk = bytes[start..start + size].to_vec();
    if !little {
        chunk.reverse();
    }
    Ok(Value::Number(read_le(&chunk, signed, float)))
}

/// A little-endian integer or float.
fn read_le(chunk: &[u8], signed: bool, float: bool) -> f64 {
    if float {
        return if chunk.len() == 4 {
            f32::from_le_bytes(chunk.try_into().unwrap()) as f64
        } else {
            f64::from_le_bytes(chunk.try_into().unwrap())
        };
    }
    let mut value: u64 = 0;
    for (i, b) in chunk.iter().enumerate() {
        value |= (*b as u64) << (8 * i);
    }
    let bits = chunk.len() * 8;
    if signed && value >> (bits - 1) & 1 == 1 {
        return value as f64 - 2f64.powi(bits as i32);
    }
    value as f64
}

/// `readUIntLE`/`readUIntBE`/`readIntLE`/`readIntBE(offset, byteLength)`.
fn read_variable(name: &str, bytes: &[u8], args: &[Arg]) -> JsResult<Value> {
    let (little, signed) = match name {
        "readUIntLE" => (true, false),
        "readUIntBE" => (false, false),
        "readIntLE" => (true, true),
        "readIntBE" => (false, true),
        _ => unreachable!("{name}"),
    };
    let offset = arg_at(args, 0);
    if matches!(offset, Arg::Undefined) {
        return Err(invalid_type("offset", "of type number", &offset));
    }
    let size = match arg_at(args, 1) {
        Arg::Number(n) if (1.0..=6.0).contains(&n) && n.fract() == 0.0 => n as usize,
        other => return Err(bounds_error(&other, 6.0, Some("byteLength"))),
    };
    let len = bytes.len();
    let n = number_arg(&offset, "offset")?;
    let start = match (index(n, len), index(n + size as f64 - 1.0, len)) {
        (Some(i), Some(_)) => i,
        _ => return Err(bounds_error(&offset, len as f64 - size as f64, None)),
    };
    let mut chunk = bytes[start..start + size].to_vec();
    if !little {
        chunk.reverse();
    }
    Ok(Value::Number(read_le(&chunk, signed, false)))
}

/// The range text of Bun's integer write checks.
fn int_range(min: f64, max: f64, size: usize) -> String {
    if size > 4 {
        let bits = size * 8;
        if min == 0.0 {
            return format!(">= 0 and < 2 ** {bits}");
        }
        return format!(">= -(2 ** {}) and < 2 ** {}", bits - 1, bits - 1);
    }
    format!(">= {} and <= {}", number_text(min), number_text(max))
}

fn write_number(name: &str, buf: &BytesData, args: &[Arg]) -> JsResult<Value> {
    let len = buf.length;
    let value = arg_at(args, 0);
    let (size, little, min, max, float) = match name.trim_start_matches("write") {
        "UInt8" | "Uint8" => (1, true, 0.0, 255.0, false),
        "Int8" => (1, true, -128.0, 127.0, false),
        "UInt16LE" | "Uint16LE" => (2, true, 0.0, 65535.0, false),
        "UInt16BE" | "Uint16BE" => (2, false, 0.0, 65535.0, false),
        "Int16LE" => (2, true, -32768.0, 32767.0, false),
        "Int16BE" => (2, false, -32768.0, 32767.0, false),
        "UInt32LE" | "Uint32LE" => (4, true, 0.0, 4294967295.0, false),
        "UInt32BE" | "Uint32BE" => (4, false, 0.0, 4294967295.0, false),
        "Int32LE" => (4, true, -2147483648.0, 2147483647.0, false),
        "Int32BE" => (4, false, -2147483648.0, 2147483647.0, false),
        "FloatLE" => (4, true, 0.0, 0.0, true),
        "FloatBE" => (4, false, 0.0, 0.0, true),
        "DoubleLE" => (8, true, 0.0, 0.0, true),
        "DoubleBE" => (8, false, 0.0, 0.0, true),
        _ => return write_variable(name, buf, args),
    };
    let v = to_number(&value)?;
    let offset = offset_or_zero(args.get(1));
    let start = if float {
        check_bounds(&offset, len, size - 1)?
    } else if size == 1 {
        let n = number_arg(&offset, "offset")?;
        if v > max || v < min {
            return Err(out_of_range("value", &int_range(min, max, 1), &Arg::Number(v)));
        }
        match index(n, len) {
            Some(i) => i,
            None => return Err(bounds_error(&offset, len as f64 - 1.0, None)),
        }
    } else {
        if v > max || v < min {
            return Err(out_of_range("value", &int_range(min, max, size), &Arg::Number(v)));
        }
        check_bounds(&offset, len, size - 1)?
    };
    let mut bytes = if float {
        if size == 4 { (v as f32).to_le_bytes().to_vec() } else { v.to_le_bytes().to_vec() }
    } else {
        let bits = to_bits(v);
        bits.to_le_bytes()[..size].to_vec()
    };
    if !little {
        bytes.reverse();
    }
    write_bytes(buf, start, &bytes);
    Ok(Value::Number((start + size) as f64))
}

/// A number's low 64 bits as JS integer conversion gives them (NaN and infinities are 0).
fn to_bits(v: f64) -> u64 {
    if !v.is_finite() {
        return 0;
    }
    let t = v.trunc();
    if t.abs() < 9_223_372_036_854_775_808.0 {
        return t as i64 as u64;
    }
    t.rem_euclid(18_446_744_073_709_551_616.0) as u64
}

/// `writeUIntLE`/`writeUIntBE`/`writeIntLE`/`writeIntBE(value, offset, byteLength)`.
fn write_variable(name: &str, buf: &BytesData, args: &[Arg]) -> JsResult<Value> {
    let (little, signed) = match name {
        "writeUIntLE" => (true, false),
        "writeUIntBE" => (false, false),
        "writeIntLE" => (true, true),
        "writeIntBE" => (false, true),
        _ => unreachable!("{name}"),
    };
    let size = match arg_at(args, 2) {
        Arg::Number(n) if (1.0..=6.0).contains(&n) && n.fract() == 0.0 => n as usize,
        other => return Err(bounds_error(&other, 6.0, Some("byteLength"))),
    };
    let (min, max) = match (signed, size) {
        (false, 6) => (0.0, 281_474_976_710_655.0),
        (false, 5) => (0.0, 1_099_511_627_775.0),
        (false, 4) => (0.0, 4_294_967_295.0),
        (false, 3) => (0.0, 16_777_215.0),
        (false, 2) => (0.0, 65_535.0),
        (false, _) => (0.0, 255.0),
        (true, 6) => (-140_737_488_355_328.0, 140_737_488_355_327.0),
        (true, 5) => (-549_755_813_888.0, 549_755_813_887.0),
        (true, 4) => (-2_147_483_648.0, 2_147_483_647.0),
        (true, 3) => (-8_388_608.0, 8_388_607.0),
        (true, 2) => (-32_768.0, 32_767.0),
        (true, _) => (-128.0, 127.0),
    };
    let v = to_number(&arg_at(args, 0))?;
    let offset = arg_at(args, 1);
    let len = buf.length;
    let start = if size == 1 {
        let n = number_arg(&offset, "offset")?;
        if v > max || v < min {
            return Err(out_of_range("value", &int_range(min, max, 1), &Arg::Number(v)));
        }
        match index(n, len) {
            Some(i) => i,
            None => return Err(bounds_error(&offset, len as f64 - 1.0, None)),
        }
    } else {
        if v > max || v < min {
            return Err(out_of_range("value", &int_range(min, max, size), &Arg::Number(v)));
        }
        check_bounds(&offset, len, size - 1)?
    };
    let mut bytes = to_bits(v).to_le_bytes()[..size].to_vec();
    if !little {
        bytes.reverse();
    }
    write_bytes(buf, start, &bytes);
    Ok(Value::Number((start + size) as f64))
}

fn swap(buf: &BytesData, width: usize) -> JsResult<()> {
    if !buf.length.is_multiple_of(width) {
        return Err(fail(
            ErrorType::RangeError,
            format!("Buffer size must be a multiple of {}-bits", width * 8),
            Some("ERR_INVALID_BUFFER_SIZE"),
        ));
    }
    let mut bytes = view(buf);
    for chunk in bytes.chunks_mut(width) {
        chunk.reverse();
    }
    write_bytes(buf, 0, &bytes);
    Ok(())
}

/// Node's `IndexOfOffset`.
fn index_of_offset(length: i64, offset: i64, needle: i64, forward: bool) -> i64 {
    if offset < 0 {
        if offset + length >= 0 {
            length + offset
        } else if forward || needle == 0 {
            0
        } else {
            -1
        }
    } else if offset + needle <= length {
        offset
    } else if needle == 0 {
        length
    } else if forward {
        -1
    } else {
        length - 1
    }
}

/// Node's `SearchString`: the first match at or after `from` (or the last at or before it), by `unit` bytes.
fn search(haystack: &[u8], needle: &[u8], from: usize, forward: bool, unit: usize) -> i64 {
    let n = needle.len();
    if n > haystack.len() {
        return -1;
    }
    let last = haystack.len() - n;
    let matches = |i: usize| haystack[i..i + n] == *needle;
    if forward {
        let mut i = from;
        while i <= last {
            if matches(i) {
                return i as i64;
            }
            i += unit;
        }
    } else {
        let mut i = from.min(last) as i64;
        if unit == 2 {
            i -= i % 2;
        }
        while i >= 0 {
            if matches(i as usize) {
                return i;
            }
            i -= unit as i64;
        }
    }
    -1
}

/// `indexOf`/`lastIndexOf` (`bidirectionalIndexOf`).
fn index_of(buf: &BytesData, args: &[Arg], forward: bool) -> JsResult<f64> {
    let haystack = view(buf);
    let value = arg_at(args, 0);
    let mut encoding_arg = arg_at(args, 2);
    let offset = match arg_at(args, 1) {
        Arg::String(s) => {
            encoding_arg = Arg::String(s);
            f64::NAN
        }
        other => to_number(&other)?.clamp(-2_147_483_648.0, 2_147_483_647.0),
    };
    let len = haystack.len() as i64;
    let offset = if offset.is_nan() { if forward { 0 } else { len } } else { offset.trunc() as i64 };
    if let Arg::Number(n) = value {
        let needle = to_bits(n) as u8;
        let at = index_of_offset(len, offset, 1, forward);
        if at <= -1 || len == 0 {
            return Ok(-1.0);
        }
        return Ok(search(&haystack, &[needle], at as usize, forward, 1) as f64);
    }
    let enc = match &encoding_arg {
        Arg::Undefined => Some(Encoding::Utf8),
        Arg::String(s) => Encoding::named(&s.to_rust()),
        _ => None,
    };
    let encoding_name = || match &encoding_arg {
        Arg::String(s) => s.to_rust(),
        Arg::Number(n) => number_text(*n),
        Arg::Bool(b) => b.to_string(),
        Arg::Null => "null".into(),
        _ => "undefined".into(),
    };
    let (needle, unit) = match &value {
        Arg::String(s) => {
            let Some(enc) = enc else { return Err(unknown_encoding(&encoding_name())) };
            let unit = if enc == Encoding::Utf16le { 2 } else { 1 };
            (encode(s.units(), enc), unit)
        }
        Arg::Bytes(b) => (view(b), if enc == Some(Encoding::Utf16le) { 2 } else { 1 }),
        other => {
            return Err(invalid_type(
                "value",
                "one of type number or string or an instance of Buffer or Uint8Array",
                other,
            ));
        }
    };
    let n = needle.len() as i64;
    let at = index_of_offset(len, offset, n, forward);
    if n == 0 {
        return Ok(at as f64);
    }
    if len == 0 || at <= -1 {
        return Ok(-1.0);
    }
    if (forward && n + at > len) || n > len {
        return Ok(-1.0);
    }
    if unit == 2 && (len < 2 || n < 2) {
        return Ok(-1.0);
    }
    let from = if unit == 2 { (at as usize / 2) * 2 } else { at as usize };
    Ok(search(&haystack, &needle, from, forward, unit) as f64)
}

/// `write(string, offset, length, encoding)`.
fn write_string(buf: &BytesData, args: &[Arg]) -> JsResult<Value> {
    let len = buf.length as f64;
    // Bun reads an empty encoding as UTF-8 (`!encoding`), except one given in place of the offset.
    let mut strict = false;
    let (offset, length, encoding_arg) = match (arg_at(args, 1), arg_at(args, 2), arg_at(args, 3)) {
        (Arg::Undefined, _, _) => (0.0, len, Arg::Undefined),
        (Arg::String(e), Arg::Undefined, _) => {
            strict = true;
            (0.0, len, Arg::String(e))
        }
        (offset, length, encoding) => {
            let offset = integer_arg(&offset, "offset", 0.0, len)?;
            let remaining = len - offset;
            match length {
                Arg::Undefined => (offset, remaining, encoding),
                Arg::String(e) => (offset, remaining, Arg::String(e)),
                length => (offset, integer_arg(&length, "length", 0.0, len)?.min(remaining), encoding),
            }
        }
    };
    let string = match arg_at(args, 0) {
        Arg::String(s) => s,
        other => return Err(invalid_type("string", "of type string", &other)),
    };
    let enc = match &encoding_arg {
        Arg::Undefined => Encoding::Utf8,
        Arg::String(s) if s.units().is_empty() && !strict => Encoding::Utf8,
        Arg::String(s) => Encoding::named(&s.to_rust()).ok_or_else(|| unknown_encoding(&s.to_rust()))?,
        other => return Err(unknown_encoding(&received(other))),
    };
    let room = length as usize;
    let bytes: Vec<u8> = match enc {
        Encoding::Utf8 => {
            let mut out = Vec::new();
            for c in utf8_chars(string.units()) {
                if out.len() + c.len() > room {
                    break;
                }
                out.extend(c);
            }
            out
        }
        Encoding::Utf16le => {
            string.units().iter().take(room / 2).flat_map(|u| u.to_le_bytes()).collect()
        }
        Encoding::Hex => hex_decode(string.units(), room),
        other => {
            let mut all = encode(string.units(), other);
            all.truncate(room);
            all
        }
    };
    write_bytes(buf, offset as usize, &bytes);
    Ok(Value::Number(bytes.len() as f64))
}

/// `fill(value, offset, end, encoding)` (`_fill` and the native fill).
fn fill(buf: &BytesData, args: &[Arg]) -> JsResult<()> {
    let len = buf.length;
    let mut value = arg_at(args, 0);
    let mut offset = arg_at(args, 1);
    let mut end = arg_at(args, 2);
    let mut enc = None;
    if let Arg::String(s) = &value {
        let mut encoding = arg_at(args, 3);
        if matches!(offset, Arg::Undefined | Arg::String(_)) {
            encoding = offset.clone();
            offset = Arg::Number(0.0);
            end = Arg::Number(len as f64);
        } else if let Arg::String(_) = end {
            encoding = end.clone();
            end = Arg::Number(len as f64);
        }
        let normalized = match &encoding {
            Arg::Undefined => Some(Encoding::Utf8),
            Arg::String(e) => Encoding::named(&e.to_rust()),
            _ => None,
        };
        let Some(normalized) = normalized else {
            if !matches!(encoding, Arg::String(_)) {
                return Err(invalid_type("encoding", "of type string", &encoding));
            }
            return Err(unknown_encoding(received(&encoding).trim_matches('\'')));
        };
        let units = s.units();
        if units.is_empty() {
            value = Arg::Number(0.0);
        } else if units.len() == 1 {
            if (normalized == Encoding::Utf8 && units[0] < 128) || normalized == Encoding::Latin1 {
                value = Arg::Number(units[0] as f64);
            }
        }
        enc = Some(normalized);
    }
    let (start, stop) = match offset {
        Arg::Undefined => (0, len),
        offset => {
            let start = integer_arg(&offset, "offset", 0.0, MAX_LENGTH)? as usize;
            let stop = match end {
                Arg::Undefined => len,
                end => integer_arg(&end, "end", 0.0, len as f64)? as usize,
            };
            if start >= stop {
                return Ok(());
            }
            (start, stop)
        }
    };
    let span = stop - start;
    let pattern: Vec<u8> = match &value {
        Arg::Number(n) => {
            let byte = to_uint8(*n);
            write_bytes(buf, start, &vec![byte; span]);
            return Ok(());
        }
        Arg::String(s) => match enc.unwrap_or(Encoding::Utf8) {
            Encoding::Hex => hex_decode(s.units(), span),
            other => encode(s.units(), other),
        },
        Arg::Bytes(b) => view(b),
        other => {
            let byte = (to_number(other)? as i64 & 255) as u8;
            write_bytes(buf, start, &vec![byte; span]);
            return Ok(());
        }
    };
    if pattern.is_empty() {
        if let Arg::Bytes(_) = value {
            return Err(fail(ErrorType::TypeError, "Buffer cannot be empty", Some("ERR_INVALID_ARG_VALUE")));
        }
        let shown = match &value {
            Arg::String(s) => format!("'{}'", s.to_rust()),
            Arg::Bytes(_) => "<Buffer >".into(),
            other => received(other),
        };
        return Err(fail(
            ErrorType::TypeError,
            format!("The argument 'value' is invalid. Received {shown}"),
            Some("ERR_INVALID_ARG_VALUE"),
        ));
    }
    let filled: Vec<u8> = pattern.iter().copied().cycle().take(span).collect();
    write_bytes(buf, start, &filled);
    Ok(())
}

/// `copy(target, targetStart, sourceStart, sourceEnd)`.
fn copy(buf: &BytesData, args: &[Arg]) -> JsResult<Value> {
    let target = match arg_at(args, 0) {
        Arg::Bytes(t) => t,
        other => return Err(invalid_type("target", "an instance of Buffer or Uint8Array", &other)),
    };
    let integer = |value: &Arg| -> JsResult<f64> {
        let n = to_number(value)?;
        Ok(if n.fract() == 0.0 {
            n
        } else if !n.is_nan() && n.abs() <= 9_007_199_254_740_991.0 {
            n.floor()
        } else {
            0.0
        })
    };
    let source_len = buf.length as f64;
    let target_start = match arg_at(args, 1) {
        Arg::Undefined => 0.0,
        v => {
            let n = integer(&v)?;
            if n < 0.0 {
                return Err(out_of_range("targetStart", ">= 0", &Arg::Number(n)));
            }
            n
        }
    };
    let source_start = match arg_at(args, 2) {
        Arg::Undefined => 0.0,
        v => {
            let n = integer(&v)?;
            if n < 0.0 || n > source_len {
                return Err(out_of_range("sourceStart", &format!(">= 0 && <= {}", number_text(source_len)), &Arg::Number(n)));
            }
            n
        }
    };
    let mut source_end = match arg_at(args, 3) {
        Arg::Undefined => source_len,
        v => {
            let n = integer(&v)?;
            if n < 0.0 {
                return Err(out_of_range("sourceEnd", ">= 0", &Arg::Number(n)));
            }
            n
        }
    };
    let target_len = target.length as f64;
    if target_start >= target_len || source_start >= source_end {
        return Ok(Value::Number(0.0));
    }
    if source_end - source_start > target_len - target_start {
        source_end = source_start + target_len - target_start;
    }
    let mut count = source_end - source_start;
    if count > source_len - source_start {
        count = source_len - source_start;
    }
    if count <= 0.0 {
        return Ok(Value::Number(0.0));
    }
    let bytes = view(buf);
    let from = source_start as usize;
    write_bytes(&target, target_start as usize, &bytes[from..from + count as usize]);
    Ok(Value::Number(count))
}

/// `compare(target, targetStart, targetEnd, sourceStart, sourceEnd)`, as Bun checks its offsets.
fn compare(buf: &BytesData, args: &[Arg]) -> JsResult<Value> {
    let target = match arg_at(args, 0) {
        Arg::Bytes(t) => t,
        other => return Err(invalid_type("target", "an instance of Buffer or Uint8Array", &other)),
    };
    let source = view(buf);
    let other = view(&target);
    let start_arg = |i: usize, name: &str| -> JsResult<f64> {
        match arg_at(args, i) {
            Arg::Undefined => Ok(0.0),
            v => integer_arg(&v, name, 0.0, MAX_LENGTH),
        }
    };
    let end_arg = |i: usize, name: &str, len: usize| -> JsResult<f64> {
        match arg_at(args, i) {
            Arg::Undefined => Ok(len as f64),
            v => {
                let n = integer_arg(&v, name, 0.0, MAX_LENGTH)?;
                if n > len as f64 {
                    return Err(out_of_range(name, &format!(">= 0 and <= {len}"), &v));
                }
                Ok(n)
            }
        }
    };
    let target_start = start_arg(1, "targetStart")?;
    let target_end = end_arg(2, "targetEnd", other.len())?;
    let source_start = start_arg(3, "sourceStart")?;
    let source_end = end_arg(4, "sourceEnd", source.len())?;
    if source_start >= source_end {
        return Ok(Value::Number(if target_start >= target_end { 0.0 } else { -1.0 }));
    }
    if target_start >= target_end {
        return Ok(Value::Number(1.0));
    }
    let a = &source[(source_start as usize).min(source.len())..source_end as usize];
    let b = &other[(target_start as usize).min(other.len())..target_end as usize];
    Ok(Value::Number(match a.cmp(b) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    }))
}

fn host_method(name: &str, buf: &BytesData, args: &[Arg]) -> JsResult<Out> {
    Ok(Out::Value(match name {
        "compare" => compare(buf, args)?,
        "indexOf" => Value::Number(index_of(buf, args, true)?),
        "lastIndexOf" => Value::Number(index_of(buf, args, false)?),
        "includes" => Value::Bool(index_of(buf, args, true)? != -1.0),
        "write" => write_string(buf, args)?,
        "fill" => {
            fill(buf, args)?;
            return Ok(Out::This);
        }
        "copy" => copy(buf, args)?,
        "swap16" | "swap32" | "swap64" => {
            swap(buf, name[4..].parse::<usize>().unwrap() / 8)?;
            return Ok(Out::This);
        }
        n if n.starts_with("read") => read_number(n, buf, args)?,
        n => write_number(n, buf, args)?,
    }))
}

// Read and write methods that take only numbers, delegated to the host Buffer as is.
const NUMERIC: &[&str] = &[
    "readUInt8", "readUint8", "readInt8", "readUInt16LE", "readUint16LE", "readUInt16BE", "readUint16BE", "readInt16LE",
    "readInt16BE", "readUInt32LE", "readUint32LE", "readUInt32BE", "readUint32BE", "readInt32LE", "readInt32BE",
    "readFloatLE", "readFloatBE", "readDoubleLE", "readDoubleBE", "readUIntLE", "readUIntBE", "readIntLE", "readIntBE",
    "writeUInt8", "writeUint8", "writeInt8", "writeUInt16LE", "writeUint16LE", "writeUInt16BE", "writeUint16BE",
    "writeInt16LE", "writeInt16BE", "writeUInt32LE", "writeUint32LE", "writeUInt32BE", "writeUint32BE", "writeInt32LE",
    "writeInt32BE", "writeFloatLE", "writeFloatBE", "writeDoubleLE", "writeDoubleBE", "writeUIntLE", "writeUIntBE",
    "writeIntLE", "writeIntBE", "swap16", "swap32", "swap64",
];

// --- The global ---

/// A Buffer of `bytes`.
pub fn buffer_value(rt: &Rt, bytes: Vec<u8>) -> Value {
    Value::Object(Obj::new(Some(rt.builtins.buffer.clone()), ObjKind::Bytes(BytesData::node(bytes))))
}

/// A Buffer's built-in string form: its bytes as UTF-8 text.
pub fn utf8_text(bytes: &[u8]) -> JsStr {
    decode(bytes, Encoding::Utf8)
}

fn self_bytes(this: &Value, name: &str) -> JsResult<BytesData> {
    receiver(this, &format!("Buffer.prototype.{name}"), |o| o.bytes().is_some()).map(|o| o.bytes().unwrap().clone())
}

fn bytes_from(value: &Value, second: &Value, third: &Value) -> JsResult<Vec<u8>> {
    if let Value::String(s) = value {
        let enc = encoding(second)?.unwrap_or(Encoding::Utf8);
        return Ok(encode(s.units(), enc));
    }
    if let Value::Object(o) = value {
        if let Some(b) = o.bytes() {
            let bytes = b.to_vec();
            let len = bytes.len() as f64;
            let offset = if second.is_undefined() { 0.0 } else { num(second)? };
            let end = if third.is_undefined() { None } else { Some(offset + num(third)?) };
            // TypedArray.prototype.slice(offset, end).
            let relative = |n: f64| {
                let n = if n.is_nan() { 0.0 } else { n.trunc() };
                if n < 0.0 { (len + n).max(0.0) } else { n.min(len) }
            };
            let start = relative(offset) as usize;
            let stop = end.map(relative).unwrap_or(len) as usize;
            return Ok(if start < stop { bytes[start..stop].to_vec() } else { Vec::new() });
        }
        if let ObjKind::Array(_) = o.kind() {
            return o.array_values().iter().map(|v| num(v).map(to_uint8)).collect();
        }
        let data = get_str(o, "data")?;
        if let (Value::String(t), Value::Object(d)) = (&get_str(o, "type")?, &data)
            && t.to_rust() == "Buffer"
            && d.is_array()
        {
            return d.array_values().iter().map(|v| num(v).map(to_uint8)).collect();
        }
        if let Value::Number(length) = get_str(o, "length")? {
            let n = if length.is_finite() && length > 0.0 { length.trunc() as usize } else { 0 };
            crate::interp::limits::check_array_length(n as f64)?;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(to_uint8(num(&get_str(o, &i.to_string())?)?));
            }
            return Ok(out);
        }
    }
    Err(fail(
        ErrorType::TypeError,
        format!(
            "The first argument must be of type string or an instance of Buffer, Uint8Array, or Array or an Array-like Object. Received {}",
            describe_value(value)
        ),
        Some("ERR_INVALID_ARG_TYPE"),
    ))
}

/// Upstream's `coerceToNumber`, which throws for a symbol (the host's `Number(symbol)`).
fn num(value: &Value) -> JsResult<f64> {
    match value {
        Value::Symbol(_) => Err(type_error("Cannot convert a symbol to a number")),
        other => Ok(coerce_to_number(other)),
    }
}

fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() { f64::NAN } else { a.max(b) }
}

fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() { f64::NAN } else { a.min(b) }
}

/// `size`: a non-negative integer length.
fn size(value: &Value) -> JsResult<usize> {
    let length = num(value)?;
    if !(length.is_finite() && length.fract() == 0.0) || length < 0.0 {
        return Err(fail(
            ErrorType::RangeError,
            format!("The argument 'size' is invalid. Received {}", coerce_to_string(value).to_rust()),
            Some("ERR_OUT_OF_RANGE"),
        ));
    }
    crate::interp::limits::check_array_length(length)?;
    Ok(length as usize)
}

fn result<T>(r: JsResult<T>) -> NResult
where
    T: Into<Value>,
{
    NResult::from(r.map(Into::into))
}

pub fn buffer_global(rt: &Rt) -> Value {
    let proto = rt.builtins.buffer.clone();
    let fp = rt.builtins.function.clone();
    let construct_proto = proto.clone();
    let ctor = constructor(
        &fp,
        &proto,
        "Buffer",
        2,
        NativeFn {
            call: Rc::new(|rt, _, _, args| {
                result(bytes_from(&arg(&args, 0), &arg(&args, 1), &arg(&args, 2)).map(|b| buffer_value(rt, b)))
            }),
            construct: Some(Rc::new(move |_, _, args, new_target| {
                let into = prototype_from(&new_target, &construct_proto);
                result(bytes_from(&arg(&args, 0), &arg(&args, 1), &arg(&args, 2)).map(|b| {
                    Value::Object(Obj::new(Some(into), ObjKind::Bytes(BytesData::node(b))))
                }))
            })),
            callback: true,
        },
    );
    define(&ctor, "BYTES_PER_ELEMENT", Value::Number(1.0), crate::object::FROZEN);
    methods(
        &fp,
        &ctor,
        vec![
            method("from", 3, |rt, _, _, args| {
                result(bytes_from(&arg(&args, 0), &arg(&args, 1), &arg(&args, 2)).map(|b| buffer_value(rt, b)))
            }),
            method("alloc", 3, |rt, _, _, args| {
                let made = (|| -> JsResult<Value> {
                    let bytes = BytesData::node(vec![0; size(&arg(&args, 0))?]);
                    match arg(&args, 1) {
                        Value::Undefined => {}
                        fill_value => {
                            let fill_value = argument(&fill_value);
                            // Node reads an encoding only after a string fill; after a number it would be an offset.
                            if let Arg::String(_) = fill_value {
                                let name = match encoding(&arg(&args, 2))? {
                                    None => "utf8".to_string(),
                                    Some(_) => coerce_to_string(&arg(&args, 2)).to_rust(),
                                };
                                fill(&bytes, &[fill_value, Arg::String(JsStr::from(name.as_str()))])?;
                            } else {
                                fill(&bytes, &[fill_value])?;
                            }
                        }
                    }
                    Ok(Value::Object(Obj::new(Some(rt.builtins.buffer.clone()), ObjKind::Bytes(bytes))))
                })();
                NResult::from(made)
            }),
            method("allocUnsafe", 1, |rt, _, _, args| result(size(&arg(&args, 0)).map(|n| buffer_value(rt, vec![0; n])))),
            method("allocUnsafeSlow", 1, |rt, _, _, args| {
                result(size(&arg(&args, 0)).map(|n| buffer_value(rt, vec![0; n])))
            }),
            method("byteLength", 2, |_, _, _, args| {
                if let Value::Object(o) = arg(&args, 0)
                    && let Some(b) = o.bytes()
                {
                    return NResult::ok(Value::Number(b.length as f64));
                }
                let text = coerce_to_string(&arg(&args, 0));
                result(encoding(&arg(&args, 1)).map(|enc| byte_length(text.units(), enc.unwrap_or(Encoding::Utf8)) as f64))
            }),
            method("concat", 2, |rt, _, _, args| {
                let made = (|| -> JsResult<Value> {
                    let list = arg(&args, 0);
                    let items = match &list {
                        Value::Object(o) if o.is_array() => o.array_values(),
                        _ => {
                            return Err(type_error(format!(
                                "Buffer.concat expects an array of Buffers, received {}.",
                                describe_value(&list)
                            )));
                        }
                    };
                    let mut parts = Vec::with_capacity(items.len());
                    for item in &items {
                        match item {
                            Value::Object(o) if o.bytes().is_some() => parts.push(o.bytes().unwrap().to_vec()),
                            other => {
                                return Err(type_error(format!(
                                    "Buffer.concat expects an array of Buffers, received {} in it.",
                                    describe_value(other)
                                )));
                            }
                        }
                    }
                    let total = match arg(&args, 1) {
                        Value::Undefined => parts.iter().map(Vec::len).sum(),
                        n => size(&n)?,
                    };
                    let mut joined: Vec<u8> = parts.concat();
                    joined.resize(total, 0);
                    Ok(buffer_value(rt, joined))
                })();
                NResult::from(made)
            }),
            method("isBuffer", 1, |_, _, _, args| {
                NResult::ok(Value::Bool(matches!(arg(&args, 0), Value::Object(o) if o.bytes().is_some_and(|b| b.node))))
            }),
            method("isEncoding", 1, |_, _, _, args| NResult::ok(Value::Bool(is_encoding(&arg(&args, 0))))),
            method("compare", 2, |_, _, _, args| {
                let (Value::Object(a), Value::Object(b)) = (arg(&args, 0), arg(&args, 1)) else {
                    return NResult::err(type_error("Buffer.compare expects two Buffers or Uint8Arrays."));
                };
                let (Some(a), Some(b)) = (a.bytes(), b.bytes()) else {
                    return NResult::err(type_error("Buffer.compare expects two Buffers or Uint8Arrays."));
                };
                NResult::ok(Value::Number(match a.to_vec().cmp(&b.to_vec()) {
                    std::cmp::Ordering::Less => -1.0,
                    std::cmp::Ordering::Equal => 0.0,
                    std::cmp::Ordering::Greater => 1.0,
                }))
            }),
        ],
    );
    define(&ctor, "poolSize", Value::Number(8192.0), HIDDEN);

    let view_method = |name: &'static str| -> Method {
        method(name, 2, move |rt, _, this, args| {
            let made = self_bytes(&this, name).and_then(|bytes| {
                let length = bytes.length as f64;
                let clamp = |value: &Value, fallback: f64| -> JsResult<f64> {
                    if value.is_undefined() {
                        return Ok(fallback);
                    }
                    let n = num(value)?.trunc();
                    let n = if n.is_nan() { 0.0 } else { n };
                    Ok(if n < 0.0 { (length + n).max(0.0) } else { n.min(length) })
                };
                let start = clamp(&arg(&args, 0), 0.0)? as usize;
                let end = clamp(&arg(&args, 1), length)? as usize;
                let sub = BytesData {
                    buffer: bytes.buffer.clone(),
                    offset: bytes.offset + start,
                    length: end.saturating_sub(start),
                    node: true,
                };
                Ok(Value::Object(Obj::new(Some(rt.builtins.buffer.clone()), ObjKind::Bytes(sub))))
            });
            NResult::from(made)
        })
    };
    let delegate = |name: &'static str, length: usize| -> Method {
        method(name, length, move |_, _, this, args| {
            let made = self_bytes(&this, name).and_then(|bytes| {
                let args: Vec<Arg> = args.iter().map(argument).collect();
                Ok(match host_method(name, &bytes, &args)? {
                    Out::This => this.clone(),
                    Out::Value(v) => v,
                })
            });
            NResult::from(made)
        })
    };
    let mut table = vec![
        method("toString", 3, |_, _, this, args| {
            let made = self_bytes(&this, "toString").and_then(|bytes| {
                let data = bytes.to_vec();
                let len = data.len() as f64;
                // `Math.max(0, start)` and `Math.min(length, end)`: NaN stays NaN.
                let start = if arg(&args, 1).is_undefined() { 0.0 } else { js_max(0.0, num(&arg(&args, 1))?) };
                let end = if arg(&args, 2).is_undefined() { len } else { js_min(len, num(&arg(&args, 2))?) };
                let enc = encoding(&arg(&args, 0))?.unwrap_or(Encoding::Utf8);
                // The host Buffer's `toString(encoding, start, end)`.
                if start >= len {
                    return Ok(Value::str(""));
                }
                let start = if start <= 0.0 || start.is_nan() { 0.0 } else { start.trunc() };
                let end = if end > len { len } else if end.is_nan() { 0.0 } else { end.trunc() };
                if end <= start {
                    return Ok(Value::str(""));
                }
                Ok(Value::String(decode(&data[start as usize..end as usize], enc)))
            });
            NResult::from(made)
        }),
        method("toJSON", 0, |rt, _, this, _| {
            let made = self_bytes(&this, "toJSON").map(|bytes| {
                let data: Vec<Value> = bytes.to_vec().into_iter().map(|b| Value::Number(b as f64)).collect();
                Value::Object(record(
                    &rt.builtins.object,
                    vec![("type", Value::str("Buffer")), ("data", Value::Object(Obj::array(&rt.builtins.array, data)))],
                ))
            });
            NResult::from(made)
        }),
        view_method("slice"),
        view_method("subarray"),
        method("equals", 1, |_, _, this, args| {
            let made = self_bytes(&this, "equals").and_then(|bytes| {
                let other = arg(&args, 0);
                match &other {
                    Value::Object(o) if o.bytes().is_some() => {
                        Ok(Value::Bool(bytes.to_vec() == o.bytes().unwrap().to_vec()))
                    }
                    _ => Err(type_error(format!(
                        "The \"otherBuffer\" argument must be an instance of Buffer or Uint8Array. Received {}",
                        describe_value(&other)
                    ))),
                }
            });
            NResult::from(made)
        }),
        delegate("compare", 5),
        delegate("indexOf", 3),
        delegate("lastIndexOf", 3),
        delegate("includes", 3),
        delegate("write", 4),
        delegate("fill", 4),
        delegate("copy", 4),
    ];
    table.extend(NUMERIC.iter().map(|&name| delegate(name, 2)));
    methods(&fp, &proto, table);
    Value::Object(ctor)
}

/// The text a Buffer converts to where a string is needed (upstream `BufferObj.toString`).
pub fn to_text(o: &Obj) -> Option<JsStr> {
    let bytes = o.bytes()?;
    bytes.node.then(|| utf8_text(&bytes.to_vec()))
}
