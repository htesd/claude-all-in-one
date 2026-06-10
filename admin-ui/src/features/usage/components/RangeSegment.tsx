import { Segment, type SegmentOption } from '@/components/ui/segment'
import { useI18n } from '@/lib/i18n'

import type { TimeRange } from '../types'

interface RangeSegmentProps {
  value: TimeRange
  onChange: (value: TimeRange) => void
}

export function RangeSegment({ value, onChange }: RangeSegmentProps) {
  const { t } = useI18n()
  const options: SegmentOption<TimeRange>[] = [
    { value: 7, label: t('range.7d') },
    { value: 30, label: t('range.30d') },
    { value: 'all', label: t('range.all') },
  ]
  return <Segment options={options} value={value} onChange={onChange} />
}
