package relay

// Phase 2 (characterisation): native Go fuzz targets of the relay's parsers.
// Run one with e.g. `go test -run '^$' -fuzz '^FuzzHandleControl$' -fuzztime 5m`;
// without -fuzz the seeds below run as ordinary tests.

import (
	"bytes"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/netip"
	"testing"
	"time"
)

// fuzzToken is the token Allowed in every FuzzHandleControl run.
var fuzzToken = [tokenLen]byte{0xA5, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15}

func fuzzBind(role byte, tail ...byte) []byte {
	p := append([]byte{}, magic...)
	p = append(p, typeBind)
	p = append(p, fuzzToken[:]...)
	p = append(p, role)
	return append(p, tail...)
}

// FuzzHandleControl feeds arbitrary control datagrams from two addresses (a
// viewer-like and an agent-like one) to a relay that knows one token. It checks
// that nothing panics and that every answer is a well formed "bound" for the
// allowed token, whose address list parses back.
func FuzzHandleControl(f *testing.F) {
	f.Add(fuzzBind(1), fuzzBind(2), false)
	f.Add(fuzzBind(1, 1, 4, 192, 168, 1, 7, 0x1f, 0x90), fuzzBind(2, 2, 4, 10, 0, 0, 1, 0, 80, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 187), true)
	f.Add(fuzzBind(2|roleAnnounce), fuzzBind(1), true)
	f.Add(fuzzBind(3), fuzzBind(0), false)
	f.Add(fuzzBind(1, 5), fuzzBind(2, 1, 9), false)
	f.Add([]byte{0, 'N', 'X', 'R', 2}, []byte{0, 'N', 'X', 'R'}, true)
	f.Add(fuzzBind(1, 4, 4, 1, 1, 1, 1, 0, 1, 4, 2, 2, 2, 2, 0, 2, 4, 3, 3, 3, 3, 0, 3, 4, 4, 4, 4, 4, 0, 4), fuzzBind(2), true)
	r := New(slog.New(slog.NewTextHandler(io.Discard, nil)))
	viewer := &net.UDPAddr{IP: net.IPv4(198, 51, 100, 1), Port: 1000}
	agent := &net.UDPAddr{IP: net.ParseIP("2001:db8::2"), Port: 2000}
	f.Fuzz(func(t *testing.T, a, b []byte, ws bool) {
		r.Allow(fuzzToken) // fresh session every run
		var second net.Addr = agent
		if ws {
			second = wsAddr("ws:1@203.0.113.9")
		}
		for i, step := range []struct {
			p    []byte
			from net.Addr
		}{{a, viewer}, {b, second}, {a, viewer}, {b, &net.UDPAddr{IP: net.IPv4(198, 51, 100, 1), Port: 1001}}} {
			reply := r.HandleControl(step.p, step.from)
			if reply == nil {
				continue
			}
			checkBound(t, i, reply)
		}
		_ = r.Destination(viewer, len(a))
		_ = r.Destination(second, len(b))
	})
}

func checkBound(t *testing.T, step int, reply []byte) {
	t.Helper()
	if len(reply) < 5+tokenLen+1 || !bytes.Equal(reply[:4], magic) || reply[4] != typeBound ||
		!bytes.Equal(reply[5:5+tokenLen], fuzzToken[:]) {
		t.Fatalf("step %d: malformed answer %x", step, reply)
	}
	rest := reply[5+tokenLen:]
	switch rest[0] {
	case 0:
		if len(rest) != 1 {
			t.Fatalf("step %d: not-ready answer with a tail %x", step, reply)
		}
	case 1:
		rest = rest[1:]
		// Address of the other side: family 0 (WebSocket) or a UDP address.
		if len(rest) > 0 && rest[0] == 0 {
			rest = rest[1:]
		} else {
			l := addrLen(rest)
			if l == 0 || l > len(rest) {
				t.Fatalf("step %d: other side's address unreadable %x", step, reply)
			}
			rest = rest[l:]
		}
		// The others: at most maxLocals of the side plus one announced address.
		if len(rest) == 0 || int(rest[0]) > maxLocals+1 {
			t.Fatalf("step %d: bad list of the other side %x", step, reply)
		}
		n := int(rest[0])
		rest = rest[1:]
		for range n {
			l := addrLen(rest)
			if l == 0 || l > len(rest) {
				t.Fatalf("step %d: truncated list %x", step, reply)
			}
			rest = rest[l:]
		}
		if len(rest) != 0 {
			t.Fatalf("step %d: trailing bytes %x", step, reply)
		}
	default:
		t.Fatalf("step %d: ready byte %d", step, rest[0])
	}
}

// addrLen is the length of the encoded address at the start of b (0 if unknown).
func addrLen(b []byte) int {
	switch {
	case len(b) > 0 && b[0] == 4:
		return 1 + 4 + 2
	case len(b) > 0 && b[0] == 6:
		return 1 + 16 + 2
	}
	return 0
}

