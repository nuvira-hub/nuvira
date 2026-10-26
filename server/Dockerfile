# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM golang:1.27-alpine AS build
ARG TARGETOS
ARG TARGETARCH
ARG VERSION=dev
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 GOOS=$TARGETOS GOARCH=$TARGETARCH \
    go build -trimpath -ldflags "-s -w -X main.version=${VERSION}" -o /out/nexodesk-server ./cmd/nexodesk-server \
 && mkdir -p /out/data /out/tmp && chmod 1777 /out/tmp

FROM scratch
COPY --from=build /out/nexodesk-server /nexodesk-server
COPY --from=build /out/tmp /tmp
COPY --from=build --chown=65532:65532 /out/data /data
USER 65532:65532
ENV NEXODESK_DATA_DIR=/data NEXODESK_LISTEN=:21116
VOLUME /data
EXPOSE 21116/udp
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["/nexodesk-server", "healthcheck"]
ENTRYPOINT ["/nexodesk-server"]
