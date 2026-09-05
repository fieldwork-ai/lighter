//! Translating macOS error numbers into Linux ones.
//!
//! # Why this file exists at all
//!
//! A FUSE reply carries a raw negative errno, and the guest interprets it with
//! Linux's table. The two tables agree for the first 34 values and then
//! diverge completely: macOS `ENOTEMPTY` is 66, Linux's is 39, and 39 on macOS
//! is `EDESTADDRREQ`. Passing a host errno straight through therefore does not
//! produce a slightly-wrong message — it produces `rmdir` on a non-empty
//! directory reporting "destination address required", and `getxattr` on a
//! missing attribute reporting "no space left on device", which is how a
//! coherent filesystem comes to look like failing hardware.
//!
//! Anything not in the table becomes `EIO`. A wrong-but-plausible errno is far
//! worse than a blunt one: it sends whoever is debugging in a direction the
//! filesystem never went.

/// Linux error numbers, for the ones we produce ourselves.
pub mod linux {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EBADF: i32 = 9;
    pub const EACCES: i32 = 13;
    pub const EEXIST: i32 = 17;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EINVAL: i32 = 22;
    pub const ENOTTY: i32 = 25;
    pub const ERANGE: i32 = 34;
    pub const ENAMETOOLONG: i32 = 36;
    pub const ENOSYS: i32 = 38;
    pub const ENOTEMPTY: i32 = 39;
    pub const ENODATA: i32 = 61;
    pub const EPROTO: i32 = 71;
    pub const EOVERFLOW: i32 = 75;
    pub const EOPNOTSUPP: i32 = 95;
    /// What a parked inode returns when the path it was parked at now names
    /// something else. The guest re-looks-up rather than failing the syscall.
    pub const ESTALE: i32 = 116;
}

/// Maps a macOS `errno` to the Linux value with the same meaning.
pub fn to_linux(host: i32) -> i32 {
    // The two tables agree from 1 to 34 with exactly one exception: 11 is
    // `EDEADLK` on macOS and `EAGAIN` on Linux, and the pair is swapped — 35 is
    // the other way round. It is the worst possible collision, because both are
    // plausible answers from a filesystem call, so it is excluded from the
    // shared range rather than left to the arms below.
    if (1..=10).contains(&host) || (12..=34).contains(&host) {
        return host;
    }
    match host {
        libc::EAGAIN => 11,  // == EWOULDBLOCK on macOS (35)
        libc::EDEADLK => 35, // macOS 11
        libc::ENOTSOCK => 88,
        libc::EDESTADDRREQ => 89,
        libc::EMSGSIZE => 90,
        libc::EPROTOTYPE => 91,
        libc::ENOPROTOOPT => 92,
        libc::EPROTONOSUPPORT => 93,
        libc::ENOTSUP => linux::EOPNOTSUPP,
        libc::EAFNOSUPPORT => 97,
        libc::EADDRINUSE => 98,
        libc::EADDRNOTAVAIL => 99,
        libc::ENETDOWN => 100,
        libc::ENETUNREACH => 101,
        libc::ENETRESET => 102,
        libc::ECONNABORTED => 103,
        libc::ECONNRESET => 104,
        libc::ENOBUFS => 105,
        libc::EISCONN => 106,
        libc::ENOTCONN => 107,
        libc::ETIMEDOUT => 110,
        libc::ECONNREFUSED => 111,
        libc::ELOOP => 40,
        libc::ENAMETOOLONG => linux::ENAMETOOLONG,
        libc::EHOSTDOWN => 112,
        libc::EHOSTUNREACH => 113,
        libc::ENOTEMPTY => linux::ENOTEMPTY,
        libc::EUSERS => 87,
        libc::EDQUOT => 122,
        libc::ESTALE => 116,
        libc::ENOLCK => 37,
        libc::ENOSYS => linux::ENOSYS,
        libc::EOVERFLOW => linux::EOVERFLOW,
        libc::ECANCELED => 125,
        libc::EIDRM => 43,
        libc::ENOMSG => 42,
        libc::EILSEQ => 84,
        libc::EBADMSG => 74,
        libc::EMULTIHOP => 72,
        // macOS has *both*: ENOATTR (93) is what the xattr calls return, and
        // ENODATA (96) exists for STREAMS. Linux has only ENODATA (61), and
        // every xattr miss arrives as the first of the two.
        libc::ENOATTR => linux::ENODATA,
        libc::ENODATA => linux::ENODATA,
        libc::ENOLINK => 67,
        libc::ENOSR => 63,
        libc::ENOSTR => 60,
        libc::EPROTO => linux::EPROTO,
        libc::ETIME => 62,
        libc::EOWNERDEAD => 130,
        libc::ENOTRECOVERABLE => 131,
        _ => linux::EIO,
    }
}

/// The last `errno`, translated. Call immediately after a failing syscall.
pub fn last() -> i32 {
    to_linux(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four that motivated the whole module. Each one, passed through
    /// unmapped, produces a specific and very confusing lie.
    #[test]
    fn the_divergent_values_are_mapped() {
        assert_eq!(
            to_linux(libc::ENOTEMPTY),
            39,
            "macOS 66 is EDESTADDRREQ-ish nonsense on Linux"
        );
        assert_eq!(to_linux(libc::ENOSYS), 38);
        assert_eq!(
            to_linux(libc::ENODATA),
            61,
            "macOS ENOATTR must become Linux ENODATA"
        );
        assert_eq!(to_linux(libc::ELOOP), 40);
        assert_eq!(to_linux(libc::ENAMETOOLONG), 36);
    }

    /// EAGAIN and EDEADLK are swapped between the two systems, which is the
    /// nastiest pair in the table: both are plausible in filesystem code, and
    /// 11 sits inside what otherwise looks like a safe identical prefix.
    #[test]
    fn eagain_and_edeadlk_cross_over() {
        assert_eq!(to_linux(libc::EAGAIN), 11);
        assert_eq!(to_linux(libc::EDEADLK), 35);
    }

    #[test]
    fn the_shared_prefix_passes_through() {
        for value in [libc::ENOENT, libc::EEXIST, libc::EINVAL, libc::ERANGE] {
            assert_eq!(to_linux(value), value);
        }
    }

    /// A host errno we have never seen must not be forwarded as itself: on
    /// Linux the same number means something unrelated.
    #[test]
    fn an_unknown_errno_becomes_eio() {
        assert_eq!(to_linux(9999), linux::EIO);
        assert_eq!(to_linux(0), linux::EIO);
    }
}
