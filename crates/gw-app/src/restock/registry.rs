//! 供应商名册:配置在哪、怎么建出客户端、每家的独立状态键。
//!
//! ## 为什么配置不在 `system.yaml`
//!
//! [`gw_core::config::RestockConfig`] 挂着 `deny_unknown_fields`。给它加一个 `suppliers:`
//! 段之后,**回滚到不认识该字段的旧镜像会在启动时报错** —— 整个网关起不来。
//! 补货参数当初绕开 `SystemSettings` 就是同一个理由(见 `params.rs` 开头)。
//!
//! 所以名册存 control.db 的 `settings` 表(键 [`KEY_SUPPLIERS`]):旧镜像**读不到就当没有**,
//! 回落成「只有 drop 一家」,与本次改动前逐字节等价。这是唯一一个既能热改、又不阻断回滚的位置。
//!
//! ## 密钥不出现在任何响应里
//!
//! `api_key` 只进 DB 与客户端,`GET /restock/suppliers` 只回 [`SupplierCfg::has_key`]。
//! 连掩码形态都不给 —— 前端没有任何理由需要它,而掩码值曾经在 `PUT /settings` 上
//! 引发过一次「把 `***` 当成真值存回去」的事故。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::drop::DropClient;
use super::kiroapp::KiroappClient;
use super::supplier::Supplier;

/// 名册在 `settings` 表里的键。
pub const KEY_SUPPLIERS: &str = "restock_suppliers";

pub const KIND_DROP: &str = "drop";
pub const KIND_KIROAPP: &str = "kiroapp";

/// 一家供应商的配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SupplierCfg {
    /// 稳定标识。进订单表、熔断键与决策流水,**改名等于换一家**(旧订单与旧熔断状态会失联),
    /// 所以它不是显示名 —— 显示名就用它本身,别另设一个会漂移的字段。
    pub id: String,
    /// 用哪个适配器。未知 kind 会被跳过并告警,而不是猜一个。
    pub kind: String,
    #[serde(default)]
    pub enabled: bool,
    /// 空 = 用适配器的默认地址。
    #[serde(default)]
    pub base_url: String,
    /// 空 = 对 `drop` 回落 `system.yaml` 里的密钥(历史配置不用迁移);其余 kind 视为未配置。
    #[serde(default)]
    pub api_key: String,
    /// 本家当日花费上限(¥)。**0 = 不限**,由全局日上限兜底。
    ///
    /// 存在的意义是**限制单家的敞口**:kiroapp 实测既无服务端限价、也不校验数量上限
    /// (发 99 会 clamp 成 10 并成交),所以「这家最多能从我这里拿走多少钱」必须由我方表达。
    #[serde(default)]
    pub daily_cap_cny: f64,
    /// 档位,**数值越小越优先**(与账号优先级同向)。缺省 `0`。
    ///
    /// 语义是**软优先**不是硬绑定:高档位有货就买高档位,高档位缺货/超上限/熔断时
    /// 自动落到下一档。绝不会因为首选家没货就停止补货 —— 那正是多供应商要消除的断供。
    ///
    /// 为什么需要它:价格可观测,**号的质量不可观测**。2026-08-05 实测 drop 侧 29 个号
    /// 0 次 `temporarily_suspended`,kiroapp/eu 侧 12 个号 12 次(其中一个零成功即封)。
    /// 「便宜 48% 但到手即封」无法编码进单价,只能由人按观察结果表达成档位。
    #[serde(default)]
    pub priority: i64,
    /// 逐货架的档位覆盖,键是**供应商自己的货架标识**(kiroapp 是 `us`/`eu`;drop 是空串)。
    /// 缺省空 = 该家所有货架都用 [`Self::priority`]。
    ///
    /// 必须细到货架而不是只到家:实测出问题的是 `kiroapp/eu` 这一个货架,而
    /// `kiroapp/us` 是同一家的另一批号。只能整家降级的话,要么误伤 US、要么放过 EU。
    ///
    /// 键写错(如把 `eu` 写成 `EU`)是**静默失效**而非报错 —— 校验不认识具体哪家有哪些
    /// 货架。所以面板必须显示每个货架的**生效档位**,让写错当场看得见。
    #[serde(default)]
    pub shelf_priority: std::collections::BTreeMap<String, i64>,
}

impl SupplierCfg {
    /// 只有 drop 一家的默认名册 —— 名册键缺失时用它,行为与多供应商引入前完全一致。
    pub fn default_roster() -> Vec<SupplierCfg> {
        vec![SupplierCfg {
            id: KIND_DROP.into(),
            kind: KIND_DROP.into(),
            enabled: true,
            base_url: String::new(),
            api_key: String::new(),
            daily_cap_cny: 0.0,
            priority: 0,
            shelf_priority: Default::default(),
        }]
    }

