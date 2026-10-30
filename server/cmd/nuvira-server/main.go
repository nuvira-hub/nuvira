// nuvira-server: NuviraHub signalling and relay.
//
// Configuration from environment variables:
//
//	NUVIRA_LISTEN    UDP address (default ":21116")
//	NUVIRA_WS_LISTEN TCP address for the WebSocket fallback, path /nuvira
//	               (default ":21116"; "off" to disable it). Also to be published
//	               as wss:// on 443 behind a reverse proxy.
//	NUVIRA_DATA_DIR  folder for database and certificate (default "./data")
//	NUVIRA_LOG       debug | info (default) | warn
//	NUVIRA_RELAY_FAKE_REFLEXIVE  tests only: announces unreachable public addresses
//	                   (simulates two devices behind the same router)
//	NUVIRA_RELAY_IMPAIR  tests only: simulates a worse network on the relay, e.g.
//	                   "delay=40ms,jitter=10ms,loss=1%,rate=3mbit,queue=150ms"
//
// Builds with modules (NuviraHub Pro) register them with package ext and read
// their own variables.
//
// `nuvira-server healthcheck` checks that the server answers (used by Docker);
// modules can add maintenance commands (`nuvira-server help` lists them).
package main

import (
	"context"
	"crypto/tls"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/quic-go/quic-go"

	"github.com/nuvira-hub/nuvira/server/internal/ext"
	"github.com/nuvira-hub/nuvira/server/internal/relay"
	nuvirasignal "github.com/nuvira-hub/nuvira/server/internal/signal"
	"github.com/nuvira-hub/nuvira/server/internal/store"
	"github.com/nuvira-hub/nuvira/server/internal/tlsutil"
	"github.com/nuvira-hub/nuvira/server/internal/updates"
)

var version = "dev"

// "open", or set by builds with modules (NuviraHub Pro).
var edition = "open"

const copyright = "NuviraHub © 2026 Alessandro Blason · AGPL-3.0 · https://github.com/nuvira-hub/nuvira"

// Signalling ALPN (different from the one of sessions between devices).
const alpn = "nuvira-signal/1"

