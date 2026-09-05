//! Loading an aarch64 Linux kernel and its initramfs.
//!
//! We boot the kernel directly: no EFI, no bootloader, no self-decompressing
//! wrapper. The kernel is an uncompressed `Image` and we honour its documented
//! boot protocol (`Documentation/arch/arm64/booting.rst`) ourselves. This is
//! most of the reason a lighter VM reaches userspace fast — there is simply
//! nothing between power-on and the kernel's first instruction.

use std::fs;
use std::path::Path;

use crate::layout::GuestLayout;
use crate::memory::GuestMemory;

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{path} is not an arm64 kernel Image (magic {found:#010x}, expected {expected:#010x}) \
         — a vmlinux ELF or a compressed Image.gz will not boot; use arch/arm64/boot/Image"
    )]
    BadMagic {
        path: String,
        found: u32,
        expected: u32,
    },
    #[error("kernel Image is truncated: {0} bytes is shorter than its own 64-byte header")]
    Truncated(usize),
    #[error("the kernel needs {needed:#x} bytes at {base:#x} but guest RAM is only {available:#x}")]
    DoesNotFit {
        base: u64,
        needed: u64,
        available: u64,
    },
    #[error("writing the kernel into guest memory: {0}")]
    Memory(#[from] crate::memory::MemoryError),
}

type Result<T> = std::result::Result<T, KernelError>;

/// `"ARM\x64"` little-endian, at offset 56 of the Image header.
const ARM64_IMAGE_MAGIC: u32 = 0x644d_5241;

/// Where the guest starts executing, and what its registers hold.
#[derive(Debug, Clone, Copy)]
pub struct BootInfo {
    /// Guest-physical address of the kernel's first instruction.
    pub entry: u64,
    /// Guest-physical address of the flattened device tree.
    pub dtb: u64,
}

/// The parsed head of an arm64 `Image`.
#[derive(Debug, Clone, Copy)]
struct ImageHeader {
    /// Offset from the start of 2 MiB-aligned RAM where the image wants to be.
    text_offset: u64,
    /// Bytes of memory the image needs, including its BSS.
    image_size: u64,
}

impl ImageHeader {
    fn parse(bytes: &[u8], path: &str) -> Result<ImageHeader> {
        if bytes.len() < 64 {
            return Err(KernelError::Truncated(bytes.len()));
        }
        let u32_at = |off: usize| {
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        };
        let u64_at = |off: usize| u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());

        let magic = u32_at(56);
        if magic != ARM64_IMAGE_MAGIC {
            return Err(KernelError::BadMagic {
                path: path.to_string(),
                found: magic,
                expected: ARM64_IMAGE_MAGIC,
            });
        }

        let text_offset = u64_at(8);
        let image_size = u64_at(16);

        // Kernels built before 3.17 left image_size zero and expected the
        // loader to guess. We refuse to guess: every kernel we build reports
        // it, and silently assuming a size is how a guest ends up overlapping
        // its own initramfs.
        let image_size = if image_size == 0 {
            bytes.len() as u64
        } else {
            image_size
        };

        Ok(ImageHeader {
            text_offset,
            image_size,
        })
    }
}

/// A kernel and optional initramfs, staged into guest memory.
pub struct KernelLoader<'a> {
    kernel: Vec<u8>,
    kernel_path: String,
    initramfs: Option<Vec<u8>>,
    layout: &'a GuestLayout,
}

/// Where the initramfs ended up, so the device tree can point the kernel at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitramfsPlacement {
    pub start: u64,
    pub end: u64,
}

impl<'a> KernelLoader<'a> {
    pub fn new(layout: &'a GuestLayout, kernel_path: &Path) -> Result<Self> {
        let kernel = fs::read(kernel_path).map_err(|source| KernelError::Io {
            path: kernel_path.display().to_string(),
            source,
        })?;
        Ok(KernelLoader {
            kernel,
            kernel_path: kernel_path.display().to_string(),
            initramfs: None,
            layout,
        })
    }

