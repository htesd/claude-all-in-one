import type {
  HTMLAttributes,
  TdHTMLAttributes,
  ThHTMLAttributes,
} from 'react'

import { cn } from '@/lib/utils'

export function Table({ className, ...props }: HTMLAttributes<HTMLTableElement>) {
  return (
    <div className="w-full overflow-x-auto">
      <table className={cn('w-full caption-bottom text-sm', className)} {...props} />
    </div>
  )
}

export function THead({ className, ...props }: HTMLAttributes<HTMLTableSectionElement>) {
  return (
    <thead
      className={cn('border-b border-black/5 dark:border-white/5', className)}
      {...props}
    />
  )
}

export function TBody({ className, ...props }: HTMLAttributes<HTMLTableSectionElement>) {
  return <tbody className={className} {...props} />
}

export function TR({ className, ...props }: HTMLAttributes<HTMLTableRowElement>) {
  return (
    <tr
      className={cn(
        'border-b border-black/5 transition-colors last:border-0 hover:bg-black/[0.025] dark:border-white/5 dark:hover:bg-white/[0.04]',
        className,
      )}
      {...props}
    />
  )
}

export function TH({ className, ...props }: ThHTMLAttributes<HTMLTableCellElement>) {
  return (
    <th
      className={cn(
        'h-10 whitespace-nowrap px-4 text-left align-middle text-[11px] font-semibold uppercase tracking-[0.16em] text-muted-foreground',
        className,
      )}
      {...props}
    />
  )
}

export function TD({ className, ...props }: TdHTMLAttributes<HTMLTableCellElement>) {
  return (
    <td
      className={cn('whitespace-nowrap px-4 py-3 align-middle', className)}
      {...props}
    />
  )
}
