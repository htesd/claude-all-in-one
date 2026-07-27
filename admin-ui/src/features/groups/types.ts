/** 后端 /admin/api/groups 返回的单行（GET 列表与 POST/PATCH 响应同构）。 */
export interface GroupRow {
  name: string
  /** 颜色（hex）；空串 = 未设置，展示时取 DEFAULT_GROUP_COLOR 兜底。 */
  color: string
  note: string
  /**
   * **归属**本组的账号数（accounts.group_name = 本组，即哪个 worker 独占管它们的运行态）。
   * 与 `member_count` 是两件事：一个号只归属一个组，却可以是多个组的成员。
   */
  account_count: number
  key_count: number
  /** 创建时间，Unix 秒。 */
  created_at: number
  /**
   * 本组的**成员数**（account_groups 里的边数）= 本组的客户能用到多少个号。
   * 卡片必须展示它：一个只借用别组账号的分组，`account_count` 恒为 0，
   * 光看账号数会让运维以为这个组坏了。
   */
  member_count: number
}

/** GET /groups/{name}/members 的单行。 */
export interface GroupMember {
  account_id: string
  /** 该号**在本组内**的优先级（数值越小越优先）。同一个号在别的组里可以是另一个值。 */
  priority: number
}

/** POST /groups/{name}/members/bulk:字段缺省 = 该维度不筛。 */
export interface BulkAddMembersPayload {
  /** 只加归属于该 owner 的号。 */
  owner?: string
  /** 只加该订阅档位的号,如 "KIRO POWER" / "KIRO PRO MAX"。 */
  subscription_title?: string
  /** 这批号在本组内的优先级(数值越小越优先)。 */
  priority: number
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
