package limits

import (
	"net/netip"
	"testing"
)

func TestParseMode(t *testing.T) {
	for _, c := range []struct {
		in   string
		want Mode
		ok   bool
	}{{"", Public, true}, {"off", Off, true}, {"all", All, true}, {"on", Public, false}, {"ALL", Public, false}} {
		if m, ok := ParseMode(c.in); m != c.want || ok != c.ok {
			t.Errorf("%q: %v %v", c.in, m, ok)
		}
	}
}

func TestModeApplies(t *testing.T) {
	local := []string{"127.0.0.1", "::1", "10.1.2.3", "172.16.0.1", "192.168.0.1", "fd12::1", "::ffff:10.0.0.1", "::ffff:127.0.0.1"}
	public := []string{"203.0.113.10", "2001:db8::1", "100.64.0.1", "169.254.1.1", "fe80::1", "::ffff:8.8.8.8"}
	for _, s := range local {
		ip := netip.MustParseAddr(s)
		if Public.Applies(ip) || Off.Applies(ip) || !All.Applies(ip) {
			t.Errorf("local %s: public %v off %v all %v", s, Public.Applies(ip), Off.Applies(ip), All.Applies(ip))
		}
	}
	for _, s := range public {
		ip := netip.MustParseAddr(s)
		if !Public.Applies(ip) || Off.Applies(ip) || !All.Applies(ip) {
			t.Errorf("public %s: public %v off %v all %v", s, Public.Applies(ip), Off.Applies(ip), All.Applies(ip))
		}
	}
}

func TestKey(t *testing.T) {
	for _, c := range []struct{ ip, want string }{
		{"203.0.113.10", "203.0.113.10/32"},
		{"::ffff:203.0.113.10", "203.0.113.10/32"},
		{"2001:db8:1:2:3:4:5:6", "2001:db8:1:2::/64"},
	} {
		if got := Key(netip.MustParseAddr(c.ip)).String(); got != c.want {
			t.Errorf("%s: %s", c.ip, got)
		}
	}
	if got := KeyBits(netip.MustParseAddr("2001:db8:1:2::9"), 48).String(); got != "2001:db8:1::/48" {
		t.Errorf("/48: %s", got)
	}
}
