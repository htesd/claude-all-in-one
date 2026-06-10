/** 后端 /admin/api/keys 返回的单行（GET 列表与 POST/PATCH 响应同构）。 */
export interface ApiKeyRow {
  key: string
  /** 备注；后端允许 null（从未设置过）。 */
  label: string | null
  disabled: boolean
  /** 所属分组名；'' = 未分组。 */
  group_name: string
  /** Token 限额；null = 不限。 */
  quota_tokens: number | null
  /** 已用 tokens（限额进度分子）。 */
  used_tokens: number
  /** 创建时间，Unix 秒。 */
  created_at: number
}

/** POST /keys 请求体；key 缺省时由服务端生成 sk-gw-<32hex>。 */
export interface CreateKeyPayload {
  label?: string | null
  key?: string
  group?: string
}

/**
 * PATCH /keys/{key} 请求体。
 * 后端语义：字段缺省 = 不修改；`label: ''` = 清空备注；
 * `group_name: ''` = 移出分组；`quota_tokens <= 0` = 清除限额；
 * `reset_used: true` = 已用归零。
 */
export interface UpdateKeyPayload {
  label?: string
  disabled?: boolean
  group_name?: string
  quota_tokens?: number
  reset_used?: boolean
}

/**
 * 自定义 key 规则（与后端一致）：8–128 个 URL-safe 字符 [A-Za-z0-9._~-]
 * （RFC 3986 unreserved，避免 / # ? 等保留字符进入 PATCH/DELETE 的路径段）。
 * 提交前先在客户端校验，避免一次必然 400 的请求。
 */
export const CUSTOM_KEY_PATTERN = /^[A-Za-z0-9._~-]{8,128}$/
