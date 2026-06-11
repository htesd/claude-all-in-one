# 多阶段构建:bun 建前端 → 嵌进二进制 → rust release → slim 运行时。
# 单二进制 claude-all-in-one,按 --mode router|worker 复用同一镜像。

# --- 1. 前端:admin-ui/dist(供 embed-ui 内嵌) ---
FROM oven/bun:1 AS frontend
WORKDIR /ui
COPY admin-ui/package.json admin-ui/bun.lock ./
RUN bun install --frozen-lockfile
COPY admin-ui/ ./
RUN bun run build

# --- 2. Rust release(--features embed-ui 把 dist 嵌进二进制) ---
FROM rust:1.95-bookworm AS builder
WORKDIR /app
COPY . .
# 用前端构建产物覆盖(.dockerignore 已排除本地 dist,保证用 CI 重建的版本)
COPY --from=frontend /ui/dist ./admin-ui/dist
RUN cargo build --release -p gw-app --features embed-ui

# --- 3. 运行时:slim + CA 证书(reqwest rustls 需根证书;rusqlite bundled 无需系统 sqlite) ---
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/claude-all-in-one /usr/local/bin/claude-all-in-one
# config/(只读)与 data/(SQLite)运行时挂载;默认路径相对 CWD=/app。
ENTRYPOINT ["claude-all-in-one"]
