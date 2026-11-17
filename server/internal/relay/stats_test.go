package relay

import (
	"bytes"
	"io"
	"log/slog"
	"net"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestParseStatsInterval(t *testing.T) {
	for in, want := range map[string]time.Duration{
		"": 0, "off": 0, "OFF": 0, "0": 0, "10s": 10 * time.Second, " 1m ": time.Minute, "5": 5 * time.Second, "2.5": 2500 * time.Millisecond,
	} {
		got, err := ParseStatsInterval(in)
		if err != nil || got != want {
			t.Errorf("%q: %v, %v; want %v", in, got, err, want)
		}
	}
	for _, in := range []string{"x", "10q", "100ms", "-1s", "0.5"} {
		if d, err := ParseStatsInterval(in); err == nil {
			t.Errorf("%q accepted as %v", in, d)
		}
	}
}

// testConn is a Conn without a socket: deliver and the counters only.
func testConn(r *Relay, queue int) *Conn {
	return &Conn{relay: r, in: make(chan packet, queue), closed: make(chan struct{})}
}

func TestCounters(t *testing.T) {
	r := New(slog.New(slog.NewTextHandler(io.Discard, nil)))
	c := testConn(r, 2)
	var token, waiting [tokenLen]byte
	token[0], waiting[0] = 1, 2
	r.Allow(token)
	r.Allow(waiting)
	viewer := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 1), Port: 1000}
	agent := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 2), Port: 2000}
	r.HandleControl(bind(token, 1), viewer)
	r.HandleControl(bind(token, 2), agent)
	if n := c.Counters(); n != (Counters{SessionsOpen: 1, SessionsWaiting: 1}) {
		t.Fatalf("before traffic: %+v", n)
	}
	r.Destination(viewer, 100)
	r.Destination(agent, 1200)
	// Not part of a session: not forwarded, not counted.
	r.Destination(&net.UDPAddr{IP: net.IPv4(10, 0, 0, 9), Port: 9}, 500)
	// A queue of two: the third datagram is dropped.
	for range 3 {
		c.deliver([]byte{1}, viewer)
	}
	if n := c.Counters(); n != (Counters{Packets: 2, Bytes: 1300, Drops: 1, SessionsOpen: 1, SessionsWaiting: 1}) {
		t.Fatalf("after traffic: %+v", n)
	}
}

type syncBuffer struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (s *syncBuffer) Write(p []byte) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.Write(p)
}

func (s *syncBuffer) String() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.String()
}

func TestLogStats(t *testing.T) {
	r := New(slog.New(slog.NewTextHandler(io.Discard, nil)))
	c := testConn(r, 1)
	var token [tokenLen]byte
	r.Allow(token)
	viewer := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 1), Port: 1000}
	agent := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 2), Port: 2000}
	r.HandleControl(bind(token, 1), viewer)
	r.HandleControl(bind(token, 2), agent)
	r.Destination(viewer, 1000)
	c.deliver([]byte{1}, viewer)
	c.deliver([]byte{1}, viewer) // dropped

	var out syncBuffer
	done := make(chan struct{})
	go func() {
		c.LogStats(200*time.Millisecond, slog.New(slog.NewTextHandler(&out, nil)))
		close(done)
	}()
	time.Sleep(50 * time.Millisecond)
	r.Destination(agent, 500)
	c.deliver([]byte{1}, agent) // dropped: the queue still holds one
	time.Sleep(450 * time.Millisecond)
	close(c.closed)
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("LogStats did not stop with the Conn")
	}
	lines := strings.Split(strings.TrimSpace(out.String()), "\n")
	if len(lines) < 2 {
		t.Fatalf("lines: %q", lines)
	}
	// The first interval holds only what came after the start.
	first := lines[0]
	for _, want := range []string{`msg="relay stats"`, "packets=1 ", "bytes=500 ", "queue_drops=1 ", "queue_len=1 ", "sessions_open=1 ", "sessions_waiting=0"} {
		if !strings.Contains(first, want) {
			t.Errorf("first line without %q: %s", want, first)
		}
	}
	if !strings.Contains(lines[1], "packets=0 ") || !strings.Contains(lines[1], "queue_drops=0 ") {
		t.Errorf("second line: %s", lines[1])
	}
	// Numbers only: no address of the sessions.
	if strings.Contains(out.String(), "10.0.0.") {
		t.Errorf("an address in the log: %s", out.String())
	}
}
