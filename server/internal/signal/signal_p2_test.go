package signal

// Phase 2 (characterisation): signalling over real QUIC: registration, abuse
// limits as they are, presence, and many devices/sessions at once.

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"fmt"
	"net/netip"
	"sync"
	"testing"
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/limits"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
	"github.com/nuvira-hub/nuvira/server/internal/store"
)

func TestRegisterOverQUIC(t *testing.T) {
	ts := startServer(t)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	a := ts.dial(t)
	if err := a.register(key, "pc-a", false); err != nil {
		t.Fatal(err)
	}
	if len(a.id) != 9 {
		t.Fatalf("ID %q", a.id)
	}
	ts.waitOnline(t, a.id, true)
	// Same key again: same ID, the old connection is closed (replaced).
	b := ts.dial(t)
	if err := b.register(key, "pc-a", false); err != nil {
		t.Fatal(err)
	}
	if b.id != a.id {
		t.Fatalf("same key, new ID %s != %s", b.id, a.id)
	}
	select {
	case <-a.conn.Context().Done():
	case <-time.After(5 * time.Second):
		t.Fatal("old connection not replaced")
	}
	// apiOnly with the same key: not listed online, does not replace b.
	c := ts.dial(t)
	if err := c.register(key, "viewer", true); err != nil {
		t.Fatal(err)
	}
	time.Sleep(200 * time.Millisecond)
	if b.conn.Context().Err() != nil || !ts.srv.Online(b.id) {
		t.Fatal("an apiOnly connection replaced the device")
	}
}

// refusal reads what a refused client gets: the error text, or "" when the
// connection closed first.
func refusal(t *testing.T, c *testClient) string {
	t.Helper()
	m, err := c.read(5 * time.Second)
	if err != nil {
		select {
		case <-c.conn.Context().Done():
			return ""
		case <-time.After(5 * time.Second):
			t.Fatalf("neither an error nor a close: %v", err)
		}
	}
	if m.GetError() == nil {
		t.Fatalf("not an error: %v", m)
	}
	if _, err := c.read(5 * time.Second); err == nil {
		t.Fatal("connection still open after a refusal")
	}
	return m.GetError().GetMessage()
}

// The refusals during registration (bad proof, old protocol, blocked, rate
// limited) reach the client before the close (K19, SRV-1: they used to be lost
// to handle's immediate CloseWithError, 0-4 of 30 delivered).
func TestRegistrationRefusalDelivered(t *testing.T) {
	ts := startServer(t)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	pub := key.Public().(ed25519.PublicKey)
	const tries = 10
	for i := range tries {
		// Wrong signature: no device row either.
		c := ts.dial(t)
		_ = c.send(registerMsg(pub, "x", false))
		if m, err := c.read(5 * time.Second); err != nil || m.GetChallenge() == nil {
			t.Fatalf("challenge: %v %v", m, err)
		}
		_ = c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Proof{Proof: &pb.Proof{Signature: make([]byte, 64)}}})
		if text := refusal(t, c); text != "invalid proof of the key" {
			t.Fatalf("bad proof, try %d: %q", i+1, text)
		}
		// Old protocol: refused before any challenge.
		c = ts.dial(t)
		old := registerMsg(pub, "x", false)
		old.GetRegister().ProtocolVersion = 0
		_ = c.send(old)
		if text := refusal(t, c); text != "@server.old_version" {
			t.Fatalf("old protocol, try %d: %q", i+1, text)
		}
	}
	var rows int
	if err := ts.store.DB().QueryRow(`SELECT COUNT(*) FROM devices`).Scan(&rows); err != nil || rows != 0 {
		t.Fatalf("device rows %d %v", rows, err)
	}
}

