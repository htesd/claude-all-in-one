//! 模型元数据与设备指纹契约。

use serde::{Deserialize, Serialize};

/// 对外暴露的模型信息(用于 `/v1/models` 与 catalog)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    /// 对外模型 id(客户端看到的,如 `claude-opus-4-8`)。
    pub id: String,
    /// 展示名。
    #[serde(default)]
    pub display_name: Option<String>,
    /// 上下文窗口 token 数。
    #[serde(default)]
    pub context_length: Option<u32>,
    /// 能力标记。
    #[serde(default)]
    pub supports_thinking: bool,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_vision: bool,
}

impl ModelInfo {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            context_length: None,
            supports_thinking: false,
            supports_tools: false,
            supports_vision: false,
        }
    }
}

/// 设备指纹 —— **Kiro 专属概念**(防封一致性核心)。
///
/// ⚠️ 技术债(审查 H5a):此类型当前只服务 Kiro provider。subprocess/dario
/// 通道用 OAuth token + 独立 HOME 隔离,没有 machineId/UA 概念。暂留 gw-core
/// 是为复用 serde/校验,但它**不**属于通用 Provider trait(已从 trait 移除
/// `machine_identity` 方法,降为 Kiro 的 inherent 方法)。等出现第二个需要设备
/// 指纹的 provider 时,再考虑抽象或迁入 gw-kiro。
///
/// 关键事实(见 memory rewrite-recon-findings):`machine_id` 嵌在
/// 上游请求的 `User-Agent` / `x-amz-user-agent` 末尾
/// (`KiroIDE-{version}-{machine_id}`)。**同一账号的 machine_id 必须
/// 跨"激活/刷新/发包"始终一致**,否则触发风控(¥900 封号根因)。
///
/// 一致性保证:理想是账号**显式持有**真机 machine_id(import 带入)。但 Social/OAuth
/// 号常无显式值,此时 Kiro provider 按 `sha256("KotlinNativeAPI/"+refresh_token)` 用
/// **当前** rt 派生(对齐上游客户端 + kiro.rs 的派生公式)。即"显式优先,缺失则**每次**
/// 用当前 rt 派生"——**不冻结**(2026-06-12 撤销旧冻结):真实客户端 rt 滚动时 machineId
/// 随之滚动,冻结会发陈旧值反而像换设备 → 封号(见 machine_id::freeze 的弃用说明)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineIdentity {
    /// 64 字符小写 hex 机器码。
    pub machine_id: String,
    /// 客户端版本(如 Kiro `0.12.155`)。
    pub client_version: String,
    /// 完整 `User-Agent`。
    pub user_agent: String,
    /// `x-amz-user-agent`。
    pub x_amz_user_agent: String,
}

impl MachineIdentity {
    /// 是否为合法的 64 字符**小写**十六进制 machine_id。
    ///
    /// 收紧为小写(审查 low):大小写混用会让从 UA 派生的签名/指纹/缓存 key
    /// 不一致。真实 Kiro machineId 是小写 hex,这里强制规范形态。
    pub fn is_valid_machine_id(&self) -> bool {
        self.machine_id.len() == 64
            && self
                .machine_id
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_info_defaults() {
        let m = ModelInfo::new("claude-opus-4-8");
        assert_eq!(m.id, "claude-opus-4-8");
        assert!(!m.supports_thinking);
    }

    #[test]
    fn machine_id_validation() {
        let good = MachineIdentity {
            machine_id: "a".repeat(64),
            client_version: "0.12.155".into(),
            user_agent: "ua".into(),
            x_amz_user_agent: "xua".into(),
        };
        assert!(good.is_valid_machine_id());

        let bad = MachineIdentity {
            machine_id: "xyz".into(),
            ..good.clone()
        };
        assert!(!bad.is_valid_machine_id());
    }

    #[test]
    fn model_info_roundtrips_yaml() {
        let m = ModelInfo {
            id: "m".into(),
            display_name: Some("M".into()),
            context_length: Some(200_000),
            supports_thinking: true,
            supports_tools: true,
            supports_vision: false,
        };
        let y = serde_yaml::to_string(&m).unwrap();
        let back: ModelInfo = serde_yaml::from_str(&y).unwrap();
        assert_eq!(m, back);
    }
}
