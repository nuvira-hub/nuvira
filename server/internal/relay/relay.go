// Package relay inoltra i datagrammi UDP tra i due lati di una sessione,
// come un server TURN minimale: il contenuto è QUIC già cifrato end-to-end
// e il relay non può leggerlo.
//
// Formato dei messaggi di controllo (il primo byte 0x00 non è mai l'inizio di
// un pacchetto QUIC, che ha sempre il bit 0x40 impostato):
//
//	bind:  00 'N' 'X' 'R' 01 | gettone (16 byte) | ruolo (1 byte)
//	bound: 00 'N' 'X' 'R' 02 | gettone (16 byte) | pronto (1 byte: 1 se anche l'altro lato è collegato)
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

	// Sessione dimenticata dopo questo tempo senza traffico.
	idleTimeout = 2 * time.Minute
	// Tempo massimo per collegare entrambi i lati dopo la creazione.
	bindTimeout = 60 * time.Second
)

type session struct {
	token   [tokenLen]byte
	peers   [2]*net.UDPAddr // [0] viewer, [1] agent
	created time.Time
	last    time.Time
	bytes   uint64
}

func (s *session) ready() bool { return s.peers[0] != nil && s.peers[1] != nil }

// Relay tiene le sessioni e la tabella indirizzo → indirizzo dell'altro lato.
type Relay struct {
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
	if len(p) != 5+tokenLen+1 || p[4] != typeBind {
		return nil
	}
	var token [tokenLen]byte
	copy(token[:], p[5:5+tokenLen])
	role := int(p[5+tokenLen])
	if role != 1 && role != 2 {
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
	s.last = time.Now()

	reply := make([]byte, 0, 5+tokenLen+1)
	reply = append(reply, magic...)
	reply = append(reply, typeBound)
	reply = append(reply, token[:]...)
	if s.ready() {
		reply = append(reply, 1)
	} else {
		reply = append(reply, 0)
	}
	return reply
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
