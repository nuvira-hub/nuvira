package relay

import (
	"io"
	"log/slog"
	"net"
	"testing"
)

func bind(token [tokenLen]byte, role byte) []byte {
	p := append([]byte{}, magic...)
	p = append(p, typeBind)
	p = append(p, token[:]...)
	return append(p, role)
}

func TestBindAndForward(t *testing.T) {
	r := New(slog.New(slog.NewTextHandler(io.Discard, nil)))
	var token [tokenLen]byte
	token[0] = 42
	viewer := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 1), Port: 1000}
	agent := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 2), Port: 2000}

	// Unknown token: no answer.
	if reply := r.HandleControl(bind(token, 1), viewer); reply != nil {
		t.Fatal("bind accepted without Allow")
	}
	r.Allow(token)
	if reply := r.HandleControl(bind(token, 1), viewer); reply == nil || reply[len(reply)-1] != 0 {
		t.Fatalf("first side: answer %v", reply)
	}
	if dst := r.Destination(viewer, 10); dst != nil {
		t.Fatal("forwarding before both sides are bound")
	}
	// The agent also announces a local address.
	agentBind := append(bind(token, 2), 1, 4, 192, 168, 1, 7, 0x1f, 0x90)
	reply := r.HandleControl(agentBind, agent)
	if len(reply) != 5+tokenLen+1+7+1 || reply[5+tokenLen] != 1 {
		t.Fatalf("second side: answer %v", reply)
	}
	// The viewer, binding again, receives the address seen by the relay and the agent's local address.
	if reply := r.HandleControl(bind(token, 1), viewer); len(reply) != 5+tokenLen+1+7+1+7 ||
		reply[5+tokenLen+1+7] != 1 || reply[5+tokenLen+1+7+1+1] != 192 {
		t.Fatalf("answer to the viewer: %v", reply)
	}
	// The agent receives the viewer's address: 10.0.0.1:1000.
	if got := reply[5+tokenLen+1:]; got[0] != 4 || got[1] != 10 || got[4] != 1 || int(got[5])<<8|int(got[6]) != 1000 {
		t.Fatalf("address of the other side: %v", got)
	}
	if dst := r.Destination(viewer, 10); dst.String() != agent.String() {
		t.Fatalf("viewer → %v", dst)
	}
	if dst := r.Destination(agent, 10); dst.String() != viewer.String() {
		t.Fatalf("agent → %v", dst)
	}

	// The viewer changes address (NAT): the old one no longer forwards.
	moved := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 1), Port: 1001}
	r.HandleControl(bind(token, 1), moved)
	if dst := r.Destination(viewer, 10); dst != nil {
		t.Fatal("the old address still forwards")
	}
	if dst := r.Destination(agent, 10); dst.String() != moved.String() {
		t.Fatalf("agent → %v after the change", dst)
	}
}

func TestIsControlIgnoresQUIC(t *testing.T) {
	// A QUIC packet always has the 0x40 bit in the first byte.
	if IsControl([]byte{0x40, 'N', 'X', 'R', 1}) {
		t.Fatal("QUIC packet mistaken for control")
	}
}

func TestParseAddrs(t *testing.T) {
	if a, ok := parseAddrs(nil); !ok || len(a) != 0 {
		t.Fatal("missing list")
	}
	a, ok := parseAddrs([]byte{2, 4, 10, 0, 0, 1, 0, 80, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 187})
	if !ok || len(a) != 2 || a[0].String() != "10.0.0.1:80" || a[1].String() != "[::1]:443" {
		t.Fatalf("%v %v", a, ok)
	}
	for _, bad := range [][]byte{{1}, {1, 9, 1, 2}, {5}, {1, 4, 10, 0, 0, 1, 0}} {
		if _, ok := parseAddrs(bad); ok {
			t.Fatalf("accepted %v", bad)
		}
	}
}
