#!/usr/bin/env bash
# Rigenera il codice Go dei messaggi in server/internal/pb (il lato Rust si genera
# da solo in client/proto/build.rs). Richiede protoc-gen-go nel PATH:
#   go install google.golang.org/protobuf/cmd/protoc-gen-go@latest
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROTOC="${PROTOC:-$(ls -d "$HOME"/.cargo/registry/src/*/protoc-bin-vendored-win32-*/bin/protoc.exe 2>/dev/null | tail -1)}"
"$PROTOC" -I "$ROOT/proto" \
    --go_out="$ROOT/server/internal/pb" --go_opt=paths=source_relative \
    --go_opt=Mnexodesk/v1/rendezvous.proto=github.com/alexbn71/NexoDesk/server/internal/pb \
    nexodesk/v1/rendezvous.proto
# paths=source_relative crea nexodesk/v1/: il pacchetto pb resta piatto.
mv "$ROOT/server/internal/pb/nexodesk/v1/"*.pb.go "$ROOT/server/internal/pb/"
rm -rf "$ROOT/server/internal/pb/nexodesk"
