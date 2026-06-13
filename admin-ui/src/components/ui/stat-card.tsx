import type { LucideIcon } from 'lucide-react'

import { Card } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'

interface StatCardProps {
  icon: LucideIcon
  label: string
  value: string
  sub?: string
  loading?: boolean
}

export function StatCard({ icon: Icon, label, value, sub, loading = false }: StatCardProps) {
  return (
    <Card className="hover-lift p-5">
      <div className="flex items-center justify-between gap-2 text-muted-foreground">
        <span className="truncate text-sm">{label}</span>
        <Icon className="h-4 w-4 shrink-0 opacity-60" />
      </div>
      <div className="mt-3">
        {loading ? (
          <>
            <Skeleton className="h-8 w-24" />
            <Skeleton className="mt-2 h-3 w-16" />
          </>
        ) : (
          <>
            <div className="font-display text-3xl font-black tracking-[-0.04em]">{value}</div>
            {sub !== undefined && (
              <div className="mt-1 truncate text-xs text-muted-foreground">{sub}</div>
            )}
          </>
        )}
      </div>
    </Card>
  )
}
