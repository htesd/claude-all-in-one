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
  /** 历史 assistant thinking 保留轮数：0 = 全部丢弃（默认，前缀缓存最稳）；N > 0 = 只保留倒数
   *  最近 N 个 assistant 合并单元，更早的一律丢弃；负数 = 全部保留（测试用）。
   *  ⚠️ 改动会让所有在途会话下一轮缓存全量 miss 一次，建议低峰切换。 */
  history_thinking_turns: number | null
  /** RPM 闸等待预算（毫秒，默认 10000）：合格号全部仅因 RPM 定频暂不可选时，等窗口腾名额的最长时长。 */
  rpm_wait_ms?: number
  /** 低优先新号暖机总开关（默认开；调度 rank ≥ 100 的号按号龄两期限速，高优号豁免）。 */
  warmup_enabled?: boolean
  /** 暖机适应期时长（小时，默认 2）。 */
  warmup_phase1_hours?: number
  /** 暖机适应期 RPM 上限（默认 2）。 */
  warmup_phase1_rpm?: number
  /** 暖机爬坡期截止（小时，默认 24；号龄 ≥ 此值即毕业）。 */
  warmup_phase2_hours?: number
  /** 暖机爬坡期 RPM 上限（默认 6）。 */
  warmup_phase2_rpm?: number
  /** 按分组的新号暖机策略：命中的组完全接管（单期；hours=0 = 该组关闭暖机），未列出的组走全局两期。 */
  warmup_group_policies?: Record<string, GroupWarmupPolicy> | null
  /**
   * 逐 worker 的**实然值**：该 worker 此刻真正在用的热调参数 + 最近一次同步的结果。
   *
   * 本响应体的其余字段是**应然值**（库里的 overlay 叠 YAML 基线算出来的）。
   * 两者不一致，就是「我保存了不生效」的全部内容 —— 在此之前面板上只有应然值，
   * 于是那个问题在面板上根本不可见，只能 SSH 上去翻库反推。
   *
   * 仅 GET 携带；PUT 不带（刚写完库时 worker 还没轮询到，回一份必然过时的值只会误导）。
   */
  workers?: WorkerSettingsView[]
}

/** 一个 worker 的设置同步实况。 */
export interface WorkerSettingsView {
  instance: number
  group: string
  online: boolean
  /**
   * 该 worker 在线、但 `/health` 里**没有** `settings` 字段 = 它的镜像旧到还不带这个
   * 回显。必须单独标出来：若与「正常」混为一谈，前端会把它渲染成绿色「一致」，
   * 而这恰恰是本功能唯一存在的理由（旧镜像忽略新字段 → 保存不生效）。
   */
  stale_image?: boolean
  settings?: {
    /** 最近一次**成功应用**的 unix 秒；0 = 启动后一次都没成功过。 */
    applied_at: number
    /** 距今秒数。轮询周期 30s，所以 >60 基本就是同步停了；-1 = 从未成功。 */
    age_secs: number
    /** 非空 = 同步出错，配置已僵在上一次成功的值上（每 30s 重复且不自愈）。 */
    error: string
    /** 本版本不认识、已被忽略的字段 = 本 worker 镜像比写库的那个旧。 */
    unknown: string[]
    /** worker 应用之后真正在用的值（键名与本响应体的应然值逐字对齐）。 */
    effective: Record<string, unknown>
    /**
     * 该 worker 的 provider 是否**真的**热应用 provider 级设置（缓存计费/图像/实验开关）。
     *
     * false（如 claude-dario）时 `effective` 里那半边是「算得出但从未应用」——
     * 不说出来的话，面板会对着一份没生效的值报「一致」，把原本要抓的 bug 原样重演。
     * scheduler 那半边不受影响，一直是热的。
     */
    provider_hot?: boolean
  } | null
}

/** 上游 effort 档位全集（由低到高）。与后端 `VALID_EFFORTS` 一致。 */
export const THINKING_EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max'] as const

export type ThinkingEffort = (typeof THINKING_EFFORTS)[number]

/** 一个分组的新号暖机策略（与后端 `GroupWarmupPolicy` 对齐）。 */
export interface GroupWarmupPolicy {
  /** 新号 RPM 上限（生效期内）。 */
  rpm: number
  /** 暖机时长（小时）；0 = 该组显式关闭暖机。 */
  hours: number
}

/**
 * PUT /admin/api/settings 请求体（局部覆写）。
 * - 字段存在且非 null → 设置该覆写值
 * - 字段存在且为 null → 重置为 YAML 默认值
 * - 字段缺失 → 不动
 */
/**
 * PUT 的部分 patch。
 *
 * `Omit<..., 'workers'>` 是必须的：`workers` 是 GET **只读**回显的实然值，不是可写设置。
 * 留在这里的话，任何把 GET 数据摊开进 patch 的写法都会把它发上去，而写侧的未知字段
 * 保护会用 400「未知设置字段: workers」把**整次保存**挡掉 —— 报错文案还与用户的操作
 * 毫无关系（对抗审查 Architect#8）。
 */
export type SystemSettingsPatch = {
  [K in keyof Omit<SystemSettings, 'workers'>]?: SystemSettings[K] | null
}
