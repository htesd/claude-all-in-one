import { Card } from '@/components/ui/card'
import { Segment, type SegmentOption } from '@/components/ui/segment'
import { Skeleton } from '@/components/ui/skeleton'
import { useI18n } from '@/lib/i18n'
import { formatCompact, formatInt, formatMillions, formatUsd } from '@/lib/utils'

import type { CostBasis, UsageSummary } from '../types'

interface CostStatsCardProps {
  data: UsageSummary | undefined
  loading: boolean
  basis: CostBasis
  onBasisChange: (b: CostBasis) => void
}

export function CostStatsCard({ data, loading, basis, onBasisChange }: CostStatsCardProps) {
  const { t } = useI18n()

  const options: SegmentOption<CostBasis>[] = [
    { value: 'billed', label: t('cost.basis.billed') },
    { value: 'real', label: t('cost.basis.real') },
  ]

  const cost = data ? (basis === 'billed' ? data.cost_billed_usd : data.cost_real_usd) : 0
  const credit = data?.metering_credit ?? 0
  const cacheRead = data ? (basis === 'billed' ? data.cache_read_tokens : data.real_cache_read_tokens) : 0
  // input_tokens 是总上下文(含缓存命中子集);按 Anthropic 口径展示"未命中输入" = 总 - 命中,
  // 与缓存读分列、与成本折算口径一致(真实口径命中为 0 时 ≈ 全部算新鲜输入)。
  const uncachedInput = data ? Math.max(0, data.input_tokens - cacheRead) : 0

  const row1: { label: string; value: string; title?: string }[] = [
    { label: t('cost.totalCost'), value: data ? formatUsd(cost) : '—' },
    { label: t('cost.totalRequests'), value: data ? formatInt(data.requests) : '—' },
    { label: t('cost.totalCredit'), value: data ? credit.toFixed(2) : '—' },
    {
      label: t('cost.costPerCredit'),
      value: data ? (credit > 0 ? `$${(cost / credit).toFixed(4)}` : '—') : '—',
    },
  ]

  const row2: { label: string; value: string; title?: string }[] = [
    {
      label: t('cost.inputTokens'),
      value: data ? formatMillions(uncachedInput) : '—',
      title: data ? formatInt(uncachedInput) : undefined,
    },
    {
      label: t('cost.outputTokens'),
      value: data ? formatMillions(data.output_tokens) : '—',
      title: data ? formatInt(data.output_tokens) : undefined,
    },
    {
      label: t('cost.cacheRead'),
      value: data ? formatMillions(cacheRead) : '—',
      title: data ? formatInt(cacheRead) : undefined,
    },
    {
      label: t('cost.perCreditInOut'),
      value: data
        ? credit > 0
          ? `${formatCompact(uncachedInput / credit)} / ${formatCompact(data.output_tokens / credit)}`
          : '—'
        : '—',
    },
  ]

  return (
    <Card className="px-5 py-4">
      <div className="mb-4 flex items-start justify-between gap-4">
        <div>
          <p className="eyebrow text-muted-foreground">{t('cost.subtitle')}</p>
          <h2 className="mt-1 font-display text-xl font-black tracking-[-0.04em]">
            {t('cost.title')}
          </h2>
        </div>
        <Segment options={options} value={basis} onChange={onBasisChange} />
      </div>

      <div className="grid grid-cols-2 gap-x-6 gap-y-4 sm:grid-cols-4">
        {loading
          ? Array.from({ length: 4 }, (_, index) => (
              <div key={index}>
                <Skeleton className="h-3 w-16" />
                <Skeleton className="mt-2 h-6 w-20" />
              </div>
            ))
          : row1.map((item) => (
              <div key={item.label}>
                <div className="eyebrow text-muted-foreground">{item.label}</div>
                <div
                  className="mt-1 font-display text-2xl font-black tracking-[-0.04em] tabular-nums"
                  title={item.title}
                >
                  {item.value}
                </div>
              </div>
            ))}
      </div>

      <div className="mt-4 grid grid-cols-2 gap-x-6 gap-y-4 border-t pt-4 sm:grid-cols-4">
        {loading
          ? Array.from({ length: 4 }, (_, index) => (
              <div key={index}>
                <Skeleton className="h-3 w-16" />
                <Skeleton className="mt-2 h-6 w-20" />
              </div>
            ))
          : row2.map((item) => (
              <div key={item.label}>
                <div className="eyebrow text-muted-foreground">{item.label}</div>
                <div
                  className="mt-1 font-display text-2xl font-black tracking-[-0.04em] tabular-nums"
                  title={item.title}
                >
                  {item.value}
                </div>
              </div>
            ))}
      </div>

      {basis === 'real' && (
        <p className="mt-3 text-xs text-muted-foreground">{t('cost.realNote')}</p>
      )}
      {data && data.unpriced_requests > 0 && (
        <p className="mt-1 text-xs text-muted-foreground">
          {t('cost.unpricedNote', { n: formatInt(data.unpriced_requests) })}
        </p>
      )}
    </Card>
  )
}
