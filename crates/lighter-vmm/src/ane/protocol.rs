//! The wire between a guest's ONNX Runtime plugin EP and the host runner.
//!
//! Frames, little-endian: `u32 kind`, `u64 len`, `len` bytes. A request gets
//! exactly one reply of the same kind, or `ERR` with a UTF-8 message.
//!
//! - `LOAD`: the payload is an ONNX model; the reply is a `u64` session id.
//! - `RUN`: `u64 session`, `u32 n`, then n tensors; the reply is `u32 n` and
//!   n tensors. A tensor is `u32 element type` (ONNX's numbering), `u32 rank`,
//!   `i64` dims, `u64 byte length`, bytes, row-major.
//! - `CLOSE`: `u64 session`; the reply is empty.
//!
//! The guest side is `guest/ane-ep`; both must move together.

pub const LOAD: u32 = 1;
pub const RUN: u32 = 2;
pub const CLOSE: u32 = 3;
pub const ERR: u32 = 0xffff;

pub const HEADER_LEN: usize = 12;
/// A frame larger than this is refused rather than allocated: a model or a
/// batch of inputs should be well under it, and a corrupt length must not
/// take the host down.
pub const MAX_FRAME: u64 = 4 << 30;

#[derive(Debug, Clone)]
pub struct Tensor {
    pub element_type: u32,
    pub dims: Vec<i64>,
    pub bytes: Vec<u8>,
}

/// Bytes per element for ONNX's element types, or 0 for the ones a tensor
/// cannot carry raw (strings).
pub fn element_size(element_type: u32) -> usize {
    match element_type {
        1 | 6 | 12 => 4,      // float, int32, uint32
        2 | 3 | 9 => 1,       // uint8, int8, bool
        4 | 5 | 10 | 16 => 2, // uint16, int16, float16, bfloat16
        7 | 11 | 13 => 8,     // int64, double, uint64
        _ => 0,
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).ok_or("length overflow")?;
        if end > self.buf.len() {
            return Err(format!("frame truncated at {}", self.at));
        }
        let s = &self.buf[self.at..end];
        self.at = end;
        Ok(s)
    }

    pub fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn tensor(&mut self) -> Result<Tensor, String> {
        let element_type = self.u32()?;
        let rank = self.u32()? as usize;
        if rank > 64 {
            return Err(format!("rank {rank} is absurd"));
        }
        let mut dims = Vec::with_capacity(rank);
        for _ in 0..rank {
            dims.push(self.u64()? as i64);
        }
        let len = self.u64()? as usize;
        let bytes = self.take(len)?.to_vec();
        let elements: usize = dims.iter().map(|d| (*d).max(0) as usize).product();
        let size = element_size(element_type);
        if size == 0 || elements.saturating_mul(size) != len {
            return Err(format!(
                "tensor of type {element_type} with {elements} elements carries {len} bytes"
            ));
        }
        Ok(Tensor {
            element_type,
            dims,
            bytes,
        })
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.at..];
        self.at = self.buf.len();
        s
    }
}

pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn new() -> Writer {
        Writer(Vec::new())
    }

    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn tensor(&mut self, t: &Tensor) {
        self.u32(t.element_type);
        self.u32(t.dims.len() as u32);
        for d in &t.dims {
            self.u64(*d as u64);
        }
        self.u64(t.bytes.len() as u64);
        self.0.extend_from_slice(&t.bytes);
    }
}

impl Default for Writer {
    fn default() -> Self {
        Writer::new()
    }
}

/// A whole frame, header and payload, ready to write.
pub fn frame(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tensor_round_trips() {
        let t = Tensor {
            element_type: 1,
            dims: vec![1, 3],
            bytes: vec![0u8; 12],
        };
        let mut w = Writer::new();
        w.tensor(&t);
        let mut r = Reader::new(&w.0);
        let back = r.tensor().unwrap();
        assert_eq!(back.dims, t.dims);
        assert_eq!(back.bytes.len(), 12);
    }

    #[test]
    fn a_short_tensor_is_refused() {
        let mut w = Writer::new();
        w.u32(1);
        w.u32(1);
        w.u64(3);
        w.u64(4);
        w.0.extend_from_slice(&[0; 4]);
        assert!(Reader::new(&w.0).tensor().is_err());
    }
}
