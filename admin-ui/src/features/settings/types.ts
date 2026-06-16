/** GET /admin/api/settings 响应（全量有效值）。 */
export interface SystemSettings {
  /** 全局默认出口代理 URL；null = 走节点源 IP。 */
  default_proxy: string | null
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
}

/**
 * PUT /admin/api/settings 请求体（局部覆写）。
 * - 字段存在且非 null → 设置该覆写值
 * - 字段存在且为 null → 重置为 YAML 默认值
 * - 字段缺失 → 不动
 */
export type SystemSettingsPatch = {
  [K in keyof SystemSettings]?: SystemSettings[K] | null
}
