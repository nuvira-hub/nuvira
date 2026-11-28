//! DNS-SD through Windows' DNS Client service (dnsapi.dll, Windows 10 1903 or
//! later): the service already answers on port 5353 and has its own firewall
//! rule, so nothing here listens on the network.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use tracing::{debug, warn};
use windows::Win32::Foundation::DNS_REQUEST_PENDING;
use windows::Win32::NetworkManagement::Dns::*;
use windows::core::{PCWSTR, PWSTR};

use crate::{Announcement, Found, SERVICE_TYPE};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// # Safety
/// `p` is null or a valid NUL-terminated wide string.
unsafe fn string(p: PWSTR) -> String {
    if p.is_null() { String::new() } else { unsafe { p.to_string().unwrap_or_default() } }
}

fn full_type() -> String {
    format!("{SERVICE_TYPE}.local")
}

/// A registration in the DNS Client service; deregistered on drop.
pub(crate) struct Registration {
    /// Boxed: its address stays the same while the registration lives (the
    /// registration is moved around after the call).
    request: Box<DNS_SERVICE_REGISTER_REQUEST>,
    instance: *mut DNS_SERVICE_INSTANCE,
}

// The request and the instance are only read by dnsapi, until `drop`.
unsafe impl Send for Registration {}
unsafe impl Sync for Registration {}

unsafe extern "system" fn registered(
    status: u32,
    _ctx: *const c_void,
    instance: *const DNS_SERVICE_INSTANCE,
) {
    if status != 0 {
        warn!(status, "announcement on the local network refused by the DNS Client service");
    }
    if !instance.is_null() {
        // The instance of the callback is a copy for the caller to free.
        unsafe { DnsServiceFreeInstance(instance) };
    }
}

pub(crate) fn register(a: &Announcement) -> Result<Registration> {
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "nuvira".into());
    let name = wide(&format!("{}.{}", a.instance(), full_type()));
    let host = wide(&format!("{host}.local"));
    let (keys, values): (Vec<Vec<u16>>, Vec<Vec<u16>>) =
        a.txt().iter().filter_map(|s| s.split_once('=').map(|(k, v)| (wide(k), wide(v)))).unzip();
    let key_ptrs: Vec<PCWSTR> = keys.iter().map(|k| PCWSTR(k.as_ptr())).collect();
    let value_ptrs: Vec<PCWSTR> = values.iter().map(|v| PCWSTR(v.as_ptr())).collect();
    // SAFETY: every pointer is to a NUL-terminated string alive for the call; the
    // function copies them into the instance it returns.
    let instance = unsafe {
        DnsServiceConstructInstance(
            PCWSTR(name.as_ptr()),
            PCWSTR(host.as_ptr()),
            None,
            None,
            // Not a port anyone connects to: the connection goes through the server.
            21116,
            0,
            0,
            key_ptrs.len() as u32,
            key_ptrs.as_ptr(),
            value_ptrs.as_ptr(),
        )
    };
    if instance.is_null() {
        bail!("DnsServiceConstructInstance failed");
    }
    let request = Box::new(DNS_SERVICE_REGISTER_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1.0,
        InterfaceIndex: 0,
        pServiceInstance: instance,
        pRegisterCompletionCallback: Some(registered),
        pQueryContext: std::ptr::null_mut(),
        hCredentials: Default::default(),
        unicastEnabled: false.into(),
    });
    // SAFETY: the request and its instance are valid and stay so until drop; the
    // callback needs no context.
    let status = unsafe { DnsServiceRegister(&*request, None) };
    if status != DNS_REQUEST_PENDING as u32 {
        // SAFETY: made above, not used by dnsapi after a failed call.
        unsafe { DnsServiceFreeInstance(instance) };
        bail!("DnsServiceRegister failed: {status}");
    }
    Ok(Registration { request, instance })
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: the same request that registered the instance.
        let status = unsafe { DnsServiceDeRegister(&*self.request, None) };
        if status != DNS_REQUEST_PENDING as u32 {
            debug!(status, "DnsServiceDeRegister");
        }
        // SAFETY: dnsapi copied what it needed during the call.
        unsafe { DnsServiceFreeInstance(self.instance) };
    }
}

/// What a browse has gathered, by its number (the callbacks' context): a callback
/// that arrives after its browse ended finds nothing and does nothing.
static BROWSES: Mutex<Option<HashMap<usize, Gathered>>> = Mutex::new(None);
static NEXT: AtomicUsize = AtomicUsize::new(1);

#[derive(Default)]
struct Gathered {
    /// Instance name → its TXT strings (None: announced, TXT not seen yet).
    instances: HashMap<String, Option<Vec<String>>>,
}

fn with_browse(key: usize, f: impl FnOnce(&mut Gathered)) {
    if let Some(g) = BROWSES.lock().expect("browses").as_mut().and_then(|m| m.get_mut(&key)) {
        f(g)
    }
}

