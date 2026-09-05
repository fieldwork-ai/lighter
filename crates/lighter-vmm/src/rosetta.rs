//! Rosetta for Linux: where it is on the Mac, and the answer its file server
//! has to give.
//!
//! Apple ships one arm64 binary, `rosetta`, which translates x86-64 Linux
//! programs. Before it will run, it asks the file server that carries it a
//! few ioctls and expects a 69-byte constant back for two of them (observed
//! with a probe inside a Virtualization.framework machine, 2026-09-05,
//! `docs/worklog.md`). That constant is Apple's text, and this program does
//! not contain it: it is found in the user's own installed copy of Rosetta,
//! which everyone who runs x86-64 containers has installed anyway, and
//! recognised by its hash. A Rosetta that stops carrying it is reported, not
//! guessed at.

use std::io;
use std::path::Path;

/// The share tag the guest mounts Rosetta under, and the only tag whose
/// server answers Rosetta's ioctls.
pub const TAG: &str = "rosetta";

/// Where Apple installs Rosetta for Linux.
pub const DIR: &str = "/Library/Apple/usr/libexec/oah/RosettaLinux";

/// Length of the constant the two verification ioctls return.
const KEY_LEN: usize = 69;

/// SHA-256 of that constant.
const KEY_SHA256: [u8; 32] = [
    0x20, 0x39, 0x80, 0xd5, 0x04, 0x72, 0x29, 0x67, 0xf0, 0xf9, 0xee, 0x2f, 0xc7, 0xa9, 0x76, 0x68,
    0x9d, 0xa9, 0x1d, 0x71, 0x5d, 0x30, 0x2f, 0x80, 0xbf, 0xbd, 0xfb, 0xfa, 0x1a, 0xd4, 0xff, 0x83,
];

/// Whether Rosetta for Linux is installed.
pub fn installed() -> bool {
    Path::new(DIR).join("rosetta").is_file()
}

/// The constant Rosetta expects from its file server, read out of the
/// installed binary.
///
/// The constant is a NUL-terminated string in the binary's read-only data,
/// so only windows that are one — 68 bytes without a NUL, then a NUL — are
/// hashed: a few thousand, not the 1.7 million a byte-by-byte scan would
/// cost.
pub fn key() -> io::Result<Vec<u8>> {
    let path = Path::new(DIR).join("rosetta");
    let data = std::fs::read(&path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Rosetta is not installed ({}): {e}", path.display()),
        )
    })?;
    if let Some(key) = find_key(&data) {
        return Ok(key.to_vec());
    }
    Err(io::Error::other(format!(
        "this Rosetta ({}) does not carry the constant its file server is expected to \
         answer with; lighter needs updating for it",
        path.display()
    )))
}

fn find_key(data: &[u8]) -> Option<&[u8]> {
    if data.len() < KEY_LEN {
        return None;
    }
    let mut last_nul: Option<usize> = None;
    for (end, &byte) in data.iter().enumerate() {
        if byte != 0 {
            continue;
        }
        let run = end - last_nul.map_or(0, |n| n + 1);
        last_nul = Some(end);
        if run < KEY_LEN - 1 {
            continue;
        }
        let window = &data[end + 1 - KEY_LEN..=end];
        if sha256(window) == KEY_SHA256 {
            return Some(window);
        }
    }
    None
}

fn sha256(data: &[u8]) -> [u8; 32] {
    unsafe extern "C" {
        fn CC_SHA256(data: *const u8, len: u32, md: *mut u8) -> *mut u8;
    }
    let mut out = [0u8; 32];
    // SAFETY: CommonCrypto reads `len` bytes of `data` and writes 32 bytes
    // to `md`; both buffers are exactly that large.
    unsafe {
        CC_SHA256(data.as_ptr(), data.len() as u32, out.as_mut_ptr());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_a_known_vector() {
        let got = sha256(b"abc");
        assert_eq!(
            got[..4],
            [0xba, 0x78, 0x16, 0xbf],
            "SHA-256(\"abc\") starts ba7816bf"
        );
    }

    #[test]
    fn a_window_is_only_found_between_nuls() {
        // A buffer with no NUL-delimited 69-byte window yields nothing, and
        // does so quickly.
        let data = vec![b'x'; 4096];
        assert!(find_key(&data).is_none());
    }

    #[test]
    fn the_installed_rosetta_carries_the_key() {
        if !installed() {
            return;
        }
        let key = key().expect("installed Rosetta carries the constant");
        assert_eq!(key.len(), KEY_LEN);
        assert_eq!(key[KEY_LEN - 1], 0, "NUL-terminated");
    }
}
