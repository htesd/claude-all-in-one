# claude-all-in-one Phase 2 计划:上下文处理 + thinking + 签名

> 三块按用户定的优先级:① 上下文处理(去 Claude Code 恶心 + projection 双锚提缓存)
> ② thinking 注入(保智力)③ 签名(换 f6 + 保留加密体)。
> 来源标记:🔵搬旧 kiro.rs / 🟢借鉴 static_flow 自写 / 🟣自创。
> 详细侦察见 memory `static-flow-context-thinking-signature-recon`。

## 现状基线(已完成 Phase 1)
- converter 已拆 `converter/{mod,session,content,tools,pairing,history,cache_point,model_map}.rs`
- converter 已有 `generate_thinking_prefix`(history.rs,enabled/adaptive 两形态)+ `derive_conversation_id_from_messages`(session.rs)——但注入进 system 折叠块、单开关
- chat.rs(271行)只桥接 assistantResponseEvent 文本→SSE,**无 reasoning/thinking/签名**
- 旧 kiro.rs `stream.rs`(2749行)有完整 thinking/reasoning/签名状态机可搬;`signature.rs`(已读)有 rewrite_model_in_signature + synthesize_signature
- static_flow `cache_sim/`(projection 889 + anchor_index 258)是双锚参考

---

## 块 1:上下文处理(最高 ROI)

### 1a. system 净化升级(🟢 static_flow `identity.rs:361-375` / `normalize.rs:211-356`)
改 `converter/session.rs` 的 `strip_rolling_fingerprints`(现仅删 `x-anthropic-` 前缀行)→ 扩展为完整三级分流。新建 `converter/normalize.rs`:
- **删 billing 行**:行首 `x-anthropic-billing-header:` 且含 `cc_version=`/`cc_entrypoint=`/`cch=` → 删整行
- **三级分流**:Stable(`You are Claude Code, Anthropic's official CLI for Claude.` 等整行 / `SessionStart hook additional context:` 开头 → promote)/ DynamicNoise(`The task tools haven't been used recently.` → drop)/ Unknown(包 `<system_context>...</system_context>` 转 user 保序)
- **interrupted-user 特例**:`The user sent a new message while you were working:` → 提取正文(遇 `\n\nIMPORTANT:` 截断)转 user
- **model identity 规范化**:统一成 `You are powered by the model named {short}. The exact model ID is {id}.`

### 1b. 当前轮范围锚定(🟢 static_flow `convert.rs:42-64`)
改 `converter/mod.rs` 的 `convert_request`:current turn = **尾部连续 user 消息**(从最后 user 回溯到非 user),不是只取最后一条。新增 `current_user_message_range()`。

### 1c. 🔑 projection 双锚 + conversationId 恢复(🟢 static_flow `cache_sim/`,我方没做,核心)
新建 crate 或 gw-kiro 模块 `cache/`:
- `projection.rs`:`PromptProjection::from_conversation_state()` → 产 `history_anchor_segments` / `stable_prefix_pages`(含当前轮 tools)/ `current_turn_history_segments`。`lookup_anchor_hash=hash(history only)`、`resume_anchor_hash=hash(history+当前轮+assistant响应)`。tools 进 stable_prefix 不进 anchor。
- `anchor_index.rs`:`ConversationAnchorIndex`,TTL + bounded LRU,`resume_anchor_hash → real conversationId`。
- conversationId 解析顺序改为:① metadata.user_id 的 session_id/legacy(已有 extract_session_id)② fallback 时用 lookup_anchor_hash 查 index 恢复 ③ 都没有才 derive_conversation_id_from_messages(现有哈希派生作最后兜底)
- chat 成功后写 `resume_anchor_hash → conversationId` 到 index
- **决策**:anchor index 放 worker 进程内(per-worker),Mutex<LRU>。跨 worker 不共享(会话亲和已保证同 session 同 worker)。

### 1d. 历史 thinking 处理(🔵 沿用我方旧策略,**不照搬 static_flow**)
保持我方 `convert_assistant_message` 丢弃历史 thinking(缓存稳定)。static_flow 保留是保真但毒缓存,我方目标是缓存命中,旧策略更优。已实现,无需改。

---

## 块 2:thinking 注入(保智力,🔵搬旧 stream.rs + 🟢借鉴 static_flow 双开关)

### 2a. 请求侧双开关(🟢 static_flow `kiro_dispatch.rs:393-399`)
改 `anthropic_types.rs` 的 Thinking + 新增判定:
- `upstream_thinking_enabled = thinking.is_enabled()`(type=enabled/adaptive)
- `surface_thinking_enabled = exposes_anthropic_thinking()`:enabled 总暴露;adaptive 仅当 `display==summarized` 或 `output_config.effort` 存在
- `hidden_thinking_enabled = upstream && !surface`

