package relay

import (
	"net"
	"net/http"
	"testing"
)

func TestHostOf(t *testing.T) {
	for addr, want := range map[net.Addr]string{
		&net.UDPAddr{IP: net.ParseIP("203.0.113.7"), Port: 5000}:     "203.0.113.7",
		&net.UDPAddr{IP: net.ParseIP("::ffff:203.0.113.7"), Port: 1}: "203.0.113.7",
		&net.UDPAddr{IP: net.ParseIP("2001:db8::1"), Port: 1}:        "2001:db8::1",
		wsAddr("ws:3@198.51.100.4"):                                  "198.51.100.4",
	} {
		if ip, ok := HostOf(addr); !ok || ip.String() != want {
			t.Errorf("%v: %v %v", addr, ip, ok)
		}
	}
}

func TestRemoteHostForwardedOnlyFromProxy(t *testing.T) {
	req := func(remote, fwd string) *http.Request {
		r := &http.Request{RemoteAddr: remote, Header: http.Header{}}
		if fwd != "" {
			r.Header.Set("X-Forwarded-For", fwd)
		}
		return r
	}
	for _, c := range []struct{ remote, fwd, want string }{
		{"203.0.113.7:4000", "", "203.0.113.7"},
		{"203.0.113.7:4000", "10.9.9.9", "203.0.113.7"},             // from the Internet: ignored
		{"127.0.0.1:4000", "1.2.3.4, 198.51.100.4", "198.51.100.4"}, // proxy: the last one
		{"[::1]:4000", "garbage", "::1"},
	} {
		if got := remoteHost(req(c.remote, c.fwd)); got != c.want {
			t.Errorf("%s + %q: %s, want %s", c.remote, c.fwd, got, c.want)
		}
	}
}
