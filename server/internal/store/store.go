// Package store keeps the registered devices in SQLite.
package store

import (
	"crypto/rand"
	"database/sql"
	"errors"
	"fmt"
	"math/big"
	"strings"
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
	// Alias is the readable name it chose (SetAlias); "" none.
	Alias string
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
	if err := migrateAlias(db); err != nil {
		db.Close()
		return nil, fmt.Errorf("schema: %w", err)
	}
	return &Store{db: db}, nil
}

// migrateAlias adds the devices' alias to databases of earlier versions.
func migrateAlias(db *sql.DB) error {
	var n int
	if err := db.QueryRow(`SELECT COUNT(*) FROM pragma_table_info('devices') WHERE name = 'alias'`).Scan(&n); err != nil {
		return err
	}
	if n == 0 {
		if _, err := db.Exec(`ALTER TABLE devices ADD COLUMN alias TEXT NOT NULL DEFAULT ''`); err != nil {
			return err
		}
	}
	_, err := db.Exec(`CREATE UNIQUE INDEX IF NOT EXISTS devices_alias ON devices(alias) WHERE alias <> ''`)
	return err
}

// ErrAliasTaken: another device has the alias.
var ErrAliasTaken = errors.New("alias taken")

// ErrAliasInvalid: not a valid alias (see ValidAlias).
var ErrAliasInvalid = errors.New("alias not valid")

// NormalizeAlias is the alias as it is kept: trimmed, lower case.
func NormalizeAlias(alias string) string {
	return strings.ToLower(strings.TrimSpace(alias))
}

// ValidAlias: 3 to 32 of a-z, 0-9 and "-", at least one letter (so that it is never
// taken for an ID), no "-" at the ends. Empty is valid: no alias.
func ValidAlias(alias string) bool {
	if alias == "" {
		return true
	}
	if len(alias) < 3 || len(alias) > 32 || alias[0] == '-' || alias[len(alias)-1] == '-' {
		return false
	}
	letter := false
	for _, r := range alias {
		switch {
		case r >= 'a' && r <= 'z':
			letter = true
		case r >= '0' && r <= '9', r == '-':
		default:
			return false
		}
	}
	return letter
}

// SetAlias gives device id the alias (normalized; "" removes it). The device
// keeps its earlier alias when the new one is taken or not valid.
func (s *Store) SetAlias(id, alias string) (string, error) {
	alias = NormalizeAlias(alias)
	if !ValidAlias(alias) {
		return s.aliasOf(id), ErrAliasInvalid
	}
	if alias != "" {
		var other string
		err := s.db.QueryRow(`SELECT id FROM devices WHERE alias = ?`, alias).Scan(&other)
		if err == nil && other != id {
			return s.aliasOf(id), ErrAliasTaken
		}
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return s.aliasOf(id), err
		}
	}
	if _, err := s.db.Exec(`UPDATE devices SET alias = ? WHERE id = ?`, alias, id); err != nil {
		if strings.Contains(err.Error(), "UNIQUE") {
			return s.aliasOf(id), ErrAliasTaken
		}
		return s.aliasOf(id), err
	}
	return alias, nil
}

func (s *Store) aliasOf(id string) string {
	var alias string
	_ = s.db.QueryRow(`SELECT alias FROM devices WHERE id = ?`, id).Scan(&alias)
	return alias
}

// ResolveTarget turns what a viewer typed into a device ID: digits (spaces and
// dashes ignored) are an ID; anything else an alias, "" when no device has it.
func (s *Store) ResolveTarget(target string) string {
	digits := strings.Map(func(r rune) rune {
		switch {
		case r >= '0' && r <= '9':
			return r
		case r == ' ' || r == '-' || r == '.':
			return -1
		}
		return 'x'
	}, strings.TrimSpace(target))
	if digits != "" && !strings.ContainsRune(digits, 'x') {
		return digits
	}
	var id string
	if err := s.db.QueryRow(`SELECT id FROM devices WHERE alias = ?`, NormalizeAlias(target)).Scan(&id); err != nil {
		return ""
	}
	return id
}

func (s *Store) Close() error { return s.db.Close() }

// Wrap uses a database already open with Open (maintenance commands get its *sql.DB).
func Wrap(db *sql.DB) *Store { return &Store{db: db} }

// DB exposes the database to the other stores of the same file (accounts and address book).
func (s *Store) DB() *sql.DB { return s.db }

// ErrNewDeviceRefused is returned by RegisterWith when allowNew refuses a new row.
var ErrNewDeviceRefused = errors.New("new device refused")

// Register returns the device with this key, creating it with a new ID the first
// time; it updates name, system and last access.
func (s *Store) Register(publicKey []byte, name, os string) (*Device, error) {
	return s.RegisterWith(publicKey, name, os, nil)
}

// RegisterWith is Register; when the key is new, allowNew (if not nil) is asked
// first, and ErrNewDeviceRefused returned if it says no (a budget of new rows).
func (s *Store) RegisterWith(publicKey []byte, name, os string, allowNew func() bool) (*Device, error) {
	now := time.Now()
	d := &Device{PublicKey: publicKey, Name: name, OS: os, LastSeen: now}
	// known finds the device of this key and updates it; false if there is none.
	known := func() (bool, error) {
		var created int64
		err := s.db.QueryRow(`SELECT id, created_at FROM devices WHERE public_key = ?`, publicKey).Scan(&d.ID, &created)
		if errors.Is(err, sql.ErrNoRows) {
			return false, nil
		} else if err != nil {
			return false, err
		}
		d.CreatedAt = time.Unix(created, 0)
		_, err = s.db.Exec(`UPDATE devices SET name = ?, os = ?, last_seen = ? WHERE id = ?`,
			name, os, now.Unix(), d.ID)
		return true, err
	}
	found, err := known()
	if err != nil {
		return nil, err
	} else if found {
		return d, nil
	}
	if allowNew != nil && !allowNew() {
		return nil, ErrNewDeviceRefused
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
		// The same key registered at the same moment by another connection
		// (the SELECT above and this INSERT are two statements): its row wins.
		if found, e := known(); e != nil {
			return nil, e
		} else if found {
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
	err := s.db.QueryRow(`SELECT public_key, name, os, created_at, last_seen, alias FROM devices WHERE id = ?`, id).
		Scan(&d.PublicKey, &d.Name, &d.OS, &created, &seen, &d.Alias)
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
