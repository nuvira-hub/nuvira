package relay

import (
	"net"
	"time"
)

// Conn condivide una porta UDP tra il QUIC del server e il relay: i datagrammi
// di controllo e quelli che arrivano da un lato di una sessione attiva vengono
// gestiti qui; tutto il resto passa a quic-go attraverso ReadFrom.
//
// Implementa solo net.PacketConn, di proposito: se esponesse i metodi di
// *net.UDPConn (ReadMsgUDP, SyscallConn) quic-go li userebbe per leggere
// direttamente dal socket, scavalcando lo smistamento.
type Conn struct {
	udp    *net.UDPConn
	relay  *Relay
	impair *impairer // solo per i test (NEXO_RELAY_IMPAIR)
}

var _ net.PacketConn = (*Conn)(nil)

func NewConn(udp *net.UDPConn, r *Relay) *Conn {
	return &Conn{udp: udp, relay: r}
}

// Impair applica la simulazione di rete al traffico inoltrato (solo test).
func (c *Conn) Impair(im *Impairment) {
	c.impair = newImpairer(im, func(b []byte, to *net.UDPAddr) { _, _ = c.udp.WriteToUDP(b, to) })
}

func (c *Conn) ReadFrom(b []byte) (int, net.Addr, error) {
	for {
		n, from, err := c.udp.ReadFromUDP(b)
		if err != nil {
			return n, from, err
		}
		p := b[:n]
		if IsControl(p) {
			if reply := c.relay.HandleControl(p, from); reply != nil {
				_, _ = c.udp.WriteToUDP(reply, from)
			}
			continue
		}
		if to := c.relay.Destination(from, n); to != nil {
			if c.impair != nil {
				c.impair.send(p, to)
			} else {
				_, _ = c.udp.WriteToUDP(p, to)
			}
			continue
		}
		return n, from, nil
	}
}

func (c *Conn) WriteTo(b []byte, addr net.Addr) (int, error) { return c.udp.WriteTo(b, addr) }
func (c *Conn) Close() error                                 { return c.udp.Close() }
func (c *Conn) LocalAddr() net.Addr                          { return c.udp.LocalAddr() }
func (c *Conn) SetDeadline(t time.Time) error                { return c.udp.SetDeadline(t) }
func (c *Conn) SetReadDeadline(t time.Time) error            { return c.udp.SetReadDeadline(t) }
func (c *Conn) SetWriteDeadline(t time.Time) error           { return c.udp.SetWriteDeadline(t) }
