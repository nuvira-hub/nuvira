// Benchmarks of the relay (bench suite, B-GO-01..05 and B-RES-05).
//
// This file lives in nuvira/bench/go/relay/ and is copied by
// bench/scripts/bench-compare.sh into server/internal/relay/ of each side's
// worktree, so the same code measures the baseline tag and the new version.
// External test package: only exported identifiers are used, plus the wire
// format written as literals (cross-checked by TestBenchWireLiterals).
// Every identifier starts with "bench" so nothing clashes with the package's
// own tests.
//
// Each Benchmark carries "// BENCH-ID:" (the suite ID) and optional
// "// BENCH-METRIC: <name> <better> <gate>" lines read by `nuvira-bench import-go`.
package relay_test

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"math"
	"net"
	"net/http/httptest"
	"os"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/coder/websocket"

	"github.com/nuvira-hub/nuvira/server/internal/relay"
)

// Wire format of the relay control messages (relay.go package comment).
var benchMagic = []byte{0x00, 'N', 'X', 'R'}

const (
	benchTypeBind  = 0x01
	benchTypeBound = 0x02
	benchTokenLen  = 16
	benchViewer    = 1
	benchAgent     = 2
)

var benchLogOnce sync.Once

// benchLog is a logger that discards, as cheap as production's minus the I/O.
// The first record formats a time, which on Windows loads the time zone (~300
// ms): it is done once here, outside every timed region.
func benchLog() *slog.Logger {
	l := slog.New(slog.NewTextHandler(io.Discard, nil))
	benchLogOnce.Do(func() { l.Info("bench warm-up", "addr", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: 1}) })
	return l
}

func benchToken(i int) [benchTokenLen]byte {
	var t [benchTokenLen]byte
	binary.BigEndian.PutUint64(t[:8], 0x6e75766972610000|uint64(i))
	binary.BigEndian.PutUint64(t[8:], uint64(i)*0x9e3779b97f4a7c15)
	return t
}

// benchBind builds a bind with the given local addresses (IPv4 only).
func benchBind(token [benchTokenLen]byte, role byte, locals ...*net.UDPAddr) []byte {
	p := append([]byte{}, benchMagic...)
	p = append(p, benchTypeBind)
	p = append(p, token[:]...)
	p = append(p, role)
	if len(locals) > 0 {
		p = append(p, byte(len(locals)))
		for _, a := range locals {
			p = append(p, 4)
			p = append(p, a.IP.To4()...)
			p = append(p, byte(a.Port>>8), byte(a.Port))
		}
	}
	return p
}

func benchAddr(i, port int) *net.UDPAddr {
	return &net.UDPAddr{IP: net.IPv4(10, byte(i>>16), byte(i>>8), byte(i)), Port: port}
}

// TestBenchWireLiterals checks that the literals above still describe the
// relay's wire format: if it fails, the benchmarks below measure nothing.
func TestBenchWireLiterals(t *testing.T) {
	r := relay.New(benchLog())
	tok := benchToken(1)
	v, a := benchAddr(1, 1000), benchAddr(2, 2000)
	bind := benchBind(tok, benchViewer)
	if !relay.IsControl(bind) {
		t.Fatal("IsControl does not recognise the bind literal")
	}
	if relay.IsControl([]byte{0x41, 'N', 'X', 'R', 1}) {
		t.Fatal("IsControl accepts a QUIC-like packet")
	}
	if r.HandleControl(bind, v) != nil {
		t.Fatal("bind accepted without Allow")
	}
	r.Allow(tok)
	reply := r.HandleControl(bind, v)
	want := append(append(append([]byte{}, benchMagic...), benchTypeBound), tok[:]...)
	if len(reply) != len(want)+1 || string(reply[:len(want)]) != string(want) || reply[len(want)] != 0 {
		t.Fatalf("bound literal mismatch: %v", reply)
	}
	reply = r.HandleControl(benchBind(tok, benchAgent, benchAddr(3, 3000)), a)
	if len(reply) < len(want)+1 || reply[len(want)] != 1 {
		t.Fatalf("second bind not ready: %v", reply)
	}
	if d := r.Destination(v, 1200); d == nil || d.String() != a.String() {
		t.Fatalf("viewer forwards to %v", d)
	}
}

