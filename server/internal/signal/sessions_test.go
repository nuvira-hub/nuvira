package signal

import (
	"net/http"
	"testing"
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/ext"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
	"github.com/nuvira-hub/nuvira/server/internal/store"
)

type observer struct{ events []ext.SessionEvent }

func (o *observer) Features() []string                                          { return nil }
func (o *observer) Policy() (*pb.Policy, error)                                 { return nil, nil }
func (o *observer) Routes(*http.ServeMux)                                       {}
func (o *observer) Api(ext.Conn, *pb.ApiRequest, *pb.ApiResponse) (bool, error) { return false, nil }
func (o *observer) SessionEvent(ev ext.SessionEvent)                            { o.events = append(o.events, ev) }

func TestSessionReport(t *testing.T) {
	s := newTestServer(t)
	o := &observer{}
	s.SetModules([]ext.Module{o})
	agent := &client{device: &store.Device{ID: "111111111"}}
	other := &client{device: &store.Device{ID: "222222222"}}
	s.routed["abc"] = &routed{agentID: agent.device.ID, accepted: time.Now()}

	s.report(other, &pb.SessionReport{SessionId: "abc", DurationS: 5})
	if len(o.events) != 0 {
		t.Fatal("report of another device accepted")
	}
	s.report(agent, &pb.SessionReport{SessionId: "abc", DurationS: 42, FilesIn: 2})
	if len(o.events) != 1 || o.events[0].Kind != ext.SessionEnded || o.events[0].Report.DurationS != 42 || o.events[0].AgentID != agent.device.ID {
		t.Fatalf("events %+v", o.events)
	}
	// A second report: the server no longer has the session, the modules get it
	// marked (the session log keeps the first one).
	s.report(agent, &pb.SessionReport{SessionId: "abc", DurationS: 1})
	if len(o.events) != 2 || !o.events[1].Unrouted || o.events[0].Unrouted {
		t.Fatalf("second report: %+v", o.events)
	}

	gone := time.Now().Add(-time.Hour)
	s.routed["old"] = &routed{agentID: agent.device.ID, accepted: time.Now().Add(-reportWindow - time.Minute), gone: gone}
	s.forgetRouted(time.Now())
	if _, ok := s.routed["old"]; ok {
		t.Fatal("old routed session kept")
	}
	if len(o.events) != 3 || o.events[2].Kind != ext.SessionUnreported || !o.events[2].End.Equal(gone) || o.events[2].ID != "old" {
		t.Fatalf("report window passed: %+v", o.events)
	}
}

// The agent's connection drops during a session; what happens to its report
// depends on the process that comes back.
func TestSessionReportAfterReconnecting(t *testing.T) {
	s := newTestServer(t)
	o := &observer{}
	s.SetModules([]ext.Module{o})
	first := &client{device: &store.Device{ID: "111111111"}, instance: []byte("process-1")}
	s.setOnline(first)
	s.routed["s1"] = &routed{agentID: "111111111", accepted: time.Now(), instance: first.instance}

	// The same process again: the session goes on, no estimate; its late report
	// is taken as usual.
	s.setOffline(first)
	if s.routed["s1"].gone.IsZero() {
		t.Fatal("time the agent went away not noted")
	}
	again := &client{device: &store.Device{ID: "111111111"}, instance: []byte("process-1")}
	s.setOnline(again)
	if len(o.events) != 0 || !s.routed["s1"].gone.IsZero() {
		t.Fatalf("same process: %+v, gone %v", o.events, s.routed["s1"].gone)
	}
	s.report(again, &pb.SessionReport{SessionId: "s1", DurationS: 300})
	if len(o.events) != 1 || o.events[0].Kind != ext.SessionEnded || o.events[0].Unrouted {
		t.Fatalf("late report: %+v", o.events)
	}

	// A new process: the old one's session ended when it went away.
	s.routed["s2"] = &routed{agentID: "111111111", accepted: time.Now(), instance: again.instance}
	s.setOffline(again)
	wentAway := s.routed["s2"].gone
	restarted := &client{device: &store.Device{ID: "111111111"}, instance: []byte("process-2")}
	s.setOnline(restarted)
	if len(o.events) != 2 || o.events[1].Kind != ext.SessionUnreported || o.events[1].ID != "s2" || !o.events[1].End.Equal(wentAway) {
		t.Fatalf("new process: %+v", o.events)
	}
	// Estimated once only, and a report that comes after all replaces it.
	s.setOffline(restarted)
	third := &client{device: &store.Device{ID: "111111111"}, instance: []byte("process-3")}
	s.setOnline(third)
	if len(o.events) != 2 {
		t.Fatalf("estimated twice: %+v", o.events)
	}
	s.report(restarted, &pb.SessionReport{SessionId: "s2", DurationS: 10})
	if len(o.events) != 3 || o.events[2].Kind != ext.SessionEnded || o.events[2].Unrouted {
		t.Fatalf("report after the estimate: %+v", o.events)
	}
	s.forgetRouted(time.Now().Add(reportWindow + time.Hour))
	if len(o.events) != 3 {
		t.Fatalf("estimate after the report: %+v", o.events)
	}

	// An older agent (no instance): nothing estimated when it comes back.
	s.routed["s3"] = &routed{agentID: "111111111", accepted: time.Now()}
	s.setOffline(third)
	older := &client{device: &store.Device{ID: "111111111"}}
	s.setOnline(older)
	if len(o.events) != 3 {
		t.Fatalf("older agent: %+v", o.events)
	}
	// Another device's sessions are not touched.
	s.routed["s4"] = &routed{agentID: "222222222", accepted: time.Now(), instance: []byte("x")}
	s.setOffline(older)
	s.setOnline(&client{device: &store.Device{ID: "111111111"}, instance: []byte("process-4")})
	if s.routed["s4"].estimated || len(o.events) != 3 {
		t.Fatalf("another device's session: %+v", o.events)
	}
}