func env(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func main() {
	// The UDP buffers are set by us on the socket shared with the relay.
	_ = os.Setenv("QUIC_GO_DISABLE_RECEIVE_BUFFER_WARNING", "true")
	if len(os.Args) > 1 && os.Args[1] == "healthcheck" {
		if err := healthcheck(); err != nil {
			fmt.Fprintln(os.Stderr, "not answering:", err)
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 {
		os.Exit(command(os.Args[1], os.Args[2:]))
	}
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

func run() error {
	level := slog.LevelInfo
	_ = level.UnmarshalText([]byte(env("NUVIRA_LOG", "info")))
	log := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: level}))
	fmt.Println(copyright)

	dataDir := env("NUVIRA_DATA_DIR", "./data")
	if err := os.MkdirAll(dataDir, 0o700); err != nil {
		return err
	}
	st, err := store.Open(filepath.Join(dataDir, "nuvira.db"))
	if err != nil {
		return fmt.Errorf("database: %w", err)
	}
	defer st.Close()

	cert, err := tlsutil.LoadOrCreate(dataDir)
	if err != nil {
		return fmt.Errorf("certificate: %w", err)
	}

	addr, err := net.ResolveUDPAddr("udp", env("NUVIRA_LISTEN", ":21116"))
	if err != nil {
		return err
	}
	udp, err := net.ListenUDP("udp", addr)
	if err != nil {
		return err
	}
	// Large buffers: with many relayed sessions datagrams arrive in bursts.
	_ = udp.SetReadBuffer(7 << 20)
	_ = udp.SetWriteBuffer(7 << 20)
	rl := relay.New(log)
	if os.Getenv("NUVIRA_RELAY_FAKE_REFLEXIVE") != "" {
		rl.FakeReflexive = true
		log.Warn("addresses seen by the relay faked: hairpin simulation (tests only)")
	}
	conn := relay.NewConn(udp, rl)
	impairment, err := relay.ParseImpairment(os.Getenv("NUVIRA_RELAY_IMPAIR"))
	if err != nil {
		return fmt.Errorf("NUVIRA_RELAY_IMPAIR: %w", err)
	}
	if impairment != nil {
		conn.Impair(impairment)
		log.Warn("network simulation active on the relay (tests only)", "conditions", impairment.String())
	}
	tr := &quic.Transport{Conn: conn}

	srv := nuvirasignal.New(st, rl, log)
	modules, err := ext.Open(ext.Env{DB: st.DB(), DataDir: dataDir, Log: log, Host: srv})
	if err != nil {
		return fmt.Errorf("modules: %w", err)
	}
	srv.SetModules(modules)

	if wsListen := env("NUVIRA_WS_LISTEN", ":21116"); wsListen != "off" {
		conn.WebSockets().SetLogger(log)
		mux := http.NewServeMux()
		mux.Handle("/nuvira", conn.WebSockets())
		mux.Handle(updates.Prefix, updates.Handler(filepath.Join(dataDir, "updates")))
		for _, m := range modules {
			m.Routes(mux)
		}
		wsServer := &http.Server{Addr: wsListen, Handler: mux, ReadHeaderTimeout: 10 * time.Second}
		go func() {
			if err := wsServer.ListenAndServe(); err != nil && err != http.ErrServerClosed {
				log.Error("WebSocket fallback not available", "err", err)
			}
		}()
		defer wsServer.Close()
		log.Info("WebSocket fallback active", "tcp", wsListen, "path", "/nuvira")
		log.Info("updates served", "path", updates.Prefix, "dir", filepath.Join(dataDir, "updates"))
	}
	ln, err := tr.Listen(
		&tls.Config{Certificates: []tls.Certificate{cert}, NextProtos: []string{alpn}, MinVersion: tls.VersionTLS13},
		&quic.Config{KeepAlivePeriod: 10 * time.Second, MaxIdleTimeout: 30 * time.Second},
	)
	if err != nil {
		return err
	}

	log.Info("nuvira-server started", "version", version, "edition", edition, "udp", udp.LocalAddr(), "fingerprint", tlsutil.Fingerprint(cert))

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	go func() {
		<-ctx.Done()
		ln.Close()
	}()
	if err := srv.Serve(ctx, ln); err != nil && ctx.Err() == nil {
		return err
	}
	log.Info("nuvira-server stopped")
	return nil
}

// command runs a maintenance command of a module, on the same data as the server.
func command(name string, args []string) int {
	for _, c := range ext.Commands() {
		if c.Name != name {
			continue
		}
		dataDir := env("NUVIRA_DATA_DIR", "./data")
		st, err := store.Open(filepath.Join(dataDir, "nuvira.db"))
		if err != nil {
			fmt.Fprintln(os.Stderr, "database:", err)
			return 1
		}
		defer st.Close()
		log := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelWarn}))
		if err := c.Run(ext.Env{DB: st.DB(), DataDir: dataDir, Log: log}, args, os.Stdout); err != nil {
			fmt.Fprintln(os.Stderr, "error:", err)
			return 1
		}
		return 0
	}
	fmt.Fprintln(os.Stderr, "nuvira-server", version, "("+edition+")")
	fmt.Fprintln(os.Stderr, "\nCommands:\n  healthcheck")
	for _, c := range ext.Commands() {
		fmt.Fprintln(os.Stderr, " ", c.Name, c.Usage)
	}
	if name == "help" || name == "-h" || name == "--help" {
		return 0
	}
	fmt.Fprintf(os.Stderr, "\nunknown command %q\n", name)
	return 2
}

// healthcheck completes a QUIC handshake with the local server.
func healthcheck() error {
	_, port, err := net.SplitHostPort(env("NUVIRA_LISTEN", ":21116"))
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	conn, err := quic.DialAddr(ctx, net.JoinHostPort("127.0.0.1", port),
		&tls.Config{InsecureSkipVerify: true, NextProtos: []string{alpn}}, nil)
	if err != nil {
		return err
	}
	return conn.CloseWithError(0, "")
}
