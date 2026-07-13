# Memoria 多阶段构建镜像（P2-6 DX）
# Multi-stage build. Compile on rust:1.81-slim, run on debian:bookworm-slim.
#
# 安全默认：MEMORIA_ADMIN_KEY 不在镜像中硬编码，
# 运行时通过 `-e MEMORIA_ADMIN_KEY=...` 或 compose 的 env_file 注入。
FROM rust:1.81-slim AS builder
WORKDIR /build
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libsqlite3-dev build-essential \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release \
    && strip target/release/memoria-server

FROM debian:bookworm-slim
WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/memoria-server /usr/local/bin/memoria-server
COPY web ./web

# 持久化数据卷：主库 / 审计库 / 备份
VOLUME ["/app/data"]

ENV MEMORIA_DB_PATH=/app/data/memoria.db \
    MEMORIA_AUTH_DB_PATH=/app/data/audit.db \
    MEMORIA_BACKUP_DIR=/app/data/backups \
    MEMORIA_WEB_DIR=/app/web \
    MEMORIA_HOST=0.0.0.0 \
    MEMORIA_PORT=9003

EXPOSE 9003

# shell 形式：运行时展开环境变量（MEMORIA_ADMIN_KEY 等可由 -e 注入）
CMD ["sh", "-c", "memoria-server"]
