package signal

// Phase 2 (characterisation): registration with small-order Ed25519 keys.

import (
	"bytes"
	"crypto/ed25519"
	"encoding/hex"
	"math/big"
	"testing"
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

// smallOrderKeys are the encodings of the 8 small-order points of edwards25519
// with both sign bits, plus the non-canonical encodings y = p and y = p + 1.
var smallOrderKeys = func() map[string]bool {
	p := new(big.Int).Sub(new(big.Int).Lsh(big.NewInt(1), 255), big.NewInt(19))
	le := func(h string) *big.Int { // little-endian hex without the sign bit
		b, _ := hex.DecodeString(h)
		b[31] &= 0x7f
		for i, j := 0, len(b)-1; i < j; i, j = i+1, j-1 {
			b[i], b[j] = b[j], b[i]
		}
		return new(big.Int).SetBytes(b)
	}
	ys := []*big.Int{
		big.NewInt(0), big.NewInt(1), new(big.Int).Sub(p, big.NewInt(1)),
		le("26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05"),
		le("c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a"),
		new(big.Int).Set(p), new(big.Int).Add(p, big.NewInt(1)),
	}
	keys := map[string]bool{}
	for _, y := range ys {
		be := y.FillBytes(make([]byte, 32))
		for i, j := 0, 31; i < j; i, j = i+1, j-1 {
			be[i], be[j] = be[j], be[i]
		}
		keys[string(be)] = true
		neg := bytes.Clone(be)
		neg[31] |= 0x80
		keys[string(neg)] = true
	}
	return keys
}()

func smallOrderKey(pub []byte) bool { return smallOrderKeys[string(pub)] }

// The test's own list (computed from the curve) is the production one.
func TestWeakKeysAreTheSmallOrderPoints(t *testing.T) {
	if len(weakKeys) != 14 || len(smallOrderKeys) != 14 {
		t.Fatalf("%d / %d encodings", len(weakKeys), len(smallOrderKeys))
	}
	for k := range smallOrderKeys {
		if !weakKey([]byte(k)) {
			t.Errorf("%x not refused", k)
		}
	}
	for range 1000 {
		pub, _, _ := ed25519.GenerateKey(nil)
		if weakKey(pub) {
			t.Fatalf("real key %x refused", pub)
		}
	}
}

// Fixed (SRV-6, was K20): the proof of the key used crypto/ed25519.Verify alone,
// which does not refuse small-order public keys: with the all-zero key (a point
// of order 4) and the all-zero signature the check passed for about 1 nonce in
// 4, so anybody registered a device without holding any private key. Now every
// small-order (and non-canonical small-order) key gets "invalid proof of the key".
func TestCharacterisesBug_SmallOrderKeyRegisters(t *testing.T) {
	ts := startServer(t)
	const tries = 64 // with the bug: P(no success) = (3/4)^64 ≈ 1e-8
	keys := make([][]byte, 0, len(smallOrderKeys))
	for k := range smallOrderKeys {
		keys = append(keys, []byte(k))
	}
	for i := range tries {
		key := keys[i%len(keys)]
		if i < tries/2 {
			key = make([]byte, ed25519.PublicKeySize) // the all-zero key, half the tries
		}
		c := ts.dial(t)
		_ = c.send(registerMsg(key, "weak", false))
		if m, err := c.read(5 * time.Second); err != nil || m.GetChallenge() == nil {
			t.Fatalf("challenge: %v %v", m, err)
		}
		_ = c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Proof{Proof: &pb.Proof{Signature: make([]byte, 64)}}})
		m, err := c.read(5 * time.Second)
		if err != nil || m.GetError().GetMessage() != "invalid proof of the key" {
			t.Fatalf("key %x, try %d: %v %v", key, i+1, m, err)
		}
		_ = c.conn.CloseWithError(0, "")
	}
	var rows int
	if err := ts.store.DB().QueryRow(`SELECT COUNT(*) FROM devices`).Scan(&rows); err != nil || rows != 0 {
		t.Fatalf("%d device rows (%v)", rows, err)
	}
}
