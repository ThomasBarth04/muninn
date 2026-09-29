# One image: the Rust binary serving /api, /hooks, the job worker and the built
# SPA from one origin (ADR 0007).
#   docker build -t muninn .

FROM node:22 AS frontend
WORKDIR /build
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

FROM rust:1.98 AS backend
WORKDIR /build
COPY backend/ ./
# Cache mounts keep the registry and compiled deps between builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked && cp target/release/muninn /muninn

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 muninn
COPY --from=backend /muninn /usr/local/bin/muninn
COPY --from=frontend /build/dist /app/dist
ENV STATIC_DIR=/app/dist BIND=0.0.0.0:3000
USER muninn
EXPOSE 3000
CMD ["muninn"]
