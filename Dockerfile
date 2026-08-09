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
# 钉到镜像**自带**的工具链。不钉的话 rustup 会照 rust-toolchain.toml 的
# `channel = "1.95"` + rustfmt/clippy 去 static.rust-lang.org 重下整条工具链 ——
# 那台机连不上它(2026-08-10 实测 `channel-rust-1.95.toml.sha256` connect timeout,
# 构建直接失败)。release 构建不需要 rustfmt/clippy,镜像里的 1.95.0 就是要的版本。
# 与 caio-check.sh 用的是同一个变量、同一个理由。
ENV RUSTUP_TOOLCHAIN=1.95.0-x86_64-unknown-linux-gnu
WORKDIR /app
COPY . .
# 用前端构建产物覆盖(.dockerignore 已排除本地 dist,保证用 CI 重建的版本)
COPY --from=frontend /ui/dist ./admin-ui/dist
RUN cargo build --release -p gw-app --features embed-ui

# --- 3. 运行时:slim + CA 证书(reqwest rustls 需根证书;rusqlite bundled 无需系统 sqlite) ---
FROM debian:bookworm-slim AS runtime
# curl:web search 执行器经它发 DuckDuckGo 请求(reqwest/rustls 的 TLS 指纹被 DDG 反爬拦截,
# curl 的 OpenSSL 指纹放行;经 curl 调用完全隔离,绝不触碰 provider/egress 的 reqwest TLS 栈)。
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/claude-all-in-one /usr/local/bin/claude-all-in-one
# config/(只读)与 data/(SQLite)运行时挂载;默认路径相对 CWD=/app。
ENTRYPOINT ["claude-all-in-one"]
