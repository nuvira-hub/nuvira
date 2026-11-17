// Benchmarks of the signalling server (bench suite, B-GO-06 and B-GO-07).
//
// This file lives in nuvira/bench/go/signal/ and is copied by
// bench/scripts/bench-compare.sh into server/internal/signal/ of each side's
// worktree. External test package: only exported identifiers, the generated pb
// messages and the wire constants written as literals (ALPN and the prefix of
// the registration signature, cross-checked by TestBenchSignalLiterals).
package signal_test

import (
	"bufio"
	"context"
	"crypto/ed25519"
	"crypto/tls"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"testing"
	"time"

	"github.com/quic-go/quic-go"
	"google.golang.org/protobuf/encoding/protodelim"

	"github.com/nuvira-hub/nuvira/server/internal/pb"
	"github.com/nuvira-hub/nuvira/server/internal/relay"
	"github.com/nuvira-hub/nuvira/server/internal/signal"
	"github.com/nuvira-hub/nuvira/server/internal/store"
	"github.com/nuvira-hub/nuvira/server/internal/tlsutil"
)

const (
	benchALPN            = "nuvira-signal/1"    // cmd/nuvira-server/main.go
	benchRegisterContext = "nuvira-register-v1" // signal.go registerContext
)

var benchLogOnce sync.Once

func benchLog() *slog.Logger {
	l := slog.New(slog.NewTextHandler(io.Discard, nil))
	// The first record loads the time zone (slow on Windows): never inside a timed region.
	benchLogOnce.Do(func() { l.Info("bench warm-up") })
	return l
}

// benchTempDir is like tb.TempDir, but a failed removal (the antivirus still
// scanning the database on Windows) does not fail the benchmark.
func benchTempDir(tb testing.TB) string {
	tb.Helper()
	dir, err := os.MkdirTemp("", "nuvira-bench-")
	if err != nil {
		tb.Fatal(err)
	}
	tb.Cleanup(func() {
		for range 10 {
			if os.RemoveAll(dir) == nil {
				return
			}
			time.Sleep(100 * time.Millisecond)
		}
	})
	return dir
}

// benchServer starts the signalling server as cmd/nuvira-server does, on
// 127.0.0.1 with a fresh data directory and the rate limits off.
func benchServer(tb testing.TB) (string, *signal.Server) {
	tb.Helper()
	dir := benchTempDir(tb)
	st, err := store.Open(filepath.Join(dir, "nuvira.db"))
	if err != nil {
		tb.Fatal(err)
	}
	cert, err := tlsutil.LoadOrCreate(dir)
	if err != nil {
		tb.Fatal(err)
	}
	udp, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		tb.Fatal(err)
	}
	log := benchLog()
	rl := relay.New(log)
	conn := relay.NewConn(udp, rl)
	tr := &quic.Transport{Conn: conn}
	ln, err := tr.Listen(
		&tls.Config{Certificates: []tls.Certificate{cert}, NextProtos: []string{benchALPN}, MinVersion: tls.VersionTLS13},
		&quic.Config{KeepAlivePeriod: 10 * time.Second, MaxIdleTimeout: 30 * time.Second},
	)
	if err != nil {
		tb.Fatal(err)
	}
	srv := signal.New(st, rl, log)
	srv.SetRateLimits(false)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		defer close(done)
		_ = srv.Serve(ctx, ln)
	}()
	tb.Cleanup(func() {
		cancel()
		_ = ln.Close()
		<-done
		// The transport does not own conn: close it first, or Close waits for
		// its reader, blocked in conn.ReadFrom.
		_ = conn.Close()
		_ = tr.Close()
		_ = st.Close()
	})
	return udp.LocalAddr().String(), srv
}

type benchClient struct {
	conn   *quic.Conn
	stream *quic.Stream
	r      *bufio.Reader
	id     string
}

