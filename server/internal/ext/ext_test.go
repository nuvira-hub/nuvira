package ext

// Phase 2 (characterisation): the extension points of the open server.

import (
	"errors"
	"io"
	"net/http"
	"testing"

	"google.golang.org/protobuf/reflect/protoreflect"

	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

type nopModule struct{ name string }

func (m nopModule) Features() []string                                      { return []string{m.name} }
func (m nopModule) Policy() (*pb.Policy, error)                             { return nil, nil }
func (m nopModule) Api(Conn, *pb.ApiRequest, *pb.ApiResponse) (bool, error) { return false, nil }
func (m nopModule) Routes(*http.ServeMux)                                   {}

func TestRegisterAndOpen(t *testing.T) {
	saved, savedCmds := factories, commands
	t.Cleanup(func() { factories, commands = saved, savedCmds })
	factories, commands = nil, nil

	if mods, err := Open(Env{}); err != nil || len(mods) != 0 {
		t.Fatalf("no modules: %v %v", mods, err)
	}
	var got []string
	Register(func(env Env) (Module, error) { got = append(got, "a:"+env.DataDir); return nopModule{"a"}, nil })
	Register(func(env Env) (Module, error) { got = append(got, "b"); return nopModule{"b"}, nil })
	mods, err := Open(Env{DataDir: "d"})
	if err != nil || len(mods) != 2 || mods[0].Features()[0] != "a" || mods[1].Features()[0] != "b" {
		t.Fatalf("%v %v", mods, err)
	}
	if len(got) != 2 || got[0] != "a:d" {
		t.Fatalf("order %v", got)
	}
	// A failing factory stops the start; the modules opened before are dropped.
	boom := errors.New("boom")
	Register(func(Env) (Module, error) { return nil, boom })
	Register(func(Env) (Module, error) { t.Fatal("factory after a failure called"); return nil, nil })
	if mods, err := Open(Env{}); !errors.Is(err, boom) || mods != nil {
		t.Fatalf("%v %v", mods, err)
	}

	RegisterCommand(Command{Name: "x", Run: func(Env, []string, io.Writer) error { return nil }})
	RegisterCommand(Command{Name: "x"}) // duplicates are kept (main runs the first match)
	if c := Commands(); len(c) != 2 || c[0].Name != "x" {
		t.Fatalf("commands %v", c)
	}
}

func TestShorterIdle(t *testing.T) {
	for _, c := range [][3]uint32{{0, 0, 0}, {0, 5, 5}, {5, 0, 5}, {5, 3, 3}, {3, 5, 3}, {7, 7, 7}} {
		if got := ShorterIdle(c[0], c[1]); got != c[2] {
			t.Errorf("ShorterIdle(%d, %d) = %d, want %d", c[0], c[1], got, c[2])
		}
	}
}

// Every field of pb.Policy, by name: which ones MergeLimits combines and
// AnyLimit counts. A field added to Policy and not listed here fails the test,
// so that it is a decision whether it is a session limit.
func TestMergeLimitsAndAnyLimitCoverPolicy(t *testing.T) {
	limits := map[string]bool{
		"confirm_always": true, "no_files": true, "no_clipboard": true, "no_privacy": true, "no_terminal": true,
		"no_tunnel": true, "no_print": true, "record_always": true, "no_input": true, "no_restart": true,
		"no_record": true, "no_resolution": true, "no_audio": true, "lock_on_end": true, "idle_minutes": true,
		"no_microphone": true,
	}
	if AnyLimit(nil) || AnyLimit(&pb.Policy{}) {
		t.Fatal("no limits")
	}
	if MergeLimits(nil, nil) != nil {
		t.Fatal("nil + nil")
	}
	a := &pb.Policy{}
	if MergeLimits(a, nil) != a {
		t.Fatal("a + nil must be a")
	}
	fields := (&pb.Policy{}).ProtoReflect().Descriptor().Fields()
	var notLimits []string
	for i := range fields.Len() {
		fd := fields.Get(i)
		name := string(fd.Name())
		b := &pb.Policy{}
		switch {
		case fd.Kind() == protoreflect.BoolKind && fd.Cardinality() != protoreflect.Repeated:
			b.ProtoReflect().Set(fd, protoreflect.ValueOfBool(true))
		case fd.Kind() == protoreflect.Uint32Kind && fd.Cardinality() != protoreflect.Repeated:
			b.ProtoReflect().Set(fd, protoreflect.ValueOfUint32(9))
		default:
			if limits[name] {
				t.Errorf("limit %s has kind %v", name, fd.Kind())
			}
			notLimits = append(notLimits, name)
			delete(limits, name)
			continue
		}
		carried := MergeLimits(nil, b).ProtoReflect().Has(fd)
		counted := AnyLimit(b)
		if limits[name] {
			if !carried || !counted {
				t.Errorf("%s: merged %v, counted %v", name, carried, counted)
			}
		} else {
			if carried || counted {
				t.Errorf("%s is not a listed limit but merged %v / counted %v: update this test", name, carried, counted)
			}
			notLimits = append(notLimits, name)
		}
		delete(limits, name)
	}
	if len(limits) != 0 {
		t.Errorf("limits no longer in Policy: %v", limits)
	}
	t.Logf("Policy fields that are not session limits (not merged, not counted): %v", notLimits)
	if got := MergeLimits(&pb.Policy{IdleMinutes: 30}, &pb.Policy{IdleMinutes: 10}); got.IdleMinutes != 10 {
		t.Fatalf("idle %d", got.IdleMinutes)
	}
	if got := MergeLimits(&pb.Policy{IdleMinutes: 30}, &pb.Policy{NoFiles: true}); got.IdleMinutes != 30 || !got.NoFiles {
		t.Fatalf("merge %v", got)
	}
}
