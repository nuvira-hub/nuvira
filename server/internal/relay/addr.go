package relay

import (
	"net"
	"net/netip"
	"strings"
)

// HostOf is the IP of a device's address: UDP, or WebSocket (the client's
// address as seen by the server or its reverse proxy).
func HostOf(addr net.Addr) (netip.Addr, bool) {
	s := addr.String()
	if a, ok := addr.(wsAddr); ok {
		_, s, _ = strings.Cut(string(a), "@")
		ip, err := netip.ParseAddr(s)
		return ip.Unmap(), err == nil
	}
	ap, err := netip.ParseAddrPort(s)
	return ap.Addr().Unmap(), err == nil
}
