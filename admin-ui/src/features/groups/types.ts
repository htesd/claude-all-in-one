/** 后端 /admin/api/groups 返回的单行（GET 列表与 POST/PATCH 响应同构）。 */
export interface GroupRow {
  name: string
  /** 颜色（hex）；空串 = 未设置，展示时取 DEFAULT_GROUP_COLOR 兜底。 */
  color: string
  note: string
  account_count: number
  key_count: number
  /** 创建时间，Unix 秒。 */
  created_at: number
  /**
   * 影子组：指向真正持有账号的源组（'' = 普通组）。
   * 影子组自己**不持有账号**，所以 `account_count` 恒为 0 —— 卡片必须据本字段显示
   * "影子组 → 源组" 徽章，否则运维会以为这个组坏了。
   */
  shadow_of: string
  /** 影子组可见的最高档位（只允许 priority ≤ 此值；null = 不限）。 */
  tier_max_priority: number | null
  /**
   * 影子组可见档位的**下界**（只允许 priority ≥ 此值；null = 不限）。
   * 数值越小越优先，所以这一侧用来把主力号挡在档位外 —— 低价流量烧不到高价客户的号。
   */
  tier_min_priority: number | null
}

export interface CreateGroupPayload {
  name: string
  color?: string
  note?: string
}

/** PATCH /groups/{name}：字段缺省 = 不修改；`note: ''` = 清空备注。 */
export interface UpdateGroupPayload {
  color?: string
  note?: string
}

/**
 * 分组名规则（与后端一致）：1–64 个 URL-safe 字符 [A-Za-z0-9._~-]。
 * 提交前先在客户端校验，避免一次必然 400 的请求。
 */
export const GROUP_NAME_PATTERN = /^[A-Za-z0-9._~-]{1,64}$/

/** 预设色板（新建时默认选第一个）。 */
export const GROUP_COLOR_PRESETS = [
  '#7c6cf6',
  '#10b981',
  '#f59e0b',
  '#ef4444',
  '#3b82f6',
  '#ec4899',
  '#14b8a6',
  '#8b5cf6',
] as const

/** 分组未设置颜色时的兜底色。 */
export const DEFAULT_GROUP_COLOR: string = GROUP_COLOR_PRESETS[0]
