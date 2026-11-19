package signal

import (
	"context"
	"net/netip"
	"strings"
	"sync"
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/limits"
	"github.com/nuvira-hub/nuvira/server/internal/relay"
	"github.com/nuvira-hub/nuvira/server/internal/store"
)

// Protection of a server open to anybody (like NuviraHub's public one): blocked
// device IDs and addresses, limits per address on registrations and connection
// requests. The blocks are in the database (`nuvira-server block`, also run
// from another process) and read again every reloadEvery.

const reloadEvery = 30 * time.Second

// Limits per IP address (per /64 for IPv6, which a single home usually holds).
// Addresses of this machine and of local networks have none, unless the server
// runs with NUVIRA_RATE_LIMITS=all (limits.Mode).
var (
	// Registrations: an app opens a few connections (agent, viewer, address book),
	// reconnects after network changes; an office behind one address reconnects
	// all its PCs together after a restart of the server.
	registerLimit = rate{perMinute: 30, burst: 100}
	// Connection requests: guessing IDs, or bothering a PC with requests.
	connectLimit = rate{perMinute: 10, burst: 15}
	// Registrations of a key never seen (each creates a device row, kept): the
	// burst lets an office install all its PCs at once. Per IPv6 /48, since a
	// site holding many /64s must not multiply it.
	newDeviceLimit = rate{perMinute: 10, burst: 100, v6bits: 48}
	// Device IDs asked by GetPresence (no sign-in needed): the apps' address
	// books ask for their entries every few seconds, at most 256 per request;
	// enumerating the 900 million IDs must take years, not hours.
	presenceLimit = rate{perMinute: 2048, burst: 4096}
)

// Connections open at once from one address, registered or not. Each app
// keeps a few (agent, address book, viewer while in a session): an office of
// about 80 PCs behind one address stays below it.
const maxConnsPerAddress = 256

// rate is a token bucket: tokens per minute, burst, and the IPv6 prefix of
// its key (64 when 0).
type rate struct {
	perMinute, burst float64
	v6bits           int
}

// limiter is a token bucket per address.
type limiter struct {
	rate    rate
	mode    limits.Mode
	mu      sync.Mutex
	buckets map[netip.Prefix]*bucket
}

type bucket struct {
	tokens float64
	last   time.Time
}

// newLimiter limits the public addresses (limits.Public).
func newLimiter(r rate) *limiter { return newLimiterMode(r, limits.Public) }

// newLimiterMode limits the addresses the mode applies to; nil for limits.Off.
func newLimiterMode(r rate, mode limits.Mode) *limiter {
	if mode == limits.Off {
		return nil
	}
	return &limiter{rate: r, mode: mode, buckets: map[netip.Prefix]*bucket{}}
}

// allow takes a request of ip; a nil limiter (limits off) allows everything.
func (l *limiter) allow(ip netip.Addr, now time.Time) bool { return l.allowN(ip, now, 1) }

// allowN takes n tokens for ip at once, or none.
func (l *limiter) allowN(ip netip.Addr, now time.Time, n int) bool {
	if l == nil || !l.mode.Applies(ip) {
		return true
	}
	bits := l.rate.v6bits
	if bits == 0 {
		bits = 64
	}
	key := limits.KeyBits(ip, bits)
	l.mu.Lock()
	defer l.mu.Unlock()
	b := l.buckets[key]
	if b == nil {
		b = &bucket{tokens: l.rate.burst, last: now}
		l.buckets[key] = b
	}
	b.tokens = min(l.rate.burst, b.tokens+now.Sub(b.last).Minutes()*l.rate.perMinute)
	b.last = now
	if b.tokens < float64(n) {
		return false
	}
	b.tokens -= float64(n)
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

// Devices registered and never seen again (a key used once, as in a flood of
// Register from fresh keys) are removed after pruneAge: the row of a device
// whose whole life was within pruneLife of its registration, with no alias and
// not mentioned by a module's table (store.PruneUnused). Such a device coming
// back gets a new ID.
const (
	pruneAge   = 30 * 24 * time.Hour
	pruneLife  = 10 * time.Minute
	pruneEvery = 6 * time.Hour
	pruneBatch = 500
)

// Pause between two batches of pruneDevices (a variable: shorter in tests).
var pruneYield = 100 * time.Millisecond

// pruneLoop runs pruneDevices a minute after the start, then every pruneEvery.
func (s *Server) pruneLoop(ctx context.Context) {
	t := time.NewTimer(time.Minute)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case now := <-t.C:
			s.pruneDevices(ctx, now)
			t.Reset(pruneEvery)
		}
	}
}

// pruneDevices removes the devices never seen again, in batches (the database
// has one connection: others get their turn between batches).
func (s *Server) pruneDevices(ctx context.Context, now time.Time) int {
	total, after := 0, ""
	for ctx.Err() == nil {
		n, next, err := s.store.PruneUnused(now.Add(-pruneAge), pruneLife, s.Online, after, pruneBatch)
		if err != nil {
			s.log.Warn("devices never seen again not removed", "err", err)
			break
		}
		total += n
		if next == "" {
			break
		}
		after = next
		time.Sleep(pruneYield)
	}
	if total > 0 {
		s.log.Info("devices registered and never seen again removed", "count", total, "older than", pruneAge)
	}
	return total
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
