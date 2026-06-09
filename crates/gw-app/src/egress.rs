//! 出口绑定 —— 多进程防关联的核心(🟣自创,三参考项目都不做源IP绑定)。
//!
//! 每个 worker 进程按 [`EgressConfig`] 构造一个**固定出口**的 HTTP client:
//! - `Direct`:系统默认出口
//! - `LocalIp`:reqwest `local_address` 绑定本机源 IP(单机双 IPv4)
//! - `Proxy`:走外部代理(固定 IP 代理池)
//!
//! ⚠️ Phase 3 待实测:`local_address` 对 Kiro 上游(纯 IPv4)是否生效。

use std::net::IpAddr;
use std::time::Duration;

use gw_core::config::EgressConfig;
use reqwest::Client;

/// 按 egress 配置构造 HTTP client。
///
/// 整个 worker 进程的上游请求(发包/刷新/usage)都用这个 client,
/// 保证同出口一致性(static_flow 的代理一致性铁律,见 IMPROVEMENTS §2.3)。
pub fn build_client(egress: &EgressConfig) -> anyhow::Result<Client> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(300))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60));

    match egress {
        EgressConfig::Direct => {}
        EgressConfig::LocalIp { address } => {
            let ip: IpAddr = address
                .parse()
                .map_err(|e| anyhow::anyhow!("egress local_ip 地址非法 '{address}': {e}"))?;
            builder = builder.local_address(ip);
            tracing::info!(%ip, "egress 绑定本机源 IP");
        }
        EgressConfig::Proxy {
            url,
            username,
            password,
        } => {
            let mut proxy = reqwest::Proxy::all(url)
                .map_err(|e| anyhow::anyhow!("egress 代理 URL 非法 '{url}': {e}"))?;
            if let (Some(u), Some(p)) = (username, password) {
                proxy = proxy.basic_auth(u, p);
            }
            builder = builder.proxy(proxy);
            tracing::info!(%url, "egress 走外部代理");
        }
    }

    Ok(builder.build()?)
}

/// 出口的人类可读描述(日志/admin 用)。
pub fn describe(egress: &EgressConfig) -> String {
    match egress {
        EgressConfig::Direct => "direct".into(),
        EgressConfig::LocalIp { address } => format!("local_ip:{address}"),
        EgressConfig::Proxy { url, .. } => format!("proxy:{url}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_direct_client() {
        assert!(build_client(&EgressConfig::Direct).is_ok());
    }

    #[test]
    fn build_local_ip_client() {
        // 回环地址一定可解析(绑定到回环出口在构造期不报错)。
        let cfg = EgressConfig::LocalIp {
            address: "127.0.0.1".into(),
        };
        assert!(build_client(&cfg).is_ok());
    }

    #[test]
    fn build_local_ip_rejects_garbage() {
        let cfg = EgressConfig::LocalIp {
            address: "not-an-ip".into(),
        };
        assert!(build_client(&cfg).is_err());
    }

    #[test]
    fn describe_variants() {
        assert_eq!(describe(&EgressConfig::Direct), "direct");
        assert_eq!(
            describe(&EgressConfig::LocalIp {
                address: "1.2.3.4".into()
            }),
            "local_ip:1.2.3.4"
        );
    }
}
