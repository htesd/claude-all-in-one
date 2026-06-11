import { useEffect, useState, type FormEvent } from 'react'
import { ChevronRight, Info, Loader2 } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Modal } from '@/components/ui/modal'
import { Select } from '@/components/ui/select'
import { useGroups } from '@/features/groups/hooks'
import { extractErrorMessage } from '@/lib/api'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import { useUpdateAccount } from '../hooks'
import { buildRotatedExtra, getMaskedRefreshToken, parseConcurrency } from '../lib'
import type { AccountRow, UpdateAccountPayload } from '../types'

const inputClass =
  'w-full rounded-xl border bg-input px-3 py-2 text-sm text-foreground transition-colors placeholder:text-muted-foreground focus:outline-none'

interface EditAccountDialogProps {
  open: boolean
  /** 被编辑的账号行；open 为 true 时必有值。 */
  row: AccountRow | null
  onClose: () => void
}

/**
 * 编辑账号对话框：分组、并发数，以及可选的「更换凭据」折叠区。
 * 凭据 textarea 留空 = 不动 extra；填了 = 整体替换（保留原非敏感字段，
 * 脱敏的 `***` 字段绝不回写）。
 */
export function EditAccountDialog({ open, row, onClose }: EditAccountDialogProps) {
  const { t } = useI18n()
  const groupsQuery = useGroups()
  const mutation = useUpdateAccount()

  const [group, setGroup] = useState('')
  const [concurrency, setConcurrency] = useState('1')
  const [rotateOpen, setRotateOpen] = useState(false)
  const [token, setToken] = useState('')
  const [proxyUrl, setProxyUrl] = useState('')
  const [initialProxyUrl, setInitialProxyUrl] = useState('')
  const [error, setError] = useState<string | null>(null)

  // 打开时预填当前值；编辑过程中列表 refetch 换引用不打断草稿
  useEffect(() => {
    if (open && row) {
      setGroup(row.group_name)
      setConcurrency(String(row.max_concurrency))
      setRotateOpen(false)
      setToken('')
      const currentProxy = typeof row.extra.proxy === 'string' ? row.extra.proxy : ''
      setProxyUrl(currentProxy)
      setInitialProxyUrl(currentProxy)
      setError(null)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  const maskedToken = row ? getMaskedRefreshToken(row.extra) : null

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (!row || mutation.isPending) return

    const parsedConcurrency = parseConcurrency(concurrency)
    if (parsedConcurrency === null) {
      setError(t('accounts.error.invalidConcurrency'))
      return
    }
    setError(null)

    // 只 PATCH 变化的字段；凭据留空 = 不传 extra
    const patch: UpdateAccountPayload = {}
    if (group !== row.group_name) patch.group_name = group
    if (parsedConcurrency !== row.max_concurrency) patch.max_concurrency = parsedConcurrency
    const trimmedToken = token.trim()
    if (trimmedToken !== '') patch.extra = buildRotatedExtra(row.extra, trimmedToken)
    // proxy_url: 不传=不动，'' = 清除，非空字符串 = 设置
    if (proxyUrl !== initialProxyUrl) patch.proxy_url = proxyUrl

    if (Object.keys(patch).length === 0) {
      onClose()
      return
    }

    mutation.mutate(
      { id: row.account_id, patch },
      {
        onSuccess: onClose,
        onError: (err) => setError(extractErrorMessage(err)),
      },
    )
  }

  return (
    <Modal open={open && row !== null} onClose={onClose} title={t('accounts.edit.title')}>
      {row !== null && (
        <form onSubmit={handleSubmit} className="mt-4 space-y-4">
          {/* 账号 ID（只读） */}
          <div className="space-y-1.5">
            <span className="text-xs font-medium text-muted-foreground">
              {t('accounts.field.id')}
            </span>
            <p>
              <code className="rounded-md bg-muted px-2 py-0.5 font-mono text-xs">
                {row.account_id}
              </code>
            </p>
          </div>

          {/* 分组 */}
          <div className="space-y-1.5">
            <label
              htmlFor="edit-account-group"
              className="text-xs font-medium text-muted-foreground"
            >
              {t('accounts.field.group')}
            </label>
            <Select
              id="edit-account-group"
              value={group}
              onChange={(event) => setGroup(event.target.value)}
              className="w-full"
            >
              <option value="">{t('groups.ungrouped')}</option>
              {(groupsQuery.data ?? []).map((g) => (
                <option key={g.name} value={g.name}>
                  {g.name}
                </option>
              ))}
            </Select>
          </div>

          {/* 并发上限 */}
          <div className="space-y-1.5">
            <label
              htmlFor="edit-account-concurrency"
              className="text-xs font-medium text-muted-foreground"
            >
              {t('accounts.field.concurrency')}
            </label>
            <input
              id="edit-account-concurrency"
              type="number"
              min={1}
              step={1}
              value={concurrency}
              onChange={(event) => setConcurrency(event.target.value)}
              className={inputClass}
            />
          </div>

          {/* 出口代理（可选） */}
          <div className="space-y-1.5">
            <label
              htmlFor="edit-account-proxy"
              className="text-xs font-medium text-muted-foreground"
            >
              {t('accounts.field.proxy')}
            </label>
            <input
              id="edit-account-proxy"
              type="text"
              value={proxyUrl}
              onChange={(event) => setProxyUrl(event.target.value)}
              placeholder={t('accounts.field.proxyPlaceholder')}
              spellCheck={false}
              autoComplete="off"
              className={inputClass}
            />
          </div>

          {/* 更换凭据：默认折叠；展示脱敏尾号便于核对 */}
          <div className="space-y-1.5">
            <button
              type="button"
              onClick={() => setRotateOpen((prev) => !prev)}
              className="inline-flex items-center gap-1 rounded text-xs font-medium text-muted-foreground transition-colors hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
            >
              <ChevronRight
                className={cn('h-3.5 w-3.5 transition-transform', rotateOpen && 'rotate-90')}
              />
              {t('accounts.edit.rotateToggle')}
            </button>
            {rotateOpen && (
              <div className="space-y-1.5">
                <p className="text-xs text-muted-foreground">
                  {t('accounts.edit.currentToken')}:{' '}
                  <code className="rounded bg-muted px-1.5 py-0.5 font-mono">
                    {maskedToken ?? '—'}
                  </code>
                </p>
                <textarea
                  value={token}
                  onChange={(event) => setToken(event.target.value)}
                  placeholder={t('accounts.field.refreshTokenPlaceholder')}
                  rows={3}
                  spellCheck={false}
                  autoComplete="off"
                  className={`${inputClass} resize-none font-mono text-xs leading-5`}
                />
                <p className="text-xs text-muted-foreground">{t('accounts.edit.rotateHint')}</p>
              </div>
            )}
          </div>

          {error !== null && <p className="text-sm text-destructive">{error}</p>}

          {/* worker 周期同步提示 */}
          <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Info className="h-3.5 w-3.5 shrink-0" />
            {t('accounts.syncHint')}
          </p>

          <div className="flex justify-end gap-2 pt-1">
            <Button variant="ghost" onClick={onClose}>
              {t('common.cancel')}
            </Button>
            <Button type="submit" disabled={mutation.isPending}>
              {mutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
              {mutation.isPending ? t('accounts.edit.saving') : t('accounts.edit.submit')}
            </Button>
          </div>
        </form>
      )}
    </Modal>
  )
}
