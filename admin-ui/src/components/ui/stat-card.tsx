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
      <div className="flex items-center gap-2.5 text-muted-foreground">
        <span className="gradient-bg-primary flex h-8 w-8 shrink-0 items-center justify-center rounded-lg shadow-sm">
          <Icon className="h-4 w-4 text-white" />
        </span>
        <span className="truncate text-xs font-medium">{label}</span>
      </div>
      <div className="mt-3">
        {loading ? (
          <>
            <Skeleton className="h-8 w-24" />
            <Skeleton className="mt-2 h-3 w-16" />
          </>
        ) : (
          <>
            <div className="text-2xl font-bold tracking-tight">{value}</div>
            {sub !== undefined && (
              <div className="mt-1 truncate text-xs text-muted-foreground">{sub}</div>
            )}
          </>
        )}
      </div>
    </Card>
  )
}
