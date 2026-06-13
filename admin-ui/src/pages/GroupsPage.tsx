import { useState } from 'react'
import { Plus } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { ErrorNote } from '@/components/ui/error-note'
import { Skeleton } from '@/components/ui/skeleton'
import { GroupCard } from '@/features/groups/components/GroupCard'
import { GroupDialog } from '@/features/groups/components/GroupDialog'
import { useDeleteGroup, useGroups } from '@/features/groups/hooks'
import type { GroupRow } from '@/features/groups/types'
import { useI18n } from '@/lib/i18n'

const gridClass = 'grid gap-4 sm:grid-cols-2 xl:grid-cols-3'

export default function GroupsPage() {
  const { t } = useI18n()

  const groupsQuery = useGroups()
  const deleteMutation = useDeleteGroup()

  const [dialogOpen, setDialogOpen] = useState(false)
  /** null = 新建；非 null = 编辑该分组。 */
  const [editingGroup, setEditingGroup] = useState<GroupRow | null>(null)

  const openCreate = () => {
    setEditingGroup(null)
    setDialogOpen(true)
  }
  const openEdit = (group: GroupRow) => {
    setEditingGroup(group)
    setDialogOpen(true)
  }

  // 当前有删除 mutation 进行中的组名 —— 只置灰对应卡片的按钮
  const busyName = deleteMutation.isPending ? (deleteMutation.variables ?? null) : null

  const groups = groupsQuery.data ?? []

  return (
    <div className="space-y-6">
      {/* Page hero：标题 + 新建入口 */}
      <header className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <p className="eyebrow">Groups</p>
          <h1 className="mt-2 font-display text-4xl font-black tracking-[-0.04em]">{t('groups.title')}</h1>
          <p className="mt-2 text-sm text-muted-foreground">{t('groups.subtitle')}</p>
        </div>
        <Button onClick={openCreate}>
          <Plus className="h-4 w-4" />
          {t('groups.new')}
        </Button>
      </header>

      {groupsQuery.isError && <ErrorNote error={groupsQuery.error} />}
      {deleteMutation.isError && (
        <ErrorNote error={deleteMutation.error} labelKey="common.actionFailed" />
      )}

      {groupsQuery.isPending ? (
        <div className={gridClass}>
          {Array.from({ length: 3 }, (_, index) => (
            <Card key={index} className="space-y-3 p-5">
              <Skeleton className="h-4 w-24" />
              <Skeleton className="h-3 w-36" />
              <Skeleton className="h-3 w-20" />
            </Card>
          ))}
        </div>
      ) : groups.length === 0 ? (
        <Card className="p-10 text-center text-sm text-muted-foreground">
          {t('groups.empty')}
        </Card>
      ) : (
        <div className={gridClass}>
          {groups.map((group) => (
            <GroupCard
              key={group.name}
              group={group}
              busy={busyName === group.name}
              onEdit={openEdit}
              onDelete={(name) => deleteMutation.mutate(name)}
            />
          ))}
        </div>
      )}

      <GroupDialog open={dialogOpen} group={editingGroup} onClose={() => setDialogOpen(false)} />
    </div>
  )
}
