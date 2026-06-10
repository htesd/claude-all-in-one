import type { HTMLAttributes } from 'react'

import { cn } from '@/lib/utils'

export function Skeleton({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn('animate-pulse rounded-lg bg-black/10 dark:bg-white/10', className)}
      {...props}
    />
  )
}
