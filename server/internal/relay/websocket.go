package relay

import (
	"context"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"net/netip"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/coder/websocket"

	"github.com/nuvira-hub/nuvira/server/internal/limits"
)

// WebSockets carries QUIC datagrams inside WebSocket connections, for devices
// on networks that block UDP. Every binary message is a datagram; every
// connection is a virtual address (wsAddr) that the server's QUIC and the relay
// treat like any UDP address. The content stays encrypted QUIC.
type WebSockets struct {
	conn *Conn
	log  *slog.Logger

	// Limits per address (NUVIRA_RATE_LIMITS): connections at once and
	// datagrams per second of each connection. Set before serving.
	mode    limits.Mode
	open    *limits.Counter
	perSec  float64
	maxConn int

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

// Maximum size of a datagram: the largest UDP payload, as over UDP (QUIC uses
// at most ~1500 bytes today). A larger message ends the connection.
const wsMaxMessage = maxDatagram

// WebSocket connections open at once per address: each browser tab of the web
// client keeps one for the server and one per session, so an office of
// technicians behind one address stays far below it.
const wsMaxPerAddress = 64

// Datagrams per second (and burst) one WebSocket connection may deliver: far
// above a video session (about 2,000/s at 20 Mbit/s, keyframes in bursts of a
// few hundred), but one connection can no longer fill the queue shared with
// the UDP port (inQueue) and make every session drop.
const wsDatagramsPerSecond = 10000

func newWebSockets(c *Conn) *WebSockets {
	w := &WebSockets{conn: c, log: slog.Default(), peers: map[wsAddr]*wsPeer{}}
	w.SetLimitMode(limits.Public)
	return w
}

// SetLimitMode sets to which client addresses the limits apply, before serving.
func (w *WebSockets) SetLimitMode(mode limits.Mode) {
	w.setLimits(mode, wsMaxPerAddress, wsDatagramsPerSecond)
}

func (w *WebSockets) setLimits(mode limits.Mode, maxConn int, perSec float64) {
	w.mode, w.maxConn, w.perSec = mode, maxConn, perSec
	w.open = nil
	if mode != limits.Off {
		w.open = limits.NewCounter(mode, maxConn)
	}
}

// wsBucket is the token bucket of one connection's datagrams.
type wsBucket struct {
	rate, tokens float64
	last         time.Time
}

func (b *wsBucket) take(now time.Time) bool {
	b.tokens = min(b.rate, b.tokens+now.Sub(b.last).Seconds()*b.rate)
	b.last = now
	if b.tokens < 1 {
		return false
	}
	b.tokens--
	return true
}

// SetLogger sets the logger used for the connections.
func (w *WebSockets) SetLogger(log *slog.Logger) { w.log = log }

// ServeHTTP accepts a WebSocket connection and connects it to the dispatching.
func (w *WebSockets) ServeHTTP(rw http.ResponseWriter, req *http.Request) {
	host := remoteHost(req)
	var bucket *wsBucket
	if ip, err := netip.ParseAddr(host); err == nil {
		release, ok := w.open.Add(ip)
		if !ok {
			w.log.Debug("WebSocket refused: too many connections from this address", "from", host)
			http.Error(rw, "too many connections", http.StatusTooManyRequests)
			return
		}
		defer release()
		if w.mode.Applies(ip) {
			bucket = &wsBucket{rate: w.perSec, tokens: w.perSec, last: time.Now()}
		}
	}
	// Any Origin: the tunnel carries only end-to-end encrypted QUIC with
	// authentication by key and no cookies, so a page of another site gains no
	// authority through a visitor's browser (it could send datagrams, like any
	// UDP sender); and the web client may be hosted on another site.
	ws, err := websocket.Accept(rw, req, &websocket.AcceptOptions{InsecureSkipVerify: true})
	if err != nil {
		return
	}
	ws.SetReadLimit(wsMaxMessage)
	addr := wsAddr(fmt.Sprintf("ws:%d@%s", w.next.Add(1), host))
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
			if bucket != nil && !bucket.take(time.Now()) {
				continue // over the rate of a connection: dropped (QUIC retransmits)
			}
			w.conn.deliver(data, addr)
		}
	}
}

// remoteHost is the client's address. Behind a reverse proxy (a connection from
// this machine or the local network) it is the one the proxy adds last to
// X-Forwarded-For; from the Internet the header is ignored, since anybody can
// write it (it would dodge blocks and limits).
func remoteHost(req *http.Request) string {
	host, _, err := net.SplitHostPort(req.RemoteAddr)
	if err != nil {
		host = req.RemoteAddr
	}
	peer, err := netip.ParseAddr(host)
	if err != nil || !(peer.IsLoopback() || peer.IsPrivate()) {
		return host
	}
	if fwd := req.Header.Values("X-Forwarded-For"); len(fwd) > 0 {
		list := strings.Split(fwd[len(fwd)-1], ",")
		if last, err := netip.ParseAddr(strings.TrimSpace(list[len(list)-1])); err == nil {
			return last.String()
		}
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
