//! DNS-SD through mDNSResponder (Bonjour), with the `dns_sd.h` API of libSystem:
//! <https://developer.apple.com/documentation/dnssd>.
//!
//! macOS 15 asks the person for "Local Network" access the first time a program
//! browses: the app's Info.plist carries NSLocalNetworkUsageDescription and
//! NSBonjourServices (`_nuvira._udp`).

use std::ffi::{CStr, CString, c_char, c_void};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::{Announcement, Found, SERVICE_TYPE};

type DnsServiceRef = *mut c_void;
type BrowseReply =
    extern "C" fn(DnsServiceRef, u32, u32, i32, *const c_char, *const c_char, *const c_char, *mut c_void);
type ResolveReply = extern "C" fn(
    DnsServiceRef,
    u32,
    u32,
    i32,
    *const c_char,
    *const c_char,
    u16,
    u16,
    *const u8,
    *mut c_void,
);

const NO_ERROR: i32 = 0;
/// kDNSServiceFlagsAdd: the browse reply adds a service (otherwise it removes one).
const FLAGS_ADD: u32 = 0x2;

unsafe extern "C" {
    fn DNSServiceRegister(
        sd_ref: *mut DnsServiceRef,
        flags: u32,
        interface_index: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_len: u16,
        txt_record: *const c_void,
        callback: *const c_void,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceBrowse(
        sd_ref: *mut DnsServiceRef,
        flags: u32,
        interface_index: u32,
        regtype: *const c_char,
        domain: *const c_char,
        callback: BrowseReply,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceResolve(
        sd_ref: *mut DnsServiceRef,
        flags: u32,
        interface_index: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        callback: ResolveReply,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(sd_ref: DnsServiceRef) -> i32;
    fn DNSServiceProcessResult(sd_ref: DnsServiceRef) -> i32;
    fn DNSServiceRefDeallocate(sd_ref: DnsServiceRef);
}

/// A registration with mDNSResponder; it ends when the reference is deallocated.
pub(crate) struct Registration(DnsServiceRef);

// The reference is only used again by drop.
unsafe impl Send for Registration {}
unsafe impl Sync for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: a reference made by DNSServiceRegister, deallocated once.
        unsafe { DNSServiceRefDeallocate(self.0) };
    }
}

/// The TXT record's wire format: each string preceded by its length.
fn txt_record(strings: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in strings {
        let b = &s.as_bytes()[..s.len().min(255)];
        out.push(b.len() as u8);
        out.extend_from_slice(b);
    }
    out
}

fn txt_strings(record: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = record;
    while let Some((&len, tail)) = rest.split_first() {
        let len = (len as usize).min(tail.len());
        out.push(String::from_utf8_lossy(&tail[..len]).into_owned());
        rest = &tail[len..];
    }
    out
}

pub(crate) fn register(a: &Announcement) -> Result<Registration> {
    let name = CString::new(a.instance())?;
    let regtype = CString::new(SERVICE_TYPE)?;
    let txt = txt_record(&a.txt());
    let mut sd: DnsServiceRef = std::ptr::null_mut();
    // SAFETY: valid strings and buffer for the call; no callback (allowed: the
    // registration then reports no asynchronous errors).
    let err = unsafe {
        DNSServiceRegister(
            &mut sd,
            0,
            0,
            name.as_ptr(),
            regtype.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            21116u16.to_be(),
            txt.len() as u16,
            txt.as_ptr().cast(),
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    if err != NO_ERROR {
        bail!("DNSServiceRegister failed: {err}");
    }
    Ok(Registration(sd))
}

/// Runs the replies of `sd` until `deadline`.
fn pump(sd: DnsServiceRef, deadline: Instant) {
    // SAFETY: a valid reference.
    let fd = unsafe { DNSServiceRefSockFD(sd) };
    if fd < 0 {
        return;
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if n <= 0 {
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        // SAFETY: data is ready on the reference's socket.
        if unsafe { DNSServiceProcessResult(sd) } != NO_ERROR {
            return;
        }
    }
}

struct Browsed {
    /// (instance, domain, interface) of the services seen.
    names: Vec<(String, String, u32)>,
}

extern "C" fn browse_reply(
    _sd: DnsServiceRef,
    flags: u32,
    interface: u32,
    err: i32,
    name: *const c_char,
    _regtype: *const c_char,
    domain: *const c_char,
    ctx: *mut c_void,
) {
    if err != NO_ERROR || name.is_null() || domain.is_null() {
        return;
    }
    // SAFETY: the context is the Browsed of `browse_blocking`, alive while it pumps.
    let b = unsafe { &mut *(ctx as *mut Browsed) };
    let entry = unsafe {
        (
            CStr::from_ptr(name).to_string_lossy().into_owned(),
            CStr::from_ptr(domain).to_string_lossy().into_owned(),
        )
    };
    b.names.retain(|(n, _, _)| *n != entry.0);
    if flags & FLAGS_ADD != 0 {
        b.names.push((entry.0, entry.1, interface));
    }
}

extern "C" fn resolve_reply(
    _sd: DnsServiceRef,
    _flags: u32,
    _interface: u32,
    err: i32,
    _fullname: *const c_char,
    _host: *const c_char,
    _port: u16,
    txt_len: u16,
    txt: *const u8,
    ctx: *mut c_void,
) {
    if err != NO_ERROR || txt.is_null() {
        return;
    }
    // SAFETY: the context is the Option of `browse_blocking`, alive while it pumps;
    // the record is txt_len bytes.
    let out = unsafe { &mut *(ctx as *mut Option<Vec<String>>) };
    let record = unsafe { std::slice::from_raw_parts(txt, txt_len as usize) };
    *out = Some(txt_strings(record));
}

fn browse_blocking(wait: Duration) -> Result<Vec<Found>> {
    let regtype = CString::new(SERVICE_TYPE)?;
    let mut browsed = Browsed { names: Vec::new() };
    let mut sd: DnsServiceRef = std::ptr::null_mut();
    // SAFETY: valid strings; the context outlives the reference (deallocated below).
    let err = unsafe {
        DNSServiceBrowse(
            &mut sd,
            0,
            0,
            regtype.as_ptr(),
            std::ptr::null(),
            browse_reply,
            (&mut browsed as *mut Browsed).cast(),
        )
    };
    if err != NO_ERROR {
        bail!("DNSServiceBrowse failed: {err}");
    }
    pump(sd, Instant::now() + wait);
    // SAFETY: made above, deallocated once.
    unsafe { DNSServiceRefDeallocate(sd) };

    let mut found = Vec::new();
    for (name, domain, interface) in browsed.names {
        let (Ok(cname), Ok(cdomain)) = (CString::new(name), CString::new(domain)) else { continue };
        let mut txt: Option<Vec<String>> = None;
        let mut rd: DnsServiceRef = std::ptr::null_mut();
        // SAFETY: as for the browse.
        let err = unsafe {
            DNSServiceResolve(
                &mut rd,
                0,
                interface,
                cname.as_ptr(),
                regtype.as_ptr(),
                cdomain.as_ptr(),
                resolve_reply,
                (&mut txt as *mut Option<Vec<String>>).cast(),
            )
        };
        if err != NO_ERROR {
            continue;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while txt.is_none() && Instant::now() < deadline {
            pump(rd, (Instant::now() + Duration::from_millis(200)).min(deadline));
        }
        // SAFETY: made above, deallocated once.
        unsafe { DNSServiceRefDeallocate(rd) };
        found.extend(txt.and_then(|t| Announcement::from_txt(&t)));
    }
    Ok(found)
}

pub(crate) async fn browse(wait: Duration) -> Result<Vec<Found>> {
    tokio::task::spawn_blocking(move || browse_blocking(wait)).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_record_round_trip() {
        let s = vec!["id=123456789".to_string(), "n=Mac".to_string(), String::new()];
        assert_eq!(txt_strings(&txt_record(&s)), s);
        // A length past the end is cut, not read beyond.
        assert_eq!(txt_strings(&[5, b'a', b'b']), vec!["ab".to_string()]);
    }
}
