# NexoDesk — Features — complete list

[← Index](README.md)

Priority legend: **M** = MVP, **1** = version 1.0, **2** = later versions.

### 4.1 Connection and access
| Feature | Priority |
|---|---|
| Numeric ID + one-time password generated at every start | M |
| Confirmation by the remote user, showing who is connecting | M |
| Unattended access with a permanent password / device key | M |
| P2P connection with hole punching + fallback relay | M |
| Direct LAN connection without a server (mDNS discovery) | 1 |
| Technician account with 2FA (TOTP), passkey/FIDO2 | 1 |
| OIDC SSO (Google, Microsoft Entra, Authentik, Keycloak) | 2 |
| Several technicians on the same session | 1 |
| Wake-on-LAN through another device on the same LAN | 1 |
| Automatic reconnection after the remote PC restarts | M |

### 4.2 Remote control
| Feature | Priority |
|---|---|
| Viewing and mouse/keyboard control | M |
| Multiple monitors (single, all, separate tabs) | M |
| Keyboard mapping and special combinations (Ctrl+Alt+Del, Win, Cmd) | M |
| Secure desktop / UAC / sign-in screen (Windows), macOS login | 1 |
| Scaling, fit to window, full screen, remote resolution | M |
| Adjustable quality/codec/fps, live statistics | M |
| View only | M |
| Privacy mode (remote screen blanked, local input blocked) | 1 |
| Remote restart with reconnection, restart in safe mode | 1 |
| Remote audio | 1 |
| Technician's microphone towards the remote PC (voice call) | 2 |

### 4.3 Data
| Feature | Priority |
|---|---|
| Synchronised clipboard (text, images) | M |
| Copy/paste of files through the clipboard | 1 |
| Two-pane file manager, drag and drop, resumable transfers | M |
| Remote printing (a document of the remote PC on the technician's printer) | 2 |
| Port forwarding / TCP tunnel (e.g. RDP, SSH, a router's web interface) | 1 |

### 4.4 Tools without video
| Feature | Priority |
|---|---|
| Remote terminal (PowerShell/cmd, bash/zsh) | 1 |
| Process and service management | 2 |
| System information and hardware/software inventory | 1 |
| Event log / journal viewer | 2 |

### 4.5 Management (server console)
| Feature | Priority |
|---|---|
| Personal and shared address book, groups, tags, search | M |
| Users, roles, permissions per device group | 1 |
| Central policies (e.g. "confirmation always required", "no file transfer") | 1 |
| Session reports (who, what, when, duration) exportable as CSV for billing | 1 |
| Session recording (client side or enforced by policy) | 1 |
| Generator of customised installers (server, key, logo, default group) | 1 |
| Mass deployment (MSI/GPO, pkg, deb/rpm) | 1 |
| Online/offline state and last access of the devices | M |
| Webhooks and API for ticketing/automation (Node-RED, Home Assistant) | 2 |

### 4.6 Communication
| Feature | Priority |
|---|---|
| Chat during the session | M |
| Credits "NexoDesk © 2026 Alessandro Blason · AGPL-3.0 · Source code" in the client and the console (licence requirement) | M |
| Interface in English, Italian, French, German and Spanish (D15) | M |
| Whiteboard/annotations on the remote screen | 2 |
