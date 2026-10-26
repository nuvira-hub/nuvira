// nexo-server: segnalazione e relay di Nexo.
//
// Configurazione da variabili d'ambiente:
//
//	NEXO_LISTEN    indirizzo UDP (predefinito ":21116")
//	NEXO_DATA_DIR  cartella per database e certificato (predefinito "./data")
//	NEXO_LOG       debug | info (predefinito) | warn
package main

import (
	"context"
	"crypto/tls"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/quic-go/quic-go"

	"github.com/alexbn71/Nexo/server/internal/relay"
	nexosignal "github.com/alexbn71/Nexo/server/internal/signal"
	"github.com/alexbn71/Nexo/server/internal/store"
	"github.com/alexbn71/Nexo/server/internal/tlsutil"
)

var version = "dev"

const copyright = "Nexo © 2026 Alessandro Blason · AGPL-3.0 · https://github.com/alexbn71/Nexo"

// ALPN della segnalazione (diverso da quello delle sessioni tra dispositivi).
const alpn = "nexo-signal/1"

func env(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func main() {
	// I buffer UDP sono impostati da noi sul socket condiviso con il relay.
	_ = os.Setenv("QUIC_GO_DISABLE_RECEIVE_BUFFER_WARNING", "true")
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "errore:", err)
		os.Exit(1)
	}
}

func run() error {
	level := slog.LevelInfo
	_ = level.UnmarshalText([]byte(env("NEXO_LOG", "info")))
	log := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: level}))
	fmt.Println(copyright)

	dataDir := env("NEXO_DATA_DIR", "./data")
	if err := os.MkdirAll(dataDir, 0o700); err != nil {
		return err
	}
	st, err := store.Open(filepath.Join(dataDir, "nexo.db"))
	if err != nil {
		return fmt.Errorf("database: %w", err)
	}
	defer st.Close()

	cert, err := tlsutil.LoadOrCreate(dataDir)
	if err != nil {
		return fmt.Errorf("certificato: %w", err)
	}

	addr, err := net.ResolveUDPAddr("udp", env("NEXO_LISTEN", ":21116"))
	if err != nil {
		return err
	}
	udp, err := net.ListenUDP("udp", addr)
	if err != nil {
		return err
	}
	// Buffer ampi: con molte sessioni in relay i datagrammi arrivano a raffiche.
	_ = udp.SetReadBuffer(7 << 20)
	_ = udp.SetWriteBuffer(7 << 20)
	rl := relay.New(log)
	tr := &quic.Transport{Conn: relay.NewConn(udp, rl)}
	ln, err := tr.Listen(
		&tls.Config{Certificates: []tls.Certificate{cert}, NextProtos: []string{alpn}, MinVersion: tls.VersionTLS13},
		&quic.Config{KeepAlivePeriod: 10 * time.Second, MaxIdleTimeout: 30 * time.Second},
	)
	if err != nil {
		return err
	}

	log.Info("nexo-server avviato", "versione", version, "udp", udp.LocalAddr(), "impronta", tlsutil.Fingerprint(cert))

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	srv := nexosignal.New(st, rl, log)
	go func() {
		<-ctx.Done()
		ln.Close()
	}()
	if err := srv.Serve(ctx, ln); err != nil && ctx.Err() == nil {
		return err
	}
	log.Info("nexo-server fermato")
	return nil
}
