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

// KNOWN BUG (Phase 2, new finding, signal.go register): the proof of the key
// uses crypto/ed25519.Verify, which does not refuse small-order public keys.
// With the all-zero key (a point of order 4) and the all-zero signature the
// check passes for about 1 nonce in 4, so anybody registers a device without
// holding any private key (always the same ID for the same weak key).
// Expected after fix: Register refuses small-order (and non-canonical) keys.
func TestCharacterisesBug_SmallOrderKeyRegisters(t *testing.T) {
	ts := startServer(t)
	zero := make([]byte, ed25519.PublicKeySize)
	if !smallOrderKey(zero) {
		t.Fatal("list of small-order keys")
	}
	ok, ids := 0, map[string]bool{}
	const tries = 64 // P(no success) = (3/4)^64 ≈ 1e-8
	for range tries {
		c := ts.dial(t)
		_ = c.send(registerMsg(zero, "weak", false))
		if m, err := c.read(5 * time.Second); err != nil || m.GetChallenge() == nil {
			t.Fatalf("challenge: %v %v", m, err)
		}
		_ = c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Proof{Proof: &pb.Proof{Signature: make([]byte, 64)}}})
		m, err := c.read(5 * time.Second)
		if err == nil && m.GetRegistered() != nil {
			ok++
			ids[m.GetRegistered().DeviceId] = true
		}
		_ = c.conn.CloseWithError(0, "")
	}
	t.Logf("all-zero key and signature registered %d/%d times", ok, tries)
	if ok == 0 || len(ids) != 1 {
		t.Fatalf("registered %d times, IDs %v", ok, ids)
	}
}
