# syntax=docker/dockerfile:1
#
# mneme as a network service: `mneme serve` speaking MCP Streamable
# HTTP on :7878, with all state in the /data volume.
#
#   docker build -t mneme .
#   docker run -d --name mneme -p 127.0.0.1:7878:7878 \
#     -e MNEME_HTTP_TOKEN=change-me -v mneme-data:/data mneme
#
# Agents then connect to http://<host>:7878/mcp with
# `Authorization: Bearer <MNEME_HTTP_TOKEN>`. See
# deploy/compose/hermes/ for a ready-made mneme + Hermes Agent stack.

ARG RUST_VERSION=1
ARG DEBIAN_RELEASE=bookworm

FROM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin mneme \
 && install -Dm755 target/release/mneme /out/mneme

FROM debian:${DEBIAN_RELEASE}-slim
# The first boot downloads the embedding model from Hugging Face into
# /data/models over TLS. The CA bundle comes from the build stage, so
# this stage needs no package manager or network access.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
RUN useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin mneme \
 && mkdir -p /data \
 && chown mneme:mneme /data
COPY --from=build /out/mneme /usr/local/bin/mneme

ENV MNEME_DATA_DIR=/data \
    MNEME_HTTP_BIND=0.0.0.0:7878 \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
VOLUME ["/data"]
EXPOSE 7878
USER mneme

# No curl in the slim image; bash's /dev/tcp is enough to probe the
# unauthenticated health endpoint.
HEALTHCHECK --interval=15s --timeout=5s --start-period=120s --retries=3 \
  CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/7878 && printf 'GET /healthz HTTP/1.0\\r\\n\\r\\n' >&3 && head -n1 <&3 | grep -q ' 200 '"]

ENTRYPOINT ["mneme"]
CMD ["serve"]
