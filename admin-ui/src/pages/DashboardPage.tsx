import { useState } from 'react'
import {
  Activity,
  ArrowDownToLine,
  ArrowUpFromLine,
  CheckCircle2,
  DatabaseZap,
} from 'lucide-react'

import { ErrorNote } from '@/components/ui/error-note'
import { StatCard } from '@/components/ui/stat-card'
import { ByKeyTable } from '@/features/usage/components/ByKeyTable'
import { ByModelTable } from '@/features/usage/components/ByModelTable'
import { RangeSegment } from '@/features/usage/components/RangeSegment'
import { useUsageByKey, useUsageByModel, useUsageSummary } from '@/features/usage/hooks'
import type { TimeRange } from '@/features/usage/types'
import { useI18n } from '@/lib/i18n'
import { formatCompact, formatInt, formatPercent } from '@/lib/utils'

export default function DashboardPage() {
  const { t } = useI18n()
  const [range, setRange] = useState<TimeRange>(30)

  const summaryQuery = useUsageSummary(range)
  const byModelQuery = useUsageByModel(range)
  const byKeyQuery = useUsageByKey(range)

  const summary = summaryQuery.data
  const loading = summaryQuery.isPending

  return (
    <div className="space-y-6">
      {/* Page hero */}
      <div className="page-hero flex flex-wrap items-center justify-between gap-4 p-6">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">{t('dashboard.title')}</h1>
          <p className="mt-1 text-sm text-muted-foreground">{t('dashboard.subtitle')}</p>
        </div>
        <RangeSegment value={range} onChange={setRange} />
      </div>

      {summaryQuery.isError && <ErrorNote error={summaryQuery.error} />}

      {/* Stat cards */}
      <div className="grid grid-cols-2 gap-4 md:grid-cols-3 xl:grid-cols-5">
        <StatCard
          icon={Activity}
          label={t('stats.requests')}
          value={summary ? formatCompact(summary.requests) : '—'}
          sub={summary ? formatInt(summary.requests) : undefined}
          loading={loading}
        />
        <StatCard
          icon={CheckCircle2}
          label={t('stats.successRate')}
          value={summary ? formatPercent(summary.success_requests, summary.requests) : '—'}
          sub={
            summary
              ? `${formatInt(summary.success_requests)} / ${formatInt(summary.requests)}`
              : undefined
          }
          loading={loading}
        />
        <StatCard
          icon={ArrowDownToLine}
          label={t('stats.inputTokens')}
          value={summary ? formatCompact(summary.input_tokens) : '—'}
          sub={summary ? formatInt(summary.input_tokens) : undefined}
          loading={loading}
        />
        <StatCard
          icon={ArrowUpFromLine}
          label={t('stats.outputTokens')}
          value={summary ? formatCompact(summary.output_tokens) : '—'}
          sub={summary ? formatInt(summary.output_tokens) : undefined}
          loading={loading}
        />
        <StatCard
          icon={DatabaseZap}
          label={t('stats.cacheRead')}
          value={summary ? formatCompact(summary.cache_read_tokens) : '—'}
          sub={summary ? formatInt(summary.cache_read_tokens) : undefined}
          loading={loading}
        />
      </div>

      {/* Tables */}
      <div className="grid gap-6 xl:grid-cols-2">
        <ByModelTable data={byModelQuery.data} loading={byModelQuery.isPending} />
        <ByKeyTable data={byKeyQuery.data} loading={byKeyQuery.isPending} />
      </div>
    </div>
  )
}
