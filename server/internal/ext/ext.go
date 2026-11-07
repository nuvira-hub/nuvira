// Package ext is where optional server modules (NuviraHub Pro) plug in. The open
// server has none: it signals, relays and serves updates, and every address book
// lives in the apps. A build that adds modules registers them from an init
// function in package main.
package ext

import (
	"database/sql"
	"errors"
	"io"
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
	// SendSupportStatus tells a device the state of its support request; false if
	// the device is not connected.
	SendSupportStatus(deviceID string, status *pb.SupportStatus) bool
	// Online tells whether a device is connected now.
	Online(deviceID string) bool
	// SendWake asks a connected device to send Wake-on-LAN packets to these MAC
	// addresses on its network; false if it is not connected.
	SendWake(deviceID string, macs []string) bool
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

// Command is a maintenance command of a module, run as "nuvira-server <name> args…"
// (in Docker with docker exec, also while the server runs). Env.Host is nil.
type Command struct {
	Name  string
	Usage string
	Run   func(env Env, args []string, out io.Writer) error
}

var commands []Command

// RegisterCommand adds a command; called from init functions.
func RegisterCommand(c Command) { commands = append(commands, c) }

// Commands returns the registered commands.
func Commands() []Command { return commands }

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
	// ViewerToken is the address book sign-in the viewer presented (requested
	// only): a module with accounts tells whose it is. Never to be stored.
	ViewerToken string
}

// SupportDesk is implemented by the support queue module (NuviraHub Pro): it gets
// the devices' requests and learns when a device disconnects. The calls are
// synchronous: keep them short.
type SupportDesk interface {
	SupportRequest(deviceID, deviceName string, req *pb.SupportRequest)
	DeviceOffline(deviceID string)
}

// InventoryDesk is implemented by the light RMM module (NuviraHub Pro): it gets the
// inventories the devices send, with the address the server sees them from.
type InventoryDesk interface {
	Inventory(deviceID, remoteAddr string, inv *pb.Inventory)
}

// SessionObserver is implemented by modules that want the steps of the sessions
// (session log). The call is synchronous: keep it short.
type SessionObserver interface {
	SessionEvent(ev SessionEvent)
}

// SessionLimiter is implemented by modules that restrict single sessions (NuviraHub
// Pro: permissions per account, per membership of a book, without an account).
// viewerToken is the address book sign-in the viewer presented, possibly empty or
// no longer valid; the result (nil: no limits) goes to the agent with the request.
// The call is synchronous: keep it short.
type SessionLimiter interface {
	SessionLimits(viewerToken, agentID string) *pb.Policy
}

// MergeLimits adds the limits of b to a (each one that is on in either stays on).
func MergeLimits(a, b *pb.Policy) *pb.Policy {
	if b == nil {
		return a
	}
	if a == nil {
		a = &pb.Policy{}
	}
	a.ConfirmAlways = a.ConfirmAlways || b.ConfirmAlways
	a.NoFiles = a.NoFiles || b.NoFiles
	a.NoClipboard = a.NoClipboard || b.NoClipboard
	a.NoPrivacy = a.NoPrivacy || b.NoPrivacy
	a.NoTerminal = a.NoTerminal || b.NoTerminal
	a.NoTunnel = a.NoTunnel || b.NoTunnel
	a.NoPrint = a.NoPrint || b.NoPrint
	a.RecordAlways = a.RecordAlways || b.RecordAlways
	a.NoInput = a.NoInput || b.NoInput
	a.NoRestart = a.NoRestart || b.NoRestart
	a.NoRecord = a.NoRecord || b.NoRecord
	return a
}

// AnyLimit tells whether at least one limit is on.
func AnyLimit(p *pb.Policy) bool {
	return p != nil && (p.ConfirmAlways || p.NoFiles || p.NoClipboard || p.NoPrivacy || p.NoTerminal ||
		p.NoTunnel || p.NoPrint || p.RecordAlways || p.NoInput || p.NoRestart || p.NoRecord)
}

// UploadDesk is implemented by modules that take files from the devices (session
// recordings, NuviraHub Pro), sent on a stream of their signalling connection: the
// device is the one that proved its key at the registration. Upload checks the
// request and returns where the bytes go; ErrNotAvailable when the kind is not
// the module's. Any other error is sent to the device ("@key" or text).
type UploadDesk interface {
	Upload(deviceID string, up *pb.Upload) (UploadSink, error)
}

// UploadSink receives the bytes of one upload.
type UploadSink interface {
	io.Writer
	// Offset is how much of the file is already stored: the device sends the rest.
	Offset() uint64
	// Finish ends the upload; complete false when the device stopped before the end
	// (what arrived is kept, for a later upload to go on from there).
	Finish(complete bool) error
}
