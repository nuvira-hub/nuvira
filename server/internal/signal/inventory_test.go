package signal

import (
	"crypto/ed25519"
	"crypto/rand"
	"net/http"
	"sync"
	"testing"
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/ext"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

// inventoryModule records the inventories the server hands to modules.
type inventoryModule struct {
	mu   sync.Mutex
	from []string // device IDs
	got  chan struct{}
}

func (m *inventoryModule) Features() []string          { return nil }
func (m *inventoryModule) Policy() (*pb.Policy, error) { return nil, nil }
func (m *inventoryModule) Routes(*http.ServeMux)       {}
func (m *inventoryModule) Api(ext.Conn, *pb.ApiRequest, *pb.ApiResponse) (bool, error) {
	return false, nil
}
func (m *inventoryModule) Inventory(deviceID, _ string, _ *pb.Inventory) {
	m.mu.Lock()
	m.from = append(m.from, deviceID)
	m.mu.Unlock()
	m.got <- struct{}{}
}

// PRO-M2 (review Phase 4): Inventory was dispatched for every registered
// connection, apiOnly ones too (SetAlias already checked !apiOnly); only the
// device's own connection reports its inventory.
func TestInventoryOnlyFromTheDevice(t *testing.T) {
	m := &inventoryModule{got: make(chan struct{}, 4)}
	ts := startServer(t, m)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	api := ts.dial(t)
	if err := api.register(key, "viewer", true); err != nil {
		t.Fatal(err)
	}
	inv := &pb.ClientMessage{Msg: &pb.ClientMessage_Inventory{Inventory: &pb.Inventory{Hostname: "x"}}}
	if err := api.send(inv); err != nil {
		t.Fatal(err)
	}
	dev := ts.dial(t)
	if err := dev.register(key, "pc", false); err != nil {
		t.Fatal(err)
	}
	if err := dev.send(inv); err != nil {
		t.Fatal(err)
	}
	select {
	case <-m.got:
	case <-time.After(5 * time.Second):
		t.Fatal("the device's inventory never arrived")
	}
	// The apiOnly message was sent first on its own connection; give it time.
	time.Sleep(300 * time.Millisecond)
	m.mu.Lock()
	defer m.mu.Unlock()
	if len(m.from) != 1 || m.from[0] != dev.id {
		t.Fatalf("inventories from %v, want only %s", m.from, dev.id)
	}
}
