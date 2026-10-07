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
    /// A container's chown, in the guest's numbering, and the permission
    /// bits the record holds. The bits stand in for the file's own only on a
    /// volume that keeps none (exFAT, FAT), where a chmod has nowhere else to
    /// go; elsewhere the file's own are the truth.
    Set(u32, u32, Option<u32>),
}

impl Owner {
    /// The recorded owner, if there is one; otherwise the host's ownership
    /// stands, the Mac user appearing as root. A record naming root is kept
    /// only for its mode, root being how the Mac user already appears.
    pub fn recorded(self) -> Option<(u32, u32)> {
        match self {
            Owner::Set(0, 0, _) => None,
            Owner::Set(uid, gid, _) => Some((uid, gid)),
            Owner::Unknown | Owner::Mac => None,
        }
    }

    /// The permission bits recorded, if any.
    pub fn mode(self) -> Option<u32> {
        match self {
            Owner::Set(_, _, mode) => mode,
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

/// The owner a record names, and its permission bits if it has them; `None`
/// for anything that is not one.
pub fn parse(record: &[u8]) -> Option<Owner> {
    let text = std::str::from_utf8(record).ok()?;
    let mode = digits(text, "mode")
        .and_then(|digits| u32::from_str_radix(digits, 8).ok())
        .map(|mode| mode & 0o7777);
    Some(Owner::Set(field(text, "UID")?, field(text, "GID")?, mode))
}

/// A non-negative integer field of a flat JSON object. The record is written
/// by us or by Docker Desktop, both as one flat object; a parser for all of
/// JSON would accept nothing more that matters.
fn field(text: &str, key: &str) -> Option<u32> {
    digits(text, key)?.parse().ok()
}

/// The digits a field's value is spelled with.
fn digits<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips() {
        let record = encode(1000, 82, 0o100755);
        assert_eq!(record, br#"{"UID":1000,"GID":82,"mode":755}"#);
        assert_eq!(parse(&record), Some(Owner::Set(1000, 82, Some(0o755))));
    }

    #[test]
    fn docker_desktops_record_is_read() {
        assert_eq!(
            parse(br#"{"UID":405,"GID":82,"mode":2770}"#),
            Some(Owner::Set(405, 82, Some(0o2770)))
        );
        assert_eq!(parse(br#"{"UID":501,"GID":20}"#), Some(Owner::Set(501, 20, None)));
        assert_eq!(parse(br#"{ "GID" : 7 , "UID" : 3 }"#), Some(Owner::Set(3, 7, None)));
        assert_eq!(
            parse(br#"{"UID":3,"GID":7,"mode":789}"#),
            Some(Owner::Set(3, 7, None)),
            "a mode that is not octal is no mode"
        );
    }

    #[test]
    fn anything_else_is_not_a_record() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(br#"{"UID":-1,"GID":0}"#), None);
        assert_eq!(parse(br#"{"UID":1000}"#), None);
        assert_eq!(parse(&[0xff, 0xfe]), None);
    }
}
