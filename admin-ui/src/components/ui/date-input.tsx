import { forwardRef, type InputHTMLAttributes } from 'react'

import { cn } from '@/lib/utils'

export type DateInputProps = Omit<InputHTMLAttributes<HTMLInputElement>, 'type'>

/** Compact styled native date input (color-scheme follows light/dark theme). */
export const DateInput = forwardRef<HTMLInputElement, DateInputProps>(
  ({ className, ...props }, ref) => (
    <input
      ref={ref}
      type="date"
      className={cn(
        'h-8 rounded-lg border bg-input px-2.5 text-xs text-foreground transition-colors [color-scheme:light] focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50 dark:[color-scheme:dark]',
        className,
      )}
      {...props}
    />
  ),
)
DateInput.displayName = 'DateInput'