// A blocked address and a blocked device get "@server.blocked" before the close
// (SRV-1). The rate-limit refusal goes through the same refuse(), but loopback
// is never rate limited, so it is not reachable from this test.
func TestRegistrationBlockedDelivered(t *testing.T) {
	ts := startServer(t)
	dev := ts.device(t, "to-block")
	_ = dev.conn.CloseWithError(0, "")
	ts.waitOnline(t, dev.id, false)
	ts.srv.blocked.set([]store.Block{{Value: dev.id}})
	for i := range 5 {
		c := ts.dial(t)
		_ = c.send(registerMsg(dev.key.Public().(ed25519.PublicKey), "to-block", false))
		m, err := c.read(5 * time.Second)
		if err != nil || m.GetChallenge() == nil {
			t.Fatalf("challenge: %v %v", m, err)
		}
		sig := ed25519.Sign(dev.key, append([]byte(registerContext), m.GetChallenge().Nonce...))
		_ = c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Proof{Proof: &pb.Proof{Signature: sig}}})
		if text := refusal(t, c); text != "@server.blocked" {
			t.Fatalf("blocked device, try %d: %q", i+1, text)
		}
	}
	ts.srv.blocked.set([]store.Block{{Value: "127.0.0.0/8"}})
	for i := range 5 {
		c := ts.dial(t)
		_ = c.send(registerMsg(dev.key.Public().(ed25519.PublicKey), "to-block", false))
		if text := refusal(t, c); text != "@server.blocked" {
			t.Fatalf("blocked address, try %d: %q", i+1, text)
		}
	}
}

// Not a Register first, or a bad key: closed without any answer.
func TestRegisterWrongFirstMessage(t *testing.T) {
	ts := startServer(t)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	pub := key.Public().(ed25519.PublicKey)
	for _, first := range []*pb.ClientMessage{
		{Msg: &pb.ClientMessage_Connect{Connect: &pb.Connect{DeviceId: "123456789"}}},
		registerMsg(pub[:31], "x", false),
		{},
	} {
		c := ts.dial(t)
		_ = c.send(first)
		if m, err := c.read(5 * time.Second); err == nil {
			t.Fatalf("answer to %v: %v", first, m)
		}
	}
}

// Fixed (SRV-8, was K21 "unregistered QUIC connections can stay open"): the
// server set no deadline on the first message, so a connection that opened the
// stream and sent nothing (or never opened one) stayed open as long as the
// client kept it alive. Now the stream and each registration message must come
// within registerTimeout (10 s; 300 ms here), and a registered device may then
// stay silent.
func TestCharacterisesUnregisteredConnectionStaysOpen(t *testing.T) {
	const deadline = 300 * time.Millisecond
	ts := startServerWith(t, func(s *Server) { s.registerTimeout = deadline })
	closedWithin := func(c *testClient, what string) {
		t.Helper()
		select {
		case <-c.conn.Context().Done():
		case <-time.After(deadline + 3*time.Second):
			t.Fatalf("%s: still open after the registration deadline", what)
		}
	}
	// The stream exists for the server only once something is written: one byte
	// of a length prefix, never completed.
	c := ts.dial(t)
	if _, err := c.stream.Write([]byte{0x05}); err != nil {
		t.Fatal(err)
	}
	closedWithin(c, "half a Register")
	// Without opening a stream at all.
	closedWithin(ts.dial(t), "no stream")
	// Register sent, Proof never.
	c3 := ts.dial(t)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	_ = c3.send(registerMsg(key.Public().(ed25519.PublicKey), "slow", false))
	if m, err := c3.read(2 * time.Second); err != nil || m.GetChallenge() == nil {
		t.Fatalf("challenge: %v %v", m, err)
	}
	closedWithin(c3, "no Proof")
	// A registered device stays connected while silent, well past the deadline.
	dev := ts.device(t, "quiet")
	time.Sleep(4 * deadline)
	if dev.conn.Context().Err() != nil {
		t.Fatalf("registered device closed: %v", context.Cause(dev.conn.Context()))
	}
	if !ts.srv.Online(dev.id) {
		t.Fatal("registered device not online")
	}
}

