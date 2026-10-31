// Package relay forwards UDP datagrams between the two sides of a session,
// like a minimal TURN server: the content is QUIC, already end-to-end
// encrypted, and the relay cannot read it.
//
// Format of the control messages (the first byte 0x00 is never the start of a
// QUIC packet, which always has the 0x40 bit set):
//
//	bind:  00 'N' 'X' 'R' 01 | token (16 bytes) | role (1 byte)
//	       | n (1 byte) | n local addresses of the sender
//	bound: 00 'N' 'X' 'R' 02 | token (16 bytes) | ready (1 byte: 1 if the other side is bound too)
//	       [if ready: address of the other side as the relay sees it
//	        | n (1 byte) | n local addresses of the other side]
//
// Address: family (4 or 6) | IP (4 or 16 bytes) | port (2 bytes, big endian);
// family 0 (a single byte) if the other side is connected through WebSocket.
//
// The other side's addresses are used to attempt a direct connection: the one
// seen by the relay through NATs (hole punching) and the local ones, which work
// when the two devices are in the same network behind the same router (where
// the public address is often unreachable: hairpin).
package relay

import (
	"bytes"
	"log/slog"
	"net"
	"sync"
	"time"
)

var magic = []byte{0x00, 'N', 'X', 'R'}

const (
	typeBind  = 0x01
	typeBound = 0x02
	tokenLen  = 16
	// Local addresses accepted per side (they limit the size of the answers).
	maxLocals = 4

	// Session forgotten after this time without traffic.
	idleTimeout = 2 * time.Minute
	// Maximum time to bind both sides after creation.
	bindTimeout = 60 * time.Second
)

type session struct {
	token   [tokenLen]byte
	peers   [2]net.Addr // [0] viewer, [1] agent (UDP or WebSocket)
	locals  [2][]*net.UDPAddr
	created time.Time
	last    time.Time
	bytes   uint64
}

func (s *session) ready() bool { return s.peers[0] != nil && s.peers[1] != nil }

// Relay keeps the sessions and the address → other side's address table.
type Relay struct {
	// Tests only: fakes the address seen by the relay (see reflexive).
	FakeReflexive bool

	mu       sync.Mutex
	sessions map[[tokenLen]byte]*session
	links    map[string]*link
	log      *slog.Logger
	// Traffic of the current hour, logged by expireLoop (bandwidth of a public server).
	hourBytes    uint64
	hourSessions int
	hourStart    time.Time
}

type link struct {
	to      net.Addr
	session *session
}

func New(log *slog.Logger) *Relay {
	r := &Relay{sessions: map[[tokenLen]byte]*session{}, links: map[string]*link{}, log: log, hourStart: time.Now()}
	go r.expireLoop()
	return r
}

// Allow prepares a session: only whoever knows the token can bind to it.
func (r *Relay) Allow(token [tokenLen]byte) {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now()
	r.sessions[token] = &session{token: token, created: now, last: now}
}

// IsControl tells whether the datagram is a relay control message.
func IsControl(p []byte) bool { return len(p) >= 5 && bytes.Equal(p[:4], magic) }

// HandleControl handles a bind and returns the answer to send (or nil).
func (r *Relay) HandleControl(p []byte, from net.Addr) []byte {
	if len(p) < 5+tokenLen+1 || p[4] != typeBind {
		return nil
	}
	var token [tokenLen]byte
	copy(token[:], p[5:5+tokenLen])
	role := int(p[5+tokenLen])
	if role != 1 && role != 2 {
		return nil
	}
	locals, ok := parseAddrs(p[5+tokenLen+1:])
	if !ok {
		return nil
	}

	r.mu.Lock()
	defer r.mu.Unlock()
	s := r.sessions[token]
	if s == nil {
		return nil
	}
	i := role - 1
	if old := s.peers[i]; old == nil || old.String() != from.String() {
		// First bind or address change (NAT): the table is updated.
		if old != nil {
			delete(r.links, old.String())
		}
		s.peers[i] = from
		if s.ready() {
			r.links[s.peers[0].String()] = &link{to: s.peers[1], session: s}
			r.links[s.peers[1].String()] = &link{to: s.peers[0], session: s}
			r.log.Info("relay active", "viewer", s.peers[0], "agent", s.peers[1])
		}
	}
	s.locals[i] = locals
	s.last = time.Now()

	reply := make([]byte, 0, 5+tokenLen+1)
	reply = append(reply, magic...)
	reply = append(reply, typeBound)
	reply = append(reply, token[:]...)
	if !s.ready() {
		return append(reply, 0)
	}
	reply = append(reply, 1)
	reply = appendAddr(reply, r.reflexive(s.peers[1-i]))
	reply = append(reply, byte(len(s.locals[1-i])))
	for _, a := range s.locals[1-i] {
		reply = appendAddr(reply, a)
	}
	return reply
}

