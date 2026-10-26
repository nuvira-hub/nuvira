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

	// Gettone sconosciuto: nessuna risposta.
	if reply := r.HandleControl(bind(token, 1), viewer); reply != nil {
		t.Fatal("bind accettato senza Allow")
	}
	r.Allow(token)
	if reply := r.HandleControl(bind(token, 1), viewer); reply == nil || reply[len(reply)-1] != 0 {
		t.Fatalf("primo lato: risposta %v", reply)
	}
	if dst := r.Destination(viewer, 10); dst != nil {
		t.Fatal("inoltro prima che entrambi i lati siano collegati")
	}
	// L'agent comunica anche un indirizzo locale.
	agentBind := append(bind(token, 2), 1, 4, 192, 168, 1, 7, 0x1f, 0x90)
	reply := r.HandleControl(agentBind, agent)
	if len(reply) != 5+tokenLen+1+7+1 || reply[5+tokenLen] != 1 {
		t.Fatalf("secondo lato: risposta %v", reply)
	}
	// Il viewer, ricollegandosi, riceve indirizzo visto dal relay e indirizzo locale dell'agent.
	if reply := r.HandleControl(bind(token, 1), viewer); len(reply) != 5+tokenLen+1+7+1+7 ||
		reply[5+tokenLen+1+7] != 1 || reply[5+tokenLen+1+7+1+1] != 192 {
		t.Fatalf("risposta al viewer: %v", reply)
	}
	// L'agent riceve l'indirizzo del viewer: 10.0.0.1:1000.
	if got := reply[5+tokenLen+1:]; got[0] != 4 || got[1] != 10 || got[4] != 1 || int(got[5])<<8|int(got[6]) != 1000 {
		t.Fatalf("indirizzo dell'altro lato: %v", got)
	}
	if dst := r.Destination(viewer, 10); dst.String() != agent.String() {
		t.Fatalf("viewer → %v", dst)
	}
	if dst := r.Destination(agent, 10); dst.String() != viewer.String() {
		t.Fatalf("agent → %v", dst)
	}

	// Il viewer cambia indirizzo (NAT): il vecchio non inoltra più.
	moved := &net.UDPAddr{IP: net.IPv4(10, 0, 0, 1), Port: 1001}
	r.HandleControl(bind(token, 1), moved)
	if dst := r.Destination(viewer, 10); dst != nil {
		t.Fatal("il vecchio indirizzo inoltra ancora")
	}
	if dst := r.Destination(agent, 10); dst.String() != moved.String() {
		t.Fatalf("agent → %v dopo il cambio", dst)
	}
}

func TestIsControlIgnoresQUIC(t *testing.T) {
	// Un pacchetto QUIC ha sempre il bit 0x40 nel primo byte.
	if IsControl([]byte{0x40, 'N', 'X', 'R', 1}) {
		t.Fatal("pacchetto QUIC scambiato per controllo")
	}
}

func TestParseAddrs(t *testing.T) {
	if a, ok := parseAddrs(nil); !ok || len(a) != 0 {
		t.Fatal("elenco assente")
	}
	a, ok := parseAddrs([]byte{2, 4, 10, 0, 0, 1, 0, 80, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 187})
	if !ok || len(a) != 2 || a[0].String() != "10.0.0.1:80" || a[1].String() != "[::1]:443" {
		t.Fatalf("%v %v", a, ok)
	}
	for _, bad := range [][]byte{{1}, {1, 9, 1, 2}, {5}, {1, 4, 10, 0, 0, 1, 0}} {
		if _, ok := parseAddrs(bad); ok {
			t.Fatalf("accettato %v", bad)
		}
	}
}
