//! 线缆卫生:剥掉客户端塞进请求里的**每请求都变**的内容。
//!
//! 放 gw-core 是因为**每个上游都需要它**,而且必须是同一份实现:这些字段的作用是
//! 让「同一个会话」在字节层面每轮都不一样,而上游的 prefix cache、我方的会话亲和键、
//! 缓存命中统计**全都**建立在「同会话前缀稳定」这个前提上。漏剥一处,那一处的
//! 命中率直接归零,而且症状是「莫名其妙不命中」——最难查的那种。

/// Claude Code 滚动 billing 指纹行的前缀。
const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// 判定一行是否为 Claude Code 的滚动 billing 指纹行。
///
/// 现象:CC 在 system prompt **顶部**拼一行
/// `x-anthropic-billing-header: cc_version=...; cc_entrypoint=cli; cch=<5位16进制>;`,
/// 其中 `cch` 是**每请求都变**的 body 哈希。
///
/// 判据:行首(忽略前导空白与大小写)是 `x-anthropic-billing-header:` 即剥,
/// **不依赖具体 `cc_*` 字段名** —— CC 升级会改字段集(实测 header 里带
/// `cc_version=2.1.63.<build>`),收窄判据会在某次升级后静默失配,随机值重新泄进前缀。
/// 这个前缀是 Anthropic 内部专有的,真实流量里 system 仅此一处,放宽不误伤用户正文。
fn is_billing_fingerprint_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let prefix = BILLING_HEADER_PREFIX.as_bytes();
    // 按字节比较前缀,避免在多字节字符边界上做 str 切片 panic。
    trimmed.len() >= prefix.len()
        && trimmed.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// 删除文本里的滚动指纹行(保留其余内容与换行结构)。
///
/// 用 `split_inclusive('\n')` 保留每行的尾随换行,使非指纹行**逐字节原样**保留 ——
/// 这保证把它接到既有的会话键派生上时,不含指纹行的流量的键值**不发生任何变化**
/// (真实 CC 流量里唯一的 `x-anthropic-` 行就是 billing header),不会造成会话大迁移。
///
/// ## 为什么必须在**会话键派生**上也调用它
///
/// 不只是省几十字节的 wire 体积。会话键若把每请求都变的 `cch` 哈希进去,后果是
/// 一整条链路的静默失效:
///
/// 1. 调度层的会话亲和拿不到稳定键 → 每个请求都是"新会话" → 账号钉扎失效、来回换号;
/// 2. 由该键派生的上游 conversationId 每轮都变 → 服务端会话续写永远不命中,
///    每轮都退回"重铺全部历史";
/// 3. 缓存命中统计的指纹每轮都变 → 报出来的命中率恒为 0。
///
/// 三条都表现为"不知为何就是不命中",而根因只是 system 顶上多了一行随机数。
pub fn strip_rolling_fingerprints(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        if is_billing_fingerprint_line(line) {
            continue;
        }
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 剥掉滚动指纹行_其余逐字节保留() {
        let sys = "x-anthropic-billing-header: cc_version=2.1.63.a43; cc_entrypoint=cli; cch=ea527;\n\
                   You are Claude Code.\n剩下的正文\n";
        let out = strip_rolling_fingerprints(sys);
        assert_eq!(out, "You are Claude Code.\n剩下的正文\n");
    }

    #[test]
    fn 不依赖具体字段名_升级换字段也照剥() {
        // 收窄到 `cch=` 之类的判据会在 CC 改字段名后静默失配。
        let sys = "x-anthropic-billing-header: ccv=2.2.0; entry=cli; h=abcde;\nrest\n";
        assert_eq!(strip_rolling_fingerprints(sys), "rest\n");
    }

    #[test]
    fn 同一会话两次请求的指纹不同_剥后必须一致() {
        // 这条就是整件事的要害:剥之前两轮的 system 不同,剥之后逐字节相同。
        let a = "x-anthropic-billing-header: cch=aaaaa;\nYou are Claude Code.\n";
        let b = "x-anthropic-billing-header: cch=bbbbb;\nYou are Claude Code.\n";
        assert_ne!(a, b);
        assert_eq!(strip_rolling_fingerprints(a), strip_rolling_fingerprints(b));
    }

    #[test]
    fn 不误伤只是碰巧含_x_anthropic_的正文() {
        let keep = "讲一下 x-anthropic-beta 这个头是干什么的\n";
        assert_eq!(strip_rolling_fingerprints(keep), keep);
        // 前缀必须在**行首**(允许前导空白)。
        assert!(is_billing_fingerprint_line("  X-Anthropic-Billing-Header: x\n"));
        assert!(!is_billing_fingerprint_line("see x-anthropic-billing-header: x\n"));
    }

    #[test]
    fn 没有指纹行时逐字节原样返回() {
        // 保证接到既有会话键派生上时,不含指纹行的流量键值不变(不触发会话大迁移)。
        for s in ["", "single line no newline", "a\nb\n", "\n\n", "尾部无换行\na"] {
            assert_eq!(strip_rolling_fingerprints(s), s, "{s:?}");
        }
    }
}
