package signal

import (
	"errors"
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/ext"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

// Limits of GetPresence, which needs no sign-in.
const (
	maxPresenceIDs   = 256
	presenceInterval = 2 * time.Second
)

// api answers a request of the app. The open server only answers GetPresence;
// the rest belongs to the modules (address book, accounts, rules).
func (s *Server) api(c *client, req *pb.ApiRequest) {
	resp := &pb.ApiResponse{Id: req.Id}
	if err := s.apiCall(c, req, resp); err != nil {
		if !errors.Is(err, ext.ErrDenied) && !errors.Is(err, ext.ErrNotAvailable) {
			s.log.Warn("app request failed", "device", c.device.ID, "err", err)
		}
		resp.Result = nil
		resp.Error = err.Error()
	}
	_ = c.send(&pb.ServerMessage{Msg: &pb.ServerMessage_Api{Api: resp}})
}

func (s *Server) apiCall(c *client, req *pb.ApiRequest, resp *pb.ApiResponse) error {
	if call, ok := req.Call.(*pb.ApiRequest_GetPresence); ok {
		return s.getPresence(c, call.GetPresence, resp)
	}
	for _, m := range s.modules {
		if handled, err := m.Api(c, req, resp); handled {
			return err
		}
	}
	return ext.ErrNotAvailable
}

// getPresence tells which of the devices are connected, for the address books
// kept in the apps.
func (s *Server) getPresence(c *client, req *pb.GetPresence, resp *pb.ApiResponse) error {
	if len(req.GetDeviceIds()) > maxPresenceIDs {
		return errors.New("too many devices")
	}
	c.mu.Lock()
	early := time.Since(c.lastPresence) < presenceInterval
	if !early {
		c.lastPresence = time.Now()
	}
	c.mu.Unlock()
	if early {
		return errors.New("too many requests")
	}
	s.mu.Lock()
	list := make([]*pb.Presence, 0, len(req.GetDeviceIds()))
	for _, id := range req.GetDeviceIds() {
		list = append(list, &pb.Presence{DeviceId: id, Online: s.online[id] != nil})
	}
	s.mu.Unlock()
	for _, p := range list {
		if d, err := s.store.ByID(p.DeviceId); err == nil && d != nil {
			p.LastSeen = d.LastSeen.Unix()
		}
	}
	resp.Result = &pb.ApiResponse_Presence{Presence: &pb.PresenceList{Devices: list}}
	return nil
}

// DeviceID, RemoteAddr, Session and SetSession make the connection an ext.Conn.
func (c *client) DeviceID() string   { return c.device.ID }
func (c *client) RemoteAddr() string { return c.conn.RemoteAddr().String() }

func (c *client) Session() *pb.Session {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.session
}

func (c *client) SetSession(session *pb.Session) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.session = session
}

// Presence fills in whether each device of the books is connected now, or when it
// was last seen.
func (s *Server) Presence(books *pb.Books) {
	s.mu.Lock()
	online := make(map[string]bool, len(s.online))
	for id := range s.online {
		online[id] = true
	}
	s.mu.Unlock()
	for _, book := range books.GetBooks() {
		for _, e := range book.GetEntries() {
			e.Online = online[e.GetDeviceId()]
			if d, err := s.store.ByID(e.GetDeviceId()); err == nil && d != nil {
				e.LastSeen = d.LastSeen.Unix()
			}
		}
	}
}

// BroadcastPolicy sends the new rules to every device connected.
func (s *Server) BroadcastPolicy(policy *pb.Policy) {
	s.mu.Lock()
	devices := make([]*client, 0, len(s.online))
	for _, c := range s.online {
		devices = append(devices, c)
	}
	s.mu.Unlock()
	for _, c := range devices {
		_ = c.send(&pb.ServerMessage{Msg: &pb.ServerMessage_Policy{Policy: policy}})
	}
}
