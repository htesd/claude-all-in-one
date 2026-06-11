import { api } from '@/lib/api'

import type { SystemSettings, SystemSettingsPatch } from './types'

/** GET /admin/api/settings → 当前有效全量设置。 */
export async function fetchSettings(): Promise<SystemSettings> {
  const response = await api.get<SystemSettings>('/settings')
  return response.data
}

/**
 * PUT /admin/api/settings → 局部覆写；返回更新后的有效全量设置。
 * null 值表示将该字段重置为 YAML 默认。
 */
export async function updateSettings(patch: SystemSettingsPatch): Promise<SystemSettings> {
  const response = await api.put<SystemSettings>('/settings', patch)
  return response.data
}
