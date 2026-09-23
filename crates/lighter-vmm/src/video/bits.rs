//! Reading and writing the bit-level syntax of H.264 and HEVC headers: the
//! RBSP (emulation prevention removed and put back), fixed-width fields and
//! Exp-Golomb codes.

#[derive(Default)]
pub(crate) struct BitWriter {
    pub(crate) bytes: Vec<u8>,
    pub(crate) n: usize,
}

impl BitWriter {
    pub(crate) fn bit(&mut self, b: bool) {
        if self.n.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if b {
            *self.bytes.last_mut().expect("pushed") |= 1 << (7 - self.n % 8);
        }
        self.n += 1;
    }
}

/// The payload with emulation-prevention bytes put back: a 3 after any two
/// zeros that a byte of 3 or less would follow.
pub(crate) fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// The RBSP: the payload with emulation-prevention bytes (`00 00 03`)
/// removed.
pub(crate) fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

pub(crate) struct Bits<'a> {
    data: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    pub(crate) fn bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b == 1)
    }

    pub(crate) fn bits(&mut self, n: u32) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        Some(v)
    }

    /// Exp-Golomb, unsigned.
    pub(crate) fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        Some(((1u64 << zeros) - 1 + rest) as u32)
    }

    /// Exp-Golomb, signed.
    pub(crate) fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}
