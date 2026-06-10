import { useState } from 'react'
import { BarChart3 } from 'lucide-react'

import { Card } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'
import { ByKeyTable } from '@/features/usage/components/ByKeyTable'
import { ModelBarChart } from '@/features/usage/components/ModelBarChart'
import { RangeSegment } from '@/features/usage/components/RangeSegment'
import { useUsageByKey, useUsageByModel } from '@/features/usage/hooks'
import type { TimeRange } from '@/features/usage/types'
import { useI18n } from '@/lib/i18n'

export default function UsagePage() {
  const { t } = useI18n()
  const [range, setRange] = useState<TimeRange>(30)

  const byKeyQuery = useUsageByKey(range)
  const byModelQuery = useUsageByModel(range)

  return (
    <div className="space-y-6">
      {/* Page hero */}
      <div className="page-hero flex flex-wrap items-center justify-between gap-4 p-6">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">{t('usage.title')}</h1>
          <p className="mt-1 text-sm text-muted-foreground">{t('usage.subtitle')}</p>
        </div>
        <RangeSegment value={range} onChange={setRange} />
      </div>

      {byKeyQuery.isError && <ErrorNote error={byKeyQuery.error} />}

      {/* Per-key usage: the main attraction on this page */}
      <ByKeyTable data={byKeyQuery.data} loading={byKeyQuery.isPending} showCacheColumns />

      {/* Top models bar chart (hand-coded inline SVG) */}
      <Card className="overflow-hidden">
        <div className="flex items-center gap-2 border-b border-black/5 px-5 py-4 dark:border-white/5">
          <BarChart3 className="h-4 w-4 text-primary" />
          <h2 className="text-sm font-semibold">{t('chart.topModels')}</h2>
        </div>
        <div className="p-5">
          <ModelBarChart data={byModelQuery.data} loading={byModelQuery.isPending} />
        </div>
      </Card>
    </div>
  )
}
