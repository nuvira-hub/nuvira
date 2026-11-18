package tlsutil

// Phase 2 (characterisation): the server certificate as it is created and kept.

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/x509"
	"os"
	"path/filepath"
	"runtime"
	"testing"
	"time"
)

func TestLoadOrCreate(t *testing.T) {
	dir := t.TempDir()
	cert, err := LoadOrCreate(dir)
	if err != nil {
		t.Fatal(err)
	}
	fp := Fingerprint(cert)
	if len(fp) != 64 {
		t.Fatalf("fingerprint %q", fp)
	}
	leaf, err := x509.ParseCertificate(cert.Certificate[0])
	if err != nil {
		t.Fatal(err)
	}
	// Self-signed ECDSA P-256, CN and DNS name "nuvira-server", server auth only.
	if pub, ok := leaf.PublicKey.(*ecdsa.PublicKey); !ok || pub.Curve != elliptic.P256() {
		t.Fatalf("key %T", leaf.PublicKey)
	}
	if leaf.Subject.CommonName != "nuvira-server" || len(leaf.DNSNames) != 1 || leaf.DNSNames[0] != "nuvira-server" ||
		leaf.CheckSignature(leaf.SignatureAlgorithm, leaf.RawTBSCertificate, leaf.Signature) != nil || leaf.IsCA {
		t.Fatalf("certificate %+v", leaf.Subject)
	}
	if len(leaf.ExtKeyUsage) != 1 || leaf.ExtKeyUsage[0] != x509.ExtKeyUsageServerAuth || leaf.KeyUsage != x509.KeyUsageDigitalSignature {
		t.Fatalf("usage %v %v", leaf.ExtKeyUsage, leaf.KeyUsage)
	}
	// Characterisation (lead tlsutil.go:43): valid 20 years, no rotation.
	if life := leaf.NotAfter.Sub(leaf.NotBefore); life < 20*365*24*time.Hour || life > 20*366*24*time.Hour+2*time.Hour {
		t.Fatalf("lifetime %v", life)
	}
	if time.Until(leaf.NotBefore) > -50*time.Minute {
		t.Fatalf("NotBefore %v: expected one hour back", leaf.NotBefore)
	}
	if runtime.GOOS != "windows" {
		for name, want := range map[string]os.FileMode{"cert.pem": 0o644, "key.pem": 0o600} {
			fi, err := os.Stat(filepath.Join(dir, name))
			if err != nil || fi.Mode().Perm() != want {
				t.Errorf("%s: %v %v", name, fi.Mode(), err)
			}
		}
	}
	// Second start: the same certificate.
	again, err := LoadOrCreate(dir)
	if err != nil || Fingerprint(again) != fp {
		t.Fatalf("reloaded %v %v", Fingerprint(again), err)
	}
}

// A damaged file is an error (the server does not start), not a new certificate.
func TestLoadOrCreateDamaged(t *testing.T) {
	dir := t.TempDir()
	if _, err := LoadOrCreate(dir); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "key.pem"), []byte("garbage"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := LoadOrCreate(dir); err == nil {
		t.Fatal("damaged key accepted")
	}
}

// Characterisation: when only the key is missing, both files are made again,
// so the fingerprint the devices pinned changes (the old cert.pem is overwritten).
func TestLoadOrCreateMissingKeyRegenerates(t *testing.T) {
	dir := t.TempDir()
	first, err := LoadOrCreate(dir)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(filepath.Join(dir, "key.pem")); err != nil {
		t.Fatal(err)
	}
	second, err := LoadOrCreate(dir)
	if err != nil {
		t.Fatal(err)
	}
	if Fingerprint(second) == Fingerprint(first) {
		t.Fatal("same certificate without its key")
	}
}

// A missing folder is an error (main creates it first).
func TestLoadOrCreateNoDir(t *testing.T) {
	if _, err := LoadOrCreate(filepath.Join(t.TempDir(), "missing")); err == nil {
		t.Fatal("certificate written into a missing folder")
	}
}