// Characterisation (lead store.go:177): every fresh key registers a new device
// row; from loopback/private addresses there is no limit at all (abuse.go:49).
func TestCharacterisesRegisterFloodFromLoopback(t *testing.T) {
	ts := startServer(t)
	const n = 120 // above registerLimit.burst (100) that public addresses get
	ids := map[string]bool{}
	for i := range n {
		c := ts.dial(t)
		if err := c.register(nil, fmt.Sprintf("flood-%d", i), false); err != nil {
			t.Fatalf("registration %d: %v", i+1, err)
		}
		ids[c.id] = true
		_ = c.conn.CloseWithError(0, "")
	}
	if len(ids) != n {
		t.Fatalf("%d distinct IDs for %d keys", len(ids), n)
	}
	var rows int
	if err := ts.store.DB().QueryRow(`SELECT COUNT(*) FROM devices`).Scan(&rows); err != nil {
		t.Fatal(err)
	}
	if rows != n {
		t.Fatalf("%d device rows", rows)
	}
}

// Characterisation of the limits per address as configured (abuse.go): which
// addresses are exempt, and the numbers of registerLimit / connectLimit.
func TestCharacterisesAbuseLimits(t *testing.T) {
	now := time.Now()
	reg := newLimiter(registerLimit)
	pub := netip.MustParseAddr("203.0.113.10")
	for i := range 100 {
		if !reg.allow(pub, now) {
			t.Fatalf("registration %d refused within the burst", i+1)
		}
	}
	if reg.allow(pub, now) {
		t.Fatal("registration 101 at once accepted")
	}
	if !reg.allow(pub, now.Add(2*time.Second)) || reg.allow(pub, now.Add(2*time.Second)) {
		t.Fatal("30 per minute: one every 2 s")
	}
	con := newLimiter(connectLimit)
	for i := range 15 {
		if !con.allow(pub, now) {
			t.Fatalf("connect %d refused", i+1)
		}
	}
	if con.allow(pub, now) || !con.allow(pub, now.Add(6*time.Second)) {
		t.Fatal("connect: burst 15, then one every 6 s")
	}
	// Exempt from every limit (KNOWN lead abuse.go:49: behind Docker NAT or a UDP
	// proxy every client looks private and loses its limits).
	exempt := []string{"127.0.0.1", "127.9.9.9", "::1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.0.1",
		"fc00::1", "fd12:3456::1", "::ffff:10.0.0.1", "::ffff:127.0.0.1"}
	limited := []string{"169.254.1.1", "fe80::1", "100.64.0.1", "172.32.0.1", "192.0.2.1", "2001:db8::1", "0.0.0.0", "::"}
	for _, s := range exempt {
		l := newLimiter(rate{perMinute: 1, burst: 1})
		ip := netip.MustParseAddr(s)
		if !l.allow(ip, now) || !l.allow(ip, now) || !l.allow(ip, now) {
			t.Errorf("%s limited, expected exempt", s)
		}
	}
	for _, s := range limited {
		l := newLimiter(rate{perMinute: 1, burst: 1})
		ip := netip.MustParseAddr(s)
		if !l.allow(ip, now) || l.allow(ip, now) {
			t.Errorf("%s exempt, expected limited", s)
		}
	}
	// SRV-9: NUVIRA_RATE_LIMITS=all limits the local addresses too (for servers
	// behind a userland UDP proxy, where every client looks local); off: none.
	for _, s := range append(exempt, limited...) {
		l := newLimiterMode(rate{perMinute: 1, burst: 1}, limits.All)
		ip := netip.MustParseAddr(s)
		if !l.allow(ip, now) || l.allow(ip, now) {
			t.Errorf("all: %s not limited", s)
		}
	}
	if newLimiterMode(registerLimit, limits.Off) != nil {
		t.Error("off: a limiter")
	}
}