func (c *benchClient) send(msg *pb.ClientMessage) error {
	_, err := protodelim.MarshalTo(c.stream, msg)
	return err
}

func (c *benchClient) read() (*pb.ServerMessage, error) {
	msg := &pb.ServerMessage{}
	err := protodelim.UnmarshalOptions{MaxSize: 64 * 1024}.UnmarshalFrom(c.r, msg)
	return msg, err
}

// readUntil skips the other messages (policy, alias state...) until want
// returns true; a server Error stops it.
func (c *benchClient) readUntil(want func(*pb.ServerMessage) bool) (*pb.ServerMessage, error) {
	for range 32 {
		msg, err := c.read()
		if err != nil {
			return nil, err
		}
		if e := msg.GetError(); e != nil {
			return nil, fmt.Errorf("server error: %s", e.Message)
		}
		if want(msg) {
			return msg, nil
		}
	}
	return nil, errors.New("expected message not received")
}

func (c *benchClient) close() { _ = c.conn.CloseWithError(0, "") }

// benchRegister connects and registers as the app does: Register, Challenge,
// Proof (Ed25519 over prefix + nonce), Registered.
func benchRegister(ctx context.Context, addr string, priv ed25519.PrivateKey, name string) (*benchClient, error) {
	conn, err := quic.DialAddr(ctx, addr,
		&tls.Config{InsecureSkipVerify: true, NextProtos: []string{benchALPN}},
		&quic.Config{KeepAlivePeriod: 2 * time.Second, MaxIdleTimeout: 30 * time.Second})
	if err != nil {
		return nil, err
	}
	stream, err := conn.OpenStreamSync(ctx)
	if err != nil {
		_ = conn.CloseWithError(0, "")
		return nil, err
	}
	c := &benchClient{conn: conn, stream: stream, r: bufio.NewReader(stream)}
	fail := func(err error) (*benchClient, error) {
		c.close()
		return nil, err
	}
	pub := priv.Public().(ed25519.PublicKey)
	if err := c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Register{Register: &pb.Register{
		PublicKey: pub, DeviceName: name, Os: "bench", AppVersion: "bench", ProtocolVersion: signal.ProtocolVersion,
	}}}); err != nil {
		return fail(err)
	}
	msg, err := c.readUntil(func(m *pb.ServerMessage) bool { return m.GetChallenge() != nil })
	if err != nil {
		return fail(err)
	}
	signed := append([]byte(benchRegisterContext), msg.GetChallenge().Nonce...)
	if err := c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Proof{Proof: &pb.Proof{
		Signature: ed25519.Sign(priv, signed),
	}}}); err != nil {
		return fail(err)
	}
	msg, err = c.readUntil(func(m *pb.ServerMessage) bool { return m.GetRegistered() != nil })
	if err != nil {
		return fail(err)
	}
	c.id = msg.GetRegistered().DeviceId
	return c, nil
}

// benchKey is a deterministic Ed25519 key.
func benchKey(i int) ed25519.PrivateKey {
	seed := make([]byte, ed25519.SeedSize)
	for j := range seed {
		seed[j] = byte(i>>(8*(j%4))) ^ byte(j*31+7)
	}
	return ed25519.NewKeyFromSeed(seed)
}

func benchP50ms(d []time.Duration) float64 {
	if len(d) == 0 {
		return 0
	}
	sort.Slice(d, func(i, j int) bool { return d[i] < d[j] })
	return float64(d[len(d)/2]) / 1e6
}

// TestBenchSignalLiterals checks the literals: a registration must succeed.
func TestBenchSignalLiterals(t *testing.T) {
	addr, _ := benchServer(t)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	c, err := benchRegister(ctx, addr, benchKey(1), "bench")
	if err != nil {
		t.Fatal(err)
	}
	defer c.close()
	if len(c.id) != 9 {
		t.Fatalf("device ID %q", c.id)
	}
}

