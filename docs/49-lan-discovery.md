# NuviraHub — Computers on this network

[← Index](README.md)

The app lists the NuviraHub computers of the same local network under the connect form ("On this network"): a click fills in the ID, or picks the address book entry with that ID and its saved password. The connection itself is the usual one: through the server, then the direct path inside the LAN when there is one.

- **What a computer announces.** While its agent is registered on a server, it announces itself on the local network with DNS-SD (mDNS): `<ID>._nuvira._udp.local` with the ID, the computer's name, the system (`windows`, `linux`, `macos`) and a short tag of its server (the first 4 bytes of the fingerprint of the server's certificate). Nothing else: no address book, no user, no password. It stops announcing when the agent stops or is no longer registered.
- **What the app shows.** Only the computers of its own server, the ones it can reach with the same ID; its own ID is left out. With an own server the app has never connected to yet (tag unknown), it shows all of them. The list is refreshed every 20 seconds while the Connection page is open.
- **Turning it off.** At the computer: Settings → "Visible on this network" (on by default; kept with the other permissions in `permissions.json`). For many computers: the NuviraHub Pro rule "Not visible on the local network" (`no_lan_discovery`), a rule of the computer, not of a session; with the rule the setting is shown locked.
- **The system's own service.** The announcement and the search go through the DNS-SD service of each system (Windows `DnsServiceRegister`/`DnsServiceBrowse`, Windows 10 1903 or newer; Avahi over D-Bus on Linux; mDNSResponder on macOS), not a socket of NuviraHub: no firewall rule and no administrator rights are needed. Without Avahi on Linux, the list stays empty and the computer is not announced. Crate `client/discovery`; `cargo run -p nuvira-discovery --example lan -- scan` lists what is announced.
- **macOS.** The app asks once for the Local Network permission ("NuviraHub lists the computers of your network that can be controlled…"); without it the list stays empty.
- **Known behaviour.** A computer whose agent was killed (not stopped) sends no goodbye: other Windows computers may still list it for about two minutes, until their cache expires. A computer withdrawn by the setting or the rule disappears at once.

Tried on 2026-11-28: Windows 11 and Windows 10 (also as the service's agent at the sign-in screen), Fedora KDE, Ubuntu, Linux Mint (also at the LightDM sign-in screen), Kubuntu Plasma Wayland (also at the SDDM sign-in screen) and macOS 27, in every direction; the setting and the Pro rule.
