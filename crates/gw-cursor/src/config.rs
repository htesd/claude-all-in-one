//! `aiserver.v1.ServerConfigService/GetServerConfig`(unary)—— 取会话级 `config_version`。
//!
//! 逆向 3.14.27 确认(破门关键):`x-cursor-config-version` **不是**随机 uuid,也不是磁盘
//! (`cursorai/serverConfig`)里那个可能已过期的值,而是每会话从本 unary RPC 拿到、之后回显到
//! 所有请求头的服务端下发值。服务端只认当前有效值,回显随机/过期值 → 推理路径被完整性门
//! 以 `resource_exhausted`/"Update Required" 软封(unary 如 AvailableModels 不校验故能过)。
//!
//! 响应 schema:`GetServerConfigResponse{ 6: config_version(string) }`(field 6, T:9)。
//! unary Connect:`content-type: application/proto`,请求/响应体是裸 protobuf(无 5 字节信封)。

use gw_core::error::{UpstreamError, UpstreamErrorKind};

use crate::protobuf::{Reader, Value as PbValue};
use crate::wire;

/// 调 GetServerConfig 取当前会话有效的 `config_version`。
///
/// 身份三元组(machine_id / mac_machine_id / token 派生的 client-key、session-id)必须与后续
/// chat 请求一致,服务端才会把此 config_version 认作同一 client 实例的。
pub async fn fetch_config_version(
    client: &reqwest::Client,
    host: &str,
    token: &str,
    machine_id: &str,
    mac_machine_id: Option<&str>,
    // 必须与推理请求用的是**同一个**时区。硬编码一个常量会让同一个"客户端会话"的
    // unary 请求与推理请求报不同时区 —— 一个内部自相矛盾的指纹,比配错更可疑。
    timezone: &str,
) -> Result<String, UpstreamError> {
    let url = format!("https://{host}/aiserver.v1.ServerConfigService/GetServerConfig");
    let request_id = uuid::Uuid::new_v4().to_string();
    let resp = client
        .post(&url)
        .header("authorization", format!("Bearer {token}"))
        .header("connect-protocol-version", "1")
        // unary:裸 proto,非 connect+proto(那是流式带信封)。
        .header("content-type", "application/proto")
        .header("user-agent", "connect-es/1.6.1")
        .header(
            "x-cursor-checksum",
            wire::checksum(machine_id, mac_machine_id),
        )
        .header("x-cursor-client-version", wire::CLIENT_VERSION)
        .header("x-cursor-client-commit", wire::CLIENT_COMMIT)
        // ⚠️ 这里是 `ide`,而推理请求(chat.rs)发的是 `glass` —— **不是笔误**,
        // 两个端点的抓包实物就是不同的值。别为了"统一"改这里(chat.rs 那边也有同款注释)。
        .header("x-cursor-client-type", "ide")
        .header("x-cursor-client-os", wire::CLIENT_OS)
        .header("x-cursor-client-arch", wire::CLIENT_ARCH)
        .header("x-cursor-client-os-version", wire::CLIENT_OS_VERSION)
        .header("x-cursor-client-device-type", "desktop")
        .header("x-cursor-canary", "true")
        .header("x-cursor-timezone", timezone)
        .header("x-client-key", wire::client_key(token))
        .header("x-session-id", wire::session_id(token))
        .header("x-request-id", &request_id)
        .header("x-amzn-trace-id", format!("Root={request_id}"))
        .header("x-new-onboarding-completed", "true")
        .header("x-ghost-mode", "false")
        // 空请求体:GetServerConfigRequest 各字段可选,服务端接受空消息。
        .body(Vec::<u8>::new())
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("Cursor GetServerConfig 请求失败: {e}")))?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| UpstreamError::network(format!("读 GetServerConfig 响应失败: {e}")))?;
    if !status.is_success() {
        return Err(UpstreamError::new(
            UpstreamErrorKind::ServerError,
            format!(
                "Cursor GetServerConfig {}: {}",
                status.as_u16(),
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ),
        ));
    }
    parse_config_version(&bytes).ok_or_else(|| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            "GetServerConfig 响应缺少 config_version(field 6)".to_string(),
        )
    })
}

/// 从 GetServerConfigResponse 裸 protobuf 里抽 field 6(config_version, string)。
fn parse_config_version(payload: &[u8]) -> Option<String> {
    for (field, val) in Reader::new(payload) {
        if field == 6 {
            if let PbValue::Len(bytes) = val {
                return Some(String::from_utf8_lossy(bytes).to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::Writer;

    #[test]
    fn parses_field6_string() {
        let mut w = Writer::new();
        w.string(1, "ignore");
        w.string(6, "cfg-abc-123");
        w.uint(7, 2);
        assert_eq!(
            parse_config_version(&w.into_bytes()).as_deref(),
            Some("cfg-abc-123")
        );
    }

    #[test]
    fn missing_field6_is_none() {
        let mut w = Writer::new();
        w.string(1, "x");
        assert_eq!(parse_config_version(&w.into_bytes()), None);
    }
}