// With NUVIRA_RATE_LIMITS=all a loopback client is limited, so the refusals of
// the limits can be seen end to end: the Error reaches the client before the
// close (refuse), and a refused connection request leaves the viewer connected.
func TestRateLimitRefusalsOverQUIC(t *testing.T) {
	ts := startServerWith(t, func(s *Server) {
		s.SetLimitMode(limits.All)
		s.registerLimit = newLimiterMode(rate{perMinute: 1, burst: 3}, limits.All)
	})
	viewer := ts.device(t, "viewer")
	agent := ts.device(t, "agent")
	ts.waitOnline(t, agent.id, true)
	ts.device(t, "third")
	c := ts.dial(t)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	_ = c.send(registerMsg(key.Public().(ed25519.PublicKey), "fourth", false))
	if text := refusal(t, c); text != "@server.rate_limited" {
		t.Fatalf("registration over the burst: %q", text)
	}
	// connectLimit: burst 15 per address, then refused; the viewer stays.
	errorOf := func() string {
		t.Helper()
		m, err := viewer.readUntil(5*time.Second, func(m *pb.ServerMessage) bool { return m.GetError() != nil })
		if err != nil {
			t.Fatal(err)
		}
		return m.GetError().Message
	}
	refused := 0
	for i := range 15 {
		_ = viewer.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Connect{Connect: &pb.Connect{DeviceId: agent.id}}})
		in, err := agent.readUntil(5*time.Second, func(m *pb.ServerMessage) bool { return m.GetIncoming() != nil })
		if err != nil {
			t.Fatalf("connect %d: %v", i+1, err)
		}
		_ = agent.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Reject{Reject: &pb.Reject{SessionId: in.GetIncoming().SessionId}}})
		if text := errorOf(); text != "@agent.rejected" {
			t.Fatalf("connect %d: %q", i+1, text)
		}
	}
	_ = viewer.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Connect{Connect: &pb.Connect{DeviceId: agent.id}}})
	if text := errorOf(); text == "@server.rate_limited" {
		refused++
	}
	if refused != 1 || viewer.conn.Context().Err() != nil {
		t.Fatalf("refused %d, viewer closed %v", refused, viewer.conn.Context().Err())
	}
}

// Characterisation (lead api.go:45): GetPresence needs no sign-in; any device
// learns online/last seen of any ID, 256 IDs per request, one request per 2 s
// per connection (not per device: a second connection asks again at once).
// From a local address, as here, there is no budget of IDs per address (the
// default NUVIRA_RATE_LIMITS); TestPresenceBudgetPerAddress covers it (SRV-7).
func TestCharacterisesPresenceEnumeration(t *testing.T) {
	ts := startServer(t)
	target := ts.device(t, "target")
	ts.waitOnline(t, target.id, true)
	ids := make([]string, maxPresenceIDs)
	for i := range ids {
		ids[i] = fmt.Sprintf("%09d", 100000000+i)
	}
	ids[17] = target.id
	ask := func(c *testClient, reqID uint32) *pb.ApiResponse {
		t.Helper()
		_ = c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Api{Api: &pb.ApiRequest{Id: reqID,
			Call: &pb.ApiRequest_GetPresence{GetPresence: &pb.GetPresence{DeviceIds: ids}}}}})
		m, err := c.readUntil(5*time.Second, func(m *pb.ServerMessage) bool { return m.GetApi().GetId() == reqID })
		if err != nil {
			t.Fatal(err)
		}
		return m.GetApi()
	}
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	scanner := ts.dial(t)
	if err := scanner.register(key, "scanner", true); err != nil {
		t.Fatal(err)
	}
	resp := ask(scanner, 1)
	if resp.Error != "" || len(resp.GetPresence().GetDevices()) != maxPresenceIDs {
		t.Fatalf("presence: %v", resp)
	}
	for i, d := range resp.GetPresence().GetDevices() {
		if (i == 17) != d.Online || (i == 17) != (d.LastSeen != 0) {
			t.Fatalf("entry %d: %v", i, d)
		}
	}
	if r := ask(scanner, 2); r.Error != "too many requests" {
		t.Fatalf("second request at once: %v", r)
	}
	other := ts.dial(t)
	if err := other.register(key, "scanner", true); err != nil {
		t.Fatal(err)
	}
	if r := ask(other, 3); r.Error != "" {
		t.Fatalf("second connection of the same device: %v", r)
	}
	ids = append(ids, "x")
	if r := ask(ts.device(t, "big"), 4); r.Error != "too many devices" {
		t.Fatalf("257 IDs: %v", r)
	}
}

