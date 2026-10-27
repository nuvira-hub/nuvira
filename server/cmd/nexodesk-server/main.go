// nexodesk-server: NexoDesk signalling and relay.
//
// Configuration from environment variables:
//
//	NEXODESK_LISTEN    UDP address (default ":21116")
//	NEXODESK_WS_LISTEN TCP address for the WebSocket fallback, path /nexodesk
//	               (default ":21116"; "off" to disable it). Also to be published
//	               as wss:// on 443 behind a reverse proxy.
//	NEXODESK_DATA_DIR  folder for database and certificate (default "./data")
//	NEXODESK_LOG       debug | info (default) | warn
//	NEXODESK_ADMIN_USER, NEXODESK_ADMIN_PASSWORD  first address book administrator,
//	                   created only if no account exists (default "admin" with a
//	                   random password written once to the log)
//	NEXODESK_RELAY_FAKE_REFLEXIVE  tests only: announces unreachable public addresses
//	                   (simulates two devices behind the same router)
//	NEXODESK_RELAY_IMPAIR  tests only: simulates a worse network on the relay, e.g.
//	                   "delay=40ms,jitter=10ms,loss=1%,rate=3mbit,queue=150ms"
//
// `nexodesk-server healthcheck` checks that the server answers (used by Docker).
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

	"github.com/alexbn71/NexoDesk/server/internal/accounts"
	"github.com/alexbn71/NexoDesk/server/internal/relay"
	nexosignal "github.com/alexbn71/NexoDesk/server/internal/signal"
	"github.com/alexbn71/NexoDesk/server/internal/store"
	"github.com/alexbn71/NexoDesk/server/internal/tlsutil"
)

var version = "dev"

const copyright = "NexoDesk © 2026 Alessandro Blason · AGPL-3.0 · https://github.com/alexbn71/NexoDesk"

// Signalling ALPN (different from the one of sessions between devices).
const alpn = "nexodesk-signal/1"

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
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "errore:", err)
		os.Exit(1)
	}
}

func run() error {
	level := slog.LevelInfo
	_ = level.UnmarshalText([]byte(env("NEXODESK_LOG", "info")))
	log := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: level}))
	fmt.Println(copyright)

	dataDir := env("NEXODESK_DATA_DIR", "./data")
	if err := os.MkdirAll(dataDir, 0o700); err != nil {
		return err
	}
	st, err := store.Open(filepath.Join(dataDir, "nexodesk.db"))
	if err != nil {
		return fmt.Errorf("database: %w", err)
	}
	defer st.Close()

	cert, err := tlsutil.LoadOrCreate(dataDir)
	if err != nil {
		return fmt.Errorf("certificate: %w", err)
	}

	addr, err := net.ResolveUDPAddr("udp", env("NEXODESK_LISTEN", ":21116"))
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
	if os.Getenv("NEXODESK_RELAY_FAKE_REFLEXIVE") != "" {
		rl.FakeReflexive = true
		log.Warn("addresses seen by the relay faked: hairpin simulation (tests only)")
	}
	conn := relay.NewConn(udp, rl)
	impairment, err := relay.ParseImpairment(os.Getenv("NEXODESK_RELAY_IMPAIR"))
	if err != nil {
		return fmt.Errorf("NEXODESK_RELAY_IMPAIR: %w", err)
	}
	if impairment != nil {
		conn.Impair(impairment)
		log.Warn("network simulation active on the relay (tests only)", "conditions", impairment.String())
	}
	tr := &quic.Transport{Conn: conn}

	if wsListen := env("NEXODESK_WS_LISTEN", ":21116"); wsListen != "off" {
		conn.WebSockets().SetLogger(log)
		mux := http.NewServeMux()
		mux.Handle("/nexodesk", conn.WebSockets())
		wsServer := &http.Server{Addr: wsListen, Handler: mux, ReadHeaderTimeout: 10 * time.Second}
		go func() {
			if err := wsServer.ListenAndServe(); err != nil && err != http.ErrServerClosed {
				log.Error("WebSocket fallback not available", "err", err)
			}
		}()
		defer wsServer.Close()
		log.Info("WebSocket fallback active", "tcp", wsListen, "path", "/nexodesk")
	}
	ln, err := tr.Listen(
		&tls.Config{Certificates: []tls.Certificate{cert}, NextProtos: []string{alpn}, MinVersion: tls.VersionTLS13},
		&quic.Config{KeepAlivePeriod: 10 * time.Second, MaxIdleTimeout: 30 * time.Second},
	)
	if err != nil {
		return err
	}

	log.Info("nexodesk-server started", "version", version, "udp", udp.LocalAddr(), "fingerprint", tlsutil.Fingerprint(cert))

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	acc, err := accounts.Open(st.DB(), filepath.Join(dataDir, "rubrica.key"))
	if err != nil {
		return err
	}
	created, generated, err := acc.Bootstrap(os.Getenv("NEXODESK_ADMIN_USER"), os.Getenv("NEXODESK_ADMIN_PASSWORD"))
	if err != nil {
		return fmt.Errorf("first administrator: %w", err)
	}
	if created && generated != "" {
		log.Warn("address book administrator account created: change the password from the app",
			"user", env("NEXODESK_ADMIN_USER", "admin"), "password", generated)
	} else if created {
		log.Info("address book administrator account created", "user", env("NEXODESK_ADMIN_USER", "admin"))
	}
	srv := nexosignal.New(st, acc, rl, log)
	go func() {
		<-ctx.Done()
		ln.Close()
	}()
	if err := srv.Serve(ctx, ln); err != nil && ctx.Err() == nil {
		return err
	}
	log.Info("nexodesk-server stopped")
	return nil
}

// healthcheck completes a QUIC handshake with the local server.
func healthcheck() error {
	_, port, err := net.SplitHostPort(env("NEXODESK_LISTEN", ":21116"))
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
