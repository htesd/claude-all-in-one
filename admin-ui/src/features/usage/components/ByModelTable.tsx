import { Cpu } from 'lucide-react'

import { TD, TR } from '@/components/ui/table'
import { useI18n } from '@/lib/i18n'
import { formatInt } from '@/lib/utils'

import type { ModelUsage } from '../types'
import { UsageTableCard } from './UsageTableCard'

interface ByModelTableProps {
  data: ModelUsage[] | undefined
  loading: boolean
}

/**
 * Compact, table-first per-model usage. Instead of a standalone chart, the
 * 请求数 cell carries a thin (4px) inline proportion bar relative to the
 * busiest model — sparkline-in-a-table, not a hero chart.
 */
export function ByModelTable({ data, loading }: ByModelTableProps) {
  const { t } = useI18n()

  const sorted = [...(data ?? [])].sort((a, b) => b.requests - a.requests)
  const max = Math.max(...sorted.map((row) => row.requests), 1)

  const rows = sorted.map((row) => (
    <TR key={row.model}>
      <TD className="max-w-[280px] truncate py-2 text-[13px] font-medium" title={row.model}>
        {row.model}
      </TD>
      <TD className="py-2 text-right">
        <span className="inline-flex w-24 flex-col items-end gap-1 align-middle">
          <span className="text-[13px] font-semibold leading-none tabular-nums">
            {formatInt(row.requests)}
          </span>
          <span
            className="h-1 w-full overflow-hidden rounded-full bg-black/[0.06] dark:bg-white/10"
            aria-hidden="true"
          >
            <span
              className="block h-full rounded-full bg-primary/35"
              style={{ width: `${Math.max((row.requests / max) * 100, 2)}%` }}
            />
          </span>
        </span>
      </TD>
      <TD className="py-2 text-right text-xs tabular-nums text-muted-foreground">
        {formatInt(row.input_tokens)}
      </TD>
      <TD className="py-2 text-right text-xs tabular-nums text-muted-foreground">
        {formatInt(row.output_tokens)}
      </TD>
      <TD className="py-2 text-right text-xs tabular-nums text-muted-foreground">
        {formatInt(row.cache_read_tokens)}
      </TD>
    </TR>
  ))

  return (
    <UsageTableCard
      icon={Cpu}
      title={t('table.byModel')}
      count={data?.length}
      loading={loading}
      columns={[
        { label: t('table.model') },
        { label: t('table.requests'), right: true },
        { label: t('table.input'), right: true },
        { label: t('table.output'), right: true },
        { label: t('table.cacheRead'), right: true },
      ]}
      rows={rows}
    />
  )
}
