// Package store keeps the registered devices in SQLite.
package store

import (
	"crypto/rand"
	"database/sql"
	"errors"
	"fmt"
	"math/big"
	"time"

	_ "modernc.org/sqlite"
)

// Device is a registered device: 9-digit ID bound to its Ed25519 public key.
type Device struct {
	ID        string
	PublicKey []byte
	Name      string
	OS        string
	CreatedAt time.Time
	LastSeen  time.Time
}

type Store struct {
	db *sql.DB
}

const schema = `
CREATE TABLE IF NOT EXISTS devices (
	id          TEXT PRIMARY KEY,
	public_key  BLOB NOT NULL UNIQUE,
	name        TEXT NOT NULL DEFAULT '',
	os          TEXT NOT NULL DEFAULT '',
	created_at  INTEGER NOT NULL,
	last_seen   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS blocked (
	value       TEXT PRIMARY KEY,
	note        TEXT NOT NULL DEFAULT '',
	created_at  INTEGER NOT NULL
);`

// Open opens (or creates) the database in the given file.
func Open(path string) (*Store, error) {
	db, err := sql.Open("sqlite", "file:"+path+"?_pragma=journal_mode(WAL)&_pragma=busy_timeout(5000)&_pragma=foreign_keys(1)")
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(schema); err != nil {
		db.Close()
		return nil, fmt.Errorf("schema: %w", err)
	}
	return &Store{db: db}, nil
}

func (s *Store) Close() error { return s.db.Close() }

// Wrap uses a database already open with Open (maintenance commands get its *sql.DB).
func Wrap(db *sql.DB) *Store { return &Store{db: db} }

// DB exposes the database to the other stores of the same file (accounts and address book).
func (s *Store) DB() *sql.DB { return s.db }

// Register returns the device with this key, creating it with a new ID the first
// time; it updates name, system and last access.
func (s *Store) Register(publicKey []byte, name, os string) (*Device, error) {
	now := time.Now()
	d := &Device{PublicKey: publicKey, Name: name, OS: os, LastSeen: now}
	var created int64
	err := s.db.QueryRow(`SELECT id, created_at FROM devices WHERE public_key = ?`, publicKey).Scan(&d.ID, &created)
	switch {
	case err == nil:
		d.CreatedAt = time.Unix(created, 0)
		_, err = s.db.Exec(`UPDATE devices SET name = ?, os = ?, last_seen = ? WHERE id = ?`,
			name, os, now.Unix(), d.ID)
		return d, err
	case !errors.Is(err, sql.ErrNoRows):
		return nil, err
	}

	d.CreatedAt = now
	for range 20 {
		d.ID, err = newID()
		if err != nil {
			return nil, err
		}
		_, err = s.db.Exec(`INSERT INTO devices (id, public_key, name, os, created_at, last_seen)
			VALUES (?, ?, ?, ?, ?, ?)`, d.ID, publicKey, name, os, now.Unix(), now.Unix())
		if err == nil {
			return d, nil
		}
		// ID collision (very rare): another one is tried.
	}
	return nil, fmt.Errorf("no free ID: %w", err)
}

// ByID returns the device with this ID, or nil if it does not exist.
func (s *Store) ByID(id string) (*Device, error) {
	d := &Device{ID: id}
	var created, seen int64
	err := s.db.QueryRow(`SELECT public_key, name, os, created_at, last_seen FROM devices WHERE id = ?`, id).
		Scan(&d.PublicKey, &d.Name, &d.OS, &created, &seen)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	d.CreatedAt, d.LastSeen = time.Unix(created, 0), time.Unix(seen, 0)
	return d, nil
}

// Touch updates the last access.
func (s *Store) Touch(id string) error {
	_, err := s.db.Exec(`UPDATE devices SET last_seen = ? WHERE id = ?`, time.Now().Unix(), id)
	return err
}

// Block is a blocked device ID, address or network (see the signal package).
type Block struct {
	Value     string
	Note      string
	CreatedAt time.Time
}

// SetBlock adds (or updates the note of) a block.
func (s *Store) SetBlock(value, note string) error {
	_, err := s.db.Exec(`INSERT INTO blocked (value, note, created_at) VALUES (?, ?, ?)
		ON CONFLICT(value) DO UPDATE SET note = excluded.note`, value, note, time.Now().Unix())
	return err
}

// RemoveBlock deletes a block; false if there was none.
func (s *Store) RemoveBlock(value string) (bool, error) {
	res, err := s.db.Exec(`DELETE FROM blocked WHERE value = ?`, value)
	if err != nil {
		return false, err
	}
	n, err := res.RowsAffected()
	return n > 0, err
}

// Blocks lists the blocks, oldest first.
func (s *Store) Blocks() ([]Block, error) {
	rows, err := s.db.Query(`SELECT value, note, created_at FROM blocked ORDER BY created_at, value`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var list []Block
	for rows.Next() {
		var b Block
		var created int64
		if err := rows.Scan(&b.Value, &b.Note, &created); err != nil {
			return nil, err
		}
		b.CreatedAt = time.Unix(created, 0)
		list = append(list, b)
	}
	return list, rows.Err()
}

// newID generates a 9-digit ID not starting with 0 (easier to dictate).
func newID() (string, error) {
	n, err := rand.Int(rand.Reader, big.NewInt(900_000_000))
	if err != nil {
		return "", err
	}
	return fmt.Sprintf("%09d", n.Int64()+100_000_000), nil
}
