package signal

// SRV-2: a client that stops reading its stream must not block the goroutines
// that send to it (the agent's goroutine, the expiry loop).

import (
	"testing"
	"time"

	"github.com/quic-go/quic-go"

	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

const testSendTimeout = 2 * time.Second

func shortSendTimeout(s *Server) { s.sendTimeout = testSendTimeout }

// stalledViewer registers an apiOnly viewer with a tiny stream receive window,
// so that a few kilobytes of messages it never reads block the server's writes.
func stalledViewer(t *testing.T, ts *testServer) *testClient {
	t.Helper()
	c := ts.dialWith(t, &quic.Config{KeepAlivePeriod: 5 * time.Second, MaxIdleTimeout: 30 * time.Second,
		InitialStreamReceiveWindow: 1024, MaxStreamReceiveWindow: 1024})
	if err := c.register(nil, "stalled", true); err != nil {
		t.Fatal(err)
	}
	return c
}

// connectMany sends n Connect to the agent from v and reads the agent's Incoming
// for each one; it returns their session IDs.
func connectMany(t *testing.T, v, agent *testClient, n int) []string {
	t.Helper()
	for range n {
		if err := v.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Connect{Connect: &pb.Connect{DeviceId: agent.id}}}); err != nil {
			t.Fatal(err)
		}
	}
	ids := make([]string, 0, n)
	for range n {
		m, err := agent.readUntil(10*time.Second, func(m *pb.ServerMessage) bool { return m.GetIncoming() != nil })
		if err != nil {
			t.Fatalf("incoming %d: %v", len(ids)+1, err)
		}
		ids = append(ids, m.GetIncoming().SessionId)
	}
	return ids
}

func closedWithin(c *testClient, d time.Duration) bool {
	select {
	case <-c.conn.Context().Done():
		return true
	case <-time.After(d):
		return false
	}
}

// The agent rejects many requests of a viewer that does not read: the agent's
// goroutine writes the rejections to that viewer and used to block on it with
// no time limit, so the agent could not serve anybody else (reproduced in
// Phase 3/4). Now the write gives up after sendTimeout, the stalled viewer is
// disconnected, and an honest viewer gets its Route.
func TestStalledViewerDoesNotBlockAgent(t *testing.T) {
	ts := startServerWith(t, shortSendTimeout)
	agent := ts.device(t, "agent")
	ts.waitOnline(t, agent.id, true)
	bad := stalledViewer(t, ts)
	for _, id := range connectMany(t, bad, agent, 200) {
		_ = agent.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Reject{Reject: &pb.Reject{SessionId: id, Reason: "@agent.rejected"}}})
	}
	start := time.Now()
	good := ts.dial(t)
	if err := good.register(nil, "good", true); err != nil {
		t.Fatal(err)
	}
	_ = good.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Connect{Connect: &pb.Connect{DeviceId: agent.id}}})
	in, err := agent.readUntil(testSendTimeout+5*time.Second, func(m *pb.ServerMessage) bool {
		return m.GetIncoming() != nil && m.GetIncoming().ViewerId == good.id
	})
	if err != nil {
		t.Fatalf("incoming of the honest viewer: %v", err)
	}
	_ = agent.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Accept{Accept: &pb.Accept{SessionId: in.GetIncoming().SessionId}}})
	if _, err := good.readUntil(testSendTimeout+5*time.Second, func(m *pb.ServerMessage) bool { return m.GetRoute() != nil }); err != nil {
		t.Fatalf("honest viewer got no route: %v", err)
	}
	t.Logf("honest viewer routed %v after the stall started", time.Since(start).Round(time.Millisecond))
	if !closedWithin(bad, testSendTimeout+5*time.Second) {
		t.Fatal("the viewer that does not read is still connected")
	}
	if agent.conn.Context().Err() != nil {
		t.Fatal("the agent was disconnected")
	}
}

// The expiry loop sends "@agent.no_answer" to every viewer whose request
// expired; a viewer that does not read used to stop it for good (no more
// expiries for anybody). Now it costs at most one sendTimeout.
func TestStalledViewerDoesNotBlockExpiry(t *testing.T) {
	ts := startServerWith(t, shortSendTimeout)
	agent := ts.device(t, "agent")
	ts.waitOnline(t, agent.id, true)
	bad := stalledViewer(t, ts)
	connectMany(t, bad, agent, 200)
	good := ts.dial(t)
	if err := good.register(nil, "good", true); err != nil {
		t.Fatal(err)
	}
	connectMany(t, good, agent, 1)

	done := make(chan struct{})
	go func() {
		ts.srv.expire(time.Now().Add(pendingTimeout + time.Second))
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(testSendTimeout + 5*time.Second):
		t.Fatal("expire blocked by the viewer that does not read")
	}
	m, err := good.readUntil(5*time.Second, func(m *pb.ServerMessage) bool { return m.GetError() != nil })
	if err != nil || m.GetError().Message != "@agent.no_answer" {
		t.Fatalf("honest viewer: %v %v", m, err)
	}
	ts.srv.mu.Lock()
	left := len(ts.srv.sessions)
	ts.srv.mu.Unlock()
	if left != 0 {
		t.Fatalf("%d requests not expired", left)
	}
	if !closedWithin(bad, 5*time.Second) {
		t.Fatal("the viewer that does not read is still connected")
	}
}
