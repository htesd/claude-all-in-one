import axios from 'axios'

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

/** 取 HTTP 状态码（非 axios 错误返回 undefined），用于 409/400 的友好文案映射。 */
export function getErrorStatus(error: unknown): number | undefined {
  return axios.isAxiosError(error) ? error.response?.status : undefined
}