// BENCH-ID: B-GO-06
// BENCH-METRIC: regs/s higher macro_throughput
// BENCH-METRIC: p50-ms lower macro_latency
//
// One operation = a new QUIC connection + Register/Challenge/Proof/Registered +
// close. "new-device" uses a new key each time (store insert), "known-device"
// one of 64 keys already registered (store update).
func BenchmarkSignalRegister(b *testing.B) {
	for _, mode := range []string{"new-device", "known-device"} {
		b.Run(mode, func(b *testing.B) {
			addr, _ := benchServer(b)
			ctx := context.Background()
			const known = 64
			var keys []ed25519.PrivateKey
			if mode == "known-device" {
				for i := range known {
					keys = append(keys, benchKey(i))
					c, err := benchRegister(ctx, addr, keys[i], "bench")
					if err != nil {
						b.Fatal(err)
					}
					c.close()
				}
			} else {
				keys = make([]ed25519.PrivateKey, b.N)
				for i := range keys {
					keys[i] = benchKey(1_000_000 + i)
				}
			}
			times := make([]time.Duration, 0, b.N)
			b.ReportAllocs()
			b.ResetTimer()
			start := time.Now()
			for i := 0; i < b.N; i++ {
				t0 := time.Now()
				c, err := benchRegister(ctx, addr, keys[i%len(keys)], "bench")
				if err != nil {
					b.Fatal(err)
				}
				times = append(times, time.Since(t0))
				c.close()
			}
			elapsed := time.Since(start)
			b.StopTimer()
			b.ReportMetric(float64(b.N)/elapsed.Seconds(), "regs/s")
			b.ReportMetric(benchP50ms(times), "p50-ms")
		})
	}
}

// BENCH-ID: B-GO-07
// BENCH-METRIC: p50-ms lower macro_latency
//
// Connect flow between two registered, connected devices: viewer Connect ->
// agent Incoming -> agent Accept -> Route to both sides.
func BenchmarkSignalConnect(b *testing.B) {
	addr, srv := benchServer(b)
	ctx := context.Background()
	agent, err := benchRegister(ctx, addr, benchKey(1), "bench-agent")
	if err != nil {
		b.Fatal(err)
	}
	defer agent.close()
	viewer, err := benchRegister(ctx, addr, benchKey(2), "bench-viewer")
	if err != nil {
		b.Fatal(err)
	}
	defer viewer.close()
	connect := func() error {
		if err := viewer.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Connect{Connect: &pb.Connect{DeviceId: agent.id}}}); err != nil {
			return err
		}
		in, err := agent.readUntil(func(m *pb.ServerMessage) bool { return m.GetIncoming() != nil })
		if err != nil {
			return err
		}
		id := in.GetIncoming().SessionId
		if err := agent.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Accept{Accept: &pb.Accept{SessionId: id}}}); err != nil {
			return err
		}
		if _, err := agent.readUntil(func(m *pb.ServerMessage) bool { return m.GetRoute() != nil }); err != nil {
			return err
		}
		r, err := viewer.readUntil(func(m *pb.ServerMessage) bool { return m.GetRoute() != nil })
		if err != nil {
			return err
		}
		if r.GetRoute().SessionId != id || len(r.GetRoute().RelayToken) != 16 {
			return fmt.Errorf("unexpected route %v", r.GetRoute())
		}
		return nil
	}
	// The agent is listed as online right after its Registered is sent.
	for i := 0; !srv.Online(agent.id); i++ {
		if i == 500 {
			b.Fatal("agent never online")
		}
		time.Sleep(10 * time.Millisecond)
	}
	if err := connect(); err != nil {
		b.Fatal(err)
	}
	times := make([]time.Duration, 0, b.N)
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		t0 := time.Now()
		if err := connect(); err != nil {
			b.Fatal(err)
		}
		times = append(times, time.Since(t0))
	}
	b.StopTimer()
	b.ReportMetric(benchP50ms(times), "p50-ms")
}
