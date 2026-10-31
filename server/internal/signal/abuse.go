package signal

import (
	"net/netip"
	"strings"
	"sync"
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/relay"
	"github.com/nuvira-hub/nuvira/server/internal/store"
)

// Protection of a server open to anybody (like NuviraHub's public one): blocked
// device IDs and addresses, limits per address on registrations and connection
// requests. The blocks are in the database (`nuvira-server block`, also run
// from another process) and read again every reloadEvery.

const reloadEvery = 30 * time.Second

// Limits per IP address (per /64 for IPv6, which a single home usually holds).
// Addresses of this machine and of local networks have none.
var (
	// Registrations: an app opens a few connections (agent, viewer, address book),
	// reconnects after network changes; an office behind one address reconnects
	// all its PCs together after a restart of the server.
	registerLimit = rate{perMinute: 30, burst: 100}
	// Connection requests: guessing IDs, or bothering a PC with requests.
	connectLimit = rate{perMinute: 10, burst: 15}
)

type rate struct{ perMinute, burst float64 }

// limiter is a token bucket per key.
type limiter struct {
	rate    rate
	mu      sync.Mutex
	buckets map[netip.Prefix]*bucket
}

type bucket struct {
	tokens float64
	last   time.Time
}

func newLimiter(r rate) *limiter { return &limiter{rate: r, buckets: map[netip.Prefix]*bucket{}} }

// allow takes a request of ip; a nil limiter (limits off) allows everything.
func (l *limiter) allow(ip netip.Addr, now time.Time) bool {
	if l == nil || ip.IsLoopback() || ip.IsPrivate() {
		return true
	}
	key := addressKey(ip)
	l.mu.Lock()
	defer l.mu.Unlock()
	b := l.buckets[key]
	if b == nil {
		b = &bucket{tokens: l.rate.burst, last: now}
		l.buckets[key] = b
	}
	b.tokens = min(l.rate.burst, b.tokens+now.Sub(b.last).Minutes()*l.rate.perMinute)
	b.last = now
	if b.tokens < 1 {
		return false
	}
	b.tokens--
	return true
}

// prune forgets the buckets already full again.
func (l *limiter) prune(now time.Time) {
	if l == nil {
		return
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	for k, b := range l.buckets {
		if b.tokens+now.Sub(b.last).Minutes()*l.rate.perMinute >= l.rate.burst {
			delete(l.buckets, k)
		}
	}
}

// addressKey is the key of an address in the limits: the IPv4 address, or the
// IPv6 /64 network.
func addressKey(ip netip.Addr) netip.Prefix {
	if ip.Is4() {
		return netip.PrefixFrom(ip, 32)
	}
	p, _ := ip.Prefix(64)
	return p
}

// blocklist is the in-memory copy of the blocks.
type blocklist struct {
	mu       sync.RWMutex
	ids      map[string]bool
	networks []netip.Prefix
}

func (b *blocklist) set(blocks []store.Block) {
	ids := map[string]bool{}
	var networks []netip.Prefix
	for _, bl := range blocks {
		if kind, value := ParseBlock(bl.Value); kind == BlockDevice {
			ids[value] = true
		} else if kind == BlockNetwork {
			networks = append(networks, netip.MustParsePrefix(value))
		}
	}
	b.mu.Lock()
	b.ids, b.networks = ids, networks
	b.mu.Unlock()
}

func (b *blocklist) device(id string) bool {
	b.mu.RLock()
	defer b.mu.RUnlock()
	return b.ids[id]
}

func (b *blocklist) address(ip netip.Addr) bool {
	b.mu.RLock()
	defer b.mu.RUnlock()
	for _, n := range b.networks {
		if n.Contains(ip) {
			return true
		}
	}
	return false
}

// Kinds of block.
const (
	BlockInvalid = iota
	BlockDevice
	BlockNetwork
)

// ParseBlock reads a block: a 9-digit device ID (spaces allowed), an IP address
// or a network (CIDR). It returns the kind and the normalised value.
func ParseBlock(s string) (int, string) {
	s = strings.TrimSpace(s)
	if id := strings.ReplaceAll(s, " ", ""); len(id) == 9 && strings.Trim(id, "0123456789") == "" {
		return BlockDevice, id
	}
	if ip, err := netip.ParseAddr(s); err == nil {
		ip = ip.Unmap()
		return BlockNetwork, netip.PrefixFrom(ip, ip.BitLen()).String()
	}
	if p, err := netip.ParsePrefix(s); err == nil {
		return BlockNetwork, p.Masked().String()
	}
	return BlockInvalid, ""
}

// reloadBlocks reads the blocks again and disconnects the devices now blocked.
func (s *Server) reloadBlocks() {
	blocks, err := s.store.Blocks()
	if err != nil {
		s.log.Warn("blocks not read", "err", err)
		return
	}
	s.blocked.set(blocks)
	var drop []*client
	s.mu.Lock()
	for _, c := range s.online {
		if s.isBlocked(c) {
			drop = append(drop, c)
		}
	}
	s.mu.Unlock()
	for _, c := range drop {
		s.log.Info("blocked device disconnected", "id", c.device.ID, "from", c.conn.RemoteAddr())
		_ = c.send(errorMsg("@server.blocked", ""))
		_ = c.conn.CloseWithError(0, "blocked")
	}
}

func (s *Server) isBlocked(c *client) bool {
	if ip, ok := relay.HostOf(c.conn.RemoteAddr()); ok && s.blocked.address(ip) {
		return true
	}
	return c.device != nil && s.blocked.device(c.device.ID)
}
