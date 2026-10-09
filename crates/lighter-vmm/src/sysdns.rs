//! Any record type, from the Mac's own resolver.
//!
//! Address questions go through `getaddrinfo`, which is the Mac's resolver
//! with everything configured into it: per-domain resolvers in
//! `/etc/resolver`, the scoped resolvers a VPN installs, `.local` by
//! multicast DNS. The other types (TXT, SRV, MX, PTR, CAA…) were sent raw
//! to the first nameserver in `resolv.conf`, which knows none of that: a
//! VPN's internal SRV record, or a `.local` service a container browses
//! for, was not found. `DNSServiceQueryRecord` is that same resolver for
//! any type; this asks it once and returns what it says.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::time::{Duration, Instant};

type DNSServiceRef = *mut c_void;
type QueryCallback = extern "C" fn(
    DNSServiceRef,
    u32,
    u32,
    i32,
    *const c_char,
    u16,
    u16,
    u16,
    *const c_void,
    u32,
    *mut c_void,
);

const MORE_COMING: u32 = 0x1;
const ADD: u32 = 0x2;
const RETURN_INTERMEDIATES: u32 = 0x1000;
const TIMEOUT: u32 = 0x10000;
const NO_SUCH_NAME: i32 = -65538;
const NO_SUCH_RECORD: i32 = -65554;

unsafe extern "C" {
    fn DNSServiceQueryRecord(
        sd: *mut DNSServiceRef,
        flags: u32,
        interface: u32,
        fullname: *const c_char,
        rrtype: u16,
        rrclass: u16,
        callback: QueryCallback,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(sd: DNSServiceRef) -> c_int;
    fn DNSServiceProcessResult(sd: DNSServiceRef) -> i32;
    fn DNSServiceRefDeallocate(sd: DNSServiceRef);
}

/// One record of an answer: its owner name (presentation form, without the
/// final dot), type, TTL and data as it goes on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub rtype: u16,
    pub ttl: u32,
    pub rdata: Vec<u8>,
}

/// What the resolver said.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Records(Vec<Record>),
    /// The name exists without records of the type, or does not exist.
    NoData,
    NoName,
}

#[derive(Default)]
struct Collected {
    records: Vec<Record>,
    negative: Option<i32>,
    error: Option<i32>,
    batch_done: bool,
}

extern "C" fn on_record(
    _: DNSServiceRef,
    flags: u32,
    _: u32,
    error: i32,
    fullname: *const c_char,
    rrtype: u16,
    _: u16,
    rdlen: u16,
    rdata: *const c_void,
    ttl: u32,
    context: *mut c_void,
) {
    // SAFETY: the context is the `Collected` `query` passed, alive for the
    // whole query; the strings and data are valid for this call.
    let got = unsafe { &mut *context.cast::<Collected>() };
    match error {
        0 if flags & ADD != 0 => {
            let name = unsafe { CStr::from_ptr(fullname) }.to_string_lossy();
            let data = unsafe { std::slice::from_raw_parts(rdata.cast::<u8>(), rdlen as usize) };
            got.records.push(Record {
                name: name.trim_end_matches('.').to_string(),
                rtype: rrtype,
                ttl,
                rdata: data.to_vec(),
            });
        }
        0 => {}
        NO_SUCH_RECORD | NO_SUCH_NAME => got.negative = Some(error),
        e => got.error = Some(e),
    }
    if flags & MORE_COMING == 0 {
        got.batch_done = true;
    }
}