// BENCH-ID: B-GO-01
func BenchmarkRelayBind(b *testing.B) {
	const ring = 1024
	type slot struct {
		tok        [benchTokenLen]byte
		v, a       *net.UDPAddr
		vBind, aBi []byte
	}
	slots := make([]slot, ring)
	for i := range slots {
		s := &slots[i]
		s.tok = benchToken(i)
		s.v, s.a = benchAddr(2*i, 40000), benchAddr(2*i+1, 40001)
		s.vBind = benchBind(s.tok, benchViewer, benchAddr(2*i, 50000))
		s.aBi = benchBind(s.tok, benchAgent, benchAddr(2*i+1, 50001))
	}
	b.Run("first", func(b *testing.B) {
		r := relay.New(benchLog())
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			s := &slots[i%ring]
			r.Allow(s.tok)
			if r.HandleControl(s.vBind, s.v) == nil {
				b.Fatal("no reply")
			}
		}
	})
	b.Run("pair", func(b *testing.B) {
		r := relay.New(benchLog())
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			s := &slots[i%ring]
			r.Allow(s.tok)
			r.HandleControl(s.vBind, s.v)
			if r.HandleControl(s.aBi, s.a) == nil {
				b.Fatal("no reply")
			}
		}
	})
	b.Run("rebind", func(b *testing.B) {
		r := relay.New(benchLog())
		for i := range slots {
			s := &slots[i]
			r.Allow(s.tok)
			r.HandleControl(s.vBind, s.v)
			r.HandleControl(s.aBi, s.a)
		}
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			s := &slots[i%ring]
			if r.HandleControl(s.vBind, s.v) == nil {
				b.Fatal("no reply")
			}
		}
	})
	b.Run("unknown", func(b *testing.B) {
		r := relay.New(benchLog())
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			s := &slots[i%ring]
			if r.HandleControl(s.vBind, s.v) != nil {
				b.Fatal("reply to an unknown token")
			}
		}
	})
}

// BENCH-ID: B-GO-02
func BenchmarkRelayDestination(b *testing.B) {
	for _, n := range []int{1, 100, 10000} {
		r := relay.New(benchLog())
		from := make([]net.Addr, n)
		for i := 0; i < n; i++ {
			tok := benchToken(i)
			v, a := benchAddr(2*i, 40000), benchAddr(2*i+1, 40001)
			r.Allow(tok)
			r.HandleControl(benchBind(tok, benchViewer), v)
			r.HandleControl(benchBind(tok, benchAgent), a)
			from[i] = v
		}
		b.Run(fmt.Sprintf("sessions=%d", n), func(b *testing.B) {
			b.ReportAllocs()
			b.ResetTimer()
			for i := 0; i < b.N; i++ {
				if r.Destination(from[i%n], 1200) == nil {
					b.Fatal("no destination")
				}
			}
		})
		b.Run(fmt.Sprintf("sessions=%d/parallel", n), func(b *testing.B) {
			var seq atomic.Int64
			b.ReportAllocs()
			b.ResetTimer()
			b.RunParallel(func(pb *testing.PB) {
				i := int(seq.Add(1)) * 7919
				for pb.Next() {
					if r.Destination(from[i%n], 1200) == nil {
						b.Error("no destination")
						return
					}
					i++
				}
			})
		})
	}
}

// ---- Real sockets -------------------------------------------------------------

// benchRelay is a relay on 127.0.0.1 with a reader that consumes what is not
// relayed, as quic-go does in the server.
type benchRelay struct {
	r    *relay.Relay
	conn *relay.Conn
	addr *net.UDPAddr
}

func benchStartRelay(tb testing.TB, im *relay.Impairment) *benchRelay {
	tb.Helper()
	udp, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		tb.Fatal(err)
	}
	_ = udp.SetReadBuffer(8 << 20)
	_ = udp.SetWriteBuffer(8 << 20)
	r := relay.New(benchLog())
	c := relay.NewConn(udp, r)
	if im != nil {
		c.Impair(im) // before any traffic: the field is not synchronised
	}
	go func() {
		buf := make([]byte, 2048)
		for {
			if _, _, err := c.ReadFrom(buf); err != nil {
				return
			}
		}
	}()
	tb.Cleanup(func() { _ = c.Close() })
	return &benchRelay{r: r, conn: c, addr: udp.LocalAddr().(*net.UDPAddr)}
}

