/** 后端 /admin/api/keys 返回的单行（GET 列表与 POST/PATCH 响应同构）。 */
export interface ApiKeyRow {
  key: string
  /** 备注；后端允许 null（从未设置过）。 */
  label: string | null
  disabled: boolean
  /** 创建时间，Unix 秒。 */
  created_at: number
}

/** POST /keys 请求体；key 缺省时由服务端生成 sk-gw-<32hex>。 */
export interface CreateKeyPayload {
  label?: string | null
  key?: string
}

/**
 * PATCH /keys/{key} 请求体。
 * 后端语义：字段缺省 = 不修改；`label: ''` = 清空备注 —— 所以这里不需要 null。
 */
export interface UpdateKeyPayload {
  label?: string
  disabled?: boolean
}

/**
 * 自定义 key 规则（与后端一致）：8–128 个 ASCII 可见字符（0x21–0x7E，无空格）。
 * 提交前先在客户端校验，避免一次必然 400 的请求。
 */
export const CUSTOM_KEY_PATTERN = /^[\x21-\x7E]{8,128}$/