// FuzzParseAddrs checks parseAddrs: never more than maxLocals addresses, and an
// accepted list encodes back (appendAddr) to the same bytes, except that an
// IPv4-mapped IPv6 address is written back as IPv4 (current behaviour).
func FuzzParseAddrs(f *testing.F) {
	f.Add([]byte{})
	f.Add([]byte{0})
	f.Add([]byte{2, 4, 10, 0, 0, 1, 0, 80, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 187})
	f.Add([]byte{1, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 1, 2, 3, 4, 0, 9})
	f.Add([]byte{5})
	f.Add([]byte{1, 9, 1, 2})
	f.Add([]byte{1, 4, 10, 0, 0, 1, 0})
	f.Fuzz(func(t *testing.T, b []byte) {
		addrs, ok := parseAddrs(b)
		if !ok {
			// Current behaviour: with trailing bytes the addresses read so far
			// come back too (the caller ignores them when ok is false).
			return
		}
		if len(addrs) > maxLocals {
			t.Fatalf("%d addresses accepted", len(addrs))
		}
		if len(b) == 0 {
			if len(addrs) != 0 {
				t.Fatal("addresses out of nothing")
			}
			return
		}
		enc := []byte{byte(len(addrs))}
		mapped := false
		for _, a := range addrs {
			if a.Port < 0 || a.Port > 0xffff {
				t.Fatalf("port %d", a.Port)
			}
			if len(a.IP) == 16 && a.IP.To4() != nil {
				mapped = true
			}
			enc = appendAddr(enc, a)
		}
		if !mapped && !bytes.Equal(enc, b) {
			t.Fatalf("round trip %x -> %v -> %x", b, addrs, enc)
		}
	})
}

// FuzzConnReadFrom pushes one datagram through Conn's dispatching (as if read
// from UDP or WebSocket) and reads it with a buffer of a fuzzed size. A datagram
// that is neither control nor relayed reaches the reader truncated to the
// buffer (current behaviour: copy, no error and no flag); control datagrams
// never reach it.
func FuzzConnReadFrom(f *testing.F) {
	f.Add([]byte{0x40, 1, 2, 3}, uint16(1500))
	f.Add(bytes.Repeat([]byte{0xc0}, 2048), uint16(1200))
	f.Add(bytes.Repeat([]byte{0xc0}, 3000), uint16(4096)) // larger than the UDP read buffer
	f.Add(fuzzBind(1), uint16(1500))
	f.Add([]byte{0, 'N', 'X', 'R'}, uint16(1500))
	f.Add([]byte{}, uint16(0))
	udp, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		f.Fatal(err)
	}
	r := New(slog.New(slog.NewTextHandler(io.Discard, nil)))
	c := NewConn(udp, r)
	f.Cleanup(func() { c.Close() })
	// Answers to control datagrams go to a port nobody reads.
	sink, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		f.Fatal(err)
	}
	f.Cleanup(func() { sink.Close() })
	from := sink.LocalAddr()
	f.Fuzz(func(t *testing.T, data []byte, size uint16) {
		r.Allow(fuzzToken)
		c.deliver(append([]byte(nil), data...), from)
		// A marker after it: tells "dropped (control)" from "delivered".
		marker := []byte{0x41, 0xEE, 0xEE}
		c.deliver(marker, from)
		buf := make([]byte, size)
		_ = c.SetReadDeadline(time.Now().Add(2 * time.Second))
		n, got, err := c.ReadFrom(buf)
		if err != nil {
			t.Fatalf("ReadFrom: %v", err)
		}
		if got.String() != from.String() {
			t.Fatalf("from %v", got)
		}
		if IsControl(data) {
			if n != min(len(marker), len(buf)) || !bytes.Equal(buf[:n], marker[:n]) {
				t.Fatalf("control datagram %x delivered as %x", data, buf[:n])
			}
			return
		}
		if n != min(len(data), len(buf)) || !bytes.Equal(buf[:n], data[:n]) {
			t.Fatalf("datagram of %d bytes into %d: got %d", len(data), len(buf), n)
		}
		// The marker that follows.
		if n, _, err := c.ReadFrom(make([]byte, 16)); err != nil || n != len(marker) {
			t.Fatalf("marker: %d %v", n, err)
		}
	})
}

// FuzzRemoteHost checks the WebSocket client address: never a panic; from a
// public peer the header is ignored; from a loopback/private peer the result is
// the peer itself or a valid IP taken from the last X-Forwarded-For entry.
func FuzzRemoteHost(f *testing.F) {
	f.Add("203.0.113.7:4000", "10.9.9.9", "")
	f.Add("127.0.0.1:4000", "1.2.3.4, 198.51.100.4", "")
	f.Add("[::1]:4000", "garbage", "5.6.7.8")
	f.Add("10.0.0.2:1", "1.1.1.1", "2.2.2.2, ::ffff:3.3.3.3")
	f.Add("192.168.1.1", "", "")
	f.Add("[fe80::1%eth0]:80", "8.8.8.8", "")
	f.Add("", ",", ",")
	f.Fuzz(func(t *testing.T, remote, fwd1, fwd2 string) {
		req := &http.Request{RemoteAddr: remote, Header: http.Header{}}
		var last string
		for _, v := range []string{fwd1, fwd2} {
			if v != "" {
				req.Header.Add("X-Forwarded-For", v)
				last = v
			}
		}
		got := remoteHost(req)
		host, _, err := net.SplitHostPort(remote)
		if err != nil {
			host = remote
		}
		if got == host {
			return
		}
		peer, err := netip.ParseAddr(host)
		if err != nil || !(peer.IsLoopback() || peer.IsPrivate()) {
			t.Fatalf("header used from peer %q: %q", remote, got)
		}
		ip, err := netip.ParseAddr(got)
		if err != nil {
			t.Fatalf("not an IP: %q", got)
		}
		if last == "" {
			t.Fatalf("%q without a header", got)
		}
		entries := bytes.Split([]byte(last), []byte(","))
		lastEntry, _ := netip.ParseAddr(string(bytes.TrimSpace(entries[len(entries)-1])))
		if lastEntry != ip {
			t.Fatalf("%q is not the last entry of %q", got, last)
		}
	})
}
