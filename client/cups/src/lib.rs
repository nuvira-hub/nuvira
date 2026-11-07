//! The "NuviraHub" printer on Linux and macOS (remote printing,
//! docs/29-remote-printing.md): a CUPS queue whose PPD makes CUPS deliver every job
//! as PDF to our backend (`nuvira-cups`, installed as `<backend dir>/nuvira`, device
//! URI `nuvira:/`). The backend hands the PDF to the agent of the user who printed,
//! which listens only during a session whose viewer prints.
//!
//! How the backend reaches the agent depends on what the system lets a CUPS backend
//! do:
//! - Linux: TCP on 127.0.0.1:[`PORT`]. SELinux (Fedora) lets `cupsd_t` connect to
//!   any TCP port but not to a Unix socket of a user's program. Both sides check the
//!   other in `/proc/net/tcp` ([`tcp_owner`]): the backend talks only to a listener of
//!   the user who printed, the agent accepts documents only from root (the backend).
//! - macOS: the Unix socket [`socket_path`] of that user, in `/private/tmp`, the only
//!   place CUPS's sandbox lets a backend connect to besides `/private/var/run`; both
//!   sides check the other with `getpeereid`.
//!
//! On the connection the backend writes [`HEADER`], a line, the title, a line, then
//! the PDF to the end, and closes its side; the agent answers with one line, [`SENT`]
//! or the reason why the document was not sent.

/// Name of the queue, as the person at the computer sees it.
pub const PRINTER: &str = "NuviraHub";
/// Scheme of the device URI, and name of the backend in CUPS's backend folder.
pub const SCHEME: &str = "nuvira";
/// First line written by the backend.
pub const HEADER: &str = "NUVIRA-PRINT 1";
/// Longest title line accepted (bytes).
pub const MAX_TITLE: usize = 1024;
/// Answer of the agent when the document goes to the technician.
pub const SENT: &str = "sent";
/// The PPD of the queue: everything printed reaches the backend as PDF
/// (`application/vnd.cups-pdf`, made by `pdftopdf` on Linux and `cgpdftopdf` on macOS).
pub const PPD: &str = include_str!("nuvira.ppd");

/// CUPS's folder of backends.
#[cfg(target_os = "macos")]
pub const BACKEND_DIR: &str = "/usr/libexec/cups/backend";
#[cfg(not(target_os = "macos"))]
pub const BACKEND_DIR: &str = "/usr/lib/cups/backend";

/// Where the backend is installed.
pub fn backend_path() -> std::path::PathBuf {
    std::path::Path::new(BACKEND_DIR).join(SCHEME)
}

/// Port on 127.0.0.1 the agent listens on during a session that prints (Linux).
#[cfg(target_os = "linux")]
pub const PORT: u16 = 21190;

/// Socket the agent of user `uid` listens on during a session that prints (macOS).
#[cfg(target_os = "macos")]
pub fn socket_path(uid: u32) -> std::path::PathBuf {
    format!("/private/tmp/nuvira-print-{uid}").into()
}

/// Owner (uid) of the IPv4 TCP socket bound to `local`: with `remote`, the one
/// connected to it, without, the one listening. From `/proc/net/tcp`.
#[cfg(target_os = "linux")]
pub fn tcp_owner(local: std::net::SocketAddrV4, remote: Option<std::net::SocketAddrV4>) -> Option<u32> {
    let table = std::fs::read_to_string("/proc/net/tcp").ok()?;
    owner_in_table(&table, local, remote)
}

/// Lines of `/proc/net/tcp`:
/// `sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode ...`, with
/// addresses as `0100007F:52E6` (the address as a native-endian number, the port).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn owner_in_table(
    table: &str,
    local: std::net::SocketAddrV4,
    remote: Option<std::net::SocketAddrV4>,
) -> Option<u32> {
    // TCP_LISTEN
    const LISTEN: &str = "0A";
    let parse = |text: &str| -> Option<std::net::SocketAddrV4> {
        let (ip, port) = text.split_once(':')?;
        let ip = u32::from_str_radix(ip, 16).ok()?;
        let port = u16::from_str_radix(port, 16).ok()?;
        Some(std::net::SocketAddrV4::new(ip.to_ne_bytes().into(), port))
    };
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (l, r, state, uid) = (fields.get(1)?, fields.get(2)?, fields.get(3)?, fields.get(7)?);
        if parse(l)? != local {
            return None;
        }
        let matches = match remote {
            Some(remote) => parse(r)? == remote,
            None => *state == LISTEN,
        };
        matches.then(|| uid.parse().ok()).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[cfg(target_endian = "little")]
    const TABLE: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 3500007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000   991        0 3034 1 0000000000000000 100 0 0 10 5
   1: 0100007F:52E6 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 9123 1 0000000000000000 100 0 0 10 0
   2: 0100007F:52E6 0100007F:B5C4 01 00000000:00000000 00:00000000 00000000  1000        0 9200 1 0000000000000000 20 4 30 10 -1
   3: 0100007F:B5C4 0100007F:52E6 01 00000000:00000000 00:00000000 00000000     0        0 9199 1 0000000000000000 20 4 30 10 -1
";

    #[cfg(target_endian = "little")]
    #[test]
    fn owners_from_the_table() {
        let agent = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 21222);
        let backend = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 46532);
        assert_eq!(owner_in_table(TABLE, agent, None), Some(1000));
        assert_eq!(owner_in_table(TABLE, backend, Some(agent)), Some(0));
        assert_eq!(
            owner_in_table(TABLE, SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 53), 53), None),
            Some(991)
        );
        assert_eq!(owner_in_table(TABLE, SocketAddrV4::new(Ipv4Addr::LOCALHOST, 21190), None), None);
        assert_eq!(owner_in_table("", agent, None), None);
    }

    #[test]
    fn ppd_sends_pdf_to_the_backend() {
        assert!(PPD.starts_with("*PPD-Adobe: \"4.3\""));
        assert!(PPD.contains("*cupsFilter2: \"application/vnd.cups-pdf application/pdf 0 -\""));
    }
}
