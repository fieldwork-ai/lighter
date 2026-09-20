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
