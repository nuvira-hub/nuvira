package relay

import (
	"context"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"sync"
	"sync/atomic"
	"time"

	"github.com/coder/websocket"
)

// WebSockets carries QUIC datagrams inside WebSocket connections, for devices
// on networks that block UDP. Every binary message is a datagram; every
// connection is a virtual address (wsAddr) that the server's QUIC and the relay
// treat like any UDP address. The content stays encrypted QUIC.
type WebSockets struct {
	conn *Conn
	log  *slog.Logger

	next  atomic.Uint64
	mu    sync.Mutex
	peers map[wsAddr]*wsPeer
}

type wsPeer struct {
	out    chan []byte
	cancel context.CancelFunc
}

// wsAddr is the virtual address of a WebSocket connection.
type wsAddr string

func (a wsAddr) Network() string { return "ws" }
func (a wsAddr) String() string  { return string(a) }

// Outgoing datagrams queued per connection: beyond this they are dropped (QUIC retransmits).
const wsOutQueue = 1024

// Maximum size of a datagram (QUIC uses at most ~1500 bytes).
const wsMaxMessage = 2048

func newWebSockets(c *Conn) *WebSockets {
	return &WebSockets{conn: c, log: slog.Default(), peers: map[wsAddr]*wsPeer{}}
}

// SetLogger sets the logger used for the connections.
func (w *WebSockets) SetLogger(log *slog.Logger) { w.log = log }

// ServeHTTP accepts a WebSocket connection and connects it to the dispatching.
func (w *WebSockets) ServeHTTP(rw http.ResponseWriter, req *http.Request) {
	ws, err := websocket.Accept(rw, req, &websocket.AcceptOptions{InsecureSkipVerify: true})
	if err != nil {
		return
	}
	ws.SetReadLimit(wsMaxMessage)
	addr := wsAddr(fmt.Sprintf("ws:%d@%s", w.next.Add(1), remoteHost(req)))
	ctx, cancel := context.WithCancel(req.Context())
	peer := &wsPeer{out: make(chan []byte, wsOutQueue), cancel: cancel}
	w.mu.Lock()
	w.peers[addr] = peer
	w.mu.Unlock()
	w.log.Debug("WebSocket connection", "from", addr)
	defer func() {
		cancel()
		w.mu.Lock()
		delete(w.peers, addr)
		w.mu.Unlock()
		_ = ws.CloseNow()
		w.log.Debug("WebSocket closed", "from", addr)
	}()

	go func() {
		for {
			select {
			case <-ctx.Done():
				return
			case data := <-peer.out:
				wctx, wcancel := context.WithTimeout(ctx, 10*time.Second)
				err := ws.Write(wctx, websocket.MessageBinary, data)
				wcancel()
				if err != nil {
					cancel()
					return
				}
			}
		}
	}()

	for {
		typ, data, err := ws.Read(ctx)
		if err != nil {
			return
		}
		if typ == websocket.MessageBinary && len(data) > 0 {
			w.conn.deliver(data, addr)
		}
	}
}

// remoteHost is the client's address (behind a reverse proxy, the one the proxy reports).
func remoteHost(req *http.Request) string {
	if fwd := req.Header.Get("X-Forwarded-For"); fwd != "" {
		return fwd
	}
	host, _, err := net.SplitHostPort(req.RemoteAddr)
	if err != nil {
		return req.RemoteAddr
	}
	return host
}

func (w *WebSockets) send(addr wsAddr, b []byte) (int, error) {
	w.mu.Lock()
	peer := w.peers[addr]
	w.mu.Unlock()
	if peer == nil {
		return 0, net.ErrClosed
	}
	select {
	case peer.out <- append([]byte(nil), b...):
	default: // queue full: dropped
	}
	return len(b), nil
}

func (w *WebSockets) closeAll() {
	w.mu.Lock()
	defer w.mu.Unlock()
	for _, p := range w.peers {
		p.cancel()
	}
}