unsafe extern "system" fn browsed(status: u32, ctx: *const c_void, records: *const DNS_RECORDW) {
    let key = ctx as usize;
    let mut r = records;
    while status == 0 && !r.is_null() {
        // SAFETY: a record list from dnsapi, freed below.
        let rec = unsafe { &*r };
        let gone = rec.dwTtl == 0;
        if rec.wType == DNS_TYPE_PTR.0 {
            let instance = unsafe { string(rec.Data.PTR.pNameHost) };
            with_browse(key, |g| {
                if gone {
                    g.instances.remove(&instance);
                } else {
                    g.instances.entry(instance).or_default();
                }
            });
        } else if rec.wType == DNS_TYPE_TEXT.0 {
            let instance = unsafe { string(rec.pName) };
            // SAFETY: a TXT record's data holds dwStringCount strings (the array is
            // declared with one element and continues past it).
            let txt = unsafe {
                let data = &rec.Data.TXT;
                let first = data.pStringArray.as_ptr();
                (0..data.dwStringCount as usize).map(|i| string(*first.add(i))).collect::<Vec<_>>()
            };
            with_browse(key, |g| {
                if gone {
                    g.instances.remove(&instance);
                } else {
                    g.instances.insert(instance, Some(txt));
                }
            });
        }
        r = rec.pNext;
    }
    if !records.is_null() {
        // SAFETY: the list given to the callback is the callee's to free.
        unsafe { DnsFree(Some(records as *const c_void), DnsFreeRecordList) };
    }
}

unsafe extern "system" fn resolved(status: u32, ctx: *const c_void, instance: *const DNS_SERVICE_INSTANCE) {
    if instance.is_null() {
        return;
    }
    if status == 0 {
        // SAFETY: a valid instance from dnsapi, freed below.
        let i = unsafe { &*instance };
        let name = unsafe { string(i.pszInstanceName) };
        let txt: Vec<String> = (0..i.dwPropertyCount as usize)
            .map(|n| unsafe { format!("{}={}", string(*i.keys.add(n)), string(*i.values.add(n))) })
            .collect();
        with_browse(ctx as usize, |g| {
            g.instances.insert(name, Some(txt));
        });
    }
    unsafe { DnsServiceFreeInstance(instance) };
}

pub(crate) async fn browse(wait: Duration) -> Result<Vec<Found>> {
    let key = NEXT.fetch_add(1, Ordering::Relaxed);
    BROWSES.lock().expect("browses").get_or_insert_with(HashMap::new).insert(key, Gathered::default());
    let result = browse_into(key, wait).await;
    let gathered = BROWSES.lock().expect("browses").as_mut().and_then(|m| m.remove(&key));
    result?;
    Ok(gathered
        .map(|g| g.instances.into_values().flatten().filter_map(|txt| Announcement::from_txt(&txt)).collect())
        .unwrap_or_default())
}

/// A browse or resolve under way: its handle, and the request with the strings it
/// points to, which dnsapi keeps using until the request is cancelled (freed
/// earlier, every browse crashed).
struct Pending {
    cancel: DNS_SERVICE_CANCEL,
    _name: Box<[u16]>,
    _browse: Option<Box<DNS_SERVICE_BROWSE_REQUEST>>,
    _resolve: Option<Box<DNS_SERVICE_RESOLVE_REQUEST>>,
}

// Only dnsapi reads the requests; this side moves them and drops them after the
// cancel.
unsafe impl Send for Pending {}

fn start_browse(key: usize) -> Result<Pending> {
    let name: Box<[u16]> = wide(&full_type()).into_boxed_slice();
    let request = Box::new(DNS_SERVICE_BROWSE_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1.0,
        InterfaceIndex: 0,
        QueryName: PCWSTR(name.as_ptr()),
        Anonymous: DNS_SERVICE_BROWSE_REQUEST_0 { pBrowseCallback: Some(browsed) },
        pQueryContext: key as *mut c_void,
    });
    let mut pending =
        Pending { cancel: DNS_SERVICE_CANCEL::default(), _name: name, _browse: None, _resolve: None };
    // SAFETY: the request and its name live in `pending` until after the cancel;
    // the context is a number.
    let status = unsafe { DnsServiceBrowse(&*request, &mut pending.cancel) };
    if status != DNS_REQUEST_PENDING {
        bail!("DnsServiceBrowse failed: {status}");
    }
    pending._browse = Some(request);
    Ok(pending)
}

fn start_resolve(key: usize, instance: &str) -> Option<Pending> {
    let mut name: Box<[u16]> = wide(instance).into_boxed_slice();
    let request = Box::new(DNS_SERVICE_RESOLVE_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1.0,
        InterfaceIndex: 0,
        QueryName: PWSTR(name.as_mut_ptr()),
        pResolveCompletionCallback: Some(resolved),
        pQueryContext: key as *mut c_void,
    });
    let mut pending =
        Pending { cancel: DNS_SERVICE_CANCEL::default(), _name: name, _browse: None, _resolve: None };
    // SAFETY: as for the browse.
    if unsafe { DnsServiceResolve(&*request, &mut pending.cancel) } != DNS_REQUEST_PENDING {
        return None;
    }
    pending._resolve = Some(request);
    Some(pending)
}

async fn browse_into(key: usize, wait: Duration) -> Result<()> {
    let browse = start_browse(key)?;
    tokio::time::sleep(wait).await;
    // SAFETY: the handle of the browse started above.
    unsafe { DnsServiceBrowseCancel(&browse.cancel) };
    // Callbacks still on their way find their browse gone or ignore the cancel.
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(browse);

    // Instances announced without their TXT record in the same answer.
    let missing: Vec<String> = BROWSES
        .lock()
        .expect("browses")
        .as_ref()
        .and_then(|m| m.get(&key))
        .map(|g| g.instances.iter().filter(|(_, t)| t.is_none()).map(|(n, _)| n.clone()).collect())
        .unwrap_or_default();
    let cancels: Vec<Pending> = missing.iter().filter_map(|name| start_resolve(key, name)).collect();
    if !cancels.is_empty() {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        for cancel in &cancels {
            // SAFETY: handles of the resolves started above; one already completed
            // makes the cancel a no-op error.
            unsafe { DnsServiceResolveCancel(&cancel.cancel) };
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}
