import { useEffect, useState, type FormEvent } from 'react'
import { Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Select } from '@/components/ui/select'
import type { GroupRow } from '@/features/groups/types'
import { extractErrorMessage } from '@/lib/api'
import { useI18n } from '@/lib/i18n'
import { formatCompact, formatInt, maskKey } from '@/lib/utils'

import { useUpdateKey } from '../hooks'
import { parseTokenAmount } from '../quota'
import type { ApiKeyRow, UpdateKeyPayload } from '../types'

const inputClass =
  'w-full rounded-2xl border bg-input px-4 py-2.5 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground'

interface KeyManageDialogProps {
  open: boolean
  /** 被管理的 key 行；open 为 true 时必有值（行被删时由父组件关闭）。 */
  row: ApiKeyRow | null
  groups: GroupRow[]
  onClose: () => void
}

/**
 * 「限额与分组」对话框：
 * - 分组 select + 限额输入（K/M 快捷或纯数字，留空不修改）走「保存」一次 PATCH
 * - 清除限额（quota_tokens: 0）与重置已用（reset_used: true）各自带二次确认、立即生效
 */
export function KeyManageDialog({ open, row, groups, onClose }: KeyManageDialogProps) {
  const { t } = useI18n()
  const mutation = useUpdateKey()

  const [group, setGroup] = useState('')
  const [quotaInput, setQuotaInput] = useState('')
  const [confirmingClear, setConfirmingClear] = useState(false)
  const [confirmingReset, setConfirmingReset] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // 打开时预填当前分组；限额输入留空 = 不修改（避免 compact 格式往返丢精度）
  useEffect(() => {
    if (open && row) {
      setGroup(row.group_name)
      setQuotaInput('')
      setConfirmingClear(false)
      setConfirmingReset(false)
      setError(null)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  const handleSave = (event: FormEvent) => {
    event.preventDefault()
    if (!row || mutation.isPending) return

    const patch: UpdateKeyPayload = {}
    if (group !== row.group_name) patch.group_name = group
    if (quotaInput.trim() !== '') {
      const parsed = parseTokenAmount(quotaInput)
      if (parsed === null) {
        setError(t('keys.error.invalidQuota'))
        return
      }
      if (parsed !== row.quota_tokens) patch.quota_tokens = parsed
    }
    setError(null)

    if (Object.keys(patch).length === 0) {
      onClose()
      return
    }
    mutation.mutate(
      { key: row.key, patch },
      { onSuccess: onClose, onError: (err) => setError(extractErrorMessage(err)) },
    )
  }

  /** 清除限额 / 重置已用：立即 PATCH，成功后留在对话框里看到数值刷新。 */
  const runQuickPatch = (patch: UpdateKeyPayload, done: () => void) => {
    if (!row || mutation.isPending) return
    setError(null)
    mutation.mutate(
      { key: row.key, patch },
      {
        onSuccess: done,
        onError: (err) => {
          done()
          setError(extractErrorMessage(err))
        },
      },
    )
  }

  return (
    <Modal open={open && row !== null} onClose={onClose} title={t('keys.manage.title')}>
      {row !== null && (
        <form onSubmit={handleSave} className="mt-4 space-y-4">
          {/* 哪个 key（掩码） */}
          <p>
            <code className="rounded-md bg-muted px-2 py-0.5 font-mono text-xs">
              {maskKey(row.key)}
            </code>
          </p>

          {/* 分组 */}
          <div className="space-y-1.5">
            <label htmlFor="key-group" className="text-xs font-medium text-muted-foreground">
              {t('keys.manage.group')}
            </label>
            <Select
              id="key-group"
              value={group}
              onChange={(event) => setGroup(event.target.value)}
              className="w-full"
            >
              <option value="">{t('groups.ungrouped')}</option>
              {groups.map((g) => (
                /* 标出成员数:换组 = 换这个客户能用的账号集合。不标的话,运维在本页
                   把 key 从低价组改到主组时,看不出这一步把客户放进了主力号池。 */
                <option key={g.name} value={g.name}>
                  {`${g.name} (${g.member_count} ${t('groups.unitMembers')})`}
                </option>
              ))}
            </Select>
          </div>

          {/* 限额输入 + 当前值/已用展示 */}
          <div className="space-y-1.5">
            <label htmlFor="key-quota" className="text-xs font-medium text-muted-foreground">
              {t('keys.manage.quota')}
            </label>
            <input
              id="key-quota"
              value={quotaInput}
              onChange={(event) => setQuotaInput(event.target.value)}
              placeholder={t('keys.manage.quotaPlaceholder')}
              spellCheck={false}
              autoComplete="off"
              className={inputClass}
            />
            <p className="text-xs text-muted-foreground">
              {t('keys.manage.current')}:{' '}
              {row.quota_tokens === null ? (
                t('keys.quota.unlimited')
              ) : (
                <span title={formatInt(row.quota_tokens)}>
                  {formatCompact(row.quota_tokens)}
                </span>
              )}
              {' · '}
              {t('keys.manage.used')}:{' '}
              <span title={formatInt(row.used_tokens)}>{formatCompact(row.used_tokens)}</span>
            </p>
          </div>

          {/* 快捷操作：清除限额 / 重置已用，各自二次确认 */}
          <div className="flex flex-wrap items-center gap-2 border-t border-black/5 pt-3 dark:border-white/5">
            {confirmingClear ? (
              <span className="inline-flex items-center gap-1.5">
                <Button
                  variant="destructive"
                  size="sm"
                  disabled={mutation.isPending}
                  onClick={() => runQuickPatch({ quota_tokens: 0 }, () => setConfirmingClear(false))}
                >
                  {t('keys.manage.confirmClear')}
                </Button>
                <Button variant="ghost" size="sm" onClick={() => setConfirmingClear(false)}>
                  {t('common.cancel')}
                </Button>
              </span>
            ) : (
              <Button
                variant="outline"
                size="sm"
                disabled={row.quota_tokens === null || mutation.isPending}
                onClick={() => {
                  setConfirmingClear(true)
                  setConfirmingReset(false)
                }}
              >
                {t('keys.manage.clearQuota')}
              </Button>
            )}

            {confirmingReset ? (
              <span className="inline-flex items-center gap-1.5">
                <Button
                  variant="destructive"
                  size="sm"
                  disabled={mutation.isPending}
                  onClick={() => runQuickPatch({ reset_used: true }, () => setConfirmingReset(false))}
                >
                  {t('keys.manage.confirmReset')}
                </Button>
                <Button variant="ghost" size="sm" onClick={() => setConfirmingReset(false)}>
                  {t('common.cancel')}
                </Button>
              </span>
            ) : (
              <Button
                variant="outline"
                size="sm"
                disabled={mutation.isPending}
                onClick={() => {
                  setConfirmingReset(true)
                  setConfirmingClear(false)
                }}
              >
                {t('keys.manage.resetUsed')}
              </Button>
            )}
          </div>

          {error !== null && <p className="text-sm text-destructive">{error}</p>}

          <div className="flex justify-end gap-2 pt-1">
            <Button variant="ghost" onClick={onClose}>
              {t('common.cancel')}
            </Button>
            <Button type="submit" disabled={mutation.isPending}>
              {mutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
              {mutation.isPending ? t('keys.manage.saving') : t('keys.manage.save')}
            </Button>
          </div>
        </form>
      )}
    </Modal>
  )
}
