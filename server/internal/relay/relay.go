// Package relay inoltra i datagrammi UDP tra i due lati di una sessione,
// come un server TURN minimale: il contenuto è QUIC già cifrato end-to-end
// e il relay non può leggerlo.
//
// Formato dei messaggi di controllo (il primo byte 0x00 non è mai l'inizio di
// un pacchetto QUIC, che ha sempre il bit 0x40 impostato):
//
//	bind:  00 'N' 'X' 'R' 01 | gettone (16 byte) | ruolo (1 byte)
//	       | n (1 byte) | n indirizzi locali del mittente
//	bound: 00 'N' 'X' 'R' 02 | gettone (16 byte) | pronto (1 byte: 1 se anche l'altro lato è collegato)
//	       [se pronto: indirizzo dell'altro lato come lo vede il relay
//	        | n (1 byte) | n indirizzi locali dell'altro lato]
//
// Indirizzo: famiglia (4 o 6) | IP (4 o 16 byte) | porta (2 byte, big endian).
//
// Gli indirizzi dell'altro lato servono a tentare la connessione diretta:
// quello visto dal relay attraverso i NAT (hole punching) e quelli locali,
// che funzionano quando i due dispositivi sono nella stessa rete dietro lo
// stesso router (dove l'indirizzo pubblico spesso non è raggiungibile: hairpin).
package relay

import (
	"bytes"
	"log/slog"
	"net"
	"sync"
	"time"
)

var magic = []byte{0x00, 'N', 'X', 'R'}

const (
	typeBind  = 0x01
	typeBound = 0x02
	tokenLen  = 16
	// Indirizzi locali accettati per lato (limitano la dimensione delle risposte).
	maxLocals = 4

	// Sessione dimenticata dopo questo tempo senza traffico.
	idleTimeout = 2 * time.Minute
	// Tempo massimo per collegare entrambi i lati dopo la creazione.
	bindTimeout = 60 * time.Second
)

type session struct {
	token   [tokenLen]byte
	peers   [2]*net.UDPAddr // [0] viewer, [1] agent
	locals  [2][]*net.UDPAddr
	created time.Time
	last    time.Time
	bytes   uint64
}

func (s *session) ready() bool { return s.peers[0] != nil && s.peers[1] != nil }

// Relay tiene le sessioni e la tabella indirizzo → indirizzo dell'altro lato.
type Relay struct {
	// Solo test: falsifica l'indirizzo visto dal relay (vedi reflexive).
	FakeReflexive bool

	mu       sync.Mutex
	sessions map[[tokenLen]byte]*session
	links    map[string]*link
	log      *slog.Logger
}

type link struct {
	to      *net.UDPAddr
	session *session
}

func New(log *slog.Logger) *Relay {
	r := &Relay{sessions: map[[tokenLen]byte]*session{}, links: map[string]*link{}, log: log}
	go r.expireLoop()
	return r
}

// Allow prepara una sessione: solo chi conosce il gettone potrà collegarsi.
func (r *Relay) Allow(token [tokenLen]byte) {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now()
	r.sessions[token] = &session{token: token, created: now, last: now}
}

// IsControl dice se il datagramma è un messaggio di controllo del relay.
func IsControl(p []byte) bool { return len(p) >= 5 && bytes.Equal(p[:4], magic) }

