package store

import (
	"bytes"
	"database/sql"
	"errors"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

func TestRegisterKeepsID(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "nuvira.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()

	key := bytes.Repeat([]byte{7}, 32)
	a, err := s.Register(key, "pc", "windows")
	if err != nil {
		t.Fatal(err)
	}
	if len(a.ID) != 9 || a.ID[0] == '0' {
		t.Fatalf("invalid ID: %q", a.ID)
	}
	b, err := s.Register(key, "renamed pc", "windows")
	if err != nil {
		t.Fatal(err)
	}
	if a.ID != b.ID {
		t.Fatalf("the same key must have the same ID: %s != %s", a.ID, b.ID)
	}
	other, err := s.Register(bytes.Repeat([]byte{8}, 32), "altro", "linux")
	if err != nil {
		t.Fatal(err)
	}
	if other.ID == a.ID {
		t.Fatal("different keys with the same ID")
	}

	got, err := s.ByID(a.ID)
	if err != nil || got == nil || got.Name != "renamed pc" || !bytes.Equal(got.PublicKey, key) {
		t.Fatalf("ByID: %+v, %v", got, err)
	}
	if missing, err := s.ByID("123456789"); err != nil || missing != nil {
		t.Fatalf("missing ID: %+v, %v", missing, err)
	}
}

func TestBlocks(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "nuvira.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if err := s.SetBlock("123456789", "scam"); err != nil {
		t.Fatal(err)
	}
	if err := s.SetBlock("123456789", "scam, reported twice"); err != nil {
		t.Fatal(err)
	}
	list, err := s.Blocks()
	if err != nil || len(list) != 1 || list[0].Note != "scam, reported twice" {
		t.Fatalf("%v %+v", err, list)
	}
	if found, _ := Wrap(s.DB()).RemoveBlock("123456789"); !found {
		t.Fatal("block not removed")
	}
	if found, _ := s.RemoveBlock("123456789"); found {
		t.Fatal("removed twice")
	}
}

func TestAliases(t *testing.T) {
	path := filepath.Join(t.TempDir(), "nuvira.db")
	// A database of an earlier version, without the column.
	old, err := sql.Open("sqlite", "file:"+path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := old.Exec(`CREATE TABLE devices (id TEXT PRIMARY KEY, public_key BLOB NOT NULL UNIQUE,
		name TEXT NOT NULL DEFAULT '', os TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL, last_seen INTEGER NOT NULL)`); err != nil {
		t.Fatal(err)
	}
	old.Close()
	s, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	a, _ := s.Register(bytes.Repeat([]byte{1}, 32), "a", "linux")
	b, _ := s.Register(bytes.Repeat([]byte{2}, 32), "b", "linux")

	if got, err := s.SetAlias(a.ID, "  Ufficio-Mario "); err != nil || got != "ufficio-mario" {
		t.Fatalf("set: %q %v", got, err)
	}
	if got, err := s.SetAlias(b.ID, "UFFICIO-MARIO"); !errors.Is(err, ErrAliasTaken) || got != "" {
		t.Fatalf("taken: %q %v", got, err)
	}
	for _, bad := range []string{"ab", "123456", "-abc", "abc-", "a b c", "àbc", strings.Repeat("a", 33)} {
		if _, err := s.SetAlias(b.ID, bad); !errors.Is(err, ErrAliasInvalid) {
			t.Fatalf("%q accepted", bad)
		}
	}
	if s.ResolveTarget("Ufficio-Mario") != a.ID || s.ResolveTarget(" "+a.ID[:3]+" "+a.ID[3:]) != a.ID {
		t.Fatal("resolve")
	}
	if s.ResolveTarget("nobody-here") != "" {
		t.Fatal("unknown alias resolved")
	}
	if d, _ := s.ByID(a.ID); d.Alias != "ufficio-mario" {
		t.Fatalf("ByID alias %q", d.Alias)
	}
	// Removed: free for another device.
	if _, err := s.SetAlias(a.ID, ""); err != nil {
		t.Fatal(err)
	}
	if got, err := s.SetAlias(b.ID, "ufficio-mario"); err != nil || got != "ufficio-mario" {
		t.Fatalf("free again: %q %v", got, err)
	}
	// Reopened: the column is there already.
	s.Close()
	again, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	again.Close()
}

// Several connections registering the same new key at the same moment all get
// the device's one ID, none fails (SRV-3: the SELECT and the INSERT are two
// statements, the losers' INSERT failed on public_key UNIQUE and they got
// "no free ID").
func TestConcurrentFirstRegistrationOfOneKey(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "nuvira.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	const rounds, n = 50, 4
	for round := range rounds {
		key := bytes.Repeat([]byte{byte(round + 1)}, 32)
		key[0] = 0xEE
		ids := make([]string, n)
		errs := make([]error, n)
		var wg sync.WaitGroup
		for i := range n {
			wg.Add(1)
			go func() {
				defer wg.Done()
				d, err := s.Register(key, "same", "test")
				errs[i] = err
				if d != nil {
					ids[i] = d.ID
				}
			}()
		}
		wg.Wait()
		for i := range n {
			if errs[i] != nil {
				t.Fatalf("round %d, registration %d: %v", round, i, errs[i])
			}
			if ids[i] != ids[0] {
				t.Fatalf("round %d: two IDs for one key: %s, %s", round, ids[0], ids[i])
			}
		}
	}
	var rows int
	if err := s.DB().QueryRow(`SELECT COUNT(*) FROM devices`).Scan(&rows); err != nil || rows != rounds {
		t.Fatalf("%d device rows for %d keys (%v)", rows, rounds, err)
	}
}