// benchEndpoint is one side of a relayed pair.
type benchEndpoint interface {
	send(p []byte) error
	// recvLoop calls got for every datagram until stop is closed.
	recvLoop(stop <-chan struct{}, got func(p []byte))
	close()
}

type benchUDP struct {
	c     *net.UDPConn
	relay *net.UDPAddr
}

func benchNewUDP(tb testing.TB, relayAddr *net.UDPAddr) *benchUDP {
	tb.Helper()
	c, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		tb.Fatal(err)
	}
	_ = c.SetReadBuffer(4 << 20)
	_ = c.SetWriteBuffer(4 << 20)
	return &benchUDP{c: c, relay: relayAddr}
}

func (u *benchUDP) send(p []byte) error {
	_, err := u.c.WriteToUDP(p, u.relay)
	return err
}

func (u *benchUDP) recvLoop(stop <-chan struct{}, got func(p []byte)) {
	buf := make([]byte, 2048)
	for {
		select {
		case <-stop:
			return
		default:
		}
		_ = u.c.SetReadDeadline(time.Now().Add(50 * time.Millisecond))
		n, _, err := u.c.ReadFromUDP(buf)
		if err != nil {
			// Timeouts, and on Windows the ICMP "port unreachable" reported on
			// the next read (WSAECONNRESET): only a closed socket ends the loop.
			if errors.Is(err, net.ErrClosed) {
				return
			}
			continue
		}
		got(buf[:n])
	}
}

func (u *benchUDP) close() { _ = u.c.Close() }

// bind sends the bind until the relay answers (UDP can lose it).
func (u *benchUDP) bind(tok [benchTokenLen]byte, role byte) ([]byte, error) {
	buf := make([]byte, 2048)
	for range 20 {
		if err := u.send(benchBind(tok, role)); err != nil {
			return nil, err
		}
		_ = u.c.SetReadDeadline(time.Now().Add(250 * time.Millisecond))
		n, _, err := u.c.ReadFromUDP(buf)
		if err == nil && n > 5 && buf[4] == benchTypeBound {
			return append([]byte(nil), buf[:n]...), nil
		}
	}
	return nil, errors.New("relay did not answer the bind")
}

type benchWS struct {
	ws     *websocket.Conn
	ctx    context.Context
	cancel context.CancelFunc
}

func benchNewWS(tb testing.TB, url string) *benchWS {
	tb.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	dctx, dcancel := context.WithTimeout(ctx, 5*time.Second)
	defer dcancel()
	ws, _, err := websocket.Dial(dctx, url, nil)
	if err != nil {
		cancel()
		tb.Fatal(err)
	}
	ws.SetReadLimit(4096)
	return &benchWS{ws: ws, ctx: ctx, cancel: cancel}
}

func (w *benchWS) send(p []byte) error { return w.ws.Write(w.ctx, websocket.MessageBinary, p) }

func (w *benchWS) recvLoop(stop <-chan struct{}, got func(p []byte)) {
	ctx, cancel := context.WithCancel(w.ctx)
	defer cancel()
	go func() {
		select {
		case <-stop:
		case <-ctx.Done():
		}
		cancel()
	}()
	for {
		_, data, err := w.ws.Read(ctx)
		if err != nil {
			return
		}
		got(data)
	}
}

func (w *benchWS) close() {
	_ = w.ws.CloseNow()
	w.cancel()
}

func (w *benchWS) bind(tok [benchTokenLen]byte, role byte) ([]byte, error) {
	if err := w.send(benchBind(tok, role)); err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(w.ctx, 5*time.Second)
	defer cancel()
	_, data, err := w.ws.Read(ctx)
	if err != nil {
		return nil, err
	}
	if len(data) < 6 || data[4] != benchTypeBound {
		return nil, fmt.Errorf("unexpected answer %v", data)
	}
	return data, nil
}

type benchBinder interface {
	benchEndpoint
	bind(tok [benchTokenLen]byte, role byte) ([]byte, error)
}

