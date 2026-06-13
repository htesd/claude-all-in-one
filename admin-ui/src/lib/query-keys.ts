import type { LogsFilter } from '@/features/logs/types'
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
  groups: {
    /** 分组域根 key；账号/Key 的分组归属变化也会 invalidate（计数列）。 */
    root: ['groups'] as const,
    list: () => ['groups', 'list'] as const,
  },
  accounts: {
    /** 账号域根 key；invalidate 时连带 runtime 一起刷新。 */
    root: ['accounts'] as const,
    list: () => ['accounts', 'list'] as const,
    /** worker 运行态，15s 轮询。 */
    runtime: () => ['accounts', 'runtime'] as const,
  },
  settings: {
    /** 系统设置域根 key。 */
    root: ['settings'] as const,
    detail: () => ['settings', 'detail'] as const,
  },
  logs: {
    /** 请求日志域根 key。 */
    root: ['logs'] as const,
    list: (filter: LogsFilter, page: number) => ['logs', 'list', filter, page] as const,
    detail: (id: number) => ['logs', 'detail', id] as const,
  },
} as const
