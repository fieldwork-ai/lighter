//! Who a container says owns a shared file.
//!
//! The host process cannot `chown` a Mac file to anyone else, and should not:
//! the file has to stay its user's on the Mac. So a container's `chown` is
//! recorded on the file instead, in the extended attribute Docker Desktop
//! uses for the same purpose and in its format, and reported back as the
//! owner inside the guest. A tree chowned under either runtime reads the same
//! under the other.
//!
//! Reading the record costs a `getxattr`, about nine `lstat`s on APFS, so it
//! is read only where one can be. [`MARKER`] on a directory says that
//! something directly inside it has been given an owner, and a file is asked
//! for its record only when its directory is marked: a package tree pays one
//! read per directory, not one per file, even on a share where a container
//! has chowned something elsewhere. The share's root is marked with the first
//! record anywhere under it, and a share whose root is not marked reads
//! nothing at all.

use std::ffi::CStr;

/// The record: `{"UID":1000,"GID":1000,"mode":644}`, owner in the guest's
/// numbering and the mode's octal digits written as a decimal number.
pub const RECORD: &CStr = c"com.docker.grpcfuse.ownership";

/// On a directory with an owned entry directly inside it, and on the share's
/// root once anything under it has one.
pub const MARKER: &CStr = c"sh.lighter.ownership";

/// What a directory inode knows of its [`MARKER`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Unknown,
    Unmarked,
    Marked,
}

/// What an inode knows of its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// Not read yet.
    Unknown,
    /// No record: the Mac user, which the guest sees as root.
    Mac,
    /// A container's chown, in the guest's numbering.
    Set(u32, u32),
}

impl Owner {
    /// The recorded owner, if there is one; otherwise the host's ownership
    /// stands, the Mac user appearing as root.
    pub fn recorded(self) -> Option<(u32, u32)> {
        match self {
            Owner::Set(uid, gid) => Some((uid, gid)),
            Owner::Unknown | Owner::Mac => None,
        }
    }
}

/// Whether a name is one of ours, which the guest must not see or change.
pub fn is_ours(name: &[u8]) -> bool {
    name == RECORD.to_bytes() || name == MARKER.to_bytes()
}

/// The record for an owner and the file's current permission bits.
pub fn encode(uid: u32, gid: u32, mode: u32) -> Vec<u8> {
    format!(r#"{{"UID":{uid},"GID":{gid},"mode":{:o}}}"#, mode & 0o7777).into_bytes()
}

/// The owner a record names; `None` for anything that is not one.
pub fn parse(record: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(record).ok()?;
    Some((field(text, "UID")?, field(text, "GID")?))
}

/// A non-negative integer field of a flat JSON object. The record is written
/// by us or by Docker Desktop, both as one flat object; a parser for all of
/// JSON would accept nothing more that matters.
fn field(text: &str, key: &str) -> Option<u32> {
    let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..digits].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips() {
        let record = encode(1000, 82, 0o100755);
        assert_eq!(record, br#"{"UID":1000,"GID":82,"mode":755}"#);
        assert_eq!(parse(&record), Some((1000, 82)));
    }

    #[test]
    fn docker_desktops_record_is_read() {
        assert_eq!(
            parse(br#"{"UID":405,"GID":82,"mode":755}"#),
            Some((405, 82))
        );
        assert_eq!(parse(br#"{"UID":501,"GID":20}"#), Some((501, 20)));
        assert_eq!(parse(br#"{ "GID" : 7 , "UID" : 3 }"#), Some((3, 7)));
    }

    #[test]
    fn anything_else_is_not_a_record() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(br#"{"UID":-1,"GID":0}"#), None);
        assert_eq!(parse(br#"{"UID":1000}"#), None);
        assert_eq!(parse(&[0xff, 0xfe]), None);
    }
}
