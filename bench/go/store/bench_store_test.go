// Benchmarks of the SQLite device store (bench suite, B-GO-08).
//
// This file lives in nuvira/bench/go/store/ and is copied by
// bench/scripts/bench-compare.sh into server/internal/store/ of each side's
// worktree. External test package: only exported identifiers.
package store_test

import (
	"crypto/sha256"
	"encoding/binary"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/pb"
	"github.com/nuvira-hub/nuvira/server/internal/relay"
	"github.com/nuvira-hub/nuvira/server/internal/signal"
	"github.com/nuvira-hub/nuvira/server/internal/store"
)

// benchPublicKey is a deterministic 32-byte key (the store does not check it).
func benchPublicKey(i int) []byte {
	var b [8]byte
	binary.BigEndian.PutUint64(b[:], uint64(i))
	sum := sha256.Sum256(append([]byte("nuvira-bench-key"), b[:]...))
	return sum[:]
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

func benchOpen(tb testing.TB) *store.Store {
	tb.Helper()
	st, err := store.Open(filepath.Join(benchTempDir(tb), "nuvira.db"))
	if err != nil {
		tb.Fatal(err)
	}
	tb.Cleanup(func() { _ = st.Close() })
	return st
}

// benchPopulate registers n devices, the first ones with an alias.
func benchPopulate(tb testing.TB, st *store.Store, n, aliases int) (ids, names []string) {
	tb.Helper()
	for i := range n {
		d, err := st.Register(benchPublicKey(i), fmt.Sprintf("pc-%d", i), "windows")
		if err != nil {
			tb.Fatal(err)
		}
		ids = append(ids, d.ID)
		if i < aliases {
			alias := fmt.Sprintf("bench-host-%d", i)
			if _, err := st.SetAlias(d.ID, alias); err != nil {
				tb.Fatal(err)
			}
			names = append(names, alias)
		}
	}
	return ids, names
}

// BENCH-ID: B-GO-08
func BenchmarkStore(b *testing.B) {
	const devices, aliases = 200, 64
	b.Run("register-new", func(b *testing.B) {
		st := benchOpen(b)
		benchPopulate(b, st, devices, 0)
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			if _, err := st.Register(benchPublicKey(devices+i), "pc", "windows"); err != nil {
				b.Fatal(err)
			}
		}
	})
	st := benchOpen(b)
	ids, names := benchPopulate(b, st, devices, aliases)
	b.Run("register-known", func(b *testing.B) {
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			k := i % devices
			if _, err := st.Register(benchPublicKey(k), fmt.Sprintf("pc-%d", k), "windows"); err != nil {
				b.Fatal(err)
			}
		}
	})
	b.Run("by-id", func(b *testing.B) {
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			d, err := st.ByID(ids[i%devices])
			if err != nil || d == nil {
				b.Fatal("device not found", err)
			}
		}
	})
	b.Run("resolve-id", func(b *testing.B) {
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			if st.ResolveTarget(ids[i%devices]) == "" {
				b.Fatal("not resolved")
			}
		}
	})
	b.Run("resolve-alias", func(b *testing.B) {
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			if st.ResolveTarget(names[i%aliases]) == "" {
				b.Fatal("not resolved")
			}
		}
	})
	b.Run("resolve-unknown-alias", func(b *testing.B) {
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			if st.ResolveTarget("bench-nobody") != "" {
				b.Fatal("resolved an unknown alias")
			}
		}
	})
	b.Run("set-alias", func(b *testing.B) {
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			k := aliases + i%(devices-aliases)
			if _, err := st.SetAlias(ids[k], fmt.Sprintf("bench-alias-%d-%d", k, i&1)); err != nil {
				b.Fatal(err)
			}
		}
	})
	b.Run("touch", func(b *testing.B) {
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			if err := st.Touch(ids[i%devices]); err != nil {
				b.Fatal(err)
			}
		}
	})
	// Last access of the 256 devices of an address book (the 200 registered and
	// 56 unknown IDs), as Server.Presence fills them for the Pro books (SRV-7;
	// GetPresence does the same per request of the apps). Exported API, present
	// on both sides of a comparison.
	b.Run("presence-256", func(b *testing.B) {
		log := slog.New(slog.NewTextHandler(io.Discard, nil))
		srv := signal.New(st, relay.New(log), log)
		book := &pb.Book{}
		for i := range 256 {
			id := fmt.Sprintf("%09d", 100000000+i)
			if i < devices {
				id = ids[i]
			}
			book.Entries = append(book.Entries, &pb.Entry{DeviceId: id})
		}
		books := &pb.Books{Books: []*pb.Book{book}}
		b.ReportAllocs()
		b.ResetTimer()
		for i := 0; i < b.N; i++ {
			srv.Presence(books)
			if book.Entries[0].LastSeen == 0 {
				b.Fatal("no last access")
			}
		}
	})
}
