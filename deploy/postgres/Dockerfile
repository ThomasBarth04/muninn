# Postgres with wal-g for continuous WAL archiving to Hetzner Object Storage (ADR 0008).
FROM postgres:17

# ponytail: amd64 only; add the aarch64 binary if the box ever moves to ARM (CAX).
ARG WALG_VERSION=v3.0.9
ARG WALG_SHA256=8015836f246a978f27b31a2462eac7765312558ae5b11ac033322b555abf9e22
ADD https://github.com/wal-g/wal-g/releases/download/${WALG_VERSION}/wal-g-pg-24.04-amd64 /usr/local/bin/wal-g
RUN echo "${WALG_SHA256}  /usr/local/bin/wal-g" | sha256sum -c - && chmod 0755 /usr/local/bin/wal-g

COPY initdb/ /docker-entrypoint-initdb.d/
