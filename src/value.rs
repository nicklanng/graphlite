//! Property values.
//!
//! Values are stored in one encoding that is both compact and
//! order-preserving: comparing encodings bytewise orders values of the same
//! type the way the values themselves order, and each encoding marks its own
//! end. That makes stored values usable directly inside index keys.
//!
//! ```text
//! Null    0x00
//! Bool    0x01 | 0 or 1
//! Int     0x02 | i64 with the sign bit flipped, big-endian
//! Float   0x03 | f64 bits, sign-adjusted so negatives sort first, big-endian
//! String  0x04 | bytes with 0x00 escaped as 0x00 0xFF | 0x00 0x00
//! Bytes   0x05 | same as String
//! ```

use std::fmt;

use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Bytes(Vec<u8>),
}

const NULL: u8 = 0;
const BOOL: u8 = 1;
const INT: u8 = 2;
const FLOAT: u8 = 3;
const STRING: u8 = 4;
const BYTES: u8 = 5;

impl Value {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Value::Null => out.push(NULL),
            Value::Bool(b) => out.extend([BOOL, u8::from(*b)]),
            Value::Int(i) => {
                out.push(INT);
                out.extend(((*i as u64) ^ (1 << 63)).to_be_bytes());
            }
            Value::Float(f) => {
                out.push(FLOAT);
                // Normalize -0.0 so equal floats encode equally.
                let bits = if *f == 0.0 { 0 } else { f.to_bits() };
                let sortable = if bits >> 63 == 1 { !bits } else { bits | (1 << 63) };
                out.extend(sortable.to_be_bytes());
            }
            Value::String(s) => encode_bytes(STRING, s.as_bytes(), &mut out),
            Value::Bytes(b) => encode_bytes(BYTES, b, &mut out),
        }
        out
    }

    /// Decodes a value from the front of `buf`, returning it and its length.
    pub(crate) fn decode(buf: &[u8]) -> Result<(Value, usize)> {
        let bad = Error::Corrupt("bad value encoding");
        let fixed = |n: usize| buf.get(1..1 + n).ok_or(Error::Corrupt("bad value encoding"));
        let v = match *buf.first().ok_or(Error::Corrupt("empty value"))? {
            NULL => (Value::Null, 1),
            BOOL => (Value::Bool(fixed(1)?[0] != 0), 2),
            INT => {
                let u = u64::from_be_bytes(fixed(8)?.try_into().unwrap());
                (Value::Int((u ^ (1 << 63)) as i64), 9)
            }
            FLOAT => {
                let s = u64::from_be_bytes(fixed(8)?.try_into().unwrap());
                let bits = if s >> 63 == 1 { s & !(1 << 63) } else { !s };
                (Value::Float(f64::from_bits(bits)), 9)
            }
            STRING => {
                let (bytes, n) = decode_bytes(&buf[1..])?;
                (Value::String(String::from_utf8(bytes).map_err(|_| bad)?), 1 + n)
            }
            BYTES => {
                let (bytes, n) = decode_bytes(&buf[1..])?;
                (Value::Bytes(bytes), 1 + n)
            }
            _ => return Err(bad),
        };
        Ok(v)
    }

    /// Decodes a buffer holding exactly one value.
    pub(crate) fn decode_all(buf: &[u8]) -> Result<Value> {
        match Value::decode(buf)? {
            (v, n) if n == buf.len() => Ok(v),
            _ => Err(Error::Corrupt("trailing bytes after value")),
        }
    }
}

fn encode_bytes(tag: u8, bytes: &[u8], out: &mut Vec<u8>) {
    out.reserve(bytes.len() + 3);
    out.push(tag);
    for &b in bytes {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend([0, 0]);
}

/// Decodes an escaped byte string, returning it and the bytes consumed.
fn decode_bytes(buf: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i] != 0 {
            out.push(buf[i]);
            i += 1;
            continue;
        }
        match buf.get(i + 1) {
            Some(0) => return Ok((out, i + 2)),
            Some(0xFF) => {
                out.push(0);
                i += 2;
            }
            _ => return Err(Error::Corrupt("bad string escape")),
        }
    }
    Err(Error::Corrupt("unterminated string value"))
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::String(s) => write!(f, "{s:?}"),
            Value::Bytes(b) => write!(f, "<{} bytes>", b.len()),
        }
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Bool(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Value {
        Value::Int(v)
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Value {
        Value::Int(v.into())
    }
}

impl From<u32> for Value {
    fn from(v: u32) -> Value {
        Value::Int(v.into())
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Value {
        Value::Float(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::String(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::String(v)
    }
}

impl From<&[u8]> for Value {
    fn from(v: &[u8]) -> Value {
        Value::Bytes(v.to_vec())
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Value {
        Value::Bytes(v)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        v.map_or(Value::Null, Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let values = [
            Value::Null,
            Value::Bool(true),
            Value::Int(i64::MIN),
            Value::Int(-1),
            Value::Int(42),
            Value::Float(-2.5),
            Value::Float(1e300),
            Value::String(String::new()),
            Value::String("a\0b\0\0".into()),
            Value::Bytes(vec![0, 0xFF, 0, 0]),
        ];
        for v in values {
            assert_eq!(Value::decode_all(&v.encode()).unwrap(), v);
        }
    }

    #[test]
    fn encoding_preserves_order() {
        let ints = [i64::MIN, -100, -1, 0, 1, 100, i64::MAX];
        let floats = [f64::NEG_INFINITY, -1e10, -0.5, 0.0, 1e-10, 3.0, f64::INFINITY];
        let strings = ["", "\0", "a", "a\0", "a\0b", "ab", "b"];
        for w in ints.windows(2) {
            assert!(Value::Int(w[0]).encode() < Value::Int(w[1]).encode());
        }
        for w in floats.windows(2) {
            assert!(Value::Float(w[0]).encode() < Value::Float(w[1]).encode());
        }
        for w in strings.windows(2) {
            assert!(Value::from(w[0]).encode() < Value::from(w[1]).encode(), "{:?}", w);
        }
        assert_eq!(Value::Float(-0.0).encode(), Value::Float(0.0).encode());
    }

    #[test]
    fn encodings_are_self_delimiting() {
        let mut buf = Value::from("x\0y").encode();
        buf.extend(Value::Int(7).encode());
        let (a, n) = Value::decode(&buf).unwrap();
        assert_eq!(a, Value::from("x\0y"));
        assert_eq!(Value::decode_all(&buf[n..]).unwrap(), Value::Int(7));
    }
}
