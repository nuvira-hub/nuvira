package relay

// Phase 2 (characterisation): Conn over real loopback UDP sockets: datagram
// size limits and many relayed sessions at once (run with -race on Linux).

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"sync"
	"testing"
	"time"
)

func listenLoopback(t testing.TB) *net.UDPConn {
	t.Helper()
	u, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { u.Close() })
	return u
}

func newLoopbackConn(t testing.TB) (*Conn, *Relay) {
	t.Helper()
	r := New(slog.New(slog.NewTextHandler(io.Discard, nil)))
	c := NewConn(listenLoopback(t), r)
	t.Cleanup(func() { c.Close() })
	return c, r
}

// pump plays quic-go's part: it reads (and so dispatches) until the Conn closes,
// passing what is not relayed to got.
func pump(c *Conn, got chan<- []byte) {
	buf := make([]byte, 64<<10)
	for {
		n, _, err := c.ReadFrom(buf)
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				return
			}
			continue
		}
		if got != nil {
			got <- append([]byte(nil), buf[:n]...)
		}
	}
}

// readOne reads a datagram from u, or nil after the timeout.
func readOne(u *net.UDPConn, timeout time.Duration) []byte {
	buf := make([]byte, 64<<10)
	_ = u.SetReadDeadline(time.Now().Add(timeout))
	n, _, err := u.ReadFromUDP(buf)
	if err != nil {
		return nil
	}
	return buf[:n]
}

// K18 (fixed in Phase 6, SRV-11): readUDP used to read into a 2048-byte buffer,
// so a larger datagram was truncated to 2048 bytes on Linux and dropped on
// Windows, silently. Now datagrams up to the largest IPv4 UDP payload are
// delivered whole, and the datagram after them intact.
func TestLargeDatagramDeliveredWhole(t *testing.T) {
	c, _ := newLoopbackConn(t)
	got := make(chan []byte, 8)
	go pump(c, got)
	client := listenLoopback(t)
	small := []byte{0x41, 1, 2, 3}
	for _, size := range []int{2049, 3000, maxIPv4Payload} {
		big := bytes.Repeat([]byte{0xc3}, size)
		if _, err := client.WriteTo(big, c.LocalAddr()); err != nil {
			t.Fatal(err)
		}
		if _, err := client.WriteTo(small, c.LocalAddr()); err != nil {
			t.Fatal(err)
		}
		for _, want := range [][]byte{big, small} {
			select {
			case p := <-got:
				if !bytes.Equal(p, want) {
					t.Fatalf("size %d: got %d bytes, want %d", size, len(p), len(want))
				}
			case <-time.After(3 * time.Second):
				t.Fatalf("size %d: %d-byte datagram not delivered", size, len(want))
			}
		}
	}
}

// Largest UDP payload over IPv4 (65,535 - 20 - 8).
const maxIPv4Payload = 65507

// The same holds for relayed traffic: the other side receives large datagrams whole.
func TestLargeRelayedDatagramForwardedWhole(t *testing.T) {
	c, r := newLoopbackConn(t)
	go pump(c, nil)
	viewer, agent := listenLoopback(t), listenLoopback(t)
	bindPair(t, r, c, viewer, agent, [tokenLen]byte{9})

	for i, size := range []int{2048, 2049, 9000, maxIPv4Payload} {
		data := bytes.Repeat([]byte{0xc4 + byte(i)}, size)
		if _, err := viewer.WriteTo(data, c.LocalAddr()); err != nil {
			t.Fatal(err)
		}
		if got := readOne(agent, 3*time.Second); !bytes.Equal(got, data) {
			t.Fatalf("%d-byte datagram relayed as %d bytes", size, len(got))
		}
	}
}

// bindPair binds viewer and agent to a fresh session through the Conn (real
// control datagrams) and waits for both "bound" answers.
func bindPair(t testing.TB, r *Relay, c *Conn, viewer, agent *net.UDPConn, token [tokenLen]byte) {
	t.Helper()
	r.Allow(token)
	for i, u := range []*net.UDPConn{viewer, agent} {
		for attempt := 0; ; attempt++ {
			if _, err := u.WriteTo(bind(token, byte(i+1)), c.LocalAddr()); err != nil {
				t.Fatal(err)
			}
			if reply := readOne(u, time.Second); len(reply) > 5+tokenLen && reply[4] == typeBound {
				break
			}
			if attempt == 5 {
				t.Fatalf("side %d not bound", i+1)
			}
		}
	}
	if r.Destination(viewer.LocalAddr(), 0) == nil {
		t.Fatal("session not active")
	}
}

