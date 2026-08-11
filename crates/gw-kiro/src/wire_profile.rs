//! 线缆形态总开关 —— `KIRO_LEGACY_WIRE=1` 把发往 Kiro 上游的报文**整体**退回
//! 2026-07-28(`58b6f27` 对齐 1.0.212)之前的形态,即 UA 自报 0.12.155 时代跑了
//! 数月、无大规模封号的那个形态。默认关 = 现状(1.0.212 形态),一个字节都不变。
//!
//! 背景:2026-08 上旬 PRO+ 付费号批量被封(TEMPORARILY_SUSPENDED,实为永封),
//! 周末调查(`.ban-investigation.md`)把"07-28 线缆形态变更引入新指纹"列为头位
//! 可疑内因之一。本开关是最低成本的可逆判别实验:打开即整体回退,拆掉即恢复,
//! 不需要 git revert —— 这两周叠加的新功能(模型目录、RPM 闸门等)全部保留。
//!
//! 总开关协调的维度(**只支持两种自洽形态,杜绝半新半旧的混搭**):
//!
//! | 维度 | 默认(1.0.212) | legacy(0.12.155 时代) |
//! |---|---|---|
//! | UA 自报版本 | 1.0.212 | 0.12.155(headers.rs) |
//! | body 顶层 `agentMode` | 必发 `"vibe"` | 不发(chat.rs 构造时置 None) |
//! | `additionalModelRequestFields` | 按策略发 | 不发(chat.rs 构造时置 None) |
//! | 当前消息空 `userInputMessageContext` | 省略 | 照发 `{}`(conversation.rs 谓词) |
//! | 思考强度载体 | 结构化字段 | 旧文本标签(converter/history.rs) |
//! | 配额/profiles 控制面域名 | management.*.kiro.dev | q.*.amazonaws.com(usage_limits.rs) |
//!
//! 形态切换全部落在**构造点与 serde 谓词**上,不做序列化后改写:struct 字段顺序
//! 就是线缆字节顺序(与真客户端的 key 插入顺序逐字对齐过,见 HANDOFF-2026-07-28
//! §4),二次 Value 往返会把它排成字母序,反而偏离新旧两种金标准。
//!
//! 刻意的**例外**(功能保留,不随形态回退):
//! - `ListAvailableModels`(models_api.rs)是 07-28 才新增的只读控制面调用,
//!   旧形态里根本没有对应物 —— 继续走 management 域,且只在 admin 手动触发时调用,
//!   不在逐请求热路径上,不构成形态指纹。
//! - 上游 conversationId 按账号加盐(08-07 `3fd695e`)是防关联修复,与 07-28
//!   形态无关,不回退。
//! - 思考预算下限 8192(`fac24c0`)与默认档位 high 是**策略**(保智力/延迟),
//!   只改字段里的值、不改字段形态,保留。
//!
//! ⚠️ 缓存前缀:若 worker 同时开着 `KIRO_THINKING_IN_HISTORY0=1`(生产即如此),
//! 旧标签重新注入 history[0] = 前缀第一块字节变化,所有在途会话下一轮缓存 miss
//! 一次。低峰切换,与当初升级时同一注意事项(见 history.rs 的 warn_if_prefix_breaking)。

/// 当前是否为旧线缆形态。逐调用读 env(与 `generate_thinking_prefix` 等热路径
/// 现有 env 读取同口径;worker 进程 env 生命周期内不变,无热改需求)。
pub fn legacy_wire() -> bool {
    gw_core::env_flag("KIRO_LEGACY_WIRE")
}
