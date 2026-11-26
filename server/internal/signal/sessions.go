package signal

import (
	"bytes"
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
	// The session's relay token: the report ends the relay session (SRV-13).
	relayToken [16]byte
	// Register.instance of the agent's process that accepted (empty: older agent).
	instance []byte
	// Since when that process is not connected to the server (zero: it is): the
	// estimated end if its report never comes.
	gone time.Time
	// The end was estimated (SessionUnreported): the report is still taken if it
	// comes and replaces the estimate.
	estimated bool
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
// server routed to that agent. A report for a session the server does not have
// (it restarted after routing it) goes to the modules marked Unrouted: the
// session log takes it only for a session of that agent still without a report.
func (s *Server) report(agent *client, r *pb.SessionReport) {
	s.mu.Lock()
	rt := s.routed[r.GetSessionId()]
	ok := rt != nil && rt.agentID == agent.device.ID
	if ok {
		delete(s.routed, r.SessionId)
	}
	s.mu.Unlock()
	if !ok {
		if rt != nil || r.GetSessionId() == "" {
			s.log.Debug("session report not expected", "session", r.GetSessionId(), "from", agent.device.ID)
			return
		}
		s.log.Info("session report of a session not routed since the start", "session", r.SessionId, "from", agent.device.ID)
		s.event(ext.SessionEvent{ID: r.SessionId, Kind: ext.SessionEnded, AgentID: agent.device.ID, Report: r, Unrouted: true})
		return
	}
	// The session is over: its relay links go (after a short grace) instead of
	// staying for the idle timeout, where they would catch the datagrams of a
	// new socket that gets one of the old addresses (K39).
	if s.relay != nil {
		s.relay.Forget(rt.relayToken)
	}
	s.log.Info("session ended", "session", r.SessionId, "duration_s", r.DurationS, "direct", r.Direct,
		"access", r.Access, "files_in", r.FilesIn, "files_out", r.FilesOut, "error", r.Error,
		"after_estimate", rt.estimated)
	s.event(ext.SessionEvent{ID: r.SessionId, Kind: ext.SessionEnded, AgentID: agent.device.ID, Report: r})
}

// agentGone notes when the agent's connection dropped, for the sessions it
// accepted and has not reported yet. Called with s.mu held.
func (s *Server) agentGone(agentID string, now time.Time) {
	for _, rt := range s.routed {
		if rt.agentID == agentID && rt.gone.IsZero() {
			rt.gone = now
		}
	}
}

// agentBack runs when an agent registers again (s.mu held). The same process
// still has its sessions, and sends their reports when they end. A new process
// means the old one stopped (closed, crashed, the PC restarted) and its sessions
// with it: they end now, estimated at the time the old process went away, or now
// if the server had not noticed that yet (the old connection had not timed out):
// an upper bound. With
// an older agent (no instance) nothing is known: the report window decides.
func (s *Server) agentBack(c *client, now time.Time) []ext.SessionEvent {
	var ended []ext.SessionEvent
	for id, rt := range s.routed {
		if rt.agentID != c.device.ID || rt.estimated {
			continue
		}
		if len(rt.instance) == 0 || len(c.instance) == 0 || bytes.Equal(rt.instance, c.instance) {
			rt.gone = time.Time{}
			continue
		}
		end := rt.gone
		if end.IsZero() {
			end = now
		}
		rt.estimated = true
		ended = append(ended, ext.SessionEvent{ID: id, Kind: ext.SessionUnreported, AgentID: rt.agentID, End: end})
	}
	return ended
}

// forgetRouted drops the routed sessions whose report never came; those not
// estimated yet end estimated at the time their agent went away, if it did.
func (s *Server) forgetRouted(now time.Time) {
	var ended []ext.SessionEvent
	s.mu.Lock()
	for id, rt := range s.routed {
		if now.Sub(rt.accepted) > reportWindow {
			delete(s.routed, id)
			if !rt.estimated {
				ended = append(ended, ext.SessionEvent{ID: id, Kind: ext.SessionUnreported, AgentID: rt.agentID, End: rt.gone})
			}
		}
	}
	s.mu.Unlock()
	for _, ev := range ended {
		s.log.Info("session ended without report: report window passed", "session", ev.ID, "agent_away_since", ev.End)
		s.event(ev)
	}
}
