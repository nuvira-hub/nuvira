//! Light RMM (NuviraHub Pro, feature "pro.inventory"): what this computer is,
//! sent to the server at registration and every [`INTERVAL`], and the
//! Wake-on-LAN packets the server asks to send for another computer of the
//! same network.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use nuvira_proto::v1::{Disk, Inventory, NetworkInterface};
use sysinfo::{Disks, Networks, System};
use tracing::{debug, info};

/// How often the inventory is sent again.
pub const INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Collects the inventory (a few tens of milliseconds: call it off the async threads).
pub fn collect() -> Inventory {
    let mut system = System::new();
    system.refresh_memory();
    system.refresh_cpu_all();
    let cpu = system.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default();
    let disks = Disks::new_with_refreshed_list()
        .iter()
        .filter(|d| !d.is_removable() && d.total_space() > 0)
        .map(|d| Disk {
            mount: d.mount_point().to_string_lossy().into_owned(),
            total: d.total_space(),
            available: d.available_space(),
        })
        .collect();
    let interfaces = Networks::new_with_refreshed_list()
        .iter()
        .filter_map(|(name, data)| {
            let addresses: Vec<String> = data
                .ip_networks()
                .iter()
                .filter(|n| !n.addr.is_loopback())
                .map(|n| format!("{}/{}", n.addr, n.prefix))
                .collect();
            let mac = data.mac_address().to_string();
            (!addresses.is_empty() || mac != "00:00:00:00:00:00").then(|| NetworkInterface {
                name: name.clone(),
                mac,
                addresses,
            })
        })
        .collect();
    Inventory {
        os: System::name().unwrap_or_default(),
        os_version: System::long_os_version().or_else(System::os_version).unwrap_or_default(),
        hostname: nuvira_transport::device_name(),
        user: desktop_user(),
        cpu,
        cores: System::physical_core_count().unwrap_or(0) as u32,
        memory_total: system.total_memory(),
        memory_available: system.available_memory(),
        disks,
        uptime_s: System::uptime(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        interfaces,
    }
}

/// The user of the desktop, when the agent runs as that user (not as the
/// Windows service's SYSTEM account).
fn desktop_user() -> String {
    let user = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default();
    if user.eq_ignore_ascii_case("SYSTEM") || user.ends_with('$') { String::new() } else { user }
}

/// Sends the Wake-on-LAN "magic packet" of each MAC address: to the broadcast of
/// every local IPv4 network and to the limited broadcast, on ports 9 and 7.
pub fn wake(macs: &[String]) {
    let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else { return };
    if socket.set_broadcast(true).is_err() {
        return;
    }
    let mut targets = vec![Ipv4Addr::BROADCAST];
    for data in Networks::new_with_refreshed_list().values() {
        for net in data.ip_networks() {
            if let std::net::IpAddr::V4(addr) = net.addr
                && !addr.is_loopback()
                && net.prefix < 32
            {
                let mask = u32::MAX << (32 - u32::from(net.prefix));
                targets.push(Ipv4Addr::from(u32::from(addr) | !mask));
            }
        }
    }
    targets.dedup();
    for mac in macs {
        let Some(packet) = magic_packet(mac) else { continue };
        for target in &targets {
            for port in [9, 7] {
                let _ = socket.send_to(&packet, SocketAddr::from((*target, port)));
            }
        }
        info!("wake-on-LAN sent to {mac}");
    }
    debug!(?targets, "wake-on-LAN broadcasts");
}

/// Six 0xFF bytes and the MAC address sixteen times.
fn magic_packet(mac: &str) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = mac.split([':', '-']).map(|b| u8::from_str_radix(b, 16)).collect::<Result<_, _>>().ok()?;
    if bytes.len() != 6 {
        return None;
    }
    let mut packet = vec![0xFF; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&bytes);
    }
    Some(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_layout() {
        let p = magic_packet("aa:bb:cc:dd:ee:ff").unwrap();
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert_eq!(&p[6..12], &[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(&p[96..], &[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert!(magic_packet("aa-bb-cc-dd-ee-ff").is_some());
        assert!(magic_packet("nope").is_none());
    }

    #[test]
    fn collects_something() {
        let inv = collect();
        assert!(inv.memory_total > 0);
        assert!(!inv.app_version.is_empty());
    }
}
