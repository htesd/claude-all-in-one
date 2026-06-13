import { useId } from 'react'
import { motion } from 'framer-motion'

import { cn } from '@/lib/utils'

export interface SegmentOption<T extends string | number> {
  value: T
  label: string
}

interface SegmentProps<T extends string | number> {
  options: SegmentOption<T>[]
  value: T
  onChange: (value: T) => void
  className?: string
}

/** Segmented control with a spring-animated acid active pill (framer-motion layoutId). */
export function Segment<T extends string | number>({
  options,
  value,
  onChange,
  className,
}: SegmentProps<T>) {
  const layoutId = useId()

  return (
    <div
      className={cn(
        'inline-flex items-center gap-1 rounded-full bg-black/5 p-1 dark:bg-white/10',
        className,
      )}
      role="tablist"
    >
      {options.map((option) => {
        const active = option.value === value
        return (
          <button
            key={String(option.value)}
            type="button"
            role="tab"
            aria-selected={active}
            onClick={() => onChange(option.value)}
            className={cn(
              'relative rounded-full px-3.5 py-1.5 text-xs font-semibold transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/40',
              active ? 'text-ink' : 'text-muted-foreground hover:text-foreground',
            )}
          >
            {active && (
              <motion.span
                layoutId={layoutId}
                className="absolute inset-0 rounded-full bg-acid"
                transition={{ type: 'spring', stiffness: 380, damping: 32 }}
              />
            )}
            <span className="relative z-10">{option.label}</span>
          </button>
        )
      })}
    </div>
  )
}
