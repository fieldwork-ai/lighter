//! A protobuf writer, enough for an ONNX ModelProto.
use alloc::vec::Vec;

pub struct Pb(pub Vec<u8>);

impl Pb {
    pub fn new() -> Pb {
        Pb(Vec::new())
    }
    pub fn varint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(b);
                break;
            }
            self.0.push(b | 0x80);
        }
    }
    pub fn tag(&mut self, field: u32, wire: u32) {
        self.varint(((field << 3) | wire) as u64)
    }
    pub fn int(&mut self, field: u32, v: i64) {
        self.tag(field, 0);
        self.varint(v as u64)
    }
    pub fn bytes(&mut self, field: u32, b: &[u8]) {
        self.tag(field, 2);
        self.varint(b.len() as u64);
        self.0.extend_from_slice(b)
    }
    pub fn f32(&mut self, field: u32, v: f32) {
        self.tag(field, 5);
        self.0.extend_from_slice(&v.to_le_bytes())
    }
    pub fn msg(&mut self, field: u32, m: Pb) {
        self.bytes(field, &m.0)
    }
    pub fn packed_i64(&mut self, field: u32, vs: &[i64]) {
        let mut p = Pb::new();
        for v in vs {
            p.varint(*v as u64)
        }
        self.bytes(field, &p.0)
    }
    pub fn packed_f32(&mut self, field: u32, vs: &[f32]) {
        let mut p = Pb::new();
        for v in vs {
            p.0.extend_from_slice(&v.to_le_bytes())
        }
        self.bytes(field, &p.0)
    }
}

/// A protobuf reader, enough to take an ONNX ModelProto apart: each field
/// with its number, its value, and its bytes as they were, so a caller can
/// copy what it does not change.
pub struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

pub enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

pub struct Field<'a> {
    pub number: u32,
    pub value: Value<'a>,
    pub raw: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Reader<'a> {
        Reader { b, p: 0 }
    }
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.b.get(self.p)?;
            self.p += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = Field<'a>;
    /// `None` at the end, and at anything malformed.
    fn next(&mut self) -> Option<Field<'a>> {
        let start = self.p;
        let key = self.varint()?;
        let value = match key & 7 {
            0 => Value::Varint(self.varint()?),
            1 | 5 => {
                self.p += if key & 7 == 1 { 8 } else { 4 };
                Value::Fixed
            }
            2 => {
                let n = self.varint()? as usize;
                let v = self.b.get(self.p..self.p.checked_add(n)?)?;
                self.p += n;
                Value::Bytes(v)
            }
            _ => return None,
        };
        let raw = self.b.get(start..self.p)?;
        Some(Field { number: (key >> 3) as u32, value, raw })
    }
}

/// The bytes of the first field `number` in `b`.
pub fn bytes_of(b: &[u8], number: u32) -> Option<&[u8]> {
    Reader::new(b).find_map(|f| match f.value {
        Value::Bytes(v) if f.number == number => Some(v),
        _ => None,
    })
}