    /// 给面板的安全投影:**不含密钥**。
    pub fn redacted(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "kind": self.kind,
            "enabled": self.enabled,
            "base_url": self.base_url,
            "daily_cap_cny": self.daily_cap_cny,
            "priority": self.priority,
            "shelf_priority": self.shelf_priority,
            "has_key": !self.api_key.trim().is_empty(),
        })
    }
}

/// 某个货架的**生效档位**:先查逐货架覆盖,再回落本家档位,都没有就 `0`。
///
/// 名册里查不到这家时返回 `0`(与引入档位前同序),而不是排到最后:
/// 货架只会来自名册里建出来的客户端,查不到只可能是名册刚被改小,
/// 这时把在途的报价排到最后没有任何好处,反而会让「改个名册就停止补货」变成可能。
pub fn shelf_priority_of(roster: &[SupplierCfg], supplier_id: &str, shelf_id: &str) -> i64 {
    let Some(c) = roster.iter().find(|c| c.id.trim() == supplier_id) else {
        return 0;
    };
    c.shelf_priority.get(shelf_id).copied().unwrap_or(c.priority)
}

/// 解析名册 JSON。坏数据一律回落默认名册 —— 补货宁可只用 drop,也不能因为一段
/// 手滑写坏的 JSON 就整个停摆。
pub fn parse_roster(raw: Option<&str>) -> Vec<SupplierCfg> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return SupplierCfg::default_roster();
    };
    match serde_json::from_str::<Vec<SupplierCfg>>(raw) {
        Ok(v) if !v.is_empty() => dedup_by_id(v),
        Ok(_) => SupplierCfg::default_roster(),
        Err(e) => {
            tracing::error!("补货:供应商名册解析失败,本次只用 drop: {e}");
            SupplierCfg::default_roster()
        }
    }
}

/// 同 id 只留第一条。
///
/// 写路径的 [`validate_roster`] 已经拦了重复 id,但**读路径完全信任 DB**,
/// 而 `settings` 表可以被 sqlite 直接改。重复 id 的后果不是「多建一个客户端」那么轻:
/// `choose_shelf` 里的余额表按 `supplier_id` 收集,后一家会**覆盖**前一家的余额,
/// 于是用 A 的余额批准了 B 的购买;两家还共用同一个熔断键与日上限键。
fn dedup_by_id(list: Vec<SupplierCfg>) -> Vec<SupplierCfg> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(list.len());
    for c in list {
        if seen.insert(c.id.trim().to_string()) {
            out.push(c);
        } else {
            tracing::error!("补货:名册里有重复的供应商 id `{}`,只用第一条", c.id.trim());
        }
    }
    out
}

/// 校验一份待保存的名册。返回第一条错误。
///
/// 在**写入时**校验而不是读取时:读取时报错等于运行中才发现配置坏了,而那时补货已经停了。
pub fn validate_roster(list: &[SupplierCfg]) -> Result<(), String> {
    if list.is_empty() {
        return Err("名册不能为空".into());
    }
    let mut seen = std::collections::HashSet::new();
    for c in list {
        let id = c.id.trim();
        if id.is_empty() {
            return Err("供应商 id 不能为空".into());
        }
        // id 会进订单表、熔断键与日志。限成 ASCII 单词是为了让 `restock_breaker:{id}`
        // 这类拼出来的键不会因为空格/冒号而歧义。
        if !id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
            return Err(format!("供应商 id `{id}` 只能用字母数字与 - _"));
        }
        if !seen.insert(id.to_string()) {
            return Err(format!("供应商 id `{id}` 重复"));
        }
        if ![KIND_DROP, KIND_KIROAPP].contains(&c.kind.trim()) {
            return Err(format!("未知供应商类型 `{}`(可选 drop / kiroapp)", c.kind));
        }
        if c.daily_cap_cny < 0.0 || !c.daily_cap_cny.is_finite() {
            return Err(format!("`{id}` 的日上限必须是 ≥0 的有限数"));
        }
        if c.kind.trim() == KIND_KIROAPP && c.api_key.trim().is_empty() {
            return Err(format!("`{id}` 缺 API Key"));
        }
    }
    Ok(())
}

