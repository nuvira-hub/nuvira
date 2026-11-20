package relay

import (
	"bytes"
	"io"
	"log/slog"
	"net"
	"testing"
	"time"
)

// shortGrace shortens forgetGrace for one test.
func shortGrace(t *testing.T, d time.Duration) {
	t.Helper()
	old := forgetGrace
	forgetGrace = d
	t.Cleanup(func() { forgetGrace = old })
}

// SRV-13 / K39: after Forget and its grace, a datagram from an address of the
// ended session is no longer forwarded to the old peer: it reaches ReadFrom
// (quic-go), as the Initial of a new signalling socket that got that port must.
// During the grace it is still forwarded (CONNECTION_CLOSE frames).
func TestForgetReleasesTheAddresses(t *testing.T) {
	shortGrace(t, 300*time.Millisecond)
	c, r := newLoopbackConn(t)
	got := make(chan []byte, 8)
	go pump(c, got)
	viewer, agent := listenLoopback(t), listenLoopback(t)
	token := [tokenLen]byte{7}
	bindPair(t, r, c, viewer, agent, token)

	r.Forget(token)
	during := []byte{0x41, 'c', 'l', 'o', 's', 'e'}
	if _, err := viewer.WriteTo(during, c.LocalAddr()); err != nil {
		t.Fatal(err)
	}
	if p := readOne(agent, 2*time.Second); !bytes.Equal(p, during) {
		t.Fatalf("datagram during the grace not forwarded: %x", p)
	}

	deadline := time.Now().Add(5 * time.Second)
	for r.Destination(viewer.LocalAddr(), 0) != nil || r.Destination(agent.LocalAddr(), 0) != nil {
		if time.Now().After(deadline) {
			t.Fatal("links still there after the grace")
		}
		time.Sleep(20 * time.Millisecond)
	}
	initial := []byte{0xc3, 0, 0, 0, 1, 8}
	if _, err := viewer.WriteTo(initial, c.LocalAddr()); err != nil {
		t.Fatal(err)
	}
	select {
	case p := <-got:
		if !bytes.Equal(p, initial) {
			t.Fatalf("ReadFrom got %x", p)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("datagram from a forgotten address did not reach ReadFrom")
	}
	if p := readOne(agent, 300*time.Millisecond); p != nil {
		t.Fatalf("still forwarded to the old peer: %x", p)
	}
	r.mu.Lock()
	n := len(r.sessions)
	r.mu.Unlock()
	if n != 0 {
		t.Fatalf("%d sessions left", n)
	}
}

// An ended session accepts no binds, and its traffic does not refresh its
// idle time (so it cannot live on through the idle timeout either).
func TestForgottenSessionNoBindNoRefresh(t *testing.T) {
	shortGrace(t, time.Hour)
	r := New(slog.New(slog.NewTextHandler(io.Discard, nil)))
	token := [tokenLen]byte{8}
	viewer := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 1), Port: 1000}
	agent := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 2), Port: 2000}
	r.Allow(token)
	r.HandleControl(bind(token, 1), viewer)
	r.HandleControl(bind(token, 2), agent)

	r.Forget(token)
	r.mu.Lock()
	last := time.Now().Add(-time.Minute)
	r.sessions[token].last = last
	r.mu.Unlock()
	if dst := r.Destination(viewer, 10); dst == nil || dst.String() != agent.String() {
		t.Fatalf("forwarding during the grace: %v", dst)
	}
	if reply := r.HandleControl(bind(token, 1), &net.UDPAddr{IP: net.IPv4(10, 0, 0, 1), Port: 1001}); reply != nil {
		t.Fatalf("bind after Forget answered: %x", reply)
	}
	r.mu.Lock()
	s := r.sessions[token]
	r.mu.Unlock()
	if !s.last.Equal(last) {
		t.Fatal("traffic after Forget refreshed the idle time")
	}
	if s.peers[0].String() != viewer.String() {
		t.Fatalf("bind after Forget moved the viewer to %v", s.peers[0])
	}
	r.Forget(token) // twice: no effect
	r.Forget([tokenLen]byte{99})
}

// Removing an ended session leaves alone a link that another session took
// over meanwhile (the same address bound to a new session).
func TestForgetKeepsLinksOfANewerSession(t *testing.T) {
	shortGrace(t, 100*time.Millisecond)
	r := New(slog.New(slog.NewTextHandler(io.Discard, nil)))
	viewer := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 1), Port: 1000}
	agent := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 2), Port: 2000}
	agent2 := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 3), Port: 3000}
	old, next := [tokenLen]byte{1}, [tokenLen]byte{2}
	r.Allow(old)
	r.HandleControl(bind(old, 1), viewer)
	r.HandleControl(bind(old, 2), agent)
	r.Forget(old)
	r.Allow(next)
	r.HandleControl(bind(next, 1), viewer)
	r.HandleControl(bind(next, 2), agent2)

	deadline := time.Now().Add(5 * time.Second)
	for {
		r.mu.Lock()
		_, there := r.sessions[old]
		r.mu.Unlock()
		if !there {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("old session not removed")
		}
		time.Sleep(20 * time.Millisecond)
	}
	if dst := r.Destination(viewer, 10); dst == nil || dst.String() != agent2.String() {
		t.Fatalf("new session's link lost: %v", dst)
	}
	if dst := r.Destination(agent, 10); dst != nil {
		t.Fatalf("old agent address still linked to %v", dst)
	}
}