### 2b. 注入只进当前轮(🟢 static_flow `thinking.rs`,改我方现状)
**现状问题**:我方 `generate_thinking_prefix` 注入进 system 折叠块。改为只注入当前轮 user content 前缀(防污染缓存前缀)。模板确切:
- enabled:`<thinking_mode>enabled</thinking_mode><max_thinking_length>{budget}</max_thinking_length>`
- adaptive:`<thinking_mode>adaptive</thinking_mode><thinking_effort>{effort}</thinking_effort>`
- 拼接 `{prefix}\n{content}`;`has_thinking_tags` 守卫不重复注入
- effort 原样透传,budget clamp 24576(反序列化层)。effort 默认我方用 high(实测天花板),不覆盖用户显式值。

### 2c. 响应侧 reasoningContentEvent→thinking 块(🔵 搬旧 stream.rs:916-1015)
改 chat.rs 的 stream 状态机(现只处理文本)。新增状态:`native_reasoning_seen`/`thinking_block_index`/`open_thinking_content`/`completed_thinking_*`。
- frame `reasoningContentEvent` payload `{text?, signature?}`:首次见 → 发 `content_block_start(thinking)`;text → `thinking_delta`;signature → 收尾
- **时序铁律**:`content_block_start(thinking)→thinking_delta*→signature_delta→content_block_stop`(stopped block 不能再 delta)
- `native_reasoning_seen=true` 后 assistantResponse 正文绕过 inline 解析,直接 text_delta
- reasoning→text/tool_use/流末尾 三处先 close_reasoning_block_if_open

### 2d. inline `<thinking>` fallback(🔵 搬旧 stream.rs:68-171,1018-1154,可选)
仅 native_reasoning_seen=false 时启用。quote-aware 防伪标签、跨 chunk 半标签。**P2 可先不做**(我方主上游稳定走 native),标注为 P2.5 补充。

---

## 块 3:签名(换 f6 + 保留加密体,🔵 直接搬旧 signature.rs)

### 3a. 搬 signature.rs(🔵 旧 `anthropic/signature.rs` 几乎原样)
新建 `gw-kiro/src/signature.rs`:
- `rewrite_model_in_signature(sig_b64, model)`:解 protobuf 下钻 f2→f1→f6,把 claude-quince 换成客户端官方 model 名,**保留加密体**。结构异常返回 None(回退)。
- `synthesize_signature(model, thinking)`:上游无签名时自造(布局 f2.f1: f1=14,f2=1,f3=2,f5=64B,f6=model,f7=0,f8="thinking")。
- 需加 `base64` 依赖到 gw-kiro Cargo.toml。

### 3b. chat.rs 接签名(在 2c 的 signature 处理点)
- 收到 reasoningContentEvent.signature:先 `rewrite_model_in_signature(sig, 客户端model)`,成功用重写值,失败回退原样透传
- 上游无 signature(thinking 流结束仍无):`synthesize_signature(model, 累积thinking)` 兜底
- 发 `signature_delta`,值 = 上述结果

### 3c. cctest 实测验证(用 free 号)
签名改完,真发一次带 thinking 的请求,抓 SSE 里 thinking 块的 signature,确认含官方 model 名、无 claude-quince。条件允许跑一次 cctest 看签名校验项是否改善。

---

## 落地顺序与验证
1. **块1a/1b**(system 净化 + 当前轮锚定):纯 converter 改造,单测验证。
2. **块2a/2b**(thinking 双开关 + 当前轮注入):改 types + converter,单测。
3. **块2c + 3a/3b**(reasoning→thinking 块 + 签名):改 chat.rs stream 状态机 + 搬 signature.rs。**用 free 号真发带 thinking 请求端到端验证**(reasoning 块时序、签名 f6)。
4. **块1c**(projection 双锚):最大新增,独立模块,单测 + 多轮真发验证 conversationId 恢复 + 缓存命中(看 metering 降)。放最后因为它最独立、收益需多轮才显现。

## 关键决策(已定)
- 签名:**换 f6 + 保留加密体**(比 static_flow 纯 synthetic 稳健)。
- 历史 thinking:**丢弃**(我方旧策略,缓存优先,不照搬 static_flow 保留)。
- anchor index:per-worker 进程内 Mutex<LRU>,不跨 worker。
- 每块完成即 cargo build+clippy+test 全绿;涉上游的用 free 号(data/test-cred-free.json)真发验证。

## 验证铁律(沿用)
- 🔵 搬运不改逻辑,🟢 借鉴自写,🟣 自创标注。
- 单文件 <800 行(测试文件除外)。
- 真实金标准优先:能用 free 号验的不靠猜。