// HandleControl gestisce un bind e restituisce la risposta da inviare (o nil).
func (r *Relay) HandleControl(p []byte, from *net.UDPAddr) []byte {
	if len(p) < 5+tokenLen+1 || p[4] != typeBind {
		return nil
	}
	var token [tokenLen]byte
	copy(token[:], p[5:5+tokenLen])
	role := int(p[5+tokenLen])
	if role != 1 && role != 2 {
		return nil
	}
	locals, ok := parseAddrs(p[5+tokenLen+1:])
	if !ok {
		return nil
	}

	r.mu.Lock()
	defer r.mu.Unlock()
	s := r.sessions[token]
	if s == nil {
		return nil
	}
	i := role - 1
	if old := s.peers[i]; old == nil || old.String() != from.String() {
		// Primo collegamento o cambio di indirizzo (NAT): si aggiorna la tabella.
		if old != nil {
			delete(r.links, old.String())
		}
		s.peers[i] = from
		if s.ready() {
			r.links[s.peers[0].String()] = &link{to: s.peers[1], session: s}
			r.links[s.peers[1].String()] = &link{to: s.peers[0], session: s}
			r.log.Info("relay attivo", "viewer", s.peers[0], "agent", s.peers[1])
		}
	}
	s.locals[i] = locals
	s.last = time.Now()

	reply := make([]byte, 0, 5+tokenLen+1)
	reply = append(reply, magic...)
	reply = append(reply, typeBound)
	reply = append(reply, token[:]...)
	if !s.ready() {
		return append(reply, 0)
	}
	reply = append(reply, 1)
	reply = appendAddr(reply, r.reflexive(s.peers[1-i]))
	reply = append(reply, byte(len(s.locals[1-i])))
	for _, a := range s.locals[1-i] {
		reply = appendAddr(reply, a)
	}
	return reply
}

// reflexive è l'indirizzo dell'altro lato come lo vede il relay. Con
// FakeReflexive (solo test) è sostituito da uno irraggiungibile: così si
// simula in LAN il caso hairpin, dove riesce solo l'indirizzo locale.
func (r *Relay) reflexive(a *net.UDPAddr) *net.UDPAddr {
	if r.FakeReflexive {
		return &net.UDPAddr{IP: net.IPv4(203, 0, 113, 1), Port: a.Port} // TEST-NET-3, mai instradato
	}
	return a
}

// parseAddrs legge "n | n indirizzi"; un elenco assente vale vuoto.
func parseAddrs(b []byte) ([]*net.UDPAddr, bool) {
	if len(b) == 0 {
		return nil, true
	}
	n := int(b[0])
	if n > maxLocals {
		return nil, false
	}
	b = b[1:]
	addrs := make([]*net.UDPAddr, 0, n)
	for range n {
		var ipLen int
		switch {
		case len(b) > 0 && b[0] == 4:
			ipLen = 4
		case len(b) > 0 && b[0] == 6:
			ipLen = 16
		default:
			return nil, false
		}
		if len(b) < 1+ipLen+2 {
			return nil, false
		}
		ip := net.IP(append([]byte(nil), b[1:1+ipLen]...))
		port := int(b[1+ipLen])<<8 | int(b[2+ipLen])
		addrs = append(addrs, &net.UDPAddr{IP: ip, Port: port})
		b = b[3+ipLen:]
	}
	return addrs, len(b) == 0
}

func appendAddr(b []byte, a *net.UDPAddr) []byte {
	if ip4 := a.IP.To4(); ip4 != nil {
		b = append(b, 4)
		b = append(b, ip4...)
	} else {
		b = append(b, 6)
		b = append(b, a.IP.To16()...)
	}
	return append(b, byte(a.Port>>8), byte(a.Port))
}

// Destination restituisce dove inoltrare un datagramma ricevuto da `from`,
// o nil se `from` non appartiene a nessuna sessione attiva.
func (r *Relay) Destination(from *net.UDPAddr, n int) *net.UDPAddr {
	r.mu.Lock()
	defer r.mu.Unlock()
	l := r.links[from.String()]
	if l == nil {
		return nil
	}
	l.session.last = time.Now()
	l.session.bytes += uint64(n)
	return l.to
}

func (r *Relay) expireLoop() {
	for range time.Tick(15 * time.Second) {
		r.mu.Lock()
		now := time.Now()
		for token, s := range r.sessions {
			idle := now.Sub(s.last) > idleTimeout
			unbound := !s.ready() && now.Sub(s.created) > bindTimeout
			if !idle && !unbound {
				continue
			}
			for _, p := range s.peers {
				if p != nil {
					delete(r.links, p.String())
				}
			}
			delete(r.sessions, token)
			r.log.Info("sessione relay chiusa", "byte", s.bytes, "durata", now.Sub(s.created).Round(time.Second))
		}
		r.mu.Unlock()
	}
}
