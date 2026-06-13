import { useMemo, useState } from 'react'

import { ErrorNote } from '@/components/ui/error-note'
import { ByKeyTable } from '@/features/usage/components/ByKeyTable'
import { ByModelTable } from '@/features/usage/components/ByModelTable'
import { CostStatsCard } from '@/features/usage/components/CostStatsCard'
import { SummaryStrip } from '@/features/usage/components/SummaryStrip'
import { UsageFilterBar } from '@/features/usage/components/UsageFilterBar'
import { useUsageByKey, useUsageByModel, useUsageSummary } from '@/features/usage/hooks'
import type { CostBasis, TimeRange, UsageFilter } from '@/features/usage/types'
import { useI18n } from '@/lib/i18n'

/**
 * Convert a yyyy-mm-dd date-input pair to a [from, to) unix-second range
 * (`to` = start of the day AFTER the picked end date, i.e. exclusive end-of-day).
 * Returns null while the pair is incomplete or inverted.
 */
function toUnixRange(fromDate: string, toDate: string): { from: number; to: number } | null {
  if (!fromDate || !toDate) return null
  const from = new Date(`${fromDate}T00:00:00`)
  const toStart = new Date(`${toDate}T00:00:00`)
  if (Number.isNaN(from.getTime()) || Number.isNaN(toStart.getTime())) return null
  const toEnd = new Date(toStart)
  toEnd.setDate(toEnd.getDate() + 1)
  const fromSec = Math.floor(from.getTime() / 1000)
  const toSec = Math.floor(toEnd.getTime() / 1000)
  return toSec > fromSec ? { from: fromSec, to: toSec } : null
}

export default function UsagePage() {
  const { t } = useI18n()

  const [preset, setPreset] = useState<TimeRange>(30)
  const [fromDate, setFromDate] = useState('')
  const [toDate, setToDate] = useState('')
  /** undefined = all keys, '' = unattributed bucket, otherwise a client key id. */
  const [keyFilter, setKeyFilter] = useState<string | undefined>(undefined)
  const [basis, setBasis] = useState<CostBasis>('billed')

  const customRange = useMemo(() => toUnixRange(fromDate, toDate), [fromDate, toDate])

  // Custom range > preset; the key filter only narrows summary + by-model.
  const filter = useMemo<UsageFilter>(() => {
    const base: UsageFilter = customRange
      ? { mode: 'range', from: customRange.from, to: customRange.to }
      : preset === 'all'
        ? { mode: 'all' }
        : { mode: 'preset', days: preset }
    return keyFilter === undefined ? base : { ...base, key: keyFilter }
  }, [customRange, preset, keyFilter])

  const summaryQuery = useUsageSummary(filter)
  const byModelQuery = useUsageByModel(filter)
  const byKeyQuery = useUsageByKey(filter)

  const keyIds = useMemo(
    () => (byKeyQuery.data ?? []).map((row) => row.client_key_id).filter((id) => id !== ''),
    [byKeyQuery.data],
  )

  const handlePresetChange = (value: TimeRange) => {
    setPreset(value)
    // Picking a preset is an explicit choice — drop the custom range.
    setFromDate('')
    setToDate('')
  }

  const queryError = summaryQuery.isError
    ? summaryQuery.error
    : byModelQuery.isError
      ? byModelQuery.error
      : byKeyQuery.isError
        ? byKeyQuery.error
        : null

  return (
    <div className="space-y-6">
      {/* Page hero */}
      <header className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <p className="eyebrow">Usage</p>
          <h1 className="mt-2 font-display text-4xl font-black tracking-[-0.04em]">{t('usage.title')}</h1>
          <p className="mt-2 text-sm text-muted-foreground">{t('usage.subtitle')}</p>
        </div>
      </header>

      {/* Filters: time presets / custom date range / key */}
      <UsageFilterBar
        preset={preset}
        onPresetChange={handlePresetChange}
        fromDate={fromDate}
        toDate={toDate}
        onFromDateChange={setFromDate}
        onToDateChange={setToDate}
        rangeActive={customRange !== null}
        keyFilter={keyFilter}
        onKeyFilterChange={setKeyFilter}
        keyIds={keyIds}
      />

      {queryError !== null && <ErrorNote error={queryError} />}

      {/* Cumulative cost & usage (hero card with billed/real toggle) */}
      <CostStatsCard
        data={summaryQuery.data}
        loading={summaryQuery.isPending}
        basis={basis}
        onBasisChange={setBasis}
      />

      {/* Filtered summary */}
      <SummaryStrip data={summaryQuery.data} loading={summaryQuery.isPending} />

      {/* Per-model usage (compact table with inline proportion bars) */}
      <ByModelTable data={byModelQuery.data} loading={byModelQuery.isPending} basis={basis} />

      {/* Per-key usage (always all keys; time-filtered only) */}
      <ByKeyTable data={byKeyQuery.data} loading={byKeyQuery.isPending} showCacheColumns />
    </div>
  )
}