/// Asks the Mac's resolver for `name`'s records of `rtype` in `class`,
/// waiting at most `wait`. `None` when it could not say: not running,
/// timed out, refused the question.
pub fn query(name: &str, rtype: u16, class: u16, wait: Duration) -> Option<Answer> {
    let fullname = CString::new(name).ok()?;
    let mut got = Collected::default();
    let mut sd: DNSServiceRef = std::ptr::null_mut();
    // SAFETY: an out-pointer, a NUL-terminated name, a callback that lives
    // forever and a context that outlives the reference (deallocated below
    // before `got` goes).
    let error = unsafe {
        DNSServiceQueryRecord(
            &mut sd,
            RETURN_INTERMEDIATES | TIMEOUT,
            0,
            fullname.as_ptr(),
            rtype,
            class,
            on_record,
            std::ptr::addr_of_mut!(got).cast(),
        )
    };
    if error != 0 {
        return None;
    }
    let deadline = Instant::now() + wait;
    // A `.local` answer comes from whoever answers, a moment apart: past
    // the first batch, a little longer for the rest.
    let linger = name.ends_with(".local") || name.ends_with(".local.");
    let mut settled_at: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let until = match settled_at {
            Some(at) => (at + Duration::from_millis(150)).min(deadline),
            None => deadline,
        };
        if now >= until {
            break;
        }
        // SAFETY: a live reference's descriptor.
        let fd = unsafe { DNSServiceRefSockFD(sd) };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (until - now).as_millis().min(i32::MAX as u128) as c_int;
        // SAFETY: one pollfd.
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            continue;
        }
        // SAFETY: a live reference with something to read; the callback
        // writes into `got`.
        if unsafe { DNSServiceProcessResult(sd) } != 0 {
            break;
        }
        if got.error.is_some() {
            break;
        }
        if got.batch_done && (!got.records.is_empty() || got.negative.is_some()) {
            if !linger || got.records.is_empty() {
                break;
            }
            settled_at.get_or_insert_with(Instant::now);
            got.batch_done = false;
        }
    }
    // SAFETY: the reference is live and not used again; no callback runs
    // after this, so `got` is ours alone.
    unsafe { DNSServiceRefDeallocate(sd) };
    if !got.records.is_empty() {
        return Some(Answer::Records(got.records));
    }
    match got.negative {
        Some(NO_SUCH_NAME) => Some(Answer::NoName),
        Some(_) => Some(Answer::NoData),
        None => None,
    }
}

/// A name in presentation form (`My\032Printer._ipp._tcp.local`) as wire
/// labels; `None` for one that cannot be.
pub fn wire_name(name: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(name.len() + 2);
    let mut label = Vec::new();
    let bytes = name.trim_end_matches('.').as_bytes();
    let mut i = 0;
    let push = |out: &mut Vec<u8>, label: &mut Vec<u8>| -> Option<()> {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        out.push(label.len() as u8);
        out.append(label);
        Some(())
    };
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let rest = bytes.get(i + 1..)?;
                if rest.len() >= 3 && rest[..3].iter().all(u8::is_ascii_digit) {
                    let v: u32 = std::str::from_utf8(&rest[..3]).ok()?.parse().ok()?;
                    label.push(u8::try_from(v).ok()?);
                    i += 4;
                } else {
                    label.push(*rest.first()?);
                    i += 2;
                }
            }
            b'.' => {
                push(&mut out, &mut label)?;
                i += 1;
            }
            b => {
                label.push(b);
                i += 1;
            }
        }
    }
    if !label.is_empty() {
        push(&mut out, &mut label)?;
    }
    out.push(0);
    (out.len() <= 255).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_names_become_wire_labels() {
        assert_eq!(wire_name("example.com").unwrap(), b"\x07example\x03com\x00");
        assert_eq!(
            wire_name("example.com.").unwrap(),
            b"\x07example\x03com\x00"
        );
        assert_eq!(
            wire_name(r"My\032Printer._ipp._tcp.local").unwrap(),
            b"\x0aMy Printer\x04_ipp\x04_tcp\x05local\x00"
        );
        assert_eq!(wire_name(r"a\.b.c").unwrap(), b"\x03a.b\x01c\x00");
        assert_eq!(wire_name("a..b"), None);
        assert_eq!(wire_name(&"x".repeat(64)), None);
    }

    /// A question the Mac's resolver answers offline.
    #[test]
    fn the_resolver_answers_or_says_it_cannot() {
        let answer = query("localhost", 1, 1, Duration::from_secs(3));
        match answer {
            Some(Answer::Records(records)) => {
                assert!(
                    records
                        .iter()
                        .any(|r| r.rtype == 1 && r.rdata == [127, 0, 0, 1]),
                    "{records:?}"
                )
            }
            other => panic!("localhost has an A record: {other:?}"),
        }
    }
}
