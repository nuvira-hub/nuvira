// Package tlsutil creates and keeps the server certificate.
//
// For now it is self-signed: devices check its SHA-256 fingerprint (given by hand
// or remembered at the first connection). Let's Encrypt will come with TCP port 443.
package tlsutil

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/hex"
	"encoding/pem"
	"errors"
	"fmt"
	"io/fs"
	"math/big"
	"os"
	"path/filepath"
	"runtime"
	"time"
)

// LoadOrCreate reads cert.pem and key.pem from dir, creating them at first start
// (both missing). Only one of them missing is an error: a new pair would change the
// fingerprint every device has pinned.
func LoadOrCreate(dir string) (tls.Certificate, error) {
	certPath, keyPath := filepath.Join(dir, "cert.pem"), filepath.Join(dir, "key.pem")
	cert, err := tls.LoadX509KeyPair(certPath, keyPath)
	if err == nil || !errors.Is(err, fs.ErrNotExist) {
		return cert, err
	}
	for _, p := range [][2]string{{certPath, keyPath}, {keyPath, certPath}} {
		if _, err := os.Stat(p[1]); err == nil {
			return tls.Certificate{}, fmt.Errorf("%s is missing but %s is there: put it back, or remove both "+
				"to create a new certificate (every device will then have to trust the new fingerprint)", p[0], p[1])
		}
	}

	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return tls.Certificate{}, err
	}
	serial, _ := rand.Int(rand.Reader, big.NewInt(1<<62))
	tmpl := &x509.Certificate{
		SerialNumber: serial,
		Subject:      pkix.Name{CommonName: "nuvira-server"},
		DNSNames:     []string{"nuvira-server"},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().AddDate(20, 0, 0),
		KeyUsage:     x509.KeyUsageDigitalSignature,
		ExtKeyUsage:  []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	if err != nil {
		return tls.Certificate{}, err
	}
	keyDER, err := x509.MarshalPKCS8PrivateKey(key)
	if err != nil {
		return tls.Certificate{}, err
	}
	if err := os.WriteFile(certPath, pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}), 0o644); err != nil {
		return tls.Certificate{}, err
	}
	if err := os.WriteFile(keyPath, pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: keyDER}), 0o600); err != nil {
		return tls.Certificate{}, err
	}
	return tls.LoadX509KeyPair(certPath, keyPath)
}

// RestrictKey puts key.pem in dir back to 0600 when others can read or write it,
// and returns the mode it had. Some systems open up an app's whole data folder at
// every start (Runtipi adds 0777 to every file in it). It changes nothing on
// Windows, where the mode bits do not hold the file's access rights.
func RestrictKey(dir string) (was os.FileMode, changed bool, err error) {
	if runtime.GOOS == "windows" {
		return 0, false, nil
	}
	keyPath := filepath.Join(dir, "key.pem")
	fi, err := os.Stat(keyPath)
	if err != nil {
		return 0, false, err
	}
	was = fi.Mode().Perm()
	if was&0o077 == 0 {
		return was, false, nil
	}
	if err := os.Chmod(keyPath, 0o600); err != nil {
		return was, false, err
	}
	return was, true, nil
}

// Fingerprint is the SHA-256 fingerprint of the certificate, in hexadecimal.
func Fingerprint(cert tls.Certificate) string {
	sum := sha256.Sum256(cert.Certificate[0])
	return hex.EncodeToString(sum[:])
}
