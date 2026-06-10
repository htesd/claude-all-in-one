import { api } from '@/lib/api'

import type { ApiKeyRow, CreateKeyPayload, UpdateKeyPayload } from './types'

/** 列表由后端按 created_at 倒序返回，前端不再排序。 */
export async function fetchKeys(): Promise<ApiKeyRow[]> {
  const response = await api.get<ApiKeyRow[]>('/keys')
  return response.data
}

export async function createKey(payload: CreateKeyPayload): Promise<ApiKeyRow> {
  const response = await api.post<ApiKeyRow>('/keys', payload)
  return response.data
}

export async function updateKey(key: string, patch: UpdateKeyPayload): Promise<ApiKeyRow> {
  // 自定义 key 允许任意可见 ASCII（含 / ? # 等 URL 保留字符），必须编码进路径
  const response = await api.patch<ApiKeyRow>(`/keys/${encodeURIComponent(key)}`, patch)
  return response.data
}

export async function deleteKey(key: string): Promise<void> {
  await api.delete(`/keys/${encodeURIComponent(key)}`)
}

// 历史导出位置：实现已上移到 lib/api（groups/accounts 也要用），这里保持 re-export 兼容
export { getErrorStatus } from '@/lib/api'
