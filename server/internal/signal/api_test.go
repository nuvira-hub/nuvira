package signal

import (
	"errors"
	"io"
	"log/slog"
	"net/http"
	"path/filepath"
	"testing"

	"github.com/nuvira-hub/nuvira/server/internal/ext"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
	"github.com/nuvira-hub/nuvira/server/internal/store"
)

func newTestServer(t *testing.T) *Server {
	t.Helper()
	st, err := store.Open(filepath.Join(t.TempDir(), "nuvira.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { st.Close() })
	return New(st, nil, slog.New(slog.NewTextHandler(io.Discard, nil)))
}

func TestPresence(t *testing.T) {
	s := newTestServer(t)
	known, err := s.store.Register([]byte("0123456789abcdef0123456789abcdef"), "pc", "windows")
	if err != nil {
		t.Fatal(err)
	}
	s.online[known.ID] = &client{device: known}

	c := &client{}
	req := &pb.ApiRequest{Call: &pb.ApiRequest_GetPresence{GetPresence: &pb.GetPresence{DeviceIds: []string{known.ID, "999999999"}}}}
	resp := &pb.ApiResponse{}
	if err := s.apiCall(c, req, resp); err != nil {
		t.Fatal(err)
	}
	got := resp.GetPresence().GetDevices()
	if len(got) != 2 || !got[0].Online || got[0].LastSeen == 0 || got[1].Online || got[1].LastSeen != 0 {
		t.Fatalf("unexpected presence: %v", got)
	}
	// Asked again at once: refused.
	if err := s.apiCall(c, req, &pb.ApiResponse{}); err == nil {
		t.Fatal("second request within the interval accepted")
	}
	many := make([]string, maxPresenceIDs+1)
	big := &pb.ApiRequest{Call: &pb.ApiRequest_GetPresence{GetPresence: &pb.GetPresence{DeviceIds: many}}}
	if err := s.apiCall(&client{}, big, &pb.ApiResponse{}); err == nil {
		t.Fatal("too many IDs accepted")
	}
}

type fakeModule struct{ calls int }

func (m *fakeModule) Features() []string          { return []string{"pro.test"} }
func (m *fakeModule) Policy() (*pb.Policy, error) { return &pb.Policy{NoFiles: true}, nil }
func (m *fakeModule) Routes(*http.ServeMux)       {}
func (m *fakeModule) Api(c ext.Conn, req *pb.ApiRequest, resp *pb.ApiResponse) (bool, error) {
	if _, ok := req.Call.(*pb.ApiRequest_GetPolicy); !ok {
		return false, nil
	}
	m.calls++
	resp.Result = &pb.ApiResponse_Policy{Policy: &pb.Policy{NoFiles: true}}
	return true, nil
}

func TestModules(t *testing.T) {
	s := newTestServer(t)
	login := &pb.ApiRequest{Call: &pb.ApiRequest_Login{Login: &pb.Login{Username: "a", Password: "b"}}}
	if err := s.apiCall(&client{}, login, &pb.ApiResponse{}); !errors.Is(err, ext.ErrNotAvailable) {
		t.Fatalf("open server without modules: %v", err)
	}

	m := &fakeModule{}
	s.SetModules([]ext.Module{m})
	resp := &pb.ApiResponse{}
	if err := s.apiCall(&client{}, &pb.ApiRequest{Call: &pb.ApiRequest_GetPolicy{GetPolicy: &pb.GetPolicy{}}}, resp); err != nil {
		t.Fatal(err)
	}
	if m.calls != 1 || !resp.GetPolicy().GetNoFiles() {
		t.Fatalf("request not answered by the module: %v", resp)
	}
	if err := s.apiCall(&client{}, login, &pb.ApiResponse{}); !errors.Is(err, ext.ErrNotAvailable) {
		t.Fatalf("request no module handles: %v", err)
	}
}