    pub fn with_initramfs(mut self, path: &Path) -> Result<Self> {
        self.initramfs = Some(fs::read(path).map_err(|source| KernelError::Io {
            path: path.display().to_string(),
            source,
        })?);
        Ok(self)
    }

    /// Where the kernel's entry point will be.
    pub fn entry_point(&self) -> Result<u64> {
        let header = ImageHeader::parse(&self.kernel, &self.kernel_path)?;
        Ok(self.layout.ram.base + header.text_offset)
    }

    /// Copies the kernel and initramfs into guest RAM.
    ///
    /// Layout within RAM, low to high: the kernel at its requested offset, then
    /// the device tree, then the initramfs high enough that the kernel's own
    /// BSS and early allocations cannot reach it before it is unpacked.
    pub fn load(&self, mem: &GuestMemory) -> Result<(BootInfo, Option<InitramfsPlacement>)> {
        let header = ImageHeader::parse(&self.kernel, &self.kernel_path)?;
        let ram_base = self.layout.ram.base;
        let ram_size = self.layout.ram.size;

        let kernel_addr = ram_base + header.text_offset;
        let kernel_end = kernel_addr + header.image_size;
        if kernel_end > ram_base + ram_size {
            return Err(KernelError::DoesNotFit {
                base: kernel_addr,
                needed: header.image_size,
                available: ram_size,
            });
        }
        mem.write(kernel_addr, &self.kernel)?;

        // The device tree goes immediately after the kernel's image footprint,
        // 2 MiB-aligned so it shares no page with kernel BSS.
        let dtb_addr = align_up(kernel_end, 2 << 20);

        let initramfs = match &self.initramfs {
            None => None,
            Some(bytes) => {
                // Place the initramfs in the top quarter of RAM. Anywhere below
                // risks the kernel's early allocator claiming it before
                // populate_rootfs() runs, which presents as a boot that panics
                // with a corrupt cpio rather than as an obvious overlap.
                let start = align_up(ram_base + (ram_size / 4) * 3, 4096);
                let end = start + bytes.len() as u64;
                if end > ram_base + ram_size {
                    return Err(KernelError::DoesNotFit {
                        base: start,
                        needed: bytes.len() as u64,
                        available: ram_base + ram_size - start,
                    });
                }
                mem.write(start, bytes)?;
                Some(InitramfsPlacement { start, end })
            }
        };

        Ok((
            BootInfo {
                entry: kernel_addr,
                dtb: dtb_addr,
            },
            initramfs,
        ))
    }
}

#[inline]
const fn align_up(value: u64, align: u64) -> u64 {
    value.div_ceil(align) * align
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes(text_offset: u64, image_size: u64, magic: u32) -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[8..16].copy_from_slice(&text_offset.to_le_bytes());
        v[16..24].copy_from_slice(&image_size.to_le_bytes());
        v[56..60].copy_from_slice(&magic.to_le_bytes());
        v
    }

    #[test]
    fn parses_a_real_header_shape() {
        let bytes = header_bytes(0x8_0000, 0x200_0000, ARM64_IMAGE_MAGIC);
        let h = ImageHeader::parse(&bytes, "test").unwrap();
        assert_eq!(h.text_offset, 0x8_0000);
        assert_eq!(h.image_size, 0x200_0000);
    }

    // Handing the loader a vmlinux ELF or an Image.gz is the most common way to
    // get a guest that executes garbage; it must be a named error, not a hang.
    #[test]
    fn rejects_anything_that_is_not_an_image() {
        let elf = header_bytes(0, 0, 0x464c_457f);
        assert!(matches!(
            ImageHeader::parse(&elf, "vmlinux"),
            Err(KernelError::BadMagic { .. })
        ));
        assert!(matches!(
            ImageHeader::parse(&[0u8; 16], "short"),
            Err(KernelError::Truncated(16))
        ));
    }

    #[test]
    fn alignment_is_idempotent_and_rounds_up() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
    }
}