type benchPair struct{ src, dst benchEndpoint }

// benchBindPair binds dst (agent) then src (viewer) on a fresh token.
func benchBindPair(tb testing.TB, br *benchRelay, i int, src, dst benchBinder) benchPair {
	tb.Helper()
	tok := benchToken(1_000_000 + i)
	br.r.Allow(tok)
	if _, err := dst.bind(tok, benchAgent); err != nil {
		tb.Fatal(err)
	}
	reply, err := src.bind(tok, benchViewer)
	if err != nil {
		tb.Fatal(err)
	}
	if reply[5+benchTokenLen] != 1 {
		tb.Fatalf("pair %d not ready", i)
	}
	return benchPair{src: src, dst: dst}
}

func benchUDPPairs(tb testing.TB, br *benchRelay, n int) []benchPair {
	pairs := make([]benchPair, n)
	for i := range pairs {
		src, dst := benchNewUDP(tb, br.addr), benchNewUDP(tb, br.addr)
		tb.Cleanup(src.close)
		tb.Cleanup(dst.close)
		pairs[i] = benchBindPair(tb, br, i, src, dst)
	}
	return pairs
}

// benchForward sends total datagrams of size bytes split over the pairs, each
// pair keeping at most window datagrams in flight (closed loop: it measures the
// relay's forwarding rate, not the socket buffers). A datagram not received
// within 100 ms counts as lost.
func benchForward(pairs []benchPair, total, size, window int) (sent, received int64, elapsed time.Duration) {
	payload := make([]byte, size)
	for i := range payload {
		payload[i] = 0x41 // QUIC-like: first byte has the 0x40 bit, never a control message
	}
	var recv, snd atomic.Int64
	stop := make(chan struct{})
	var rwg, swg sync.WaitGroup
	sems := make([]chan struct{}, len(pairs))
	for i, p := range pairs {
		sem := make(chan struct{}, window)
		sems[i] = sem
		rwg.Add(1)
		go func() {
			defer rwg.Done()
			p.dst.recvLoop(stop, func([]byte) {
				recv.Add(1)
				select {
				case <-sem:
				default:
				}
			})
		}()
	}
	start := time.Now()
	for i, p := range pairs {
		per := total / len(pairs)
		if i < total%len(pairs) {
			per++
		}
		sem := sems[i]
		swg.Add(1)
		go func() {
			defer swg.Done()
			timer := time.NewTimer(time.Hour)
			timer.Stop()
			for range per {
				select {
				case sem <- struct{}{}:
				default:
					timer.Reset(100 * time.Millisecond)
					select {
					case sem <- struct{}{}:
						timer.Stop()
					case <-timer.C:
						// The oldest datagram in flight is taken as lost.
						select {
						case <-sem:
						default:
						}
						sem <- struct{}{}
					}
				}
				if p.src.send(payload) != nil {
					return
				}
				snd.Add(1)
			}
			// Wait for what is still in flight.
			deadline := time.Now().Add(200 * time.Millisecond)
			for len(sem) > 0 && time.Now().Before(deadline) {
				time.Sleep(time.Millisecond)
			}
		}()
	}
	swg.Wait()
	elapsed = time.Since(start)
	close(stop)
	rwg.Wait()
	return snd.Load(), recv.Load(), elapsed
}

func benchReportForward(b *testing.B, sent, received int64, elapsed time.Duration) {
	if sent == 0 || elapsed <= 0 {
		return
	}
	b.ReportMetric(float64(received)/elapsed.Seconds(), "pkts/s")
	b.ReportMetric(100*float64(sent-received)/float64(sent), "loss-%")
}

// BENCH-ID: B-GO-03
// BENCH-METRIC: pkts/s higher macro_throughput
// BENCH-METRIC: loss-% lower info
func BenchmarkRelayForward(b *testing.B) {
	for _, n := range []int{1, 8, 64} {
		b.Run(fmt.Sprintf("pairs=%d", n), func(b *testing.B) {
			br := benchStartRelay(b, nil)
			pairs := benchUDPPairs(b, br, n)
			b.ReportAllocs()
			b.SetBytes(1200)
			b.ResetTimer()
			sent, received, elapsed := benchForward(pairs, b.N, 1200, 32)
			b.StopTimer()
			benchReportForward(b, sent, received, elapsed)
		})
	}
}