/// 建出**已启用且配置完整**的客户端。跳过的每一家都会记一条 warn ——
/// 静默跳过会让人对着「为什么这家从来不被选中」查半天。
///
/// `yaml_key` 是 `system.yaml` 里 drop 的历史密钥,只在名册里没填时兜底。
pub fn build(roster: &[SupplierCfg], yaml_base: &str, yaml_key: &str) -> Vec<Arc<dyn Supplier>> {
    let mut out: Vec<Arc<dyn Supplier>> = Vec::new();
    for c in roster {
        if !c.enabled {
            continue;
        }
        let id = c.id.trim();
        let built: anyhow::Result<Arc<dyn Supplier>> = match c.kind.trim() {
            KIND_DROP => {
                // 名册留空 → 用 system.yaml 的历史配置。老部署因此不需要迁移密钥。
                let key = if c.api_key.trim().is_empty() { yaml_key } else { c.api_key.trim() };
                let base = if c.base_url.trim().is_empty() { yaml_base } else { c.base_url.trim() };
                if key.is_empty() {
                    tracing::warn!("补货:供应商 {id} 没有密钥,跳过");
                    continue;
                }
                DropClient::with_id(id, base, key).map(|c| Arc::new(c) as Arc<dyn Supplier>)
            }
            KIND_KIROAPP => {
                if c.api_key.trim().is_empty() {
                    tracing::warn!("补货:供应商 {id} 没有密钥,跳过");
                    continue;
                }
                KiroappClient::new(id, c.base_url.trim(), c.api_key.trim())
                    .map(|c| Arc::new(c) as Arc<dyn Supplier>)
            }
            other => {
                tracing::warn!("补货:供应商 {id} 的类型 `{other}` 不认识,跳过");
                continue;
            }
        };
        match built {
            Ok(c) => out.push(c),
            Err(e) => tracing::error!("补货:供应商 {id} 客户端构造失败,跳过: {e}"),
        }
    }
    out
}

/// 有几家是「已启用且拿得出密钥」的。
///
/// 与 [`build`] 同判据但**不构造客户端** —— 面板每 15 秒问一次状态,
/// 为了回答「配好了吗」而每次新建两个 reqwest 连接池是纯浪费。
pub fn usable_count(roster: &[SupplierCfg], yaml_key: &str) -> usize {
    roster
        .iter()
        .filter(|c| {
            c.enabled
                && match c.kind.trim() {
                    KIND_DROP => !c.api_key.trim().is_empty() || !yaml_key.trim().is_empty(),
                    KIND_KIROAPP => !c.api_key.trim().is_empty(),
                    _ => false,
                }
        })
        .count()
}

/// 每家独立的熔断键。
///
/// **必须逐家分开**:kiroapp 的 API Key 失效不该让 drop 也停止补货 —— 那正好是
/// 多供应商要解决的断供问题,却被一个全局开关反手制造出来。
pub fn breaker_key(supplier_id: &str) -> String {
    format!("restock_breaker:{supplier_id}")
}

