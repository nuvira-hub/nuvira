package store

import (
	"bytes"
	"path/filepath"
	"testing"
)

func TestRegisterKeepsID(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "nexo.db"))
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
		t.Fatalf("ID non valido: %q", a.ID)
	}
	b, err := s.Register(key, "pc rinominato", "windows")
	if err != nil {
		t.Fatal(err)
	}
	if a.ID != b.ID {
		t.Fatalf("la stessa chiave deve avere lo stesso ID: %s != %s", a.ID, b.ID)
	}
	other, err := s.Register(bytes.Repeat([]byte{8}, 32), "altro", "linux")
	if err != nil {
		t.Fatal(err)
	}
	if other.ID == a.ID {
		t.Fatal("chiavi diverse con lo stesso ID")
	}

	got, err := s.ByID(a.ID)
	if err != nil || got == nil || got.Name != "pc rinominato" || !bytes.Equal(got.PublicKey, key) {
		t.Fatalf("ByID: %+v, %v", got, err)
	}
	if missing, err := s.ByID("123456789"); err != nil || missing != nil {
		t.Fatalf("ID inesistente: %+v, %v", missing, err)
	}
}
