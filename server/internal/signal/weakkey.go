package signal

import "encoding/hex"

// smallOrderY are the y coordinates (little-endian, sign bit clear) of the
// small-order points of edwards25519: y = 0, 1, p-1, the two points of order 8,
// and the non-canonical encodings y = p and y = p+1 (the list libsodium's
// has_small_order refuses). crypto/ed25519.Verify accepts such public keys, and
// with them a signature made without any private key passes for some messages
// (the all-zero key and signature pass for about 1 nonce in 4). Keys made by
// ed25519::generate are never among them.
var smallOrderY = []string{
	"0000000000000000000000000000000000000000000000000000000000000000",
	"0100000000000000000000000000000000000000000000000000000000000000",
	"ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
	"26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05",
	"c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a",
	"edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
	"eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
}

// weakKeys are the 14 encodings of smallOrderY, with both sign bits.
var weakKeys = func() map[string]bool {
	keys := map[string]bool{}
	for _, h := range smallOrderY {
		b, err := hex.DecodeString(h)
		if err != nil || len(b) != 32 {
			panic("smallOrderY")
		}
		keys[string(b)] = true
		b[31] |= 0x80
		keys[string(b)] = true
	}
	return keys
}()

// weakKey tells whether a 32-byte Ed25519 public key is a small-order point:
// it proves nothing, since no private key is needed to "sign" with it.
func weakKey(pub []byte) bool { return weakKeys[string(pub)] }
