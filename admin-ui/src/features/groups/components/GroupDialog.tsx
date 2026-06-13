import { useEffect, useMemo, useState, type FormEvent } from 'react'
import { Check, Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { extractErrorMessage, getErrorStatus } from '@/lib/api'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import { useCreateGroup, useUpdateGroup } from '../hooks'
import {
  GROUP_COLOR_PRESETS,
  GROUP_NAME_PATTERN,
  type CreateGroupPayload,
  type GroupRow,
  type UpdateGroupPayload,
} from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

interface GroupDialogProps {
  open: boolean
  /** null = 新建；非 null = 编辑该分组（名称不可改）。 */
  group: GroupRow | null
  onClose: () => void
}

/** 新建/编辑分组对话框：名称（仅新建）、8 色预设色板、备注。 */
export function GroupDialog({ open, group, onClose }: GroupDialogProps) {
  const { t } = useI18n()
  const createMutation = useCreateGroup()
  const updateMutation = useUpdateGroup()

  const editing = group !== null
  const pending = createMutation.isPending || updateMutation.isPending

  const [name, setName] = useState('')
  const [color, setColor] = useState<string>(GROUP_COLOR_PRESETS[0])
  const [note, setNote] = useState('')
  const [error, setError] = useState<string | null>(null)

  // 每次打开都从干净状态开始（编辑时预填当前值）
  useEffect(() => {
    if (open) {
      setName(group?.name ?? '')
      setColor(group?.color || GROUP_COLOR_PRESETS[0])
      setNote(group?.note ?? '')
      setError(null)
    }
    // eslint 风格说明：仅在 open 翻转时重置，编辑中 group 因 refetch 换引用不打断草稿
  }, [open]) // eslint-disable-line react-hooks/exhaustive-deps

  // 编辑时若当前颜色不在预设里，把它加进色板，避免「选中态消失」
  const swatches = useMemo(() => {
    const base: string[] = [...GROUP_COLOR_PRESETS]
    if (group?.color && !base.includes(group.color)) base.unshift(group.color)
    return base
  }, [group])

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (pending) return

    if (!editing && !GROUP_NAME_PATTERN.test(name)) {
      setError(t('groups.error.invalidName'))
      return
    }
    setError(null)

    const onError = (err: unknown) => {
      // 409 = 重名，400 = 名称非法，其余透出服务端 message
      const status = getErrorStatus(err)
      if (status === 409) setError(t('groups.error.duplicate'))
      else if (status === 400) setError(t('groups.error.invalidName'))
      else setError(extractErrorMessage(err))
    }

    const trimmedNote = note.trim()
    if (editing) {
      // 只 PATCH 变化的字段；都没变直接关
      const patch: UpdateGroupPayload = {}
      if (color !== group.color) patch.color = color
      if (trimmedNote !== group.note) patch.note = trimmedNote
      if (Object.keys(patch).length === 0) {
        onClose()
        return
      }
      updateMutation.mutate({ name: group.name, patch }, { onSuccess: onClose, onError })
    } else {
      const payload: CreateGroupPayload = { name, color }
      if (trimmedNote !== '') payload.note = trimmedNote
      createMutation.mutate(payload, { onSuccess: onClose, onError })
    }
  }

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={editing ? t('groups.dialog.editTitle') : t('groups.dialog.createTitle')}
    >
      <form onSubmit={handleSubmit} className="mt-4 space-y-4">
        {/* 名称：新建可输入，编辑只读展示 */}
        <div className="space-y-1.5">
          <label htmlFor="group-name" className="text-xs font-medium text-muted-foreground">
            {t('groups.dialog.name')}
          </label>
          {editing ? (
            <p className="text-sm font-medium">{group.name}</p>
          ) : (
            <div className="space-y-1">
              <input
                id="group-name"
                value={name}
                onChange={(event) => setName(event.target.value)}
                placeholder={t('groups.dialog.namePlaceholder')}
                spellCheck={false}
                autoComplete="off"
                autoFocus
                className={inputClass}
              />
              <p className="text-xs text-muted-foreground">{t('groups.dialog.nameRule')}</p>
            </div>
          )}
        </div>

        {/* 颜色：预设色板色块选择 */}
        <div className="space-y-1.5">
          <span className="text-xs font-medium text-muted-foreground">
            {t('groups.dialog.color')}
          </span>
          <div className="flex flex-wrap gap-2">
            {swatches.map((swatch) => (
              <button
                key={swatch}
                type="button"
                onClick={() => setColor(swatch)}
                title={swatch}
                aria-label={swatch}
                className={cn(
                  'flex h-7 w-7 items-center justify-center rounded-full transition-transform focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50',
                  color === swatch ? 'scale-110 ring-2 ring-ring/60' : 'hover:scale-105',
                )}
                style={{ backgroundColor: swatch }}
              >
                {color === swatch && <Check className="h-3.5 w-3.5 text-white" />}
              </button>
            ))}
          </div>
        </div>

        {/* 备注 */}
        <div className="space-y-1.5">
          <label htmlFor="group-note" className="text-xs font-medium text-muted-foreground">
            {t('groups.dialog.note')}
          </label>
          <input
            id="group-note"
            value={note}
            onChange={(event) => setNote(event.target.value)}
            placeholder={t('groups.dialog.notePlaceholder')}
            className={inputClass}
          />
        </div>

        {error !== null && <p className="text-sm text-destructive">{error}</p>}

        <div className="flex justify-end gap-2 pt-1">
          <Button variant="ghost" onClick={onClose}>
            {t('common.cancel')}
          </Button>
          <Button type="submit" disabled={pending}>
            {pending && <Loader2 className="h-4 w-4 animate-spin" />}
            {editing
              ? pending
                ? t('groups.dialog.saving')
                : t('groups.dialog.save')
              : pending
                ? t('groups.dialog.creating')
                : t('groups.dialog.create')}
          </Button>
        </div>
      </form>
    </Modal>
  )
}
