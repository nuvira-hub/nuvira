package signal

// Phase 2 (characterisation): an in-process signalling server over real QUIC on
// 127.0.0.1, as cmd/nuvira-server starts it, and a minimal device client. Same
// shape as bench/go/signal (B-GO-06/07), but inside the package so that tests
// can look at the server's state.

import (
	"bufio"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/tls"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/quic-go/quic-go"
	"google.golang.org/protobuf/encoding/protodelim"

	"github.com/nuvira-hub/nuvira/server/internal/ext"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
	"github.com/nuvira-hub/nuvira/server/internal/relay"
	"github.com/nuvira-hub/nuvira/server/internal/store"
	"github.com/nuvira-hub/nuvira/server/internal/tlsutil"
)

const testALPN = "nuvira-signal/1" // cmd/nuvira-server/main.go

// p2TempDir is t.TempDir without failing on a removal the antivirus delays (Windows).
func p2TempDir(tb testing.TB) string {
	tb.Helper()
	dir, err := os.MkdirTemp("", "nuvira-p2-signal-")
	if err != nil {
		tb.Fatal(err)
	}
	tb.Cleanup(func() {
		for range 20 {
			if os.RemoveAll(dir) == nil {
				return
			}
			time.Sleep(100 * time.Millisecond)
		}
	})
	return dir
}

type testServer struct {
	addr string
	// One client UDP socket for every dial (cheaper than quic.DialAddr's own
	// socket per connection, which matters to the fuzz targets).
	client *quic.Transport
	udp    *net.UDPAddr
	srv    *Server
	relay  *relay.Relay
	store  *store.Store
}

// startServer starts Serve on a fresh database; modules are set before Serve.
func startServer(tb testing.TB, modules ...ext.Module) *testServer {
	tb.Helper()
	dir := p2TempDir(tb)
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
	log := slog.New(slog.NewTextHandler(io.Discard, nil))
	rl := relay.New(log)
	conn := relay.NewConn(udp, rl)
	tr := &quic.Transport{Conn: conn}
	ln, err := tr.Listen(
		&tls.Config{Certificates: []tls.Certificate{cert}, NextProtos: []string{testALPN}, MinVersion: tls.VersionTLS13},
		&quic.Config{KeepAlivePeriod: 10 * time.Second, MaxIdleTimeout: 30 * time.Second},
	)
	if err != nil {
		tb.Fatal(err)
	}
	srv := New(st, rl, log)
	srv.SetModules(modules)
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
		_ = conn.Close()
		_ = tr.Close()
		_ = st.Close()
	})
	cu, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		tb.Fatal(err)
	}
	ctr := &quic.Transport{Conn: cu}
	tb.Cleanup(func() {
		_ = ctr.Close()
		_ = cu.Close()
	})
	return &testServer{addr: udp.LocalAddr().String(), udp: udp.LocalAddr().(*net.UDPAddr), client: ctr,
		srv: srv, relay: rl, store: st}
}

type testClient struct {
	conn   *quic.Conn
	stream *quic.Stream
	r      *bufio.Reader
	key    ed25519.PrivateKey
	id     string
}

// dial opens a QUIC connection and the signalling stream (nothing sent yet).
func (ts *testServer) dial(tb testing.TB) *testClient {
	tb.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	conn, err := ts.client.Dial(ctx, ts.udp, &tls.Config{InsecureSkipVerify: true, NextProtos: []string{testALPN}},
		&quic.Config{KeepAlivePeriod: 5 * time.Second, MaxIdleTimeout: 30 * time.Second})
	if err != nil {
		tb.Fatal(err)
	}
	tb.Cleanup(func() { _ = conn.CloseWithError(0, "") })
	st, err := conn.OpenStreamSync(ctx)
	if err != nil {
		tb.Fatal(err)
	}
	return &testClient{conn: conn, stream: st, r: bufio.NewReader(st)}
}

func (c *testClient) send(msg *pb.ClientMessage) error {
	_, err := protodelim.MarshalTo(c.stream, msg)
	return err
}

func (c *testClient) read(timeout time.Duration) (*pb.ServerMessage, error) {
	_ = c.stream.SetReadDeadline(time.Now().Add(timeout))
	msg := &pb.ServerMessage{}
	err := protodelim.UnmarshalOptions{MaxSize: maxMessage}.UnmarshalFrom(c.r, msg)
	return msg, err
}

// readUntil skips other messages until want is true; a server Error is returned
// as an error unless want accepts it.
func (c *testClient) readUntil(timeout time.Duration, want func(*pb.ServerMessage) bool) (*pb.ServerMessage, error) {
	deadline := time.Now().Add(timeout)
	for {
		left := time.Until(deadline)
		if left <= 0 {
			return nil, errors.New("timeout")
		}
		msg, err := c.read(left)
		if err != nil {
			return nil, err
		}
		if want(msg) {
			return msg, nil
		}
		if e := msg.GetError(); e != nil {
			return nil, fmt.Errorf("server error: %s", e.Message)
		}
	}
}

func registerMsg(pub ed25519.PublicKey, name string, apiOnly bool) *pb.ClientMessage {
	return &pb.ClientMessage{Msg: &pb.ClientMessage_Register{Register: &pb.Register{
		PublicKey: pub, DeviceName: name, Os: "test", AppVersion: "p2", ProtocolVersion: 2, ApiOnly: apiOnly,
	}}}
}

// register runs Register / Challenge / Proof with key (a new one if nil).
func (c *testClient) register(key ed25519.PrivateKey, name string, apiOnly bool) error {
	if key == nil {
		_, k, err := ed25519.GenerateKey(rand.Reader)
		if err != nil {
			return err
		}
		key = k
	}
	c.key = key
	if err := c.send(registerMsg(key.Public().(ed25519.PublicKey), name, apiOnly)); err != nil {
		return err
	}
	msg, err := c.read(5 * time.Second)
	if err != nil {
		return err
	}
	ch := msg.GetChallenge()
	if ch == nil {
		return fmt.Errorf("no challenge: %v", msg)
	}
	sig := ed25519.Sign(key, append([]byte(registerContext), ch.Nonce...))
	if err := c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Proof{Proof: &pb.Proof{Signature: sig}}}); err != nil {
		return err
	}
	msg, err = c.readUntil(5*time.Second, func(m *pb.ServerMessage) bool { return m.GetRegistered() != nil })
	if err != nil {
		return err
	}
	c.id = msg.GetRegistered().DeviceId
	return nil
}

// device dials and registers a device with a fresh key.
func (ts *testServer) device(tb testing.TB, name string) *testClient {
	tb.Helper()
	c := ts.dial(tb)
	if err := c.register(nil, name, false); err != nil {
		tb.Fatal(err)
	}
	return c
}

// waitOnline waits until the server lists id as online (setOnline runs after Registered is sent).
func (ts *testServer) waitOnline(tb testing.TB, id string, online bool) {
	tb.Helper()
	for range 200 {
		if ts.srv.Online(id) == online {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	tb.Fatalf("device %s online=%v never reached", id, online)
}
