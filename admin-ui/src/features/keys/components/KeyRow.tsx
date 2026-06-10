import { useState, type KeyboardEvent as ReactKeyboardEvent } from 'react'
import { Check, Pencil, Trash2, X } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Skeleton } from '@/components/ui/skeleton'
import { TD, TR } from '@/components/ui/table'
import type { KeyUsage } from '@/features/usage/types'
import { useI18n } from '@/lib/i18n'
import { cn, formatCompact, formatInt, maskKey } from '@/lib/utils'

import type { ApiKeyRow } from '../types'
import { CopyKeyButton } from './CopyKeyButton'

/** 行内小图标按钮的统一样式（备注铅笔 / 删除等）。 */
const iconButtonClass =
  'inline-flex h-6 w-6 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-black/5 hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50 disabled:pointer-events-none disabled:opacity-50 dark:hover:bg-white/10'

interface KeyRowProps {
  row: ApiKeyRow
  /** 前端按 client_key_id === key join 出的用量行；该 key 没有流量时为 undefined。 */
  usage: KeyUsage | undefined
  usageLoading: boolean
  /** 本行是否有进行中的 mutation（按钮置灰防连点）。 */
  busy: boolean
  onToggleDisabled: (row: ApiKeyRow) => void
  onDelete: (key: string) => void
  onSaveLabel: (key: string, label: string) => void
}

export function KeyRow({
  row,
  usage,
  usageLoading,
  busy,
  onToggleDisabled,
  onDelete,
  onSaveLabel,
}: KeyRowProps) {
  const { t, lang } = useI18n()
  const [editingLabel, setEditingLabel] = useState(false)
  const [labelDraft, setLabelDraft] = useState('')
  const [confirmingDelete, setConfirmingDelete] = useState(false)

  const locale = lang === 'zh' ? 'zh-CN' : 'en-US'
  const createdAt = new Date(row.created_at * 1000)
  const tokensTotal = usage ? usage.input_tokens + usage.output_tokens : 0

  const startEditLabel = () => {
    setLabelDraft(row.label ?? '')
    setEditingLabel(true)
  }

  const saveLabel = () => {
    const next = labelDraft.trim()
    setEditingLabel(false)
    // 没有变化就不发请求；空串 = 清空备注（与后端 PATCH 语义一致）
    if (next === (row.label ?? '')) return
    onSaveLabel(row.key, next)
  }

  const handleLabelKeyDown = (event: ReactKeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'Enter') saveLabel()
    if (event.key === 'Escape') setEditingLabel(false)
  }

  return (
    <TR>
      {/* Key：掩码展示 + 复制完整 key */}
      <TD>
        <span className="inline-flex items-center gap-1.5">
          <code className="rounded-md bg-muted px-2 py-0.5 font-mono text-xs">
            {maskKey(row.key)}
          </code>
          <CopyKeyButton value={row.key} />
        </span>
      </TD>

      {/* 备注：铅笔进入行内编辑，Enter 保存 / Esc 取消 */}
      <TD>
        {editingLabel ? (
          <span className="inline-flex items-center gap-1">
            <input
              value={labelDraft}
              onChange={(event) => setLabelDraft(event.target.value)}
              onKeyDown={handleLabelKeyDown}
              placeholder={t('keys.label.placeholder')}
              autoFocus
              className="h-7 w-36 rounded-lg border bg-input px-2 text-xs text-foreground placeholder:text-muted-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
            />
            <button
              type="button"
              onClick={saveLabel}
              title={t('keys.label.save')}
              className={iconButtonClass}
            >
              <Check className="h-3.5 w-3.5" />
            </button>
            <button
              type="button"
              onClick={() => setEditingLabel(false)}
              title={t('common.cancel')}
              className={iconButtonClass}
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </span>
        ) : (
          <span className="inline-flex items-center gap-1.5">
            <span className={cn('max-w-44 truncate', !row.label && 'text-muted-foreground')}>
              {row.label || '—'}
            </span>
            <button
              type="button"
              onClick={startEditLabel}
              title={t('keys.action.editLabel')}
              disabled={busy}
              className={iconButtonClass}
            >
              <Pencil className="h-3 w-3" />
            </button>
          </span>
        )}
      </TD>

      {/* 状态 */}
      <TD>
        <Badge variant={row.disabled ? 'destructive' : 'success'}>
          {row.disabled ? t('keys.status.disabled') : t('keys.status.enabled')}
        </Badge>
      </TD>

      {/* 创建时间：本地化日期，悬停显示完整时间 */}
      <TD className="text-muted-foreground" title={createdAt.toLocaleString(locale)}>
        {createdAt.toLocaleDateString(locale, {
          year: 'numeric',
          month: '2-digit',
          day: '2-digit',
        })}
      </TD>

      {/* 请求数（来自用量联表，无流量显示 —） */}
      <TD className="text-right">
        {usageLoading ? (
          <Skeleton className="ml-auto h-4 w-12" />
        ) : usage ? (
          <span className="font-medium">{formatInt(usage.requests)}</span>
        ) : (
          <span className="text-muted-foreground">—</span>
        )}
      </TD>

      {/* Token 合计 = input + output，紧凑格式，悬停显示精确值 */}
      <TD className="text-right text-muted-foreground">
        {usageLoading ? (
          <Skeleton className="ml-auto h-4 w-12" />
        ) : usage ? (
          <span title={formatInt(tokensTotal)}>{formatCompact(tokensTotal)}</span>
        ) : (
          '—'
        )}
      </TD>

      {/* 操作：启停 + 删除（删除走二次确认态，提示用量记录保留） */}
      <TD className="text-right">
        {confirmingDelete ? (
          <span className="inline-flex items-center gap-2">
            <span className="text-xs text-muted-foreground">{t('keys.delete.hint')}</span>
            <Button
              variant="destructive"
              size="sm"
              disabled={busy}
              onClick={() => onDelete(row.key)}
            >
              {t('keys.delete.confirm')}
            </Button>
            <Button variant="ghost" size="sm" onClick={() => setConfirmingDelete(false)}>
              {t('common.cancel')}
            </Button>
          </span>
        ) : (
          <span className="inline-flex items-center gap-1.5">
            <Button
              variant="outline"
              size="sm"
              disabled={busy}
              onClick={() => onToggleDisabled(row)}
            >
              {row.disabled ? t('keys.action.enable') : t('keys.action.disable')}
            </Button>
            <button
              type="button"
              onClick={() => setConfirmingDelete(true)}
              title={t('keys.action.delete')}
              disabled={busy}
              className={cn(iconButtonClass, 'hover:text-destructive')}
            >
              <Trash2 className="h-3.5 w-3.5" />
            </button>
          </span>
        )}
      </TD>
    </TR>
  )
}
