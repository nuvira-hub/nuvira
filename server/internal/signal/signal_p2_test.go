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

	"github.com/nuvira-hub/nuvira/server/internal/pb"
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

// KNOWN BUG (Phase 2, new finding): the refusals during registration (bad
// proof, old protocol, blocked, rate limited) send an Error and return at once;
// handle's deferred conn.CloseWithError(0, "") then closes the connection
// before the stream data is delivered, so the client usually gets only the
// close (measured on Windows loopback: old_version 1/30, invalid proof 3-4/30).
// This test accepts both outcomes and counts them.
// Expected after fix: the client always gets the Error text before the close.
func TestCharacterisesBug_RegistrationRefusalUsuallyLost(t *testing.T) {
	ts := startServer(t)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	pub := key.Public().(ed25519.PublicKey)
	const tries = 10
	got := map[string]int{}
	for range tries {
		// Wrong signature: no device row either.
		c := ts.dial(t)
		_ = c.send(registerMsg(pub, "x", false))
		if m, err := c.read(5 * time.Second); err != nil || m.GetChallenge() == nil {
			t.Fatalf("challenge: %v %v", m, err)
		}
		_ = c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Proof{Proof: &pb.Proof{Signature: make([]byte, 64)}}})
		switch text := refusal(t, c); text {
		case "", "invalid proof of the key":
			got["proof:"+text]++
		default:
			t.Fatalf("bad proof: %q", text)
		}
		// Old protocol: refused before any challenge.
		c = ts.dial(t)
		old := registerMsg(pub, "x", false)
		old.GetRegister().ProtocolVersion = 0
		_ = c.send(old)
		switch text := refusal(t, c); text {
		case "", "@server.old_version":
			got["old:"+text]++
		default:
			t.Fatalf("old protocol: %q", text)
		}
	}
	t.Logf("outcomes over %d tries (\"\" = closed without the error): %v", tries, got)
	var rows int
	if err := ts.store.DB().QueryRow(`SELECT COUNT(*) FROM devices`).Scan(&rows); err != nil || rows != 0 {
		t.Fatalf("device rows %d %v", rows, err)
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

// Characterisation (lead: "unregistered QUIC connections can stay open"): the
// server sets no deadline on the first message; a connection that opens the
// stream and sends nothing stays open as long as the client keeps it alive
// (here 3 s; in production until the 30 s idle timeout, forever with keep-alives).
func TestCharacterisesUnregisteredConnectionStaysOpen(t *testing.T) {
	ts := startServer(t)
	c := ts.dial(t)
	// The stream exists for the server only once something is written: one byte
	// of a length prefix, never completed.
	if _, err := c.stream.Write([]byte{0x05}); err != nil {
		t.Fatal(err)
	}
	time.Sleep(3 * time.Second)
	if err := c.conn.Context().Err(); err != nil {
		t.Fatalf("connection closed by the server: %v", context.Cause(c.conn.Context()))
	}
	// Without opening a stream at all, the same.
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	c2 := ts.dial(t)
	_ = c2
	<-ctx.Done()
	if c2.conn.Context().Err() != nil {
		t.Fatal("silent connection closed")
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
}

// Characterisation (lead api.go:45): GetPresence needs no sign-in; any device
// learns online/last seen of any ID, 256 IDs per request, one request per 2 s
// per connection (not per device: a second connection asks again at once).
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
// stays online, the others are closed ("replaced by a new connection"). A
// connection replaced while its registration is in flight may lose even the
// Registered answer (see TestCharacterisesBug_RegistrationRefusalUsuallyLost):
// such a failed register is counted, not an error.
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
	if lost == n || id == "" {
		t.Fatal("no registration answered")
	}
	ts.waitOnline(t, id, true)
	time.Sleep(time.Second)
	open := 0
	for _, c := range cs {
		if c.conn.Context().Err() == nil {
			open++
		}
	}
	t.Logf("registrations whose answer was lost to the replacement: %d/%d", lost, n)
	if open != 1 {
		t.Fatalf("%d connections of the same device still open", open)
	}
}
