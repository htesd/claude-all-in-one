import { Card } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { useI18n } from '@/lib/i18n'
import { formatCompact, formatInt, formatPercent } from '@/lib/utils'

import type { UsageSummary } from '../types'

interface SummaryStripProps {
  data: UsageSummary | undefined
  loading: boolean
}

/** Compact single-card summary row (follows the active filters on /usage). */
export function SummaryStrip({ data, loading }: SummaryStripProps) {
  const { t } = useI18n()

  const items: { label: string; value: string; title?: string }[] = [
    {
      label: t('stats.requests'),
      value: data ? formatCompact(data.requests) : '—',
      title: data ? formatInt(data.requests) : undefined,
    },
    {
      label: t('stats.successRate'),
      value: data ? formatPercent(data.success_requests, data.requests) : '—',
      title: data
        ? `${formatInt(data.success_requests)} / ${formatInt(data.requests)}`
        : undefined,
    },
    {
      label: t('stats.inputTokens'),
      value: data ? formatCompact(data.input_tokens) : '—',
      title: data ? formatInt(data.input_tokens) : undefined,
    },
    {
      label: t('stats.outputTokens'),
      value: data ? formatCompact(data.output_tokens) : '—',
      title: data ? formatInt(data.output_tokens) : undefined,
    },
    {
      label: t('stats.cacheRead'),
      value: data ? formatCompact(data.cache_read_tokens) : '—',
      title: data ? formatInt(data.cache_read_tokens) : undefined,
    },
    {
      label: t('stats.cacheWrite'),
      value: data ? formatCompact(data.cache_creation_tokens) : '—',
      title: data ? formatInt(data.cache_creation_tokens) : undefined,
    },
  ]

  return (
    <Card className="grid grid-cols-2 gap-x-6 gap-y-4 px-5 py-4 sm:grid-cols-3 xl:grid-cols-6">
      {loading
        ? Array.from({ length: 6 }, (_, index) => (
            <div key={index}>
              <Skeleton className="h-3 w-16" />
              <Skeleton className="mt-2 h-6 w-20" />
            </div>
          ))
        : items.map((item) => (
            <div key={item.label}>
              <div className="eyebrow text-muted-foreground">
                {item.label}
              </div>
              <div
                className="mt-1 font-display text-2xl font-black tracking-[-0.04em] tabular-nums"
                title={item.title}
              >
                {item.value}
              </div>
            </div>
          ))}
    </Card>
  )
}
