package signal

import (
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

func TestLimitCutsOnCharacterBoundary(t *testing.T) {
	for _, tc := range []struct {
		in   string
		n    int
		want string
	}{
		{"", 64, ""},
		{"short", 64, "short"},
		{strings.Repeat("a", 70), 64, strings.Repeat("a", 64)},
		{strings.Repeat("a", 64), 64, strings.Repeat("a", 64)},
		{strings.Repeat("è", 40), 64, strings.Repeat("è", 32)},             // 2 bytes, cut on a boundary
		{strings.Repeat("é", 33) + "x", 64, strings.Repeat("é", 32)},       // 2 bytes, 66 bytes
		{"a" + strings.Repeat("é", 40), 64, "a" + strings.Repeat("é", 31)}, // cut inside the 32nd é
		{strings.Repeat("日", 22), 64, strings.Repeat("日", 21)},             // 3 bytes, 66 bytes
		{strings.Repeat("😀", 17), 64, strings.Repeat("😀", 16)},             // 4 bytes, 68 bytes
		{"ab" + strings.Repeat("😀", 16), 64, "ab" + strings.Repeat("😀", 15)},
		{"日本", 2, ""},
	} {
		got := limit(tc.in, tc.n)
		if got != tc.want || !utf8.ValidString(got) || len(got) > tc.n {
			t.Errorf("limit(%q, %d) = %q, want %q", tc.in, tc.n, got, tc.want)
		}
	}
}

// A viewer whose name is cut at 64 bytes inside a character could not connect
// to anybody: the Incoming with its name failed to marshal and it got
// "@server.unreachable" (SRV-4, reproduced in Phase 3/4 with 22 x "日").
func TestLongNonASCIINameCanConnect(t *testing.T) {
	ts := startServer(t)
	agent := ts.device(t, "agent")
	ts.waitOnline(t, agent.id, true)
	for _, name := range []string{strings.Repeat("日", 22), strings.Repeat("😀", 17), "a" + strings.Repeat("é", 40), strings.Repeat("a", 70)} {
		v := ts.dial(t)
		if err := v.register(nil, name, true); err != nil {
			t.Fatal(err)
		}
		_ = v.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Connect{Connect: &pb.Connect{DeviceId: agent.id}}})
		in, err := agent.readUntil(5*time.Second, func(m *pb.ServerMessage) bool { return m.GetIncoming() != nil })
		if err != nil {
			t.Fatalf("name of %d bytes: no Incoming: %v", len(name), err)
		}
		if got := in.GetIncoming().ViewerName; got != limit(name, 64) {
			t.Fatalf("viewer name %q", got)
		}
		_ = agent.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Reject{Reject: &pb.Reject{SessionId: in.GetIncoming().SessionId}}})
		if m, err := v.readUntil(5*time.Second, func(m *pb.ServerMessage) bool { return m.GetError() != nil }); err != nil || m.GetError().Message != "@agent.rejected" {
			t.Fatalf("viewer: %v %v", m, err)
		}
	}
	if agent.conn.Context().Err() != nil {
		t.Fatal("agent disconnected")
	}
}
