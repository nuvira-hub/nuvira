package relay

import (
	"errors"
	"net"
	"sync"
	"time"
)

// Conn is the "socket" shared by the server's QUIC and the relay. Datagrams arrive
// from the UDP port or, for networks that block UDP, from WebSocket connections
// (see WebSockets): every WebSocket connection is a virtual address like the
// others. Control messages and the datagrams of a relayed session are handled
// here; everything else goes to quic-go through ReadFrom.
//
// It implements only net.PacketConn, on purpose: if it exposed the methods of
// *net.UDPConn (ReadMsgUDP, SyscallConn) quic-go would use them to read straight
// from the socket, bypassing the dispatching.
type Conn struct {
	udp    *net.UDPConn
	relay  *Relay
	impair *impairer // tests only (NUVIRA_RELAY_IMPAIR)
	ws     *WebSockets

	in       chan packet
	closed   chan struct{}
	closeErr sync.Once
	deadline time.Time
	mu       sync.Mutex
}

type packet struct {
	data []byte
	from net.Addr
}

var _ net.PacketConn = (*Conn)(nil)

// Datagrams waiting to be dispatched: beyond this they are dropped (like a full socket).
const inQueue = 4096

func NewConn(udp *net.UDPConn, r *Relay) *Conn {
	c := &Conn{udp: udp, relay: r, in: make(chan packet, inQueue), closed: make(chan struct{})}
	c.ws = newWebSockets(c)
	go c.readUDP()
	return c
}

// Impair applies the network simulation to the forwarded traffic (tests only).
func (c *Conn) Impair(im *Impairment) {
	c.impair = newImpairer(im, func(b []byte, to net.Addr) { _, _ = c.WriteTo(b, to) })
}

// WebSockets returns the handler of the WebSocket connections (to mount on HTTP).
func (c *Conn) WebSockets() *WebSockets { return c.ws }

func (c *Conn) readUDP() {
	for {
		buf := make([]byte, 2048)
		n, from, err := c.udp.ReadFromUDP(buf)
		if err != nil {
			select {
			case <-c.closed:
				return
			default:
			}
			if errors.Is(err, net.ErrClosed) {
				return
			}
			continue
		}
		c.deliver(buf[:n], from)
	}
}

// deliver queues a received datagram (from UDP or WebSocket).
func (c *Conn) deliver(data []byte, from net.Addr) {
	select {
	case c.in <- packet{data: data, from: from}:
	default: // queue full: dropped
	}
}

func (c *Conn) ReadFrom(b []byte) (int, net.Addr, error) {
	for {
		var timeout <-chan time.Time
		c.mu.Lock()
		if !c.deadline.IsZero() {
			timeout = time.After(time.Until(c.deadline))
		}
		c.mu.Unlock()

		var p packet
		select {
		case p = <-c.in:
		case <-c.closed:
			return 0, nil, net.ErrClosed
		case <-timeout:
			return 0, nil, errTimeout{}
		}
		if IsControl(p.data) {
			if reply := c.relay.HandleControl(p.data, p.from); reply != nil {
				_, _ = c.WriteTo(reply, p.from)
			}
			continue
		}
		if to := c.relay.Destination(p.from, len(p.data)); to != nil {
			if c.impair != nil {
				c.impair.send(p.data, to)
			} else {
				_, _ = c.WriteTo(p.data, to)
			}
			continue
		}
		return copy(b, p.data), p.from, nil
	}
}

func (c *Conn) WriteTo(b []byte, addr net.Addr) (int, error) {
	if a, ok := addr.(wsAddr); ok {
		return c.ws.send(a, b)
	}
	return c.udp.WriteTo(b, addr)
}

func (c *Conn) Close() error {
	c.closeErr.Do(func() { close(c.closed) })
	c.ws.closeAll()
	return c.udp.Close()
}

func (c *Conn) LocalAddr() net.Addr { return c.udp.LocalAddr() }

func (c *Conn) SetDeadline(t time.Time) error { return c.SetReadDeadline(t) }

func (c *Conn) SetReadDeadline(t time.Time) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.deadline = t
	return nil
}

func (c *Conn) SetWriteDeadline(time.Time) error { return nil }

type errTimeout struct{}

func (errTimeout) Error() string   { return "timeout" }
func (errTimeout) Timeout() bool   { return true }
func (errTimeout) Temporary() bool { return true }