// Many devices register at once; half connect to the other half, the agents
// accept, both sides get the same relay token; then both sides bind the relay.
// Run with -race (Linux): the server's maps, the store and the relay are shared.
func TestConcurrentSessionsOverQUIC(t *testing.T) {
	const pairs = 25
	ts := startServer(t)
	type pair struct{ viewer, agent *testClient }
	ps := make([]pair, pairs)
	var wg sync.WaitGroup
	errs := make(chan error, pairs*4)
	for i := range ps {
		wg.Add(1)
		go func() {
			defer wg.Done()
			ps[i] = pair{ts.dial(t), ts.dial(t)}
			if err := ps[i].agent.register(nil, fmt.Sprintf("agent-%d", i), false); err != nil {
				errs <- err
				return
			}
			if err := ps[i].viewer.register(nil, fmt.Sprintf("viewer-%d", i), true); err != nil {
				errs <- err
			}
		}()
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		t.Fatal(err)
	}
	for _, p := range ps {
		ts.waitOnline(t, p.agent.id, true)
	}
	tokens := make([][2][]byte, pairs)
	errs = make(chan error, pairs*4)
	for i, p := range ps {
		wg.Add(2)
		go func() { // agent: accepts the Incoming of its viewer, gets its Route
			defer wg.Done()
			m, err := p.agent.readUntil(10*time.Second, func(m *pb.ServerMessage) bool { return m.GetIncoming() != nil })
			if err != nil {
				errs <- fmt.Errorf("agent %d: %w", i, err)
				return
			}
			if m.GetIncoming().ViewerId != p.viewer.id {
				errs <- fmt.Errorf("agent %d: incoming from %s", i, m.GetIncoming().ViewerId)
				return
			}
			_ = p.agent.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Accept{Accept: &pb.Accept{SessionId: m.GetIncoming().SessionId}}})
			r, err := p.agent.readUntil(10*time.Second, func(m *pb.ServerMessage) bool { return m.GetRoute() != nil })
			if err != nil {
				errs <- fmt.Errorf("agent %d route: %w", i, err)
				return
			}
			if r.GetRoute().Role != pb.Role_ROLE_AGENT || r.GetRoute().PeerId != p.viewer.id {
				errs <- fmt.Errorf("agent %d: route %v", i, r.GetRoute())
			}
			tokens[i][1] = r.GetRoute().RelayToken
		}()
		go func() { // viewer: connects, presence noise, gets its Route
			defer wg.Done()
			_ = p.viewer.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Connect{Connect: &pb.Connect{DeviceId: p.agent.id}}})
			_ = p.viewer.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Api{Api: &pb.ApiRequest{Id: 7,
				Call: &pb.ApiRequest_GetPresence{GetPresence: &pb.GetPresence{DeviceIds: []string{p.agent.id}}}}}})
			r, err := p.viewer.readUntil(10*time.Second, func(m *pb.ServerMessage) bool { return m.GetRoute() != nil })
			if err != nil {
				errs <- fmt.Errorf("viewer %d: %w", i, err)
				return
			}
			if r.GetRoute().Role != pb.Role_ROLE_VIEWER || r.GetRoute().PeerId != p.agent.id {
				errs <- fmt.Errorf("viewer %d: route %v", i, r.GetRoute())
			}
			tokens[i][0] = r.GetRoute().RelayToken
		}()
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		t.Error(err)
	}
	if t.Failed() {
		return
	}
	seen := map[string]bool{}
	for i, tk := range tokens {
		if len(tk[0]) != 16 || !bytes.Equal(tk[0], tk[1]) {
			t.Fatalf("pair %d: tokens %x %x", i, tk[0], tk[1])
		}
		if seen[string(tk[0])] {
			t.Fatalf("pair %d: token reused", i)
		}
		seen[string(tk[0])] = true
	}
	ts.srv.mu.Lock()
	pending, routed := len(ts.srv.sessions), len(ts.srv.routed)
	ts.srv.mu.Unlock()
	if pending != 0 || routed != pairs {
		t.Fatalf("pending %d, routed %d", pending, routed)
	}
	// Everybody leaves at once: nobody stays online.
	for _, p := range ps {
		wg.Add(1)
		go func() {
			defer wg.Done()
			_ = p.agent.conn.CloseWithError(0, "")
			_ = p.viewer.conn.CloseWithError(0, "")
		}()
	}
	wg.Wait()
	for _, p := range ps {
		ts.waitOnline(t, p.agent.id, false)
	}
}

