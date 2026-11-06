# NuviraHub — Inventory, health and Wake-on-LAN

[← Index](README.md)

With a NuviraHub Pro server that offers it (feature `pro.inventory`), the computers of the address books report what they are, the server derives a health state from it, and a computer that is off can be woken through another one of the same network. The apps are the same as always: the agent sends the inventory and the "Info" button appears only when the server announces the feature.

- **Inventory.** The agent sends it when it registers and every 15 minutes (`ClientMessage.inventory`): system and version, computer name, user of the desktop, processor and cores, memory, fixed disks, uptime, NuviraHub version, and the network cards with MAC and addresses. The Pro server keeps the latest one per device, with the time and the public address it came from.
- **Health.** Computed by the server from the latest inventory: *critical* when a disk has less than 10% free; *attention* when a disk has less than 20% free, the free memory is low or the computer has been on for more than 30 days; otherwise *good*. Without an inventory it is *no data*.
- **Wake-on-LAN.** The server needs a MAC address of the computer (learnt while it was on) and another computer that is online in the same network: first one with an address in the same subnet, then one with the same public address. It sends that computer the MAC addresses (`ServerMessage.wake`), and its agent broadcasts the magic packet on UDP ports 9 and 7 to every local IPv4 network. The computer must have Wake-on-LAN enabled in the BIOS/UEFI and in the network card.
- **Where.** The web console shows a health column in the devices list and, on a click, the inventory and the "Wake up" button. In the app, "Info" next to a computer of the address book shows the same. An account sees the inventory of the computers in its address books; administrators see all of them.
- **Server.** The base server passes the inventories to a module implementing `ext.InventoryDesk` and otherwise ignores them; agents send them only when the server asks (`pro.inventory`).

Tested on 2026-11-06 with the Pro test server on a Linux VM: inventories of Fedora, Linux Mint and Windows 10 (PC-LABORATORIO), health, and a wake request for Fedora sent by Mint (the magic packet left Mint; turning a real PC on is still to try).
