package signal

import (
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/ext"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

// How long the agent may send the report of a session it accepted.
const reportWindow = 48 * time.Hour

// routed is a session the server routed: the agent's report is accepted for it.
type routed struct {
	agentID  string
	accepted time.Time
}

// event passes a step of a session to the modules that want it (session log).
func (s *Server) event(ev ext.SessionEvent) {
	ev.Time = time.Now()
	for _, m := range s.modules {
		if o, ok := m.(ext.SessionObserver); ok {
			o.SessionEvent(ev)
		}
	}
}

// report takes the end-of-session report of an agent, only for a session the
// server routed to that agent.
func (s *Server) report(agent *client, r *pb.SessionReport) {
	s.mu.Lock()
	rt := s.routed[r.GetSessionId()]
	ok := rt != nil && rt.agentID == agent.device.ID
	if ok {
		delete(s.routed, r.SessionId)
	}
	s.mu.Unlock()
	if !ok {
		s.log.Debug("session report not expected", "session", r.GetSessionId(), "from", agent.device.ID)
		return
	}
	s.log.Info("session ended", "session", r.SessionId, "duration_s", r.DurationS, "direct", r.Direct,
		"access", r.Access, "files_in", r.FilesIn, "files_out", r.FilesOut, "error", r.Error)
	s.event(ext.SessionEvent{ID: r.SessionId, Kind: ext.SessionEnded, AgentID: agent.device.ID, Report: r})
}

// forgetRouted drops the routed sessions whose report never came.
func (s *Server) forgetRouted(now time.Time) {
	s.mu.Lock()
	defer s.mu.Unlock()
	for id, rt := range s.routed {
		if now.Sub(rt.accepted) > reportWindow {
			delete(s.routed, id)
		}
	}
}
