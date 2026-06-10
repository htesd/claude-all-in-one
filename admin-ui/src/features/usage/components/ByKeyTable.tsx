import { KeyRound } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { TD, TR } from '@/components/ui/table'
import { useI18n } from '@/lib/i18n'
import { formatInt, formatPercent, maskKey } from '@/lib/utils'

import type { KeyUsage } from '../types'
import { UsageTableCard } from './UsageTableCard'

interface ByKeyTableProps {
  data: KeyUsage[] | undefined
  loading: boolean
  /** Show the extra cache-read/cache-write columns (used on the /usage page). */
  showCacheColumns?: boolean
}

function successVariant(rate: number): 'success' | 'warning' | 'destructive' {
  if (rate >= 0.95) return 'success'
  if (rate >= 0.8) return 'warning'
  return 'destructive'
}

export function ByKeyTable({ data, loading, showCacheColumns = false }: ByKeyTableProps) {
  const { t } = useI18n()

  const columns = [
    { label: t('table.key') },
    { label: t('table.requests'), right: true },
    { label: t('table.success'), right: true },
    { label: t('table.input'), right: true },
    { label: t('table.output'), right: true },
    ...(showCacheColumns
      ? [
          { label: t('table.cacheRead'), right: true },
          { label: t('table.cacheWrite'), right: true },
        ]
      : []),
  ]

  const rows = [...(data ?? [])]
    .sort((a, b) => b.requests - a.requests)
    .map((row, index) => (
      <TR key={row.client_key_id || `__unattributed_${index}`}>
        <TD>
          {row.client_key_id ? (
            <code className="rounded-md bg-muted px-2 py-0.5 font-mono text-xs">
              {maskKey(row.client_key_id)}
            </code>
          ) : (
            <Badge variant="muted">{t('table.unattributed')}</Badge>
          )}
        </TD>
        <TD className="text-right font-semibold">{formatInt(row.requests)}</TD>
        <TD className="text-right">
          {row.requests > 0 ? (
            <Badge variant={successVariant(row.success_requests / row.requests)}>
              {formatPercent(row.success_requests, row.requests)}
            </Badge>
          ) : (
            <span className="text-muted-foreground">—</span>
          )}
        </TD>
        <TD className="text-right text-muted-foreground">{formatInt(row.input_tokens)}</TD>
        <TD className="text-right text-muted-foreground">{formatInt(row.output_tokens)}</TD>
        {showCacheColumns && (
          <>
            <TD className="text-right text-muted-foreground">
              {formatInt(row.cache_read_tokens)}
            </TD>
            <TD className="text-right text-muted-foreground">
              {formatInt(row.cache_creation_tokens)}
            </TD>
          </>
        )}
      </TR>
    ))

  return (
    <UsageTableCard
      icon={KeyRound}
      title={t('table.byKey')}
      count={data?.length}
      loading={loading}
      columns={columns}
      rows={rows}
    />
  )
}
