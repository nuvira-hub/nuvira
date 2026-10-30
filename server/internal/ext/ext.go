// Package ext is where optional server modules (NuviraHub Pro) plug in. The open
// server has none: it signals, relays and serves updates, and every address book
// lives in the apps. A build that adds modules registers them from an init
// function in package main.
package ext

import (
	"database/sql"
	"errors"
	"log/slog"
	"net/http"
	"time"

	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

// ErrDenied is a refused request (wrong password, missing rights): answered to
// the app but not logged as a failure.
var ErrDenied = errors.New("@server.denied")

// ErrNotAvailable is the answer to requests that need a module this server lacks.
var ErrNotAvailable = errors.New("@server.pro_required")

// Env is what a module gets when the server starts.
type Env struct {
	DB      *sql.DB
	DataDir string
	Log     *slog.Logger
	Host    Host
}

// Host is the part of the signalling server a module can use.
type Host interface {
	// Presence fills in Online and LastSeen of the entries.
	Presence(books *pb.Books)
	// BroadcastPolicy sends new rules to every connected device.
	BroadcastPolicy(policy *pb.Policy)
}

// Conn is the app connection an API request came on; the sign-in stays bound to it.
type Conn interface {
	DeviceID() string
	RemoteAddr() string
	Session() *pb.Session
	SetSession(session *pb.Session)
}

// Module is an optional part of the server.
type Module interface {
	// Features lists what the apps may offer with this module, e.g. "pro.address_book".
	Features() []string
	// Policy returns the rules sent to devices when they register (nil: none).
	Policy() (*pb.Policy, error)
	// Api answers the requests of the module, filling in resp. handled false
	// means the request is not one of its own.
	Api(c Conn, req *pb.ApiRequest, resp *pb.ApiResponse) (handled bool, err error)
	// Routes adds HTTP handlers on the TCP port (WebSocket and updates).
	Routes(mux *http.ServeMux)
}

// Factory opens a module.
type Factory func(env Env) (Module, error)

var factories []Factory

// Register adds a module; called from init functions.
func Register(f Factory) { factories = append(factories, f) }

// Open starts every registered module.
func Open(env Env) ([]Module, error) {
	modules := make([]Module, 0, len(factories))
	for _, f := range factories {
		m, err := f(env)
		if err != nil {
			return nil, err
		}
		modules = append(modules, m)
	}
	return modules, nil
}

// Kinds of SessionEvent.
const (
	SessionRequested   = "requested"   // a viewer asked to connect to an online agent
	SessionUnreachable = "unreachable" // the agent is not connected to the server
	SessionAccepted    = "accepted"    // the agent accepted: routes sent to both
	SessionRejected    = "rejected"    // the agent refused (Reason)
	SessionExpired     = "expired"     // the agent did not answer in time
	SessionEnded       = "ended"       // the agent's report at the end (Report)
)

// SessionEvent is a step of a session between two devices.
type SessionEvent struct {
	ID         string
	Kind       string
	Time       time.Time
	ViewerID   string
	ViewerName string
	AgentID    string
	AgentName  string
	Reason     string
	Report     *pb.SessionReport
}

// SessionObserver is implemented by modules that want the steps of the sessions
// (session log). The call is synchronous: keep it short.
type SessionObserver interface {
	SessionEvent(ev SessionEvent)
}
