//! Deterministic binary encoding: little-endian integers, u32-length-prefixed strings.
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    UnexpectedEnd,
    TrailingBytes,
    InvalidUtf8,
    InvalidFlag(u8),
    InvalidTag(u8),
    TooLong,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::UnexpectedEnd => write!(f, "unexpected end of data"),
            CodecError::TrailingBytes => write!(f, "trailing bytes after record"),
            CodecError::InvalidUtf8 => write!(f, "invalid UTF-8 in string"),
            CodecError::InvalidFlag(v) => write!(f, "invalid flag byte {v}"),
            CodecError::InvalidTag(v) => write!(f, "unknown record tag {v}"),
            CodecError::TooLong => write!(f, "field exceeds maximum length"),
        }
    }
}

impl std::error::Error for CodecError {}

#[derive(Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn raw(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    pub fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.raw(s.as_bytes());
    }
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

pub struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Decoder { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let end = self.pos.checked_add(n).ok_or(CodecError::UnexpectedEnd)?;
        if end > self.data.len() {
            return Err(CodecError::UnexpectedEnd);
        }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        self.take(N)?
            .try_into()
            .map_err(|_| CodecError::UnexpectedEnd)
    }
    pub fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.array::<1>()?[0])
    }
    pub fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_le_bytes(self.array::<4>()?))
    }
    pub fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_le_bytes(self.array::<8>()?))
    }
    pub fn i64(&mut self) -> Result<i64, CodecError> {
        Ok(i64::from_le_bytes(self.array::<8>()?))
    }
    pub fn flag(&mut self) -> Result<bool, CodecError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            x => Err(CodecError::InvalidFlag(x)),
        }
    }
    pub fn string(&mut self, max_len: usize) -> Result<String, CodecError> {
        let len = self.u32()? as usize;
        if len > max_len {
            return Err(CodecError::TooLong);
        }
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(|s| s.to_owned())
            .map_err(|_| CodecError::InvalidUtf8)
    }
    pub fn finish(self) -> Result<(), CodecError> {
        if self.pos != self.data.len() {
            Err(CodecError::TrailingBytes)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut e = Encoder::new();
        e.u8(7);
        e.u32(70_000);
        e.u64(u64::MAX);
        e.i64(-5);
        e.str("héllo");
        let bytes = e.finish();
        let mut d = Decoder::new(&bytes);
        assert_eq!(d.u8().unwrap(), 7);
        assert_eq!(d.u32().unwrap(), 70_000);
        assert_eq!(d.u64().unwrap(), u64::MAX);
        assert_eq!(d.i64().unwrap(), -5);
        assert_eq!(d.string(64).unwrap(), "héllo");
        assert!(d.finish().is_ok());
    }

    #[test]
    fn rejects_truncated_and_trailing() {
        let mut d = Decoder::new(&[1, 2]);
        assert_eq!(d.u32(), Err(CodecError::UnexpectedEnd));
        let d = Decoder::new(&[1]);
        assert_eq!(d.finish(), Err(CodecError::TrailingBytes));
    }
}