/// 每家独立的连败计数(购买故障)。
pub fn fault_streak_key(supplier_id: &str) -> String {
    format!("restock_fault_streak:{supplier_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(id: &str, kind: &str) -> SupplierCfg {
        SupplierCfg {
            id: id.into(),
            kind: kind.into(),
            enabled: true,
            base_url: String::new(),
            api_key: "k".into(),
            daily_cap_cny: 0.0,
            priority: 0,
            shelf_priority: Default::default(),
        }
    }

    #[test]
    fn 名册缺失或损坏时回落成只有drop一家() {
        // 这条钉的是**回滚安全**:老库里没有这个键,行为必须与多供应商引入前一致。
        for raw in [None, Some(""), Some("  "), Some("{坏的"), Some("[]")] {
            let r = parse_roster(raw);
            assert_eq!(r.len(), 1, "输入 {raw:?} 应回落默认名册");
            assert_eq!(r[0].kind, KIND_DROP);
            assert!(r[0].enabled);
            assert!(r[0].api_key.is_empty(), "默认名册不带密钥,靠 yaml 兜底");
        }
    }

    #[test]
    fn 默认名册建出的drop用的是yaml密钥() {
        let built = build(&SupplierCfg::default_roster(), "https://drop.example", "usr-abc");
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].id(), "drop");
    }

    #[test]
    fn 没密钥的家会被跳过而不是带着必然401的客户端上场() {
        let mut r = SupplierCfg::default_roster();
        r[0].api_key = String::new();
        assert!(build(&r, "", "").is_empty(), "yaml 也没有密钥时必须跳过");

        let mut k = cfg("kiroapp", KIND_KIROAPP);
        k.api_key = String::new();
        assert!(build(&[k], "", "usr-yaml").is_empty(), "kiroapp 不该借用 drop 的 yaml 密钥");
    }

    #[test]
    fn 停用的家不会被建出来() {
        let mut k = cfg("kiroapp", KIND_KIROAPP);
        k.enabled = false;
        assert!(build(&[k], "", "").is_empty());
    }

    #[test]
    fn 校验拦住重复id与非法id与未知类型() {
        assert!(validate_roster(&[]).is_err());
        assert!(validate_roster(&[cfg("a", KIND_DROP), cfg("a", KIND_KIROAPP)]).is_err(), "重复 id");
        assert!(validate_roster(&[cfg("", KIND_DROP)]).is_err());
        // 冒号会让 `restock_breaker:{id}` 拼出歧义键。
        assert!(validate_roster(&[cfg("a:b", KIND_DROP)]).is_err());
        assert!(validate_roster(&[cfg("a b", KIND_DROP)]).is_err());
        assert!(validate_roster(&[cfg("x", "某家新的")]).is_err());
        let mut bad = cfg("x", KIND_DROP);
        bad.daily_cap_cny = -1.0;
        assert!(validate_roster(&[bad]).is_err());
        // drop 可以不填密钥(回落 yaml),kiroapp 不行。
        let mut d = cfg("drop", KIND_DROP);
        d.api_key = String::new();
        assert!(validate_roster(&[d]).is_ok());
        let mut k = cfg("kiroapp", KIND_KIROAPP);
        k.api_key = String::new();
        assert!(validate_roster(&[k]).is_err());
    }

    #[test]
    fn 投影不含密钥() {
        let v = cfg("kiroapp", KIND_KIROAPP).redacted();
        assert_eq!(v["has_key"], true);
        assert!(v.get("api_key").is_none(), "密钥绝不能出现在响应里,掩码也不行");
        assert!(!v.to_string().contains('k') || !v.to_string().contains("\"k\""));
    }

    #[test]
    fn 货架档位先看逐货架覆盖再回落本家档位() {
        let mut k = cfg("kiroapp", KIND_KIROAPP);
        k.priority = 1;
        k.shelf_priority.insert("eu".into(), 2);
        let d = cfg("drop", KIND_DROP); // priority 缺省 0
        let roster = [d, k];

        assert_eq!(shelf_priority_of(&roster, "drop", ""), 0);
        assert_eq!(shelf_priority_of(&roster, "kiroapp", "us"), 1, "没覆盖 → 用本家档位");
        assert_eq!(shelf_priority_of(&roster, "kiroapp", "eu"), 2, "有覆盖 → 用覆盖值");
    }

    #[test]
    fn 覆盖键写错时静默回落本家档位不报错也不乱序() {
        // 面板显示生效档位是这个设计的配套 —— 这里钉住「不炸」,可见性由 UI 负责。
        let mut k = cfg("kiroapp", KIND_KIROAPP);
        k.priority = 1;
        k.shelf_priority.insert("EU".into(), 9); // 大小写写错
        assert_eq!(shelf_priority_of(&[k], "kiroapp", "eu"), 1);
    }

    #[test]
    fn 读路径对重复id只留第一条免得余额张冠李戴() {
        // 重复 id 只能绕过 admin API 直接改库产生,但读路径完全信任 DB。
        let mut a = cfg("kiroapp", KIND_KIROAPP);
        a.priority = 1;
        let mut b = cfg("kiroapp", KIND_KIROAPP);
        b.priority = 9;
        let raw = serde_json::to_string(&[a, b]).unwrap();
        let r = parse_roster(Some(&raw));
        assert_eq!(r.len(), 1, "重复 id 必须去重,否则余额表会被后一家覆盖");
        assert_eq!(r[0].priority, 1, "留第一条");
    }

    #[test]
    fn 名册里查不到的家按零档而不是排到最后() {
        // 名册刚被改小时,在途报价不该因为「查不到」被踢到队尾 ——
        // 那会让「改个名册」意外变成「停止补货」。
        assert_eq!(shelf_priority_of(&[cfg("drop", KIND_DROP)], "别家", ""), 0);
        assert_eq!(shelf_priority_of(&[], "drop", ""), 0);
    }

    #[test]
    fn 老名册没有档位字段时解析成全零() {
        // 回滚安全的另一半:旧 JSON(无 priority / shelf_priority)必须解析成缺省 0,
        // 而不是解析失败回落默认名册 —— 后者会把 kiroapp 整个丢掉。
        let raw = r#"[{"id":"drop","kind":"drop","enabled":true,"base_url":"","api_key":"","daily_cap_cny":0.0},
                      {"id":"kiroapp","kind":"kiroapp","enabled":true,"base_url":"","api_key":"km_x","daily_cap_cny":0.0}]"#;
        let r = parse_roster(Some(raw));
        assert_eq!(r.len(), 2, "缺档位字段不该被判成坏数据");
        assert!(r.iter().all(|c| c.priority == 0 && c.shelf_priority.is_empty()));
    }

    #[test]
    fn 每家的熔断键互不相同() {
        assert_ne!(breaker_key("drop"), breaker_key("kiroapp"));
        assert_ne!(breaker_key("drop"), fault_streak_key("drop"));
    }
}
