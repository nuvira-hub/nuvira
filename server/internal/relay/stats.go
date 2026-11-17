package relay

import (
	"fmt"
	"log/slog"
	"strconv"
	"strings"
	"time"
)

// Counters are the relay's totals since the start (see LogStats). They hold no
// addresses: only numbers.
type Counters struct {
	// Datagrams and bytes forwarded between the two sides of relayed sessions.
	Packets uint64
	Bytes   uint64
	// Datagrams dropped because the receive queue (inQueue) was full.
	Drops uint64
	// Relayed sessions with both sides bound, and still waiting for one side.
	SessionsOpen    int
	SessionsWaiting int
}

// Counters returns the totals now.
func (c *Conn) Counters() Counters {
	r := c.relay
	r.mu.Lock()
	n := Counters{Packets: r.fwdPackets, Bytes: r.fwdBytes}
	for _, s := range r.sessions {
		if s.ready() {
			n.SessionsOpen++
		} else {
			n.SessionsWaiting++
		}
	}
	r.mu.Unlock()
	n.Drops = c.drops.Load()
	return n
}

// MinStatsInterval is the shortest interval accepted by ParseStatsInterval.
const MinStatsInterval = time.Second

// ParseStatsInterval reads NUVIRA_RELAY_STATS: a duration ("10s", "1m") or a
// number of seconds; empty or "off" means off (0).
func ParseStatsInterval(v string) (time.Duration, error) {
	v = strings.TrimSpace(v)
	if v == "" || v == "0" || strings.EqualFold(v, "off") {
		return 0, nil
	}
	d, err := time.ParseDuration(v)
	if err != nil {
		secs, err2 := strconv.ParseFloat(v, 64)
		if err2 != nil {
			return 0, fmt.Errorf("not a duration: %q (e.g. 10s)", v)
		}
		d = time.Duration(secs * float64(time.Second))
	}
	if d < MinStatsInterval {
		return 0, fmt.Errorf("interval %v shorter than %v", d, MinStatsInterval)
	}
	return d, nil
}

// LogStats logs the relay's counters of every interval until the Conn is closed:
// datagrams and bytes forwarded, the rate, datagrams dropped by a full queue, the
// queue's length and the sessions open and waiting (no addresses).
func (c *Conn) LogStats(interval time.Duration, log *slog.Logger) {
	t := time.NewTicker(interval)
	defer t.Stop()
	prev, last := c.Counters(), time.Now()
	for {
		select {
		case <-c.closed:
			return
		case now := <-t.C:
			cur := c.Counters()
			secs := now.Sub(last).Seconds()
			bytes := cur.Bytes - prev.Bytes
			log.Info("relay stats",
				"interval_s", roundTo(secs, 3),
				"packets", cur.Packets-prev.Packets,
				"bytes", bytes,
				"mbit", roundTo(float64(bytes)*8/secs/1e6, 3),
				"queue_drops", cur.Drops-prev.Drops,
				"queue_len", len(c.in),
				"sessions_open", cur.SessionsOpen,
				"sessions_waiting", cur.SessionsWaiting)
			prev, last = cur, now
		}
	}
}

func roundTo(v float64, decimals int) float64 {
	p, _ := strconv.ParseFloat(strconv.FormatFloat(v, 'f', decimals, 64), 64)
	return p
}
