package relay

import (
	"errors"
	"net"
	"sync"
	"time"
)

// Conn è il "socket" condiviso da QUIC del server e relay. I datagrammi arrivano
// dalla porta UDP oppure, per le reti che bloccano UDP, da connessioni WebSocket
// (vedi WebSockets): ogni connessione WebSocket è un indirizzo virtuale come gli
// altri. I messaggi di controllo e i datagrammi di una sessione relay vengono
// gestiti qui; tutto il resto passa a quic-go attraverso ReadFrom.
//
// Implementa solo net.PacketConn, di proposito: se esponesse i metodi di
// *net.UDPConn (ReadMsgUDP, SyscallConn) quic-go li userebbe per leggere
// direttamente dal socket, scavalcando lo smistamento.
type Conn struct {
	udp    *net.UDPConn
	relay  *Relay
	impair *impairer // solo per i test (NEXO_RELAY_IMPAIR)
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

// Datagrammi in attesa di essere smistati: oltre, si scartano (come un socket pieno).
const inQueue = 4096

func NewConn(udp *net.UDPConn, r *Relay) *Conn {
	c := &Conn{udp: udp, relay: r, in: make(chan packet, inQueue), closed: make(chan struct{})}
	c.ws = newWebSockets(c)
	go c.readUDP()
	return c
}

// Impair applica la simulazione di rete al traffico inoltrato (solo test).
func (c *Conn) Impair(im *Impairment) {
	c.impair = newImpairer(im, func(b []byte, to net.Addr) { _, _ = c.WriteTo(b, to) })
}

// WebSockets restituisce il gestore delle connessioni WebSocket (da montare su HTTP).
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

// deliver accoda un datagramma ricevuto (da UDP o WebSocket).
func (c *Conn) deliver(data []byte, from net.Addr) {
	select {
	case c.in <- packet{data: data, from: from}:
	default: // coda piena: scartato
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