// reflexive is the other side's address as the relay sees it. With
// FakeReflexive (tests only) it is replaced by an unreachable one: this way the
// hairpin case, where only the local address works, is simulated in a LAN.
func (r *Relay) reflexive(a net.Addr) net.Addr {
	if _, udp := a.(*net.UDPAddr); r.FakeReflexive && udp {
		return &net.UDPAddr{IP: net.IPv4(203, 0, 113, 1), Port: a.(*net.UDPAddr).Port} // TEST-NET-3, never routed
	}
	return a
}

// parseAddrs reads "n | n addresses"; a missing list counts as empty.
func parseAddrs(b []byte) ([]*net.UDPAddr, bool) {
	if len(b) == 0 {
		return nil, true
	}
	n := int(b[0])
	if n > maxLocals {
		return nil, false
	}
	b = b[1:]
	addrs := make([]*net.UDPAddr, 0, n)
	for range n {
		var ipLen int
		switch {
		case len(b) > 0 && b[0] == 4:
			ipLen = 4
		case len(b) > 0 && b[0] == 6:
			ipLen = 16
		default:
			return nil, false
		}
		if len(b) < 1+ipLen+2 {
			return nil, false
		}
		ip := net.IP(append([]byte(nil), b[1:1+ipLen]...))
		port := int(b[1+ipLen])<<8 | int(b[2+ipLen])
		addrs = append(addrs, &net.UDPAddr{IP: ip, Port: port})
		b = b[3+ipLen:]
	}
	return addrs, len(b) == 0
}

func appendAddr(b []byte, addr net.Addr) []byte {
	a, ok := addr.(*net.UDPAddr)
	if !ok {
		// Side connected through WebSocket: it has no reachable UDP address.
		return append(b, 0)
	}
	if ip4 := a.IP.To4(); ip4 != nil {
		b = append(b, 4)
		b = append(b, ip4...)
	} else {
		b = append(b, 6)
		b = append(b, a.IP.To16()...)
	}
	return append(b, byte(a.Port>>8), byte(a.Port))
}

// Destination returns where to forward a datagram received from `from`,
// or nil if `from` belongs to no active session.
func (r *Relay) Destination(from net.Addr, n int) net.Addr {
	r.mu.Lock()
	defer r.mu.Unlock()
	l := r.links[from.String()]
	if l == nil {
		return nil
	}
	l.session.last = time.Now()
	l.session.bytes += uint64(n)
	r.hourBytes += uint64(n)
	return l.to
}

func (r *Relay) expireLoop() {
	for range time.Tick(15 * time.Second) {
		r.mu.Lock()
		now := time.Now()
		for token, s := range r.sessions {
			idle := now.Sub(s.last) > idleTimeout
			unbound := !s.ready() && now.Sub(s.created) > bindTimeout
			if !idle && !unbound {
				continue
			}
			for _, p := range s.peers {
				if p != nil {
					delete(r.links, p.String())
				}
			}
			delete(r.sessions, token)
			if s.ready() {
				r.hourSessions++
			}
			r.log.Info("relay session closed", "bytes", s.bytes, "duration", now.Sub(s.created).Round(time.Second))
		}
		if now.Sub(r.hourStart) >= time.Hour {
			r.log.Info("relay traffic in the last hour", "bytes", r.hourBytes,
				"mbit_avg", float64(r.hourBytes)*8/now.Sub(r.hourStart).Seconds()/1e6,
				"sessions_closed", r.hourSessions, "sessions_open", len(r.sessions))
			r.hourBytes, r.hourSessions, r.hourStart = 0, 0, now
		}
		r.mu.Unlock()
	}
}
