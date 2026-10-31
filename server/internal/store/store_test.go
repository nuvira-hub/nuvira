package store

import (
	"bytes"
	"path/filepath"
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
