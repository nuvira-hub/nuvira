package relay

import (
	"net"
	"sync"
	"testing"
	"time"
)

func TestParseImpairment(t *testing.T) {
	im, err := ParseImpairment("delay=40ms,jitter=5ms,loss=1.5%,rate=3mbit,queue=100ms")
	if err != nil {
		t.Fatal(err)
	}
	if im.Delay != 40*time.Millisecond || im.Jitter != 5*time.Millisecond || im.Loss != 0.015 ||
		im.Rate != 3e6 || im.Queue != 100*time.Millisecond {
		t.Fatalf("%+v", im)
	}
	if im, err := ParseImpairment(""); im != nil || err != nil {
		t.Fatal("stringa vuota deve disattivare la simulazione")
	}
	if _, err := ParseImpairment("ritardo=1ms"); err == nil {
		t.Fatal("chiave sconosciuta accettata")
	}
}

func TestImpairerDelayAndRate(t *testing.T) {
	var mu sync.Mutex
	var arrivals []time.Time
	p := newImpairer(&Impairment{Delay: 30 * time.Millisecond, Rate: 1e6, Queue: time.Second},
		func([]byte, net.Addr) {
			mu.Lock()
			arrivals = append(arrivals, time.Now())
			mu.Unlock()
		})
	to := &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: 9}
	start := time.Now()
	// 10 pacchetti da 1250 byte a 1 Mbit/s: 10 ms l'uno, l'ultimo parte dopo ~90 ms.
	for range 10 {
		p.send(make([]byte, 1250), to)
	}
	time.Sleep(250 * time.Millisecond)
	mu.Lock()
	defer mu.Unlock()
	if len(arrivals) != 10 {
		t.Fatalf("arrivati %d pacchetti", len(arrivals))
	}
	if d := arrivals[0].Sub(start); d < 25*time.Millisecond {
		t.Fatalf("primo pacchetto troppo presto: %v", d)
	}
	if d := arrivals[9].Sub(start); d < 110*time.Millisecond {
		t.Fatalf("la banda non è limitata: ultimo dopo %v", d)
	}
}
