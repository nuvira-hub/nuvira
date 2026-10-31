package signal

import (
	"net/netip"
	"testing"
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/store"
)

func TestParseBlock(t *testing.T) {
	for in, want := range map[string]struct {
		kind  int
		value string
	}{
		"123 456 789":        {BlockDevice, "123456789"},
		"203.0.113.7":        {BlockNetwork, "203.0.113.7/32"},
		"::ffff:203.0.113.7": {BlockNetwork, "203.0.113.7/32"},
		"2001:db8::1":        {BlockNetwork, "2001:db8::1/128"},
		"203.0.113.77/24":    {BlockNetwork, "203.0.113.0/24"},
		"12345":              {BlockInvalid, ""},
		"example.com":        {BlockInvalid, ""},
	} {
		if kind, value := ParseBlock(in); kind != want.kind || value != want.value {
			t.Errorf("%q: %d %q, want %d %q", in, kind, value, want.kind, want.value)
		}
	}
}

func TestBlocklist(t *testing.T) {
	var b blocklist
	b.set([]store.Block{{Value: "123456789"}, {Value: "203.0.113.0/24"}, {Value: "2001:db8::/48"}})
	if !b.device("123456789") || b.device("987654321") {
		t.Fatal("device blocks")
	}
	for ip, want := range map[string]bool{"203.0.113.9": true, "203.0.114.9": false, "2001:db8:0:5::1": true, "2001:db9::1": false} {
		if b.address(netip.MustParseAddr(ip)) != want {
			t.Errorf("%s blocked = %v", ip, !want)
		}
	}
}

func TestLimiter(t *testing.T) {
	l := newLimiter(rate{perMinute: 6, burst: 3})
	key := netip.MustParseAddr("2001:db8:1:2:3:4:5:6")
	now := time.Now()
	for i := range 3 {
		if !l.allow(key, now) {
			t.Fatalf("request %d refused within the burst", i+1)
		}
	}
	if l.allow(key, now) || l.allow(netip.MustParseAddr("2001:db8:1:2::9"), now) {
		t.Fatal("request beyond the burst accepted (same address or same /64)")
	}
	if !l.allow(netip.MustParseAddr("198.51.100.1"), now) {
		t.Fatal("another address shares the limit")
	}
	for range 10 {
		if !l.allow(netip.MustParseAddr("192.168.1.5"), now) || !l.allow(netip.MustParseAddr("::1"), now) {
			t.Fatal("local addresses limited")
		}
	}
	var off *limiter
	if !off.allow(key, now) {
		t.Fatal("limits off")
	}
	// 6 per minute: one more every 10 seconds.
	if !l.allow(key, now.Add(10*time.Second)) || l.allow(key, now.Add(11*time.Second)) {
		t.Fatal("refill")
	}
	l.prune(now.Add(time.Hour))
	if len(l.buckets) != 0 {
		t.Fatal("full buckets not forgotten")
	}
}
