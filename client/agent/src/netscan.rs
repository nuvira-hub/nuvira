//! Network scan (NuviraHub Pro, RMM): on the server's request ([`ScanNetwork`])
//! the agent looks for the devices of its own local IPv4 networks (computers,
//! printers, NAS, cameras...) and sends back what it found ([`NetworkScan`]).
//!
//! Found through ARP, the way the network itself works: one UDP datagram to every
//! address makes the system ask the network who has it, and the neighbour table
//! then holds the MAC of every device that answered, also one with a firewall.
//! No connection attempts, so the security software of the other computers sees
//! nothing like an attack (a first version tried 14 TCP ports on every address,
//! and ESET on another PC blocked the scanning computer: 2026-11-28). The open
//! ports are tried only when asked ([`ScanNetwork::ports`]), on the devices
//! found. No raw sockets: the same for the service and for an agent running as
//! the signed-in user. Names from the network's DNS, otherwise NetBIOS. Only
//! private networks (RFC 1918) of real interfaces, each one limited to the /22
//! around this computer's address.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nuvira_proto::v1::{NetworkScan, ScanNetwork, ScannedHost};
use tokio::sync::Semaphore;
use tracing::{debug, info};

/// The capability announced in the inventory.
pub(crate) const CAPABILITY: &str = "network_scan";
/// Ports tried on every address: remote access, web panels, file sharing,
/// printers, cameras, NAS.
const PORTS: [u16; 14] = [22, 80, 139, 443, 445, 515, 554, 631, 3389, 5000, 5001, 5900, 8080, 9100];
/// At most this many networks are scanned.
const MAX_NETWORKS: usize = 4;
/// A larger network is scanned only in the /22 around this computer (1022 addresses).
const MIN_PREFIX: u8 = 22;
/// Connection attempts at the same time (open ports, when asked).
const CONCURRENCY: usize = 64;
/// UDP port of the datagrams that make the system resolve every address
/// (discard: nothing listens there, nothing is started on the other side).
const TOUCH_PORT: u16 = 9;
/// Rounds of datagrams, and the wait after the last one for the late answers.
const TOUCH_ROUNDS: usize = 2;
const TOUCH_GAP: Duration = Duration::from_millis(1200);
const ARP_WAIT: Duration = Duration::from_millis(2500);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(700);
/// The whole scan stops here, with what it found until then.
const SCAN_LIMIT: Duration = Duration::from_secs(150);
/// Reverse DNS lookups at the same time, and the time each may take.
const NAME_CONCURRENCY: usize = 16;
const NAME_TIMEOUT: Duration = Duration::from_secs(2);

/// Interfaces that are not a local network of this computer: virtual switches,
/// VPNs, containers (names lower-cased).
const VIRTUAL: [&str; 17] = [
    "vethernet",
    "virtualbox",
    "vmware",
    "vmnet",
    "docker",
    "veth",
    "virbr",
    "br-",
    "tailscale",
    "zerotier",
    "wireguard",
    "wg",
    "tun",
    "tap",
    "utun",
    "hyper-v",
    "loopback",
];

/// A network to scan: this computer's address on it and its prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Network {
    own: Ipv4Addr,
    prefix: u8,
    /// The prefix as configured (the scan may cover less).
    configured: u8,
}

impl Network {
    fn new(own: Ipv4Addr, configured: u8) -> Self {
        Network { own, prefix: configured.max(MIN_PREFIX).min(30), configured }
    }

    fn mask(&self) -> u32 {
        u32::MAX << (32 - self.prefix)
    }

    fn base(&self) -> u32 {
        u32::from(self.own) & self.mask()
    }

    /// Every address but the network's and the broadcast one.
    fn hosts(&self) -> impl Iterator<Item = Ipv4Addr> + use<> {
        let base = self.base();
        let last = base | !self.mask();
        (base + 1..last).map(Ipv4Addr::from)
    }

    fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == self.base()
    }

    fn cidr(&self) -> String {
        format!("{}/{}", Ipv4Addr::from(self.base()), self.prefix)
    }
}

/// The MAC macOS 27 gives instead of the real one, of this computer's interfaces
/// and of the neighbours, to a program without the restricted entitlement
/// `com.apple.developer.networking.topology-observation` ("a deliberate privacy
/// hardening", Apple DTS, developer.apple.com/forums/thread/841958). The same for
/// every device: never a way to tell them apart.
pub(crate) fn hidden_mac(mac: &str) -> bool {
    mac.eq_ignore_ascii_case("02:00:00:00:00:00")
}

fn is_virtual(name: &str) -> bool {
    let name = name.to_lowercase();
    VIRTUAL
        .iter()
        .any(|v| name.starts_with(v) || name.contains(&format!(" {v}")) || name.contains(&format!("({v}")))
}

/// The private IPv4 networks of this computer's real interfaces, with the MAC of
/// each own address.
fn networks() -> (Vec<Network>, HashMap<Ipv4Addr, String>) {
    let mut out: Vec<Network> = Vec::new();
    let mut own_macs = HashMap::new();
    for (name, data) in sysinfo::Networks::new_with_refreshed_list().iter() {
        if is_virtual(name) {
            debug!(name, "network scan: virtual interface skipped");
            continue;
        }
        for n in data.ip_networks() {
            let IpAddr::V4(ip) = n.addr else { continue };
            if !ip.is_private() || n.prefix == 0 || n.prefix > 30 {
                continue;
            }
            let mac = data.mac_address().to_string();
            own_macs.insert(ip, if hidden_mac(&mac) { String::new() } else { mac });
            let net = Network::new(ip, n.prefix);
            if !out.iter().any(|o| o.cidr() == net.cidr()) {
                out.push(net);
            }
        }
    }
    out.truncate(MAX_NETWORKS);
    (out, own_macs)
}

/// The ports of `ip` that accept a connection.
async fn open_ports(ip: Ipv4Addr, slots: Arc<Semaphore>) -> Vec<u32> {
    let mut tries = tokio::task::JoinSet::new();
    for port in PORTS {
        let slots = slots.clone();
        tries.spawn(async move {
            let _slot = slots.acquire_owned().await.ok()?;
            let addr = SocketAddr::new(IpAddr::V4(ip), port);
            matches!(
                tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr)).await,
                Ok(Ok(_))
            )
            .then_some(u32::from(port))
        });
    }
    let mut open = Vec::new();
    while let Some(r) = tries.join_next().await {
        open.extend(r.ok().flatten());
    }
    open.sort_unstable();
    open
}

