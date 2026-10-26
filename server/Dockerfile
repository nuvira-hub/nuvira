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
    go build -trimpath -ldflags "-s -w -X main.version=${VERSION}" -o /out/nexo-server ./cmd/nexo-server \
 && mkdir -p /out/data /out/tmp && chmod 1777 /out/tmp

FROM scratch
COPY --from=build /out/nexo-server /nexo-server
COPY --from=build /out/tmp /tmp
COPY --from=build --chown=65532:65532 /out/data /data
USER 65532:65532
ENV NEXO_DATA_DIR=/data NEXO_LISTEN=:21116
VOLUME /data
EXPOSE 21116/udp
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["/nexo-server", "healthcheck"]
ENTRYPOINT ["/nexo-server"]