// Network profiles N2 and N3 of the bench plan (same strings as NUVIRA_RELAY_IMPAIR).
var benchProfiles = []struct{ name, spec string }{
	{"N0", ""},
	{"N2", "delay=20ms,jitter=5ms,loss=0.5%,rate=20mbit,queue=100ms"},
	{"N3", "delay=40ms,jitter=10ms,loss=1%,rate=3mbit,queue=150ms"},
}

// BENCH-ID: B-GO-04
// BENCH-METRIC: ns/op lower info
// BENCH-METRIC: delay-mean-ms lower info
// BENCH-METRIC: delay-err-ms lower info
// BENCH-METRIC: delay-p95-ms lower info
// BENCH-METRIC: loss-% lower info
// BENCH-METRIC: loss-err-pts lower info
//
// Impairment accuracy: 200-byte datagrams paced at 500 per second (below the
// rate limits) through the impaired relay; each carries its send time, the
// receiver measures the one-way delay on the same clock. ns/op is the pacing.
func BenchmarkRelayImpair(b *testing.B) {
	for _, prof := range benchProfiles {
		b.Run(prof.name, func(b *testing.B) {
			im, err := relay.ParseImpairment(prof.spec)
			if err != nil {
				b.Fatal(err)
			}
			br := benchStartRelay(b, im)
			pair := benchUDPPairs(b, br, 1)[0]
			base := time.Now()
			var mu sync.Mutex
			delays := make([]float64, 0, b.N)
			stop := make(chan struct{})
			done := make(chan struct{})
			go func() {
				defer close(done)
				pair.dst.recvLoop(stop, func(p []byte) {
					if len(p) < 9 {
						return
					}
					sentAt := time.Duration(binary.BigEndian.Uint64(p[1:9]))
					d := time.Since(base) - sentAt
					mu.Lock()
					delays = append(delays, float64(d)/1e6)
					mu.Unlock()
				})
			}()
			payload := make([]byte, 200)
			payload[0] = 0x41
			const interval = 2 * time.Millisecond
			b.ReportAllocs()
			b.ResetTimer()
			next := time.Now()
			for i := 0; i < b.N; i++ {
				if d := time.Until(next); d > 0 {
					time.Sleep(d)
				}
				next = next.Add(interval)
				binary.BigEndian.PutUint64(payload[1:9], uint64(time.Since(base)))
				if err := pair.src.send(payload); err != nil {
					b.Fatal(err)
				}
			}
			b.StopTimer()
			wait := 300 * time.Millisecond
			if im != nil {
				wait += im.Delay + 2*im.Jitter + im.Queue
			}
			time.Sleep(wait)
			close(stop)
			<-done
			mu.Lock()
			defer mu.Unlock()
			if len(delays) == 0 {
				// The first, 1-iteration run of a benchmark sends one datagram:
				// the profile's loss can drop it (seen with N3 on the NUC).
				if b.N < 100 {
					return
				}
				b.Fatal("nothing received")
			}
			sort.Float64s(delays)
			var sum float64
			for _, d := range delays {
				sum += d
			}
			mean := sum / float64(len(delays))
			spec, specLoss := 0.0, 0.0
			if im != nil {
				spec, specLoss = float64(im.Delay)/1e6, im.Loss*100
			}
			loss := 100 * float64(b.N-len(delays)) / float64(b.N)
			b.ReportMetric(mean, "delay-mean-ms")
			b.ReportMetric(math.Abs(mean-spec), "delay-err-ms")
			b.ReportMetric(delays[int(float64(len(delays)-1)*0.95)], "delay-p95-ms")
			b.ReportMetric(loss, "loss-%")
			b.ReportMetric(math.Abs(loss-specLoss), "loss-err-pts")
		})
	}
}