/// One datagram to every address, so that the system resolves each with ARP.
async fn touch(addresses: &[Ipv4Addr]) {
    let Ok(socket) = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else { return };
    for round in 0..TOUCH_ROUNDS {
        for (i, ip) in addresses.iter().enumerate() {
            // Errors (no route, the neighbour table full for a moment) only lose
            // that address in this round.
            let _ = socket.send_to(&[0], (*ip, TOUCH_PORT)).await;
            // A little pacing: a /22 at once could overflow the neighbour table.
            if i % 64 == 63 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        if round + 1 < TOUCH_ROUNDS {
            tokio::time::sleep(TOUCH_GAP).await;
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Scans the local networks (some seconds; a couple of minutes with the ports).
pub(crate) async fn scan(request: ScanNetwork) -> NetworkScan {
    let started = now_ms();
    let clock = Instant::now();
    let (nets, own_macs) = tokio::task::spawn_blocking(networks).await.unwrap_or_default();
    let mut result = NetworkScan {
        request_id: request.request_id,
        started,
        networks: nets.iter().map(Network::cidr).collect(),
        ports: request.ports,
        ..Default::default()
    };
    if nets.is_empty() {
        result.error = "@rmm.scan_no_network".into();
        result.finished = now_ms();
        return result;
    }
    info!(networks = ?result.networks, ports = request.ports, "network scan started");

    let mut addresses: Vec<Ipv4Addr> =
        nets.iter().flat_map(Network::hosts).filter(|ip| !own_macs.contains_key(ip)).collect();
    addresses.sort_unstable();
    addresses.dedup();
    touch(&addresses).await;
    tokio::time::sleep(ARP_WAIT).await;
    let neighbours = tokio::task::spawn_blocking(neighbours::table).await.unwrap_or_default();
    let mut hosts: BTreeMap<Ipv4Addr, ScannedHost> = neighbours
        .into_iter()
        .filter(|(ip, _)| nets.iter().any(|n| n.contains(*ip)) && !own_macs.contains_key(ip))
        .map(|(ip, mac)| (ip, ScannedHost { ip: ip.to_string(), mac, ..Default::default() }))
        .collect();

    if request.ports {
        let slots = Arc::new(Semaphore::new(CONCURRENCY));
        let mut probes = tokio::task::JoinSet::new();
        for ip in hosts.keys().copied() {
            let slots = slots.clone();
            probes.spawn(async move { (ip, open_ports(ip, slots).await) });
        }
        let deadline = tokio::time::Instant::now() + SCAN_LIMIT;
        loop {
            match tokio::time::timeout_at(deadline, probes.join_next()).await {
                Ok(Some(Ok((ip, open)))) => {
                    if let Some(h) = hosts.get_mut(&ip) {
                        h.open_ports = open;
                    }
                }
                Ok(Some(Err(_))) => {}
                Ok(None) => break,
                Err(_) => {
                    info!("network scan: time limit reached");
                    probes.abort_all();
                    break;
                }
            }
        }
    }

    for (ip, mac) in &own_macs {
        if nets.iter().any(|n| n.contains(*ip)) {
            hosts.insert(
                *ip,
                ScannedHost {
                    ip: ip.to_string(),
                    mac: mac.clone(),
                    name: nuvira_transport::device_name(),
                    self_: true,
                    ..Default::default()
                },
            );
        }
    }

    // Names from the network's DNS (a router usually knows its DHCP clients).
    let names = Arc::new(Semaphore::new(NAME_CONCURRENCY));
    let mut lookups = tokio::task::JoinSet::new();
    for (ip, host) in &hosts {
        if host.self_ {
            continue;
        }
        let (ip, names) = (*ip, names.clone());
        lookups.spawn(async move {
            let _slot = names.acquire_owned().await.ok()?;
            let lookup = tokio::task::spawn_blocking(move || dns_lookup::lookup_addr(&IpAddr::V4(ip)).ok());
            let name = tokio::time::timeout(NAME_TIMEOUT, lookup).await.ok()?.ok()??;
            (name != ip.to_string()).then_some((ip, name))
        });
    }
    while let Some(r) = lookups.join_next().await {
        if let Ok(Some((ip, name))) = r
            && let Some(h) = hosts.get_mut(&ip)
        {
            h.name = name.trim_end_matches('.').to_string();
        }
    }
    // Windows computers and Samba servers the DNS does not know: their NetBIOS
    // name (one datagram each; the other devices do not answer).
    let names = Arc::new(Semaphore::new(NAME_CONCURRENCY * 2));
    let mut queries = tokio::task::JoinSet::new();
    for (ip, host) in &hosts {
        if host.name.is_empty() && !host.self_ {
            let ip = *ip;
            let names = names.clone();
            queries.spawn(async move {
                let _slot = names.acquire_owned().await.ok()?;
                netbios_name(ip).await.map(|n| (ip, n))
            });
        }
    }
    while let Some(r) = queries.join_next().await {
        if let Ok(Some((ip, name))) = r
            && let Some(h) = hosts.get_mut(&ip)
        {
            h.name = name;
        }
    }

    result.hosts = hosts.into_values().collect();
    result.finished = now_ms();
    info!(hosts = result.hosts.len(), seconds = clock.elapsed().as_secs(), "network scan finished");
    result
}

/// A NetBIOS node status request (RFC 1002 4.2.17) for the name "*".
fn nbstat_request(id: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(50);
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    p.push(32);
    let mut name = [0u8; 16];
    name[0] = b'*';
    for b in name {
        p.push(b'A' + (b >> 4));
        p.push(b'A' + (b & 0x0f));
    }
    p.push(0);
    p.extend_from_slice(&[0, 0x21, 0, 1]);
    p
}

/// The computer's name in a node status answer: the first unique name of type
/// 0x00 (workstation).
fn parse_nbstat(answer: &[u8]) -> Option<String> {
    // Header (12), name (34), type, class, TTL, data length (10), count (1).
    let count = *answer.get(56)? as usize;
    let entries = answer.get(57..)?;
    (0..count)
        .find_map(|i| {
            let e = entries.get(i * 18..i * 18 + 18)?;
            let group = e[16] & 0x80 != 0;
            (e[15] == 0 && !group).then(|| String::from_utf8_lossy(&e[..15]).trim_end().to_string())
        })
        .filter(|n| !n.is_empty() && n.chars().all(|c| !c.is_control()))
}

async fn netbios_name(ip: Ipv4Addr) -> Option<String> {
    let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    let id = (u32::from(ip) & 0xffff) as u16;
    socket.send_to(&nbstat_request(id), (ip, 137)).await.ok()?;
    let mut buf = [0u8; 1024];
    let (n, from) =
        tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf)).await.ok()?.ok()?;
    (from.ip() == IpAddr::V4(ip) && buf[..2] == id.to_be_bytes()).then_some(())?;
    parse_nbstat(&buf[..n])
}

/// The system's neighbour table: IPv4 address → MAC ("aa:bb:cc:dd:ee:ff") of
/// the entries that are complete.
mod neighbours {
    use std::collections::HashMap;
    use std::net::Ipv4Addr;

    /// "a:b:c:d:e:f" or "aa-bb-…" as "aa:bb:cc:dd:ee:ff"; `None` for an empty,
    /// broadcast or malformed one; "" for the placeholder macOS 27 gives instead of
    /// the real one ([`super::hidden_mac`]): the device is there, its MAC unknown.
    pub(super) fn normalize_mac(mac: &str) -> Option<String> {
        let parts: Vec<&str> = mac.split([':', '-']).collect();
        if parts.len() != 6 {
            return None;
        }
        let bytes: Vec<u8> =
            parts.iter().map(|p| u8::from_str_radix(p, 16)).collect::<Result<_, _>>().ok()?;
        if bytes.iter().all(|b| *b == 0) || bytes.iter().all(|b| *b == 0xff) || bytes[0] & 1 == 1 {
            return None;
        }
        let mac = bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
        Some(if super::hidden_mac(&mac) { String::new() } else { mac })
    }

    /// Linux: /proc/net/arp, flag 0x2 = complete.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) fn parse_proc_arp(text: &str) -> HashMap<Ipv4Addr, String> {
        text.lines()
            .skip(1)
            .filter_map(|line| {
                let f: Vec<&str> = line.split_whitespace().collect();
                let flags = u32::from_str_radix(f.get(2)?.trim_start_matches("0x"), 16).ok()?;
                (flags & 0x2 != 0).then_some(())?;
                Some((f.first()?.parse().ok()?, normalize_mac(f.get(3)?)?))
            })
            .collect()
    }

    /// macOS: `arp -an`, lines like "? (192.168.1.1) at a:b:c:d:e:f on en0 ifscope [ethernet]".
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) fn parse_arp_an(text: &str) -> HashMap<Ipv4Addr, String> {
        text.lines()
            .filter_map(|line| {
                let ip = line.split_once('(')?.1.split_once(')')?.0.parse().ok()?;
                let mac = line.split_once(" at ")?.1.split_whitespace().next()?;
                Some((ip, normalize_mac(mac)?))
            })
            .collect()
    }

    #[cfg(target_os = "linux")]
    pub(super) fn table() -> HashMap<Ipv4Addr, String> {
        std::fs::read_to_string("/proc/net/arp").map(|t| parse_proc_arp(&t)).unwrap_or_default()
    }

    /// macOS: the dump of the routing table's link-layer entries (what `arp -an`
    /// reads): messages of `header` bytes (`rt_msghdr`), each followed by the
    /// destination (`sockaddr_in`) and the gateway (`sockaddr_dl`, with the MAC
    /// after the interface name), every address padded to 4 bytes.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) fn parse_route_dump(dump: &[u8], header: usize) -> HashMap<Ipv4Addr, String> {
        const AF_INET: u8 = 2;
        const AF_LINK: u8 = 18;
        let padded = |len: usize| if len == 0 { 4 } else { (len + 3) & !3 };
        let mut out = HashMap::new();
        let mut at = 0;
        while at + 2 <= dump.len() {
            let len = u16::from_ne_bytes([dump[at], dump[at + 1]]) as usize;
            if len == 0 || at + len > dump.len() {
                break;
            }
            let msg = &dump[at..at + len];
            at += len;
            let Some(dst) = msg.get(header..) else { continue };
            let (Some(&dst_len), Some(&AF_INET)) = (dst.first(), dst.get(1)) else { continue };
            let Some(ip) = dst.get(4..8) else { continue };
            let ip = Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]);
            let Some(gw) = dst.get(padded(dst_len as usize)..) else { continue };
            // sockaddr_dl: len, family, index (2), type, nlen, alen, slen, data.
            if gw.get(1) != Some(&AF_LINK) || gw.len() < 8 {
                continue;
            }
            let (nlen, alen) = (gw[5] as usize, gw[6] as usize);
            let Some(mac) = gw.get(8 + nlen..8 + nlen + alen) else { continue };
            if alen != 6 {
                continue;
            }
            let text = mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
            if let Some(mac) = normalize_mac(&text) {
                out.insert(ip, mac);
            }
        }
        out
    }

    /// Read in this process (sysctl), not through `arp`: on macOS 27 the real MACs
    /// depend on the entitlements of the program asking (super::hidden_mac).
    #[cfg(target_os = "macos")]
    pub(super) fn table() -> HashMap<Ipv4Addr, String> {
        let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, libc::AF_INET, libc::NET_RT_FLAGS, libc::RTF_LLINFO];
        let mut dump: Vec<u8> = Vec::new();
        for _ in 0..3 {
            let mut size = 0usize;
            // SAFETY: a size query (no buffer).
            let status = unsafe {
                libc::sysctl(mib.as_mut_ptr(), 6, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0)
            };
            if status != 0 {
                break;
            }
            // Room for entries added between the two calls.
            size += size / 8 + 512;
            dump = vec![0u8; size];
            // SAFETY: a buffer of `size` bytes.
            let status = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    6,
                    dump.as_mut_ptr().cast(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if status == 0 {
                dump.truncate(size);
                return parse_route_dump(&dump, std::mem::size_of::<libc::rt_msghdr>());
            }
            dump.clear();
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
                break;
            }
        }
        std::process::Command::new("/usr/sbin/arp")
            .arg("-an")
            .output()
            .map(|o| parse_arp_an(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default()
    }

    #[cfg(windows)]
    pub(super) fn table() -> HashMap<Ipv4Addr, String> {
        use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIpNetTable2, MIB_IPNET_TABLE2};
        use windows::Win32::Networking::WinSock::{AF_INET, NlnsIncomplete, NlnsUnreachable};

        let mut out = HashMap::new();
        let mut table: *mut MIB_IPNET_TABLE2 = std::ptr::null_mut();
        // SAFETY: the table is allocated by the call and freed below.
        if unsafe { GetIpNetTable2(AF_INET, &mut table) }.is_err() || table.is_null() {
            return out;
        }
        // SAFETY: NumEntries rows follow the header (the array is declared with one).
        unsafe {
            let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
            for row in rows {
                if row.State == NlnsIncomplete
                    || row.State == NlnsUnreachable
                    || row.PhysicalAddressLength != 6
                {
                    continue;
                }
                let ip = Ipv4Addr::from(u32::from_be(row.Address.Ipv4.sin_addr.S_un.S_addr));
                let mac =
                    row.PhysicalAddress[..6].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
                if let Some(mac) = normalize_mac(&mac) {
                    out.insert(ip, mac);
                }
            }
            FreeMibTable(table as *const _);
        }
        out
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    pub(super) fn table() -> HashMap<Ipv4Addr, String> {
        HashMap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_covers_at_most_a_slash_22() {
        let n = Network::new(Ipv4Addr::new(192, 168, 178, 185), 24);
        assert_eq!(n.cidr(), "192.168.178.0/24");
        assert_eq!(n.hosts().count(), 254);
        assert!(n.contains(Ipv4Addr::new(192, 168, 178, 1)) && !n.contains(Ipv4Addr::new(192, 168, 179, 1)));
        let big = Network::new(Ipv4Addr::new(10, 1, 7, 9), 8);
        assert_eq!(big.cidr(), "10.1.4.0/22");
        assert_eq!(big.hosts().count(), 1022);
        assert_eq!(big.configured, 8);
        let small = Network::new(Ipv4Addr::new(192, 168, 1, 10), 30);
        assert_eq!(
            small.hosts().collect::<Vec<_>>(),
            vec![Ipv4Addr::new(192, 168, 1, 9), Ipv4Addr::new(192, 168, 1, 10)]
        );
    }

    #[test]
    fn virtual_interfaces_are_skipped() {
        for name in [
            "vEthernet (WSL)",
            "VirtualBox Host-Only Network",
            "docker0",
            "virbr0",
            "br-1a2b",
            "utun3",
            "wg0",
            "Tailscale",
        ] {
            assert!(is_virtual(name), "{name}");
        }
        for name in ["Ethernet", "Wi-Fi", "en0", "enp1s0", "wlp2s0", "eth0", "Ethernet 2"] {
            assert!(!is_virtual(name), "{name}");
        }
    }

    #[test]
    fn netbios_status() {
        let req = nbstat_request(0x1234);
        assert_eq!(req.len(), 50);
        assert_eq!(&req[12..15], b" CK");
        // An answer with two names: a group one, then the workstation's.
        let mut a = vec![0x12, 0x34, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        a.extend_from_slice(&req[12..46]);
        a.extend_from_slice(&[0, 0x21, 0, 1, 0, 0, 0, 0, 0, 0x41, 2]);
        let entry = |name: &str, suffix: u8, flags: u8| {
            let mut e = format!("{name:<15}").into_bytes();
            e.extend_from_slice(&[suffix, flags, 0]);
            e
        };
        a.extend(entry("WORKGROUP", 0, 0x84));
        a.extend(entry("PC-LABORATORIO", 0, 0x04));
        assert_eq!(parse_nbstat(&a).as_deref(), Some("PC-LABORATORIO"));
        assert_eq!(parse_nbstat(&a[..40]), None);
    }

    #[test]
    fn macs_are_normalized() {
        assert_eq!(neighbours::normalize_mac("a:b:c:d:e:f").as_deref(), Some("0a:0b:0c:0d:0e:0f"));
        assert_eq!(neighbours::normalize_mac("AA-BB-CC-DD-EE-01").as_deref(), Some("aa:bb:cc:dd:ee:01"));
        assert_eq!(neighbours::normalize_mac("00:00:00:00:00:00"), None);
        assert_eq!(neighbours::normalize_mac("ff:ff:ff:ff:ff:ff"), None);
        // Multicast.
        assert_eq!(neighbours::normalize_mac("01:00:5e:00:00:fb"), None);
        assert_eq!(neighbours::normalize_mac("(incomplete)"), None);
        // macOS 27 without the topology entitlement: there, MAC unknown.
        assert_eq!(neighbours::normalize_mac("2:0:0:0:0:0").as_deref(), Some(""));
    }

    #[test]
    fn linux_arp_table() {
        let text = "IP address       HW type     Flags       HW address            Mask     Device\n\
            192.168.178.1    0x1         0x2         3c:a6:2f:11:22:33     *        enp1s0\n\
            192.168.178.77   0x1         0x0         00:00:00:00:00:00     *        enp1s0\n\
            192.168.178.12   0x1         0x2         AC:1F:6B:44:55:66     *        enp1s0\n";
        let t = neighbours::parse_proc_arp(text);
        assert_eq!(t.len(), 2);
        assert_eq!(t[&Ipv4Addr::new(192, 168, 178, 12)], "ac:1f:6b:44:55:66");
    }

    #[test]
    fn macos_arp_table() {
        let text = "? (192.168.1.1) at 3c:a6:2f:1:2:3 on en0 ifscope [ethernet]\n\
            ? (192.168.1.40) at (incomplete) on en0 ifscope [ethernet]\n\
            ? (224.0.0.251) at 1:0:5e:0:0:fb on en0 ifscope permanent [ethernet]\n";
        let t = neighbours::parse_arp_an(text);
        assert_eq!(t.len(), 1);
        assert_eq!(t[&Ipv4Addr::new(192, 168, 1, 1)], "3c:a6:2f:01:02:03");
    }

    #[test]
    fn macos_route_dump() {
        // Two messages with a 92-byte header (rt_msghdr on macOS): a neighbour
        // with its MAC after the interface name "en0", and one without a MAC.
        let message = |ip: [u8; 4], mac: &[u8]| {
            let mut m = vec![0u8; 92];
            m.extend_from_slice(&[16, 2, 0, 0]);
            m.extend_from_slice(&ip);
            m.extend_from_slice(&[0; 8]);
            let mut dl = vec![0, 18, 4, 0, 6, 3, mac.len() as u8, 0];
            dl.extend_from_slice(b"en0");
            dl.extend_from_slice(mac);
            dl[0] = dl.len() as u8;
            dl.resize((dl.len() + 3) & !3, 0);
            m.extend_from_slice(&dl);
            let len = (m.len() as u16).to_ne_bytes();
            m[..2].copy_from_slice(&len);
            m
        };
        let mut dump = message([192, 168, 1, 1], &[0x3c, 0xa6, 0x2f, 1, 2, 3]);
        dump.extend(message([192, 168, 1, 40], &[]));
        dump.extend(message([192, 168, 1, 77], &[2, 0, 0, 0, 0, 0]));
        let t = neighbours::parse_route_dump(&dump, 92);
        assert_eq!(t.len(), 2);
        assert_eq!(t[&Ipv4Addr::new(192, 168, 1, 1)], "3c:a6:2f:01:02:03");
        assert_eq!(t[&Ipv4Addr::new(192, 168, 1, 77)], "");
        assert!(neighbours::parse_route_dump(&dump[..50], 92).is_empty());
    }

    /// Scans the real network of this computer: `cargo test -p nuvira-agent
    /// netscan::tests::real -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn real() {
        let ports = std::env::var("NUVIRA_SCAN_PORTS").is_ok();
        let r = scan(ScanNetwork { request_id: "test".into(), ports }).await;
        println!("{:?} error={:?} in {} ms", r.networks, r.error, r.finished - r.started);
        for h in &r.hosts {
            println!(
                "{:<16} {:<18} {:<30} {:?}{}",
                h.ip,
                h.mac,
                h.name,
                h.open_ports,
                if h.self_ { " (self)" } else { "" }
            );
        }
    }
}
