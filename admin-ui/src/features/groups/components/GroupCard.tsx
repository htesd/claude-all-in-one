import { useState } from 'react'
import { Pencil, Trash2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import { DEFAULT_GROUP_COLOR, type GroupRow } from '../types'

/** 卡片右上角小图标按钮的统一样式（与 Key 行内按钮一致）。 */
const iconButtonClass =
  'inline-flex h-6 w-6 items-center justify-center rounded-full text-muted-foreground transition-colors hover:bg-black/5 hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50 disabled:pointer-events-none disabled:opacity-50 dark:hover:bg-white/10'

interface GroupCardProps {
  group: GroupRow
  /** 本卡是否有进行中的删除 mutation（按钮置灰防连点）。 */
  busy: boolean
  onEdit: (group: GroupRow) => void
  onDelete: (name: string) => void
}

/** 单个分组的玻璃小卡：色点 + 组名、note、成员计数、编辑/删除（带二次确认）。 */
export function GroupCard({ group, busy, onEdit, onDelete }: GroupCardProps) {
  const { t } = useI18n()
  const [confirmingDelete, setConfirmingDelete] = useState(false)

  const color = group.color || DEFAULT_GROUP_COLOR

  return (
    <Card className="flex flex-col gap-3 p-5">
      <div className="flex items-start justify-between gap-2">
        <div className="flex min-w-0 items-center gap-2">
          <span
            className="h-2.5 w-2.5 shrink-0 rounded-full"
            style={{ backgroundColor: color }}
          />
          <h3 className="truncate text-sm font-black tracking-[-0.01em]" title={group.name}>
            {group.name}
          </h3>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <button
            type="button"
            onClick={() => onEdit(group)}
            title={t('groups.action.edit')}
            disabled={busy}
            className={iconButtonClass}
          >
            <Pencil className="h-3 w-3" />
          </button>
          <button
            type="button"
            onClick={() => setConfirmingDelete(true)}
            title={t('groups.action.delete')}
            disabled={busy}
            className={cn(iconButtonClass, 'hover:text-rose-600 dark:hover:text-rose-300')}
          >
            <Trash2 className="h-3.5 w-3.5" />
          </button>
        </div>
      </div>

      {group.note !== '' && (
        <p className="line-clamp-2 text-xs text-muted-foreground" title={group.note}>
          {group.note}
        </p>
      )}

      {/* 底部：成员计数；删除确认态时切换为说明 + 确认/取消 */}
      <div className="mt-auto pt-1">
        {confirmingDelete ? (
          <div className="space-y-2">
            <p className="text-xs text-muted-foreground">{t('groups.delete.hint')}</p>
            <div className="flex items-center gap-2">
              <Button
                variant="destructive"
                size="sm"
                disabled={busy}
                onClick={() => {
                  onDelete(group.name)
                  setConfirmingDelete(false)
                }}
              >
                {t('groups.delete.confirm')}
              </Button>
              <Button variant="ghost" size="sm" onClick={() => setConfirmingDelete(false)}>
                {t('common.cancel')}
              </Button>
            </div>
          </div>
        ) : (
          <p className="text-xs text-muted-foreground">
            {group.account_count} {t('groups.unitAccounts')} · {group.key_count}{' '}
            {t('groups.unitKeys')}
          </p>
        )}
      </div>
    </Card>
  )
}
