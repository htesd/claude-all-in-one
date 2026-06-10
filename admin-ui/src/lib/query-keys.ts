import type { UsageFilter } from '@/features/usage/types'

export const queryKeys = {
  usage: {
    /** 用量域根 key，用于整域 invalidate。 */
    root: ['usage'] as const,
    summary: (filter: UsageFilter) => ['usage', 'summary', filter] as const,
    byModel: (filter: UsageFilter) => ['usage', 'by-model', filter] as const,
    byKey: (filter: UsageFilter) => ['usage', 'by-key', filter] as const,
  },
  keys: {
    /** API Key 域根 key，mutation 成功后整域 invalidate。 */
    root: ['keys'] as const,
    list: () => ['keys', 'list'] as const,
  },
} as const
