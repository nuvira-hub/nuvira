package signal

import (
	"context"
	"crypto/tls"
	"net"
	"testing"
	"time"

	"github.com/quic-go/quic-go"

	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

// relayBind binds u to the relay session of token as role (1 viewer, 2 agent)
// with a real control datagram and waits for the answer.
func relayBind(t *testing.T, ts *testServer, u *net.UDPConn, token []byte, role byte) {
	t.Helper()
	msg := append([]byte{0x00, 'N', 'X', 'R', 0x01}, token...)
	msg = append(msg, role)
	buf := make([]byte, 2048)
	for range 5 {
		if _, err := u.WriteTo(msg, ts.udp); err != nil {
			t.Fatal(err)
		}
		_ = u.SetReadDeadline(time.Now().Add(time.Second))
		if n, _, err := u.ReadFrom(buf); err == nil && n > 5 && buf[4] == 0x02 {
			return
		}
	}
	t.Fatalf("role %d not bound", role)
}

// SRV-13 / K39: the agent's report of a session ends its relay session, so a
// new signalling socket that gets one of the session's addresses (a reused
// random ephemeral port) reaches the server again. Before, the relay forwarded
// its QUIC Initial to the old peer for the 2-minute idle timeout and the
// handshake timed out.
func TestReportFreesTheRelayAddresses(t *testing.T) {
	ts := startServer(t)
	agent := ts.device(t, "agent")
	viewer := ts.dial(t)
	if err := viewer.register(nil, "viewer", true); err != nil {
		t.Fatal(err)
	}
	ts.waitOnline(t, agent.id, true)
	if err := viewer.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Connect{Connect: &pb.Connect{DeviceId: agent.id}}}); err != nil {
		t.Fatal(err)
	}
	in, err := agent.readUntil(10*time.Second, func(m *pb.ServerMessage) bool { return m.GetIncoming() != nil })
	if err != nil {
		t.Fatal(err)
	}
	id := in.GetIncoming().SessionId
	if err := agent.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Accept{Accept: &pb.Accept{SessionId: id}}}); err != nil {
		t.Fatal(err)
	}
	rt, err := agent.readUntil(10*time.Second, func(m *pb.ServerMessage) bool { return m.GetRoute() != nil })
	if err != nil {
		t.Fatal(err)
	}
	token := rt.GetRoute().RelayToken

	// The two session sockets bind to the relay: the viewer's one is the
	// address a later signalling socket will get again.
	listen := func() *net.UDPConn {
		u, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { _ = u.Close() })
		return u
	}
	vs, as := listen(), listen()
	relayBind(t, ts, vs, token, 1)
	relayBind(t, ts, as, token, 2)
	if ts.relay.Destination(vs.LocalAddr(), 0) == nil {
		t.Fatal("relay session not active")
	}

	if err := agent.send(&pb.ClientMessage{Msg: &pb.ClientMessage_SessionReport{SessionReport: &pb.SessionReport{SessionId: id, DurationS: 3}}}); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(15 * time.Second)
	for ts.relay.Destination(vs.LocalAddr(), 0) != nil {
		if time.Now().After(deadline) {
			t.Fatal("relay links kept after the report")
		}
		time.Sleep(50 * time.Millisecond)
	}

	// A signalling socket on the old address: its handshake reaches the server.
	_ = vs.SetReadDeadline(time.Time{})
	tr := &quic.Transport{Conn: vs}
	t.Cleanup(func() { _ = tr.Close() })
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	conn, err := tr.Dial(ctx, ts.udp, &tls.Config{InsecureSkipVerify: true, NextProtos: []string{testALPN}},
		&quic.Config{HandshakeIdleTimeout: 4 * time.Second})
	if err != nil {
		t.Fatalf("signalling from a former relay address: %v", err)
	}
	_ = conn.CloseWithError(0, "")
}
