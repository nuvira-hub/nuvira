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

// Impairment simulates a worse network on the traffic forwarded by the relay:
// delay, delay variation, loss, limited bandwidth with a finite queue (like a
// router with a buffer) and occasional stalls (like a mobile network, where
// delivery pauses for hundreds of milliseconds every few seconds even with little
// traffic). For tests only: enabled with NEXODESK_RELAY_IMPAIR, e.g.
//
//	NEXODESK_RELAY_IMPAIR="delay=40ms,jitter=10ms,loss=1%,rate=3mbit,queue=150ms"
//	NEXODESK_RELAY_IMPAIR="delay=20ms,stall=300ms,every=3s"
type Impairment struct {
	Delay  time.Duration
	Jitter time.Duration
	Loss   float64 // 0..1
	Rate   float64 // bit/s, 0 = unlimited
	Queue  time.Duration
	// Stall: delivery pauses for Stall, on average every StallEvery (0 = never).
	Stall      time.Duration
	StallEvery time.Duration
}

// ParseImpairment reads the specification; empty string = no simulation.
func ParseImpairment(spec string) (*Impairment, error) {
	if strings.TrimSpace(spec) == "" {
		return nil, nil
	}
	im := &Impairment{Queue: 200 * time.Millisecond}
	for _, part := range strings.Split(spec, ",") {
		key, value, ok := strings.Cut(strings.TrimSpace(part), "=")
		if !ok {
			return nil, fmt.Errorf("expected key=value: %q", part)
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
		case "stall":
			im.Stall, err = time.ParseDuration(value)
		case "every":
			im.StallEvery, err = time.ParseDuration(value)
		default:
			err = fmt.Errorf("unknown key")
		}
		if err != nil {
			return nil, fmt.Errorf("%s: %w", key, err)
		}
	}
	if im.Stall > 0 && im.StallEvery <= 0 {
		im.StallEvery = 3 * time.Second
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
	return fmt.Sprintf("delay %v ±%v, loss %.1f%%, bandwidth %.0f kbit/s, queue %v, stall %v every ~%v",
		im.Delay, im.Jitter, im.Loss*100, im.Rate/1000, im.Queue, im.Stall, im.StallEvery)
}

type delayed struct {
	data []byte
	to   net.Addr
	at   time.Time
}

// impairer applies the simulation to each destination separately (each direction
// of a session has its own queue, like two separate links).
type impairer struct {
	im    *Impairment
	write func([]byte, net.Addr)

	mu    sync.Mutex
	links map[string]*impairedLink
}

type impairedLink struct {
	queue    chan delayed
	nextFree time.Time // when the "link" will have finished sending what is queued
	lastAt   time.Time // delivery of the last packet: jitter does not reorder
	// Next stall and end of the current one.
	nextStall, stallUntil time.Time
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
	// Limited bandwidth: the packet leaves when the link has finished the previous ones.
	start := now
	if l.nextFree.After(start) {
		start = l.nextFree
	}
	if p.im.Rate > 0 {
		if start.Sub(now) > p.im.Queue {
			p.mu.Unlock()
			return // queue full: dropped, as a router would do
		}
		l.nextFree = start.Add(time.Duration(float64(len(data)*8) / p.im.Rate * float64(time.Second)))
	}
	at := start.Add(p.im.Delay)
	if p.im.Jitter > 0 {
		at = at.Add(time.Duration(rand.Int64N(int64(2*p.im.Jitter))) - p.im.Jitter)
	}
	if p.im.Stall > 0 {
		if l.nextStall.IsZero() {
			l.nextStall = now.Add(p.im.StallEvery)
		}
		if !now.Before(l.nextStall) {
			l.stallUntil = now.Add(p.im.Stall)
			// Irregular interval: from half to one and a half times the average.
			l.nextStall = now.Add(p.im.StallEvery/2 + time.Duration(rand.Int64N(int64(p.im.StallEvery))))
		}
		if at.Before(l.stallUntil.Add(p.im.Delay)) {
			at = l.stallUntil.Add(p.im.Delay)
		}
	}
	// As on a real link, the delay varies but the order stays the sending order.
	if at.Before(l.lastAt) {
		at = l.lastAt
	}
	l.lastAt = at
	p.mu.Unlock()

	select {
	case l.queue <- delayed{data: append([]byte(nil), data...), to: to, at: at}:
	default: // internal queue full: dropped
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
