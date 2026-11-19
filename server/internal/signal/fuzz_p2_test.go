package signal

// Phase 2 (characterisation): fuzz targets of what the server reads from the
// network: the length-delimited ClientMessage stream before and after the
// registration, and uploads. The QUIC ones run against a real in-process
// server (one per fuzz worker); a panic in any server goroutine crashes the
// worker and is reported by the fuzzer.
//
//	go test -run '^$' -fuzz '^FuzzPreRegistration$' -fuzztime 5m ./internal/signal/

import (
	"bufio"
	"bytes"
	"crypto/ed25519"
	"crypto/rand"
	"errors"
	"io"
	"math"
	"net/http"
	"sync"
	"testing"
	"time"

	"google.golang.org/protobuf/encoding/protodelim"
	"google.golang.org/protobuf/proto"

	"github.com/nuvira-hub/nuvira/server/internal/ext"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

func delimited(msgs ...proto.Message) []byte {
	var b bytes.Buffer
	for _, m := range msgs {
		_, _ = protodelim.MarshalTo(&b, m)
	}
	return b.Bytes()
}

func seedMessages() [][]byte {
	pub := make([]byte, 32)
	alias := "my-pc"
	return [][]byte{
		delimited(registerMsg(pub, "pc", false)),
		delimited(&pb.ClientMessage{Msg: &pb.ClientMessage_Register{Register: &pb.Register{PublicKey: pub, ProtocolVersion: 9, Alias: &alias}}},
			&pb.ClientMessage{Msg: &pb.ClientMessage_Proof{Proof: &pb.Proof{Signature: make([]byte, 64)}}}),
		delimited(&pb.ClientMessage{Msg: &pb.ClientMessage_Connect{Connect: &pb.Connect{DeviceId: "123456789", AccountToken: "t"}}}),
		delimited(&pb.ClientMessage{Msg: &pb.ClientMessage_Api{Api: &pb.ApiRequest{Id: 1,
			Call: &pb.ApiRequest_GetPresence{GetPresence: &pb.GetPresence{DeviceIds: []string{"1", "2"}}}}}}),
		delimited(&pb.ClientMessage{Msg: &pb.ClientMessage_SetAlias{SetAlias: &pb.SetAlias{Alias: "x y"}}},
			&pb.ClientMessage{Msg: &pb.ClientMessage_Accept{Accept: &pb.Accept{SessionId: "abc"}}},
			&pb.ClientMessage{Msg: &pb.ClientMessage_Reject{Reject: &pb.Reject{SessionId: "abc", Reason: "no"}}},
			&pb.ClientMessage{Msg: &pb.ClientMessage_SessionReport{SessionReport: &pb.SessionReport{SessionId: "abc"}}},
			&pb.ClientMessage{Msg: &pb.ClientMessage_SupportRequest{SupportRequest: &pb.SupportRequest{}}},
			&pb.ClientMessage{Msg: &pb.ClientMessage_Inventory{Inventory: &pb.Inventory{}}}),
		{0xff, 0xff, 0xff, 0xff, 0x0f}, // huge length prefix
		{0x80, 0x80, 0x04},             // 65536: just over maxMessage
		{0xff, 0xff, 0x03},             // 65535: allowed length, data missing
		{0x02, 0x0a, 0x00, 0x00},       // empty Register, then a zero-length message
		{0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01}, // varint overflow
		{},
	}
}

// FuzzClientMessageDecode reads a byte stream as the server's read() does:
// never a panic, never a message larger than maxMessage, and a message that
// decodes re-encodes to at most maxMessage bytes.
func FuzzClientMessageDecode(f *testing.F) {
	for _, s := range seedMessages() {
		f.Add(s)
	}
	f.Fuzz(func(t *testing.T, data []byte) {
		r := bufio.NewReader(bytes.NewReader(data))
		for range 64 {
			msg := &pb.ClientMessage{}
			err := protodelim.UnmarshalOptions{MaxSize: maxMessage}.UnmarshalFrom(r, msg)
			if err != nil {
				var big *protodelim.SizeTooLargeError
				if errors.As(err, &big) && big.Size <= maxMessage {
					t.Fatalf("size %d refused", big.Size)
				}
				return
			}
			if n := proto.Size(msg); n > maxMessage {
				t.Fatalf("decoded a message of %d bytes", n)
			}
			// What handle() does with each kind is the server's business: here
			// only that the getters are safe on whatever was decoded.
			_ = msg.GetRegister().GetPublicKey()
			_ = msg.GetApi().GetCall()
			_ = msg.GetConnect().GetDeviceId()
		}
	})
}

// fuzzTarget starts the server of one fuzz target (once per worker process:
// the fuzz function body runs once per worker, then f.Fuzz loops).
func fuzzTarget(f *testing.F) (*testServer, *uploadDesk) {
	desk := &uploadDesk{}
	return startServer(f, desk), desk
}

// FuzzPreRegistration writes arbitrary bytes as the first data of the
// signalling stream, then ends the stream. The server must never register the
// device (it cannot sign the random nonce), except with a small-order key
// (known bug, see TestCharacterisesBug_SmallOrderKeyRegisters), and every
// answer must decode.
func FuzzPreRegistration(f *testing.F) {
	for _, s := range seedMessages() {
		f.Add(s)
	}
	ts, _ := fuzzTarget(f)
	f.Fuzz(func(t *testing.T, data []byte) {
		c := ts.dial(t)
		defer c.conn.CloseWithError(0, "")
		if _, err := c.stream.Write(data); err != nil {
			return
		}
		_ = c.stream.Close()
		for range 8 {
			m, err := c.read(3 * time.Second)
			if err != nil {
				return // closed, reset or not decodable: fine for the server
			}
			// Not even with a small-order key (SRV-6): no private key, no ID.
			if m.GetRegistered() != nil {
				t.Fatalf("registered from %x (small-order key: %v)", data, smallOrderKey(firstKey(data)))
			}
		}
	})
}

// firstKey is the public key of the first message of data, if it is a Register.
func firstKey(data []byte) []byte {
	msg := &pb.ClientMessage{}
	if protodelim.UnmarshalFrom(bufio.NewReader(bytes.NewReader(data)), msg) != nil {
		return nil
	}
	return msg.GetRegister().GetPublicKey()
}

// FuzzPostRegistration registers properly, then writes arbitrary bytes. When
// the bytes are a valid sequence of messages, the connection must still serve
// a request afterwards (no message kind kills or blocks the loop).
func FuzzPostRegistration(f *testing.F) {
	for _, s := range seedMessages() {
		f.Add(s)
	}
	ts, _ := fuzzTarget(f)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	f.Fuzz(func(t *testing.T, data []byte) {
		c := ts.dial(t)
		defer c.conn.CloseWithError(0, "")
		if err := c.register(key, "fuzz", false); err != nil {
			t.Fatalf("register: %v", err)
		}
		// Is it a valid sequence of messages, as the server reads it?
		valid := true
		r := bufio.NewReader(bytes.NewReader(data))
		for {
			err := protodelim.UnmarshalOptions{MaxSize: maxMessage}.UnmarshalFrom(r, &pb.ClientMessage{})
			if err == io.EOF {
				break
			}
			if err != nil {
				valid = false
				break
			}
		}
		if _, err := c.stream.Write(data); err != nil {
			return
		}
		if !valid {
			return
		}
		const ping = math.MaxUint32
		// A call no module handles: always answered with an error.
		_ = c.send(&pb.ClientMessage{Msg: &pb.ClientMessage_Api{Api: &pb.ApiRequest{Id: ping,
			Call: &pb.ApiRequest_GetPolicy{GetPolicy: &pb.GetPolicy{}}}}})
		if _, err := c.readUntil(10*time.Second, func(m *pb.ServerMessage) bool {
			return m.GetApi().GetId() == ping || m.GetError() != nil
		}); err != nil {
			t.Fatalf("no answer after %x: %v", data, err)
		}
	})
}

// uploadDesk accepts kind "p2" uploads into memory, with a stored offset the
// test sets.
type uploadDesk struct {
	mu     sync.Mutex
	offset uint64
	last   *memSink
}

func (d *uploadDesk) Features() []string                                          { return nil }
func (d *uploadDesk) Policy() (*pb.Policy, error)                                 { return nil, nil }
func (d *uploadDesk) Routes(*http.ServeMux)                                       {}
func (d *uploadDesk) Api(ext.Conn, *pb.ApiRequest, *pb.ApiResponse) (bool, error) { return false, nil }
func (d *uploadDesk) Upload(_ string, up *pb.Upload) (ext.UploadSink, error) {
	if up.Kind != "p2" {
		return nil, ext.ErrNotAvailable
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	d.last = &memSink{offset: d.offset, done: make(chan struct{})}
	return d.last, nil
}

type memSink struct {
	offset   uint64
	mu       sync.Mutex
	data     bytes.Buffer
	finished int
	complete bool
	done     chan struct{}
}

func (s *memSink) Write(p []byte) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.data.Write(p)
}
func (s *memSink) Offset() uint64 { return s.offset }
func (s *memSink) Finish(complete bool) error {
	s.mu.Lock()
	s.finished++
	s.complete = complete
	s.mu.Unlock()
	close(s.done)
	return nil
}

// FuzzUpload sends an Upload with a fuzzed declared size, a fuzzed stored
// offset and a payload, then ends the stream. Current behaviour pinned:
// the offset is used only when <= size; the sink gets at most size-offset bytes;
// complete only when exactly that many arrived. The declared size is trusted
// (lead uploads.go:39): bounds are the module's business.
func FuzzUpload(f *testing.F) {
	f.Add(uint64(10), uint64(0), []byte("0123456789"))
	f.Add(uint64(10), uint64(4), []byte("456789"))
	f.Add(uint64(10), uint64(11), []byte("0123456789"))
	f.Add(uint64(3), uint64(0), []byte("0123456789"))
	f.Add(uint64(0), uint64(0), []byte{})
	f.Add(uint64(math.MaxUint64), uint64(0), []byte("x"))
	f.Add(uint64(1<<63), uint64(1), []byte("x"))
	ts, fuzzDesk := fuzzTarget(f)
	_, key, _ := ed25519.GenerateKey(rand.Reader)
	f.Fuzz(func(t *testing.T, size, offset uint64, payload []byte) {
		c := ts.dial(t)
		defer c.conn.CloseWithError(0, "")
		if err := c.register(key, "uploader", false); err != nil {
			t.Fatalf("register: %v", err)
		}
		fuzzDesk.mu.Lock()
		fuzzDesk.offset = offset
		fuzzDesk.mu.Unlock()
		st, err := c.conn.OpenStreamSync(t.Context())
		if err != nil {
			t.Fatal(err)
		}
		if _, err := protodelim.MarshalTo(st, &pb.Upload{Kind: "p2", Name: "f", Size: size}); err != nil {
			t.Fatal(err)
		}
		r := bufio.NewReader(st)
		_ = st.SetReadDeadline(time.Now().Add(5 * time.Second))
		ready := &pb.UploadReady{}
		if err := (protodelim.UnmarshalOptions{MaxSize: maxMessage}).UnmarshalFrom(r, ready); err != nil {
			t.Fatalf("UploadReady: %v", err)
		}
		want := offset
		if want > size {
			want = 0
		}
		if ready.Error != "" || ready.Offset != want {
			t.Fatalf("size %d offset %d: ready %v", size, offset, ready)
		}
		fuzzDesk.mu.Lock()
		sink := fuzzDesk.last
		fuzzDesk.mu.Unlock()
		_, _ = st.Write(payload)
		_ = st.Close()
		select {
		case <-sink.done:
		case <-time.After(10 * time.Second):
			t.Fatalf("size %d offset %d payload %d: upload never finished", size, offset, len(payload))
		}
		sink.mu.Lock()
		got, complete, finished := uint64(sink.data.Len()), sink.complete, sink.finished
		sink.mu.Unlock()
		rest := size - want
		if finished != 1 {
			t.Fatalf("Finish called %d times", finished)
		}
		if rest > math.MaxInt64 {
			// int64(rest) < 0: io.CopyN copies nothing (current behaviour).
			if got != 0 || complete {
				t.Fatalf("size %d: got %d complete %v", size, got, complete)
			}
			return
		}
		if got > rest || got != min(rest, uint64(len(payload))) {
			t.Fatalf("size %d offset %d payload %d: sink got %d", size, want, len(payload), got)
		}
		if complete != (uint64(len(payload)) >= rest) {
			t.Fatalf("size %d offset %d payload %d: complete %v", size, want, len(payload), complete)
		}
		if complete {
			res := &pb.UploadResult{}
			if err := (protodelim.UnmarshalOptions{MaxSize: maxMessage}).UnmarshalFrom(r, res); err != nil || !res.Ok {
				t.Fatalf("result %v %v", res, err)
			}
		}
	})
}
