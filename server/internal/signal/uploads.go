package signal

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"time"

	"github.com/quic-go/quic-go"
	"google.golang.org/protobuf/encoding/protodelim"
	"google.golang.org/protobuf/proto"

	"github.com/nuvira-hub/nuvira/server/internal/ext"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

// How long an upload may stay without receiving anything.
const uploadIdle = 2 * time.Minute

// streams serves the further bidirectional streams a registered device opens on
// its signalling connection: files sent to the server (see Upload in
// rendezvous.proto). It ends with the connection.
func (s *Server) streams(ctx context.Context, c *client) {
	for {
		st, err := c.conn.AcceptStream(ctx)
		if err != nil {
			return
		}
		go func() {
			if err := s.upload(c, st); err != nil && !isClosed(err) {
				s.log.Warn("upload", "id", c.device.ID, "err", err)
			}
		}()
	}
}

func (s *Server) upload(c *client, st *quic.Stream) error {
	defer st.Close()
	r := bufio.NewReader(st)
	answer := func(msg proto.Message) error {
		_, err := protodelim.MarshalTo(st, msg)
		return err
	}
	fail := func(text string) error {
		st.CancelRead(0)
		return answer(&pb.UploadReady{Error: text})
	}
	_ = st.SetReadDeadline(time.Now().Add(uploadIdle))
	up := &pb.Upload{}
	if err := (protodelim.UnmarshalOptions{MaxSize: maxMessage}).UnmarshalFrom(r, up); err != nil {
		return err
	}
	var sink ext.UploadSink
	for _, m := range s.modules {
		d, ok := m.(ext.UploadDesk)
		if !ok {
			continue
		}
		var err error
		sink, err = d.Upload(c.device.ID, up)
		if errors.Is(err, ext.ErrNotAvailable) {
			continue
		}
		if err != nil {
			s.log.Info("upload refused", "id", c.device.ID, "kind", up.Kind, "session", up.SessionId, "err", err)
			return fail(err.Error())
		}
		break
	}
	if sink == nil {
		return fail(ext.ErrNotAvailable.Error())
	}
	offset := sink.Offset()
	if offset > up.Size {
		offset = 0
	}
	if err := answer(&pb.UploadReady{Offset: offset}); err != nil {
		sink.Finish(false)
		return err
	}
	// The deadline moves forward as bytes arrive.
	got, err := io.CopyN(sink, &idleReader{r: r, st: st}, int64(up.Size-offset))
	complete := err == nil && uint64(got) == up.Size-offset
	if ferr := sink.Finish(complete); ferr != nil {
		s.log.Warn("upload not stored", "id", c.device.ID, "name", up.Name, "err", ferr)
		return answer(&pb.UploadResult{Error: "@server.upload_failed"})
	}
	if !complete {
		return fmt.Errorf("upload interrupted after %d bytes: %w", offset+uint64(got), err)
	}
	s.log.Info("file received", "id", c.device.ID, "kind", up.Kind, "session", up.SessionId, "name", up.Name,
		"bytes", up.Size, "resumed_at", offset)
	return answer(&pb.UploadResult{Ok: true})
}

// idleReader pushes the read deadline forward at every read.
type idleReader struct {
	r  io.Reader
	st *quic.Stream
}

func (i *idleReader) Read(p []byte) (int, error) {
	_ = i.st.SetReadDeadline(time.Now().Add(uploadIdle))
	return i.r.Read(p)
}
