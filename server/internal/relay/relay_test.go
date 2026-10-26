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
	if reply := r.HandleControl(bind(token, 2), agent); reply == nil || reply[len(reply)-1] != 1 {
		t.Fatalf("secondo lato: risposta %v", reply)
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
