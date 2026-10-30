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
	s.report(agent, &pb.SessionReport{SessionId: "abc", DurationS: 1})
	if len(o.events) != 1 {
		t.Fatal("second report of the same session accepted")
	}

	s.routed["old"] = &routed{agentID: agent.device.ID, accepted: time.Now().Add(-reportWindow - time.Minute)}
	s.forgetRouted(time.Now())
	if _, ok := s.routed["old"]; ok {
		t.Fatal("old routed session kept")
	}
}
