//! kiro-gw 二进制入口。
//!
//! 单二进制多角色:`--mode router` 或 `--mode worker --instance N`。
//! 见 docs/ARCHITECTURE.md §1。

mod egress;
mod registry;
mod router;
mod worker;

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// 前置路由:鉴权 + session→worker 亲和 + 转发。
    Router,
    /// 实际反代:绑定固定出口 + 管理一组账号。
    Worker,
}

#[derive(Debug, Parser)]
#[command(name = "kiro-gw", about = "Kiro 反代网关(多进程)")]
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
            router::run(&args.instances, &args.db).await
        }
        Mode::Worker => {
            let instance = args.instance.ok_or_else(|| {
                anyhow::anyhow!("--mode worker 需要 --instance N")
            })?;
            tracing::info!(instance, "启动 worker 角色");
            worker::run(instance, &args.instances, &args.accounts, &args.system).await
        }
    }
}
