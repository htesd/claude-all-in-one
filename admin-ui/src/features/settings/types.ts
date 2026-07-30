/** GET /admin/api/settings 响应（全量有效值）。 */
export interface SystemSettings {
  /** 全局默认出口代理 URL；null = 走节点源 IP。 */
  default_proxy: string | null
  /** 出口代理池（美国多 IP）：导入/新建账号时按最少使用自动分配粘性出口。null/空 = 不自动分配。 */
  egress_pool: string[] | null
  /** 缓存读取倍率（浮点）。 */
  cache_read_multiplier: number
  /** 缓存上限比例（浮点）。 */
  cache_cap_ratio: number
  /** 缓存下限比例（浮点）。 */
  cache_floor_ratio: number
  /** 缓存模拟 TTL（秒，整数）。 */
  cache_sim_ttl_secs: number
  /** 最大缓存会话数（整数）。 */
  cache_max_sessions: number
  /** 限流冷却时长（秒，整数）。 */
  rate_limit_cooldown_secs: number
  /** 账号临时封禁冷却时长（秒，整数，默认 3600）。 */
  suspended_cooldown_secs: number
  /** 空响应冷却时长（秒，整数）。 */
  empty_response_cooldown_secs: number
  /** 空响应统计窗口（秒，整数）。 */
  empty_response_window_secs: number
  /** 空响应触发阈值（整数）。 */
  empty_response_threshold: number
  /** 会话亲和 TTL（秒，整数）。 */
  affinity_ttl_secs: number
  /** 连续失败上限（整数）。 */
  max_failures: number
  /** 单请求换号重试硬上限（整数，默认 2；反雪崩）。 */
  max_switch_attempts: number
  /** 是否启用后台配额轮询（防封 ambient 流量；关掉则仅 /health 被打时刷配额）。 */
  quota_poll_enabled: boolean
  /** 是否启用图像压缩。 */
  image_enabled: boolean
  /** 图像最大长边（像素，整数）。 */
  image_max_long_edge: number
  /** 单图最大像素（整数）。 */
  image_max_pixels_single: number
  /** 多图最大像素（整数）。 */
  image_max_pixels_multi: number
  /** 触发多图模式的阈值（整数）。 */
  image_multi_threshold: number
  /** 实验：把工具定义放进 history[0] 前缀（蹭缓存）。⚠️ 部分客户端工具调用会失效，默认关。 */
  tools_in_prefix: boolean
  /** 实验：cache_control→cachePoint（实测 no-op，dormant）。 */
  cache_point: boolean
  /** 实验：发稳定 agentContinuationId+vibe（复刻 kiro.rs，真实缓存命中 A/B，默认关）。 */
  agent_continuation: boolean
  /** thinking 块是否附 signature。**默认开**（保留现状，过 hvoy/cctest 检测）。多上游反代关掉：
   *  caio 的 Kiro 合成签名对真 Anthropic/Bedrock 验签非法，跨通道漂移会被拒 THINKING_SIGNATURE_INVALID。 */
  thinking_signature: boolean
  /** 主推理上游端点：false=runtime.kiro.dev（默认/现状），true=q.amazonaws.com（kiro.rs 端点，做服务端
   *  prompt 缓存、真实命中 82-92% 省积分；runtime.kiro.dev 端点实测真实缓存 0%、计费 ~2x）。默认关。 */
  q_endpoint: boolean
  /** 客户端**未指定** effort 时的默认思考档位。只影响没说话的客户端——显式点了档位的请求原样透传。
   *  档位越高思考越深也越慢：实测 max 的思考量约为 xhigh 的 1.7 倍。默认 high。 */
  default_thinking_effort: ThinkingEffort
}

/** 上游 effort 档位全集（由低到高）。与后端 `VALID_EFFORTS` 一致。 */
export const THINKING_EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max'] as const

export type ThinkingEffort = (typeof THINKING_EFFORTS)[number]

/**
 * PUT /admin/api/settings 请求体（局部覆写）。
 * - 字段存在且非 null → 设置该覆写值
 * - 字段存在且为 null → 重置为 YAML 默认值
 * - 字段缺失 → 不动
 */
export type SystemSettingsPatch = {
  [K in keyof SystemSettings]?: SystemSettings[K] | null
}
