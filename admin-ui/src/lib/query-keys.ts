import type { TimeRange } from '@/features/usage/types'

export const queryKeys = {
  usage: {
    summary: (range: TimeRange) => ['usage', 'summary', range] as const,
    byModel: (range: TimeRange) => ['usage', 'by-model', range] as const,
    byKey: (range: TimeRange) => ['usage', 'by-key', range] as const,
  },
} as const
