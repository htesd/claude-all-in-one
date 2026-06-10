import type { UsageFilter } from '@/features/usage/types'

export const queryKeys = {
  usage: {
    summary: (filter: UsageFilter) => ['usage', 'summary', filter] as const,
    byModel: (filter: UsageFilter) => ['usage', 'by-model', filter] as const,
    byKey: (filter: UsageFilter) => ['usage', 'by-key', filter] as const,
  },
} as const