// BENCH-ID: B-GO-05
// BENCH-METRIC: pkts/s higher macro_throughput
// BENCH-METRIC: loss-% lower info
//
// WebSocket relay path: one peer through Conn.WebSockets() on an httptest
// server, the other on UDP; 1,200-byte datagrams in both directions.
func BenchmarkRelayWebSocket(b *testing.B) {
	for _, dir := range []string{"ws-to-udp", "udp-to-ws"} {
		b.Run(dir, func(b *testing.B) {
			br := benchStartRelay(b, nil)
			srv := httptest.NewServer(br.conn.WebSockets())
			b.Cleanup(srv.Close)
			ws := benchNewWS(b, "ws"+strings.TrimPrefix(srv.URL, "http")+"/nuvira")
			b.Cleanup(ws.close)
			udp := benchNewUDP(b, br.addr)
			b.Cleanup(udp.close)
			var pair benchPair
			if dir == "ws-to-udp" {
				pair = benchBindPair(b, br, 0, ws, udp)
			} else {
				pair = benchBindPair(b, br, 0, udp, ws)
			}
			b.ReportAllocs()
			b.SetBytes(1200)
			b.ResetTimer()
			sent, received, elapsed := benchForward([]benchPair{pair}, b.N, 1200, 32)
			b.StopTimer()
			benchReportForward(b, sent, received, elapsed)
		})
	}
}

// ---- B-RES-05: relay soak ---------------------------------------------------------

func benchEnvInt(key string, def int) int {
	if v, err := strconv.Atoi(os.Getenv(key)); err == nil && v > 0 {
		return v
	}
	return def
}

func benchOpenFDs() int {
	entries, err := os.ReadDir("/proc/self/fd")
	if err != nil {
		return -1 // not Linux: not measured here
	}
	return len(entries)
}

type benchSoakSample struct {
	T          float64 `json:"t_s"`
	HeapAlloc  uint64  `json:"heap_alloc"`
	HeapInuse  uint64  `json:"heap_inuse"`
	Sys        uint64  `json:"sys"`
	Goroutines int     `json:"goroutines"`
	FDs        int     `json:"fds"`
	Received   int64   `json:"received"`
	Sent       int64   `json:"sent"`
}

// benchSlopeMBh is the least-squares slope of the heap in MB per hour.
func benchSlopeMBh(s []benchSoakSample) float64 {
	if len(s) < 2 {
		return 0
	}
	var sx, sy, sxx, sxy float64
	for _, p := range s {
		x, y := p.T/3600, float64(p.HeapInuse)/(1<<20)
		sx, sy, sxx, sxy = sx+x, sy+y, sxx+x*x, sxy+x*y
	}
	n := float64(len(s))
	den := n*sxx - sx*sx
	if den == 0 {
		return 0
	}
	return (n*sxy - sx*sy) / den
}

