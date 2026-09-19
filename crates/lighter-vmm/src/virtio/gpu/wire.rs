//! The virtio-gpu wire format: the subset a render-only device speaks.
//! Layouts and values are the kernel's `include/uapi/linux/virtio_gpu.h`.

pub const F_VIRGL: u64 = 1 << 0;
pub const F_RESOURCE_BLOB: u64 = 1 << 3;
pub const F_CONTEXT_INIT: u64 = 1 << 4;

pub const FLAG_FENCE: u32 = 1 << 0;
pub const FLAG_INFO_RING_IDX: u32 = 1 << 1;

pub const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const CMD_RESOURCE_UNREF: u32 = 0x0102;
pub const CMD_GET_CAPSET_INFO: u32 = 0x0108;
pub const CMD_GET_CAPSET: u32 = 0x0109;
pub const CMD_RESOURCE_CREATE_BLOB: u32 = 0x010c;
pub const CMD_CTX_CREATE: u32 = 0x0200;
pub const CMD_CTX_DESTROY: u32 = 0x0201;
pub const CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
pub const CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
pub const CMD_SUBMIT_3D: u32 = 0x0207;
pub const CMD_RESOURCE_MAP_BLOB: u32 = 0x0208;
pub const CMD_RESOURCE_UNMAP_BLOB: u32 = 0x0209;

pub const RESP_OK_NODATA: u32 = 0x1100;
pub const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub const RESP_OK_CAPSET_INFO: u32 = 0x1102;
pub const RESP_OK_CAPSET: u32 = 0x1103;
pub const RESP_OK_MAP_INFO: u32 = 0x1106;
pub const RESP_ERR_UNSPEC: u32 = 0x1200;
pub const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
pub const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// The shared-memory region id the driver maps blobs through.
pub const SHM_ID_HOST_VISIBLE: u8 = 1;

pub const CAPSET_VENUS: u32 = 4;

pub const BLOB_MEM_GUEST: u32 = 0x0001;
pub const BLOB_MEM_HOST3D: u32 = 0x0002;
pub const BLOB_MEM_HOST3D_GUEST: u32 = 0x0003;

/// `struct virtio_gpu_ctrl_hdr`: 24 bytes, at the head of every command and
/// every response.
pub const HDR_LEN: usize = 24;
/// A scanout entry in `resp_display_info`, of which there are always sixteen.
pub const DISPLAY_INFO_LEN: usize = HDR_LEN + 16 * 24;

#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub kind: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
}

impl Header {
    pub fn parse(b: &[u8]) -> Option<Header> {
        if b.len() < HDR_LEN {
            return None;
        }
        Some(Header {
            kind: u32_at(b, 0),
            flags: u32_at(b, 4),
            fence_id: u64_at(b, 8),
            ctx_id: u32_at(b, 16),
            ring_idx: b[20],
        })
    }

    /// The response header for this command: same fence, context and ring,
    /// the fence flag echoed so the driver knows to wait for it.
    pub fn response(&self, kind: u32, out: &mut Vec<u8>) {
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&(self.flags & FLAG_FENCE).to_le_bytes());
        out.extend_from_slice(&self.fence_id.to_le_bytes());
        out.extend_from_slice(&self.ctx_id.to_le_bytes());
        out.push(self.ring_idx);
        out.extend_from_slice(&[0, 0, 0]);
    }
}

pub fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
pub fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
