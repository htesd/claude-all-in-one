import type { ChangeEvent } from 'react'
import { CalendarRange, KeyRound, X } from 'lucide-react'

import { Card } from '@/components/ui/card'
import { DateInput } from '@/components/ui/date-input'
import { Select } from '@/components/ui/select'
import { useI18n } from '@/lib/i18n'
import { cn, maskKey } from '@/lib/utils'

import type { TimeRange } from '../types'
import { RangeSegment } from './RangeSegment'

/** Sentinel select values; real key ids are prefixed to avoid collisions. */
const KEY_ALL = '__all__'
const KEY_UNATTRIBUTED = '__unattributed__'
const KEY_PREFIX = 'k:'

interface UsageFilterBarProps {
  preset: TimeRange
  onPresetChange: (value: TimeRange) => void
  fromDate: string
  toDate: string
  onFromDateChange: (value: string) => void
  onToDateChange: (value: string) => void
  /** True when a valid custom from/to range is in effect (overrides presets). */
  rangeActive: boolean
  /** undefined = all keys, '' = unattributed bucket, otherwise a client key id. */
  keyFilter: string | undefined
  onKeyFilterChange: (value: string | undefined) => void
  /** Raw (non-empty) client key ids collected from the by-key data. */
  keyIds: string[]
}

function toSelectValue(keyFilter: string | undefined): string {
  if (keyFilter === undefined) return KEY_ALL
  if (keyFilter === '') return KEY_UNATTRIBUTED
  return `${KEY_PREFIX}${keyFilter}`
}

/** Time (presets + custom date range) and key filters for the usage page. */
export function UsageFilterBar({
  preset,
  onPresetChange,
  fromDate,
  toDate,
  onFromDateChange,
  onToDateChange,
  rangeActive,
  keyFilter,
  onKeyFilterChange,
  keyIds,
}: UsageFilterBarProps) {
  const { t } = useI18n()

  const hasDateInput = fromDate !== '' || toDate !== ''
  // Keep the current selection visible even if it vanished from the key list
  // (e.g. after narrowing the time range).
  const options =
    keyFilter && !keyIds.includes(keyFilter) ? [...keyIds, keyFilter] : keyIds

  const handleKeyChange = (event: ChangeEvent<HTMLSelectElement>) => {
    const value = event.target.value
    if (value === KEY_ALL) onKeyFilterChange(undefined)
    else if (value === KEY_UNATTRIBUTED) onKeyFilterChange('')
    else onKeyFilterChange(value.slice(KEY_PREFIX.length))
  }

  const clearRange = () => {
    onFromDateChange('')
    onToDateChange('')
  }

  return (
    <Card
      variant="glass-subtle"
      className="flex flex-wrap items-center gap-x-5 gap-y-3 rounded-2xl px-4 py-3"
    >
      {/* Time: quick presets (dimmed while a custom range is active) */}
      <div className="flex items-center gap-2">
        <CalendarRange className="h-4 w-4 shrink-0 text-muted-foreground" />
        <div className={cn('transition-opacity', rangeActive && 'opacity-45')}>
          <RangeSegment value={preset} onChange={onPresetChange} />
        </div>
      </div>

      {/* Time: custom date range (takes priority over presets) */}
      <div className="flex items-center gap-1.5">
        <span className="text-xs text-muted-foreground">{t('filter.from')}</span>
        <DateInput
          value={fromDate}
          max={toDate || undefined}
          onChange={(event) => onFromDateChange(event.target.value)}
          aria-label={t('filter.from')}
        />
        <span className="text-xs text-muted-foreground">{t('filter.to')}</span>
        <DateInput
          value={toDate}
          min={fromDate || undefined}
          onChange={(event) => onToDateChange(event.target.value)}
          aria-label={t('filter.to')}
        />
        {hasDateInput && (
          <button
            type="button"
            onClick={clearRange}
            className="inline-flex h-8 items-center gap-1 rounded-lg px-2 text-xs text-muted-foreground transition-colors hover:bg-black/5 hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50 dark:hover:bg-white/10"
          >
            <X className="h-3 w-3" />
            {t('filter.clear')}
          </button>
        )}
      </div>

      {/* Key filter */}
      <div className="flex items-center gap-2">
        <KeyRound className="h-4 w-4 shrink-0 text-muted-foreground" />
        <Select
          value={toSelectValue(keyFilter)}
          onChange={handleKeyChange}
          aria-label={t('table.key')}
          className="min-w-[150px]"
        >
          <option value={KEY_ALL}>{t('filter.allKeys')}</option>
          <option value={KEY_UNATTRIBUTED}>{t('table.unattributed')}</option>
          {options.map((id) => (
            <option key={id} value={`${KEY_PREFIX}${id}`}>
              {maskKey(id)}
            </option>
          ))}
        </Select>
      </div>
    </Card>
  )
}
