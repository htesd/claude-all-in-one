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

export function ByModelTable({ data, loading }: ByModelTableProps) {
  const { t } = useI18n()

  const rows = [...(data ?? [])]
    .sort((a, b) => b.requests - a.requests)
    .map((row) => (
      <TR key={row.model}>
        <TD className="max-w-[280px] truncate font-medium" title={row.model}>
          {row.model}
        </TD>
        <TD className="text-right font-semibold">{formatInt(row.requests)}</TD>
        <TD className="text-right text-muted-foreground">{formatInt(row.input_tokens)}</TD>
        <TD className="text-right text-muted-foreground">{formatInt(row.output_tokens)}</TD>
        <TD className="text-right text-muted-foreground">
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
