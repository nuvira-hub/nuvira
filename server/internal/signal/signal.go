// Package signal handles the devices' connections to the server: registration
// with proof of the key, presence, connection requests and delivery of the
// routes (relay) to the two sides of a session.
package signal

import (
	"bufio"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"sync"
	"time"

	"github.com/quic-go/quic-go"
	"google.golang.org/protobuf/encoding/protodelim"

	"github.com/nuvira-hub/nuvira/server/internal/ext"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
	"github.com/nuvira-hub/nuvira/server/internal/relay"
	"github.com/nuvira-hub/nuvira/server/internal/store"
)

// ProtocolVersion is the version of the signalling protocol spoken by the server.
const ProtocolVersion = 1

// Prefix signed together with the nonce: prevents reusing the signature elsewhere.
const registerContext = "nuvira-register-v1"

// Time given to the agent to accept a request.
const pendingTimeout = 60 * time.Second

const maxMessage = 64 * 1024

type client struct {
	device *store.Device
	conn   *quic.Conn
	mu     sync.Mutex // serialises writes on the stream and protects session
	stream *quic.Stream
	// Not reachable as a device (viewer, app using the address book): it is not
	// listed as online and does not disconnect other connections with the same key.
	apiOnly bool
	// Sign-in made on this connection (modules with accounts).
	session *pb.Session
	// Last GetPresence, to limit how often it is asked.
	lastPresence time.Time
}

func (c *client) send(msg *pb.ServerMessage) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	_, err := protodelim.MarshalTo(c.stream, msg)
	return err
}

type pending struct {
	id      string
	viewer  *client
	agentID string
	created time.Time
}

type Server struct {
	store   *store.Store
	modules []ext.Module
	relay   *relay.Relay
	log     *slog.Logger

	mu       sync.Mutex
	online   map[string]*client // by device ID
	sessions map[string]*pending
}

func New(st *store.Store, r *relay.Relay, log *slog.Logger) *Server {
	return &Server{
		store: st, relay: r, log: log,
		online: map[string]*client{}, sessions: map[string]*pending{},
	}
}

// SetModules sets the optional modules, before Serve.
func (s *Server) SetModules(modules []ext.Module) { s.modules = modules }

// Serve accepts connections until the listener is closed.
func (s *Server) Serve(ctx context.Context, ln *quic.Listener) error {
	go s.expireLoop(ctx)
	for {
		conn, err := ln.Accept(ctx)
		if err != nil {
			return err
		}
		go func() {
			if err := s.handle(ctx, conn); err != nil && !isClosed(err) {
				s.log.Debug("connection ended", "from", conn.RemoteAddr(), "err", err)
			}
		}()
	}
}

func isClosed(err error) bool {
	var appErr *quic.ApplicationError
	var idle *quic.IdleTimeoutError
	return errors.Is(err, io.EOF) || errors.As(err, &appErr) || errors.As(err, &idle)
}

func errorMsg(text, session string) *pb.ServerMessage {
	return &pb.ServerMessage{Msg: &pb.ServerMessage_Error{Error: &pb.Error{Message: text, SessionId: session}}}
}

func (s *Server) handle(ctx context.Context, conn *quic.Conn) error {
	defer conn.CloseWithError(0, "")
	stream, err := conn.AcceptStream(ctx)
	if err != nil {
		return err
	}
	r := bufio.NewReader(stream)
	read := func() (*pb.ClientMessage, error) {
		msg := &pb.ClientMessage{}
		err := protodelim.UnmarshalOptions{MaxSize: maxMessage}.UnmarshalFrom(r, msg)
		return msg, err
	}

	c, err := s.register(conn, stream, read)
	if err != nil {
		return fmt.Errorf("registration: %w", err)
	}
	s.setOnline(c)
	defer s.setOffline(c)
	s.log.Info("device connected", "id", c.device.ID, "name", c.device.Name, "from", conn.RemoteAddr())

	for {
		msg, err := read()
		if err != nil {
			s.log.Info("device disconnected", "id", c.device.ID)
			return err
		}
		switch m := msg.Msg.(type) {
		case *pb.ClientMessage_Connect:
			s.connect(c, m.Connect.DeviceId)
		case *pb.ClientMessage_Accept:
			s.accept(c, m.Accept.SessionId)
		case *pb.ClientMessage_Reject:
			s.reject(c, m.Reject.SessionId, m.Reject.Reason)
		case *pb.ClientMessage_Api:
			s.api(c, m.Api)
		default:
			_ = c.send(errorMsg("unexpected message", ""))
		}
	}
}

