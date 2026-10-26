package relay

import (
	"fmt"
	"math/rand/v2"
	"net"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Impairment simula una rete peggiore sul traffico inoltrato dal relay: ritardo,
// variazione del ritardo, perdita e banda limitata con una coda finita (come un
// router con buffer). Serve solo ai test: si attiva con NEXODESK_RELAY_IMPAIR, es.
//
//	NEXODESK_RELAY_IMPAIR="delay=40ms,jitter=10ms,loss=1%,rate=3mbit,queue=150ms"
type Impairment struct {
	Delay  time.Duration
	Jitter time.Duration
	Loss   float64 // 0..1
	Rate   float64 // bit/s, 0 = illimitata
	Queue  time.Duration
}

// ParseImpairment legge la specifica; stringa vuota = nessuna simulazione.
func ParseImpairment(spec string) (*Impairment, error) {
	if strings.TrimSpace(spec) == "" {
		return nil, nil
	}
	im := &Impairment{Queue: 200 * time.Millisecond}
	for _, part := range strings.Split(spec, ",") {
		key, value, ok := strings.Cut(strings.TrimSpace(part), "=")
		if !ok {
			return nil, fmt.Errorf("atteso chiave=valore: %q", part)
		}
		var err error
		switch key {
		case "delay":
			im.Delay, err = time.ParseDuration(value)
		case "jitter":
			im.Jitter, err = time.ParseDuration(value)
		case "queue":
			im.Queue, err = time.ParseDuration(value)
		case "loss":
			var pct float64
			pct, err = strconv.ParseFloat(strings.TrimSuffix(value, "%"), 64)
			im.Loss = pct / 100
		case "rate":
			im.Rate, err = parseRate(value)
		default:
			err = fmt.Errorf("chiave sconosciuta")
		}
		if err != nil {
			return nil, fmt.Errorf("%s: %w", key, err)
		}
	}
	return im, nil
}

func parseRate(v string) (float64, error) {
	mult := 1.0
	switch {
	case strings.HasSuffix(v, "mbit"):
		mult, v = 1e6, strings.TrimSuffix(v, "mbit")
	case strings.HasSuffix(v, "kbit"):
		mult, v = 1e3, strings.TrimSuffix(v, "kbit")
	case strings.HasSuffix(v, "bit"):
		v = strings.TrimSuffix(v, "bit")
	}
	f, err := strconv.ParseFloat(v, 64)
	return f * mult, err
}

func (im *Impairment) String() string {
	return fmt.Sprintf("ritardo %v ±%v, perdita %.1f%%, banda %.0f kbit/s, coda %v",
		im.Delay, im.Jitter, im.Loss*100, im.Rate/1000, im.Queue)
}

type delayed struct {
	data []byte
	to   net.Addr
	at   time.Time
}

// impairer applica la simulazione a ogni destinazione separatamente (ciascun
// verso di una sessione ha la sua coda, come due link distinti).
type impairer struct {
	im    *Impairment
	write func([]byte, net.Addr)

	mu    sync.Mutex
	links map[string]*impairedLink
}

type impairedLink struct {
	queue    chan delayed
	nextFree time.Time // quando il "link" avrà finito di trasmettere ciò che ha in coda
	lastAt   time.Time // consegna dell'ultimo pacchetto: il jitter non riordina
}

func newImpairer(im *Impairment, write func([]byte, net.Addr)) *impairer {
	return &impairer{im: im, write: write, links: map[string]*impairedLink{}}
}

func (p *impairer) send(data []byte, to net.Addr) {
	if p.im.Loss > 0 && rand.Float64() < p.im.Loss {
		return
	}
	now := time.Now()
	p.mu.Lock()
	l := p.links[to.String()]
	if l == nil {
		l = &impairedLink{queue: make(chan delayed, 4096)}
		p.links[to.String()] = l
		go p.run(l)
	}
	// Banda limitata: il pacchetto parte quando il link ha finito i precedenti.
	start := now
	if l.nextFree.After(start) {
		start = l.nextFree
	}
	if p.im.Rate > 0 {
		if start.Sub(now) > p.im.Queue {
			p.mu.Unlock()
			return // coda piena: scartato, come farebbe un router
		}
		l.nextFree = start.Add(time.Duration(float64(len(data)*8) / p.im.Rate * float64(time.Second)))
	}
	at := start.Add(p.im.Delay)
	if p.im.Jitter > 0 {
		at = at.Add(time.Duration(rand.Int64N(int64(2*p.im.Jitter))) - p.im.Jitter)
	}
	// Come su un collegamento reale, il ritardo varia ma l'ordine resta quello d'invio.
	if at.Before(l.lastAt) {
		at = l.lastAt
	}
	l.lastAt = at
	p.mu.Unlock()

	select {
	case l.queue <- delayed{data: append([]byte(nil), data...), to: to, at: at}:
	default: // coda interna piena: scartato
	}
}

func (p *impairer) run(l *impairedLink) {
	for d := range l.queue {
		if wait := time.Until(d.at); wait > 0 {
			time.Sleep(wait)
		}
		p.write(d.data, d.to)
	}
}