// TestBenchSoakRelay (B-RES-05) runs only with NUVIRA_BENCH_SOAK=1:
// NUVIRA_BENCH_SOAK_PAIRS relayed UDP pairs (default 50) at
// NUVIRA_BENCH_SOAK_PPS datagrams per second each (default 2000) for
// NUVIRA_BENCH_SOAK_SECONDS (default 1800). It samples heap, goroutines and fds
// every 10 s and writes a nuvira-bench/1 JSON file to NUVIRA_BENCH_SOAK_OUT.
func TestBenchSoakRelay(t *testing.T) {
	if os.Getenv("NUVIRA_BENCH_SOAK") != "1" {
		t.Skip("set NUVIRA_BENCH_SOAK=1")
	}
	pairsN := benchEnvInt("NUVIRA_BENCH_SOAK_PAIRS", 50)
	pps := benchEnvInt("NUVIRA_BENCH_SOAK_PPS", 2000)
	duration := time.Duration(benchEnvInt("NUVIRA_BENCH_SOAK_SECONDS", 1800)) * time.Second
	every := time.Duration(benchEnvInt("NUVIRA_BENCH_SOAK_SAMPLE_SECONDS", 10)) * time.Second

	br := benchStartRelay(t, nil)
	pairs := benchUDPPairs(t, br, pairsN)
	runtime.GC()
	time.Sleep(time.Second)
	goroutinesBefore := runtime.NumGoroutine()
	fdsBefore := benchOpenFDs()

	var sent, recv atomic.Int64
	stop := make(chan struct{})
	var wg sync.WaitGroup
	payload := make([]byte, 1200)
	payload[0] = 0x41
	burst := max(1, pps/100) // every 10 ms
	for _, p := range pairs {
		wg.Add(2)
		go func() {
			defer wg.Done()
			p.dst.recvLoop(stop, func([]byte) { recv.Add(1) })
		}()
		go func() {
			defer wg.Done()
			tick := time.NewTicker(10 * time.Millisecond)
			defer tick.Stop()
			for {
				select {
				case <-stop:
					return
				case <-tick.C:
				}
				for range burst {
					if p.src.send(payload) == nil {
						sent.Add(1)
					}
				}
			}
		}()
	}

	start := time.Now()
	var samples []benchSoakSample
	sample := func() {
		// After a GC: the live heap, not the garbage sawtooth (a slope over a
		// short run would otherwise be noise).
		runtime.GC()
		var ms runtime.MemStats
		runtime.ReadMemStats(&ms)
		samples = append(samples, benchSoakSample{
			T: time.Since(start).Seconds(), HeapAlloc: ms.HeapAlloc, HeapInuse: ms.HeapInuse, Sys: ms.Sys,
			Goroutines: runtime.NumGoroutine(), FDs: benchOpenFDs(), Received: recv.Load(), Sent: sent.Load(),
		})
	}
	sample()
	tick := time.NewTicker(every)
	for time.Since(start) < duration {
		<-tick.C
		sample()
	}
	tick.Stop()
	close(stop)
	wg.Wait()
	time.Sleep(2 * time.Second)
	runtime.GC()
	goroutinesAfter := runtime.NumGoroutine()
	fdsAfter := benchOpenFDs()

	// Slope over the samples after the first 10% (warm-up).
	tail := samples[len(samples)/10:]
	slope := benchSlopeMBh(tail)
	s, r := sent.Load(), recv.Load()
	loss := 0.0
	if s > 0 {
		loss = 100 * float64(s-r) / float64(s)
	}
	t.Logf("heap slope %.3f MB/h, goroutines %d -> %d, fds %d -> %d, sent %d, received %d (loss %.3f%%)",
		slope, goroutinesBefore, goroutinesAfter, fdsBefore, fdsAfter, s, r, loss)

	out := os.Getenv("NUVIRA_BENCH_SOAK_OUT")
	if out == "" {
		return
	}
	metric := func(unit, better, gate string, v float64) map[string]any {
		return map[string]any{"unit": unit, "better": better, "gate": gate, "values": []float64{v}}
	}
	doc := map[string]any{
		"schema": "nuvira-bench/1",
		"run": map[string]any{
			"id": "go-soak-" + start.UTC().Format("2006-01-02T15-04-05Z"), "side": os.Getenv("NUVIRA_BENCH_SIDE"),
			"suite": "go-soak", "toolchain": map[string]any{"go": runtime.Version()},
			"machine": map[string]any{"os": runtime.GOOS, "arch": runtime.GOARCH, "threads": runtime.NumCPU()},
			"started": start.UTC().Format(time.RFC3339), "finished": time.Now().UTC().Format(time.RFC3339),
		},
		"results": []any{map[string]any{
			"id": "B-RES-05", "case": fmt.Sprintf("relay-soak/pairs=%d/pps=%d", pairsN, pps), "level": "res",
			"status": "ok",
			"params": map[string]any{"pairs": pairsN, "pps": pps, "seconds": duration.Seconds(), "sample_s": every.Seconds()},
			"metrics": map[string]any{
				"heap_slope_mb_h":  metric("MB/h", "lower", "resource", slope),
				"goroutines_delta": metric("1", "lower", "resource", float64(goroutinesAfter-goroutinesBefore)),
				"fds_delta":        metric("1", "lower", "resource", float64(fdsAfter-fdsBefore)),
				"loss_pct":         metric("%", "lower", "info", loss),
				"heap_inuse_mb":    metric("MB", "lower", "info", float64(samples[len(samples)-1].HeapInuse)/(1<<20)),
			},
			"samples": samples,
			"notes":   []string{"fds = -1: not measured on this OS"},
		}},
	}
	data, err := json.MarshalIndent(doc, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, data, 0o644); err != nil {
		t.Fatal(err)
	}
}
