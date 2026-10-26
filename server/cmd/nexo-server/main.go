// nexo-server: segnalazione e relay di Nexo.
//
// Configurazione da variabili d'ambiente:
//
//	NEXO_LISTEN    indirizzo UDP (predefinito ":21116")
//	NEXO_WS_LISTEN indirizzo TCP per il ripiego WebSocket, percorso /nexo
//	               (predefinito ":21116"; "off" per disattivarlo). Da pubblicare
//	               anche come wss:// su 443 dietro un reverse proxy.
//	NEXO_DATA_DIR  cartella per database e certificato (predefinito "./data")
//	NEXO_LOG       debug | info (predefinito) | warn
//	NEXO_RELAY_FAKE_REFLEXIVE  solo test: comunica indirizzi pubblici irraggiungibili
//	                   (simula due dispositivi dietro lo stesso router)
//	NEXO_RELAY_IMPAIR  solo test: simula una rete peggiore sul relay, es.
//	                   "delay=40ms,jitter=10ms,loss=1%,rate=3mbit,queue=150ms"
//
// `nexo-server healthcheck` verifica che il server risponda (usato da Docker).
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
	if len(os.Args) > 1 && os.Args[1] == "healthcheck" {
		if err := healthcheck(); err != nil {
			fmt.Fprintln(os.Stderr, "non risponde:", err)
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
	if os.Getenv("NEXO_RELAY_FAKE_REFLEXIVE") != "" {
		rl.FakeReflexive = true
		log.Warn("indirizzi visti dal relay falsificati: simulazione hairpin (solo test)")
	}
	conn := relay.NewConn(udp, rl)
	impairment, err := relay.ParseImpairment(os.Getenv("NEXO_RELAY_IMPAIR"))
	if err != nil {
		return fmt.Errorf("NEXO_RELAY_IMPAIR: %w", err)
	}
	if impairment != nil {
		conn.Impair(impairment)
		log.Warn("simulazione di rete attiva sul relay (solo test)", "condizioni", impairment.String())
	}
	tr := &quic.Transport{Conn: conn}

	if wsListen := env("NEXO_WS_LISTEN", ":21116"); wsListen != "off" {
		conn.WebSockets().SetLogger(log)
		mux := http.NewServeMux()
		mux.Handle("/nexo", conn.WebSockets())
		wsServer := &http.Server{Addr: wsListen, Handler: mux, ReadHeaderTimeout: 10 * time.Second}
		go func() {
			if err := wsServer.ListenAndServe(); err != nil && err != http.ErrServerClosed {
				log.Error("ripiego WebSocket non disponibile", "err", err)
			}
		}()
		defer wsServer.Close()
		log.Info("ripiego WebSocket attivo", "tcp", wsListen, "percorso", "/nexo")
	}
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

// healthcheck completa un handshake QUIC con il server locale.
func healthcheck() error {
	_, port, err := net.SplitHostPort(env("NEXO_LISTEN", ":21116"))
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
