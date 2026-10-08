# syntax=docker/dockerfile:1
#
# ncc-registry 镜像：多阶段构建成单二进制 + 最小运行层。
#
# 构建（在仓库根目录下，上下文是仓库根）：
#   docker build -f deploy/Dockerfile -t ncc-registry:dev .
# 或在 deploy/ 下直接 docker compose up -d --build。
#
# 为什么上下文是仓库根而不是 rust/：deploy/ 在上下文之外，Docker 的 COPY 只能读
# 上下文里的东西；上下文设为仓库根之后 `COPY rust/...` 就够，也便于顺带带上 web/。

# ---------- 阶段 1：编译 ----------
# 用完整 debian 镜像（不是 slim）：sqlx 的 SQLite 驱动会编译 bundled 的 sqlite3 C 源码，
# 需要 cc 与 make —— slim 镜像里没有，构建会在 libsqlite3-sys 那里失败。
FROM rust:1-bookworm AS build
WORKDIR /src

# 先拷依赖清单 + 占位源码，命中依赖层缓存：改业务代码不会重新编一遍依赖。
COPY rust/Cargo.toml rust/Cargo.lock ./
COPY rust/crates/ncc-core/Cargo.toml crates/ncc-core/Cargo.toml
COPY rust/crates/ncc-registry/Cargo.toml crates/ncc-registry/Cargo.toml
RUN mkdir -p crates/ncc-core/src crates/ncc-registry/src \
 && : > crates/ncc-core/src/lib.rs \
 && echo 'fn main() {}' > crates/ncc-registry/src/main.rs \
 && cargo build --release --bin ncc-registry

COPY rust/crates ./crates
# 真正编译一次（上面那次是空源码的依赖预热）
RUN touch crates/ncc-core/src/lib.rs crates/ncc-registry/src/main.rs \
 && cargo build --release --bin ncc-registry

# ---------- 阶段 2：运行时 ----------
FROM debian:bookworm-slim
# ca-certificates：出网（集群转发）需要根证书；curl：容器健康检查
# tzdata：心跳时间与本地时区显示
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl tzdata \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -u 10001 -m -d /app ncc

COPY --from=build /src/target/release/ncc-registry /usr/local/bin/ncc-registry
# 内置控制台：Go 版用 go:embed 打进二进制，Rust 版从目录托管，所以这里把资源带进镜像
# 并把 NCCR_CONSOLE_DIR 指过去（不指的话控制台不挂，API 照常可用）。
COPY rust/web /app/web

# 数据目录（库 + blobs + node-id + jwt-secret）挂在这里，备份 = 打包这个目录
ENV NCCR_DATA_DIR=/data \
    NCCR_PORT=8282 \
    NCCR_PUBLIC_URL=http://localhost:8282 \
    NCCR_CONSOLE_DIR=/app/web
VOLUME ["/data"]
EXPOSE 8282
USER ncc
WORKDIR /app
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:${NCCR_PORT}/api/health" >/dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/ncc-registry"]