// The same device reconnecting many times at once: exactly one connection
// stays online, the others are closed ("replaced by a new connection"). Every
// connection gets its Registered first, even one replaced while its
// registration was in flight (SRV-5: the replacement's close used to drop it;
// SRV-3: the store race failed some first registrations).
func TestConcurrentReconnectsOfOneDevice(t *testing.T) {
	ts := startServer(t)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	const n = 10
	cs := make([]*testClient, n)
	var wg sync.WaitGroup
	var mu sync.Mutex
	lost := 0
	for i := range cs {
		wg.Add(1)
		go func() {
			defer wg.Done()
			c := ts.dial(t)
			cs[i] = c
			if err := c.register(key, "same", false); err != nil {
				select {
				case <-c.conn.Context().Done(): // replaced
					mu.Lock()
					lost++
					mu.Unlock()
				case <-time.After(5 * time.Second):
					t.Error(err)
				}
			}
		}()
	}
	wg.Wait()
	if t.Failed() {
		return
	}
	id := ""
	for _, c := range cs {
		if c.id != "" {
			id = c.id
		}
	}
	if lost != 0 || id == "" {
		t.Fatalf("registrations whose answer was lost: %d/%d", lost, n)
	}
	ts.waitOnline(t, id, true)
	// Longer than the replaced connections' linger (refuseLinger).
	time.Sleep(refuseLinger + 1500*time.Millisecond)
	open := 0
	for _, c := range cs {
		if c.conn.Context().Err() == nil {
			open++
		}
	}
	if open != 1 {
		t.Fatalf("%d connections of the same device still open", open)
	}
}

