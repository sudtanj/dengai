# Packs the prebuilt static musl binaries from CI (bin/amd64/dengai, bin/arm64/dengai)
# into a scratch image. No compiling inside buildx, so no QEMU.
FROM --platform=$BUILDPLATFORM alpine:3 AS base
RUN apk add --no-cache ca-certificates && install -d -o 65532 -g 65532 /data

FROM scratch
ARG TARGETARCH
COPY --from=base /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=base --chown=65532:65532 /data /data
COPY --chmod=755 bin/${TARGETARCH}/dengai /dengai
ENV DENGAI_DATA_DIR=/data SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65532:65532
VOLUME /data
ENTRYPOINT ["/dengai"]
