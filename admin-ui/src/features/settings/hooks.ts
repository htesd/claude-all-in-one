import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { queryKeys } from '@/lib/query-keys'

import { fetchSettings, updateSettings } from './api'
import type { SystemSettingsPatch } from './types'

/** 读取当前有效系统设置（全量）。 */
export function useSettings() {
  return useQuery({
    queryKey: queryKeys.settings.detail(),
    queryFn: fetchSettings,
  })
}

/** 局部更新系统设置；成功后整域 invalidate（触发重新拉取）。 */
export function useUpdateSettings() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (patch: SystemSettingsPatch) => updateSettings(patch),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.settings.root })
    },
  })
}
