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

// WebSockets porta i datagrammi QUIC dentro connessioni WebSocket, per i
// dispositivi su reti che bloccano UDP. Ogni messaggio binario è un datagramma;
// ogni connessione è un indirizzo virtuale (wsAddr) che QUIC del server e relay
// trattano come un indirizzo UDP qualsiasi. Il contenuto resta QUIC cifrato.
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

// wsAddr è l'indirizzo virtuale di una connessione WebSocket.
type wsAddr string

func (a wsAddr) Network() string { return "ws" }
func (a wsAddr) String() string  { return string(a) }

// Datagrammi in uscita in coda per connessione: oltre, si scartano (QUIC ritrasmette).
const wsOutQueue = 1024

// Dimensione massima di un datagramma (QUIC usa al più ~1500 byte).
const wsMaxMessage = 2048

func newWebSockets(c *Conn) *WebSockets {
	return &WebSockets{conn: c, log: slog.Default(), peers: map[wsAddr]*wsPeer{}}
}

// SetLogger imposta il logger usato per le connessioni.
func (w *WebSockets) SetLogger(log *slog.Logger) { w.log = log }

// ServeHTTP accetta una connessione WebSocket e la collega allo smistamento.
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
	w.log.Debug("connessione WebSocket", "da", addr)
	defer func() {
		cancel()
		w.mu.Lock()
		delete(w.peers, addr)
		w.mu.Unlock()
		_ = ws.CloseNow()
		w.log.Debug("WebSocket chiuso", "da", addr)
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

// remoteHost è l'indirizzo del client (dietro un reverse proxy, quello che il proxy indica).
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
	default: // coda piena: scartato
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
