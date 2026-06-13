import { forwardRef, type SelectHTMLAttributes } from 'react'
import { ChevronDown } from 'lucide-react'

import { cn } from '@/lib/utils'

export interface SelectProps extends SelectHTMLAttributes<HTMLSelectElement> {
  /** Classes applied to the outer wrapper (use for width control). */
  className?: string
}

/** Compact styled native select with a chevron affordance. */
export const Select = forwardRef<HTMLSelectElement, SelectProps>(
  ({ className, children, ...props }, ref) => (
    <div className={cn('relative inline-flex items-center', className)}>
      <select
        ref={ref}
        className="h-8 w-full cursor-pointer appearance-none rounded-full border bg-input pl-3.5 pr-8 text-xs font-medium text-foreground transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
        {...props}
      >
        {children}
      </select>
      <ChevronDown className="pointer-events-none absolute right-3 h-3.5 w-3.5 text-muted-foreground" />
    </div>
  ),
)
Select.displayName = 'Select'