// SRV-8: at most maxConnsPerAddress connections open at once per address
// (3 here), registered or not; a place is given back when one closes.
func TestConnectionsPerAddressCapped(t *testing.T) {
	ts := startServerWith(t, func(s *Server) {
		s.SetLimitMode(limits.All)
		s.connections = limits.NewCounter(limits.All, 3)
	})
	a := ts.device(t, "a")
	ts.device(t, "b")
	idle := ts.dial(t) // not registered yet: counted too
	_, _ = idle.stream.Write([]byte{0x05})
	time.Sleep(200 * time.Millisecond)
	c := ts.dial(t)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	_ = c.send(registerMsg(key.Public().(ed25519.PublicKey), "d", false))
	if text := refusal(t, c); text != "@server.rate_limited" {
		t.Fatalf("4th connection: %q", text)
	}
	_ = a.conn.CloseWithError(0, "")
	ts.waitOnline(t, a.id, false)
	for range 50 {
		if ts.srv.connections.Open(netip.MustParseAddr("127.0.0.1")) < 3 {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	ts.device(t, "e")
	// The default mode does not count loopback at all.
	def := startServer(t)
	for i := range 5 {
		def.device(t, fmt.Sprintf("lan-%d", i))
	}
}

// SRV-8: registrations that create a device row have a budget of their own
// (newDeviceLimit; burst 2 here); a known key registers again freely.
func TestNewDevicesPerAddressBudget(t *testing.T) {
	ts := startServerWith(t, func(s *Server) {
		s.SetLimitMode(limits.All)
		s.newDeviceLimit = newLimiterMode(rate{perMinute: 1, burst: 2, v6bits: 48}, limits.All)
	})
	first := ts.device(t, "one")
	ts.device(t, "two")
	if err := ts.dial(t).register(nil, "three", false); err == nil || err.Error() != "server error: @server.rate_limited" {
		t.Fatalf("third new key: %v", err)
	}
	again := ts.dial(t)
	if err := again.register(first.key, "one again", false); err != nil || again.id != first.id {
		t.Fatalf("known key: %v %s", err, again.id)
	}
	var rows int
	if err := ts.store.DB().QueryRow(`SELECT COUNT(*) FROM devices`).Scan(&rows); err != nil || rows != 2 {
		t.Fatalf("%d rows (%v)", rows, err)
	}
	// The key of the IPv6 budget is the /48.
	l := newLimiterMode(rate{perMinute: 1, burst: 1, v6bits: 48}, limits.Public)
	now := time.Now()
	if !l.allow(netip.MustParseAddr("2001:db8:1:1::1"), now) || l.allow(netip.MustParseAddr("2001:db8:1:ffff::1"), now) {
		t.Fatal("/48 not shared")
	}
}

// SRV-8 pruning: a device registered long ago whose only connection was short
// is removed, unless it is connected now (its last access is written only at
// the disconnection).
func TestPruneDevicesKeepsOnlineDevices(t *testing.T) {
	ts := startServer(t)
	old := time.Now().Add(-pruneAge - time.Hour).Unix()
	on := ts.device(t, "connected for a month")
	ts.waitOnline(t, on.id, true)
	off := ts.device(t, "used once")
	_ = off.conn.CloseWithError(0, "")
	ts.waitOnline(t, off.id, false)
	time.Sleep(200 * time.Millisecond) // setOffline's Touch
	for _, id := range []string{on.id, off.id} {
		if _, err := ts.store.DB().Exec(`UPDATE devices SET created_at = ?, last_seen = ? WHERE id = ?`, old, old, id); err != nil {
			t.Fatal(err)
		}
	}
	if n := ts.srv.pruneDevices(context.Background(), time.Now()); n != 1 {
		t.Fatalf("removed %d", n)
	}
	if d, _ := ts.store.ByID(off.id); d != nil {
		t.Fatal("device used once kept")
	}
	if d, _ := ts.store.ByID(on.id); d == nil {
		t.Fatal("connected device removed")
	}
}

// SRV-7: the IDs asked by GetPresence have a budget per address (presenceLimit:
// 2048 a minute, burst 4096; burst 512 here), shared by every connection of the
// address, so new connections do not multiply the per-connection interval.
func TestPresenceBudgetPerAddress(t *testing.T) {
	ts := startServerWith(t, func(s *Server) {
		s.SetLimitMode(limits.All)
		s.presenceLimit = newLimiterMode(rate{perMinute: 1, burst: 512}, limits.All)
	})
	ids := make([]string, maxPresenceIDs)
	for i := range ids {
		ids[i] = fmt.Sprintf("%09d", 100000000+i)
	}
	ask := func(c *testClient, reqID uint32) string {
		t.Helper()
		_ = c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Api{Api: &pb.ApiRequest{Id: reqID,
			Call: &pb.ApiRequest_GetPresence{GetPresence: &pb.GetPresence{DeviceIds: ids}}}}})
		m, err := c.readUntil(5*time.Second, func(m *pb.ServerMessage) bool { return m.GetApi().GetId() == reqID })
		if err != nil {
			t.Fatal(err)
		}
		return m.GetApi().Error
	}
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	for i := range 3 {
		c := ts.dial(t)
		if err := c.register(key, "scanner", true); err != nil {
			t.Fatal(err)
		}
		want := ""
		if i == 2 {
			want = "too many requests"
		}
		if got := ask(c, uint32(i+1)); got != want {
			t.Fatalf("request %d (connection %d): %q", i+1, i+1, got)
		}
	}
	// Default mode: loopback has no budget (TestCharacterisesPresenceEnumeration).
	l := newLimiter(presenceLimit)
	pub := netip.MustParseAddr("203.0.113.7")
	if !l.allowN(pub, time.Now(), 4096) || l.allowN(pub, time.Now(), 1) {
		t.Fatal("presence burst 4096")
	}
}
