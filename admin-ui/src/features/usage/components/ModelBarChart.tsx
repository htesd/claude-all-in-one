import { useId } from 'react'

import { Skeleton } from '@/components/ui/skeleton'
import { useI18n } from '@/lib/i18n'
import { formatInt, truncate } from '@/lib/utils'

import type { ModelUsage } from '../types'

interface ModelBarChartProps {
  data: ModelUsage[] | undefined
  loading: boolean
}

const CHART_WIDTH = 640
const ROW_HEIGHT = 44

/** Hand-coded inline SVG horizontal bar chart of top models by request count. */
export function ModelBarChart({ data, loading }: ModelBarChartProps) {
  const { t } = useI18n()
  const gradientId = useId().replace(/:/g, '')

  if (loading) {
    return (
      <div className="space-y-4">
        {Array.from({ length: 5 }, (_, index) => (
          <div key={index} className="space-y-2">
            <Skeleton className="h-3 w-48" />
            <Skeleton className="h-3 w-full" />
          </div>
        ))}
      </div>
    )
  }

  const top = [...(data ?? [])].sort((a, b) => b.requests - a.requests).slice(0, 8)

  if (top.length === 0) {
    return (
      <div className="py-10 text-center text-sm text-muted-foreground">
        {t('table.empty')}
      </div>
    )
  }

  const max = Math.max(...top.map((row) => row.requests), 1)
  const height = top.length * ROW_HEIGHT

  return (
    <svg
      viewBox={`0 0 ${CHART_WIDTH} ${height}`}
      className="w-full text-foreground"
      role="img"
      aria-label={t('chart.topModels')}
    >
      <defs>
        <linearGradient id={gradientId} x1="0" y1="0" x2="1" y2="0">
          <stop offset="0%" style={{ stopColor: 'var(--gradient-from)' }} />
          <stop offset="100%" style={{ stopColor: 'var(--gradient-to)' }} />
        </linearGradient>
      </defs>
      {top.map((row, index) => {
        const y = index * ROW_HEIGHT
        const barWidth = Math.max((row.requests / max) * CHART_WIDTH, 6)
        return (
          <g key={row.model}>
            <text x={0} y={y + 15} fontSize={12} className="fill-current opacity-70">
              {truncate(row.model, 52)}
            </text>
            <text
              x={CHART_WIDTH}
              y={y + 15}
              fontSize={12}
              fontWeight={600}
              textAnchor="end"
              className="fill-current"
            >
              {formatInt(row.requests)}
            </text>
            <rect
              x={0}
              y={y + 24}
              width={CHART_WIDTH}
              height={12}
              rx={6}
              className="fill-current opacity-10"
            />
            <rect
              x={0}
              y={y + 24}
              width={barWidth}
              height={12}
              rx={6}
              fill={`url(#${gradientId})`}
            />
          </g>
        )
      })}
    </svg>
  )
}