// register checks that the device holds the private key of the public key it
// presents and assigns (or finds) its ID.
func (s *Server) register(conn *quic.Conn, stream *quic.Stream, read func() (*pb.ClientMessage, error)) (*client, error) {
	msg, err := read()
	if err != nil {
		return nil, err
	}
	reg := msg.GetRegister()
	if reg == nil || len(reg.PublicKey) != ed25519.PublicKeySize {
		return nil, errors.New("missing Register or invalid key")
	}
	c := &client{conn: conn, stream: stream, apiOnly: reg.ApiOnly}
	if reg.ProtocolVersion < ProtocolVersion {
		_ = c.send(errorMsg("@server.old_version", ""))
		return nil, fmt.Errorf("protocol %d", reg.ProtocolVersion)
	}

	nonce := make([]byte, 32)
	if _, err := rand.Read(nonce); err != nil {
		return nil, err
	}
	if err := c.send(&pb.ServerMessage{Msg: &pb.ServerMessage_Challenge{Challenge: &pb.Challenge{Nonce: nonce}}}); err != nil {
		return nil, err
	}
	msg, err = read()
	if err != nil {
		return nil, err
	}
	proof := msg.GetProof()
	signed := append([]byte(registerContext), nonce...)
	if proof == nil || !ed25519.Verify(reg.PublicKey, signed, proof.Signature) {
		_ = c.send(errorMsg("invalid proof of the key", ""))
		return nil, errors.New("invalid signature")
	}

	device, err := s.store.Register(reg.PublicKey, limit(reg.DeviceName, 64), limit(reg.Os, 32))
	if err != nil {
		return nil, err
	}
	c.device = device
	registered := &pb.Registered{DeviceId: device.ID}
	for _, m := range s.modules {
		registered.Features = append(registered.Features, m.Features()...)
		if registered.Policy == nil {
			if registered.Policy, err = m.Policy(); err != nil {
				return nil, err
			}
		}
	}
	err = c.send(&pb.ServerMessage{Msg: &pb.ServerMessage_Registered{Registered: registered}})
	return c, err
}

func limit(s string, n int) string {
	if len(s) > n {
		return s[:n]
	}
	return s
}

func (s *Server) setOnline(c *client) {
	if c.apiOnly {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if old := s.online[c.device.ID]; old != nil {
		// Same device reconnected: the old connection is no longer needed.
		_ = old.conn.CloseWithError(0, "replaced by a new connection")
	}
	s.online[c.device.ID] = c
}

func (s *Server) setOffline(c *client) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.online[c.device.ID] == c {
		delete(s.online, c.device.ID)
	}
	_ = s.store.Touch(c.device.ID)
}

func randomHex(n int) string {
	b := make([]byte, n)
	_, _ = rand.Read(b)
	return hex.EncodeToString(b)
}

func (s *Server) connect(viewer *client, target string) {
	s.mu.Lock()
	agent := s.online[target]
	var p *pending
	if agent != nil && target != viewer.device.ID {
		p = &pending{id: randomHex(8), viewer: viewer, agentID: target, created: time.Now()}
		s.sessions[p.id] = p
	}
	s.mu.Unlock()

	if agent == nil {
		_ = viewer.send(errorMsg("@server.unreachable", ""))
		return
	}
	if p == nil {
		_ = viewer.send(errorMsg("@server.self_connect", ""))
		return
	}
	s.log.Info("connection request", "session", p.id, "from", viewer.device.ID, "to", target)
	err := agent.send(&pb.ServerMessage{Msg: &pb.ServerMessage_Incoming{Incoming: &pb.Incoming{
		SessionId:       p.id,
		ViewerId:        viewer.device.ID,
		ViewerName:      viewer.device.Name,
		ViewerPublicKey: viewer.device.PublicKey,
	}}})
	if err != nil {
		s.dropSession(p.id)
		_ = viewer.send(errorMsg("@server.unreachable", p.id))
	}
}

// take removes the session from the pending requests, if it belongs to this agent.
func (s *Server) take(agent *client, id string) *pending {
	s.mu.Lock()
	defer s.mu.Unlock()
	p := s.sessions[id]
	if p == nil || p.agentID != agent.device.ID {
		return nil
	}
	delete(s.sessions, id)
	return p
}

func (s *Server) dropSession(id string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.sessions, id)
}

func (s *Server) accept(agent *client, id string) {
	p := s.take(agent, id)
	if p == nil {
		_ = agent.send(errorMsg("unknown or expired session", id))
		return
	}
	var token [16]byte
	_, _ = rand.Read(token[:])
	s.relay.Allow(token)

	route := func(role pb.Role, peer *store.Device) *pb.ServerMessage {
		return &pb.ServerMessage{Msg: &pb.ServerMessage_Route{Route: &pb.Route{
			SessionId:     id,
			Role:          role,
			PeerPublicKey: peer.PublicKey,
			PeerId:        peer.ID,
			RelayToken:    token[:],
		}}}
	}
	if err := agent.send(route(pb.Role_ROLE_AGENT, p.viewer.device)); err != nil {
		_ = p.viewer.send(errorMsg("@server.peer_left", id))
		return
	}
	if err := p.viewer.send(route(pb.Role_ROLE_VIEWER, agent.device)); err != nil {
		s.log.Debug("viewer not reachable", "session", id, "err", err)
	}
	s.log.Info("session accepted", "session", id)
}

func (s *Server) reject(agent *client, id, reason string) {
	p := s.take(agent, id)
	if p == nil {
		return
	}
	if reason == "" {
		reason = "@agent.rejected"
	}
	_ = p.viewer.send(errorMsg(reason, id))
	s.log.Info("session rejected", "session", id, "reason", reason)
}

func (s *Server) expireLoop(ctx context.Context) {
	t := time.NewTicker(10 * time.Second)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case now := <-t.C:
			var expired []*pending
			s.mu.Lock()
			for id, p := range s.sessions {
				if now.Sub(p.created) > pendingTimeout {
					expired = append(expired, p)
					delete(s.sessions, id)
				}
			}
			s.mu.Unlock()
			for _, p := range expired {
				_ = p.viewer.send(errorMsg("@agent.no_answer", p.id))
			}
		}
	}
}
