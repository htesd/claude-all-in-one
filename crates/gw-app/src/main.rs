//! claude-all-in-one 二进制入口。
//!
//! 单二进制多角色:`--mode router` 或 `--mode worker --instance N`。
//! 见 docs/ARCHITECTURE.md §1。

mod admin;
mod egress;
mod registry;
mod router;
mod websearch;
mod worker;

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// 内网头:router 鉴权后把客户 key 透传给 worker,worker 据此把 usage 归属到该客户。
/// 仅在 router→worker 的 localhost 内网跳使用(对外 Authorization 不透传给 worker)。
pub const CLIENT_KEY_HEADER: &str = "x-gw-client-key";

/// 内网头:影子组(低价档)的档位守卫策略,紧凑 JSON(如 `{"max_priority":0}`)。
/// **只由 router 依据 DB 里的组配置生成**;`send_messages_to_worker` 是白名单转发,
/// 客户端自带的同名头到不了 worker,因此无法伪造/绕过档位。
/// 头缺席 = 普通组请求(worker 走与本特性上线前完全相同的路径)。
pub const TIER_HEADER: &str = "x-gw-tier";

/// 优雅停机信号:SIGTERM(docker stop / systemd)或 Ctrl-C。
/// 触发后 axum 停止接收新连接,在途请求(含流式 SSE)自然跑完;不设排空上限——
/// 硬截止由 supervisor 兜底(docker 默认 10s 后 SIGKILL,systemd TimeoutStopSec)。
pub(crate) async fn shutdown_signal(role: &'static str) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!("注册 SIGTERM 监听失败,仅响应 Ctrl-C: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("{role} 收到停机信号:停止接收新连接,等待在途请求完成");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// 前置路由:鉴权 + session→worker 亲和 + 转发。
    Router,
    /// 实际反代:绑定固定出口 + 管理一组账号。
    Worker,
}

#[derive(Debug, Parser)]
#[command(name = "claude-all-in-one", about = "Claude All in One —— 多账号多进程反代")]
struct Args {
    /// 进程角色。
    #[arg(long, value_enum)]
    mode: Mode,

    /// worker 实例号(--mode worker 必填,对应 instances.yaml 的 instance)。
    #[arg(long)]
    instance: Option<u32>,

    /// instances.yaml 路径。
    #[arg(long, default_value = "config/instances.yaml")]
    instances: PathBuf,

    /// accounts.yaml 路径。
    #[arg(long, default_value = "config/accounts.yaml")]
    accounts: PathBuf,

    /// system.yaml 路径。
    #[arg(long, default_value = "config/system.yaml")]
    system: PathBuf,

    /// SQLite 控制面路径。
    #[arg(long, default_value = "data/control.db")]
    db: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    match args.mode {
        Mode::Router => {
            tracing::info!("启动 router 角色");
            router::run(&args.instances, &args.db, &args.system).await
        }
        Mode::Worker => {
            let instance = args.instance.ok_or_else(|| {
                anyhow::anyhow!("--mode worker 需要 --instance N")
            })?;
            tracing::info!(instance, "启动 worker 角色");
            worker::run(
                instance,
                &args.instances,
                &args.accounts,
                &args.system,
                &args.db,
            )
            .await
        }
    }
}