// Many sessions relayed at once through one Conn, both directions: every
// datagram arrives at the right side, unchanged (loopback, paced senders: a few
// drops are tolerated and counted, as on a real socket).
func TestConcurrentRelayedSessions(t *testing.T) {
	const sessions, perSide = 16, 200
	c, r := newLoopbackConn(t)
	// As main does on the server's socket.
	_ = c.udp.SetReadBuffer(7 << 20)
	_ = c.udp.SetWriteBuffer(7 << 20)
	go pump(c, nil)
	type pair struct{ v, a *net.UDPConn }
	pairs := make([]pair, sessions)
	for i := range pairs {
		pairs[i] = pair{listenLoopback(t), listenLoopback(t)}
		var token [tokenLen]byte
		binary.BigEndian.PutUint32(token[:], uint32(i+1))
		bindPair(t, r, c, pairs[i].v, pairs[i].a, token)
	}
	var wg sync.WaitGroup
	var mu sync.Mutex
	received, wrong := 0, 0
	for i, p := range pairs {
		for dir, ends := range [][2]*net.UDPConn{{p.v, p.a}, {p.a, p.v}} {
			from, to := ends[0], ends[1]
			wg.Add(2)
			go func() {
				defer wg.Done()
				for k := range perSide {
					msg := []byte(fmt.Sprintf("\x40s%03d-d%d-k%04d", i, dir, k))
					_, _ = from.WriteTo(msg, c.LocalAddr())
					// Paced: this checks routing, not the dispatcher's throughput
					// (one reader goroutine: lead conn.go:84, see the bench B-GO).
					time.Sleep(time.Millisecond)
				}
			}()
			go func() {
				defer wg.Done()
				prefix := []byte(fmt.Sprintf("\x40s%03d-d%d-", i, dir))
				for {
					got := readOne(to, 1500*time.Millisecond)
					if got == nil {
						return
					}
					mu.Lock()
					if bytes.HasPrefix(got, prefix) {
						received++
					} else {
						wrong++
					}
					mu.Unlock()
				}
			}()
		}
	}
	wg.Wait()
	total := sessions * 2 * perSide
	t.Logf("received %d of %d, misrouted %d", received, total, wrong)
	if wrong != 0 {
		t.Fatalf("%d datagrams reached the wrong side", wrong)
	}
	if received < total*95/100 {
		t.Fatalf("only %d of %d datagrams relayed", received, total)
	}
	if n := readySessions(r); n != sessions {
		t.Fatalf("%d sessions ready", n)
	}
}

// readySessions counts the sessions with both sides bound (no Counters: these
// tests also build on the tag baseline-pre-review, which has no stats.go).
func readySessions(r *Relay) int {
	r.mu.Lock()
	defer r.mu.Unlock()
	n := 0
	for _, s := range r.sessions {
		if s.ready() {
			n++
		}
	}
	return n
}

// HandleControl and Destination from many goroutines on the same relay (the
// server calls them from one reader, but the stats and the expiry run apart).
func TestConcurrentControlAndDestination(t *testing.T) {
	r := New(slog.New(slog.NewTextHandler(io.Discard, nil)))
	var wg sync.WaitGroup
	for i := range 64 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			var token [tokenLen]byte
			binary.BigEndian.PutUint32(token[:], uint32(i+1))
			r.Allow(token)
			port := 1000 + i
			a := &net.UDPAddr{IP: net.IPv4(10, 2, byte(i), 1), Port: 2000 + i}
			for k := range 50 {
				// A new address value each time, as the UDP reader makes one per datagram.
				v := &net.UDPAddr{IP: net.IPv4(10, 1, byte(i), 1), Port: port}
				r.HandleControl(bind(token, 1), v)
				r.HandleControl(bind(token, 2), a)
				if k%10 == 9 { // NAT rebinding, from the next round
					port++
				}
				if dst := r.Destination(v, 100); dst == nil || dst.String() != a.String() {
					t.Errorf("session %d: viewer → %v", i, dst)
					return
				}
				_ = readySessions(r)
			}
		}()
	}
	wg.Wait()
	if n := readySessions(r); n != 64 {
		t.Fatalf("%d sessions ready", n)
	}
}
