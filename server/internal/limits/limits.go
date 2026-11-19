// Package limits holds what the server's limits per address share: which
// addresses they apply to (NUVIRA_RATE_LIMITS), the key of an address, and a
// cap on the connections open at once from one address.
package limits

import (
	"net/netip"
	"sync"
)

// Mode says to which client addresses the limits per address apply.
type Mode int

const (
	// Public (the default): every address except this machine's and the local
	// networks' (loopback, RFC 1918, IPv6 ULA), so a company server on its LAN
	// has none. Behind a userland UDP proxy (rootless Docker/Podman, a UDP load
	// balancer) every client looks local: use All there.
	Public Mode = iota
	// Off: no limits at all (NUVIRA_RATE_LIMITS=off).
	Off
	// All: local addresses too (NUVIRA_RATE_LIMITS=all).
	All
)

// ParseMode reads NUVIRA_RATE_LIMITS: "" (Public), "off" or "all". ok is false
// for any other value, which is read as Public, as before "all" existed.
func ParseMode(s string) (m Mode, ok bool) {
	switch s {
	case "":
		return Public, true
	case "off":
		return Off, true
	case "all":
		return All, true
	}
	return Public, false
}

func (m Mode) String() string {
	switch m {
	case Off:
		return "off"
	case All:
		return "all"
	}
	return "public"
}

// Applies tells whether the limits apply to a client address.
func (m Mode) Applies(ip netip.Addr) bool {
	switch m {
	case Off:
		return false
	case All:
		return true
	}
	return !ip.IsLoopback() && !ip.IsPrivate()
}

// Key is the key of an address in the limits: the IPv4 address, or the IPv6
// /64 network (a single home usually holds a whole /64).
func Key(ip netip.Addr) netip.Prefix { return KeyBits(ip, 64) }

// KeyBits is Key with another IPv6 prefix length (e.g. 48 for a budget that a
// site holding many /64s must not multiply).
func KeyBits(ip netip.Addr, v6bits int) netip.Prefix {
	ip = ip.Unmap()
	if ip.Is4() {
		return netip.PrefixFrom(ip, 32)
	}
	p, _ := ip.Prefix(v6bits)
	return p
}

// Counter caps the connections open at once from one address (Key).
type Counter struct {
	mode Mode
	max  int
	mu   sync.Mutex
	open map[netip.Prefix]int
}

// NewCounter allows at most max connections per address the mode applies to.
func NewCounter(mode Mode, max int) *Counter {
	return &Counter{mode: mode, max: max, open: map[netip.Prefix]int{}}
}

// Add takes a place for a connection from ip. If ok, release must be called
// (once) when the connection ends. A nil Counter allows everything.
func (c *Counter) Add(ip netip.Addr) (release func(), ok bool) {
	if c == nil || !c.mode.Applies(ip) {
		return func() {}, true
	}
	key := Key(ip)
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.open[key] >= c.max {
		return nil, false
	}
	c.open[key]++
	var once sync.Once
	return func() {
		once.Do(func() {
			c.mu.Lock()
			defer c.mu.Unlock()
			if c.open[key]--; c.open[key] <= 0 {
				delete(c.open, key)
			}
		})
	}, true
}

// Open is the number of connections counted for the address of ip.
func (c *Counter) Open(ip netip.Addr) int {
	if c == nil {
		return 0
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.open[Key(ip)]
}
