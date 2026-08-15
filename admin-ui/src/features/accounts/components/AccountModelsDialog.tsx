import { useEffect, useState } from 'react'
import { Loader2 } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { ErrorNote } from '@/components/ui/error-note'
import { Modal } from '@/components/ui/modal'
import { Skeleton } from '@/components/ui/skeleton'
import { extractErrorMessage } from '@/lib/api'
import { useI18n } from '@/lib/i18n'

import { useAccountModelsLocal, useUpdateAccount } from '../hooks'
import { deriveModelAvailability, formatMarkTtl, modelAllowlistAllows } from '../lib'
import type { AccountRow } from '../types'

interface AccountModelsDialogProps {
  open: boolean
  /** 目标账号行；open 为 true 时必有值。行被列表 refetch 删掉后由调用方置 null 自动关闭。 */
  row: AccountRow | null
  onClose: () => void
}

/**
 * 「查看模型」弹窗：账号可用模型清单（`GET /accounts/{id}/models/local`）。
 *
 * 数据是 worker 的**本地认知**（静态目录 + 档位支持判断 + 已学 INVALID_MODEL_ID 标记），
 * 全程零上游调用，随便点。未观察到拒绝 ≠ 上游保证能出词。
 *
 * cursor 账号额外支持**勾选式白名单编辑**（`extra.model_allowlist`）：
 * - 勾选基线 = 现有白名单对每个目录模型的判定（null/缺失 = 不限 → 全勾）；
 * - 全勾保存 = 清除白名单（回到不限）；部分勾选 = 写明确列表；
 * - 全不勾拒绝保存 —— 后端语义是空表 = 全禁（fail-closed），真要停号用「禁用」表达；
 * - 勾选集合没变就不发请求，避免把已配的 `前缀*` 通配静默展开成明确列表。
 * 其他 provider（kiro/dario 未接线白名单）保持纯只读展示。
 */
export function AccountModelsDialog({ open, row, onClose }: AccountModelsDialogProps) {
  const { t } = useI18n()
  // 关掉/无目标时传 null 停查。hook 已设 staleTime:0 + refetchOnMount:'always'：
  // 每次打开都重新取快照，不会展示旧缓存；打开期间不轮询。
  const query = useAccountModelsLocal(open && row ? row.account_id : null)
  const mutation = useUpdateAccount()

  // 只有 cursor 接线了白名单调度；其他 provider 勾了也不生效，不给入口免得误导。
  const editable = row?.provider === 'cursor'

  const [checked, setChecked] = useState<Record<string, boolean>>({})
  /** 打开时刻的勾选基线；null = 还没 seed（数据未到）。用于脏检查。 */
  const [initial, setInitial] = useState<Record<string, boolean> | null>(null)
  const [error, setError] = useState<string | null>(null)

  // 打开且目录数据到达时 seed 一次草稿；之后列表 refetch 换 row 引用不打断编辑。
  // 关闭时清 seed，让下次打开重算基线。
  useEffect(() => {
    if (!open) {
      setInitial(null)
      setError(null)
    }
  }, [open])
  const models = query.data?.models
  useEffect(() => {
    if (open && row && models !== undefined && initial === null) {
      const base = Object.fromEntries(
        models.map((m) => [m.id, modelAllowlistAllows(row.model_allowlist ?? null, m.id)]),
      )
      setChecked(base)
      setInitial(base)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, models, initial])

  // 现有白名单含末尾通配时提示：保存勾选会替换成明确列表（通配的「自动放行未来型号」会丢）。
  const hasWildcard =
    editable && (row?.model_allowlist ?? []).some((entry) => entry.endsWith('*'))

  const handleSave = () => {
    if (!row || models === undefined || initial === null || mutation.isPending) return
    const ids = models.map((m) => m.id)
    const selected = ids.filter((id) => checked[id])
    if (selected.length === 0) {
      // 后端会把空表按全禁（fail-closed）拒绝，这里提前拦下并解释替代做法。
      setError(t('accounts.models.allowlist.emptyError'))
      return
    }
    const dirty = ids.some((id) => (checked[id] ?? false) !== (initial[id] ?? false))
    if (!dirty) {
      onClose()
      return
    }
    // 全勾 = 清除白名单（''），回到「不限」；部分勾选 = 逗号串交后端规范化落库。
    const payload = selected.length === ids.length ? '' : selected.join(', ')
    setError(null)
    void (async () => {
      try {
        await mutation.mutateAsync({
          id: row.account_id,
          patch: { model_allowlist: payload },
        })
        onClose()
      } catch (err) {
        setError(extractErrorMessage(err))
      }
    })()
  }

  const busy = mutation.isPending

  return (
    <Modal
      open={open && row !== null}
      onClose={onClose}
      title={`${t('accounts.models.title')} · ${row?.account_id ?? ''}`}
      className="max-w-lg"
    >
      <p className="mt-2 text-xs text-muted-foreground">{t('accounts.models.localNote')}</p>
      {editable && (
        <p className="mt-1.5 text-xs text-muted-foreground">
          {t('accounts.models.allowlist.hint')}
        </p>
      )}
      {hasWildcard && (
        <p className="mt-1.5 text-xs text-amber-600 dark:text-amber-500">
          {t('accounts.models.allowlist.wildcardNote')}
        </p>
      )}

      <div className="mt-4">
        {query.isPending ? (
          <div className="space-y-2">
            {Array.from({ length: 5 }, (_, i) => (
              <Skeleton key={i} className="h-8 w-full" />
            ))}
          </div>
        ) : query.isError ? (
          <ErrorNote error={query.error} />
        ) : (
          <>
            {query.data.models.length === 0 ? (
              <p className="py-6 text-center text-sm text-muted-foreground">
                {t('accounts.models.empty')}
              </p>
            ) : (
              <ul className="space-y-1.5">
                {query.data.models.map((m) => {
                  const state = deriveModelAvailability(m)
                  const name = (
                    <span className="min-w-0">
                      <span className="block truncate text-sm font-medium">
                        {m.display_name ?? m.id}
                      </span>
                      {m.display_name != null && m.display_name !== m.id && (
                        <code className="block truncate font-mono text-xs text-muted-foreground">
                          {m.id}
                        </code>
                      )}
                    </span>
                  )
                  return (
                    <li
                      key={m.id}
                      className="flex items-center justify-between gap-3 rounded-2xl bg-muted/60 px-3 py-2"
                    >
                      {editable ? (
                        <label className="flex min-w-0 flex-1 cursor-pointer items-center gap-2.5">
                          <input
                            type="checkbox"
                            checked={checked[m.id] ?? false}
                            onChange={(event) =>
                              setChecked((prev) => ({ ...prev, [m.id]: event.target.checked }))
                            }
                            className="h-4 w-4 shrink-0 rounded border accent-primary"
                          />
                          {name}
                        </label>
                      ) : (
                        name
                      )}
                      {state === 'available' ? (
                        <Badge variant="success">{t('accounts.models.state.available')}</Badge>
                      ) : state === 'marked' ? (
                        <Badge
                          variant="warning"
                          title={t('accounts.models.markedHint')}
                        >
                          {t('accounts.models.state.marked')} ·{' '}
                          {formatMarkTtl(m.mark_remaining_secs ?? 0)}
                        </Badge>
                      ) : (
                        <Badge variant="muted">{t('accounts.models.state.unsupported')}</Badge>
                      )}
                    </li>
                  )
                })}
              </ul>
            )}

            {/* 目录外标记：上游新模型/请求变体名打不中目录行，单独透出，不悄悄丢掉。 */}
            {query.data.off_catalog_marks.length > 0 && (
              <div className="mt-4">
                <h3 className="text-xs font-semibold text-muted-foreground">
                  {t('accounts.models.offCatalog')}
                </h3>
                <ul className="mt-1.5 space-y-1.5">
                  {query.data.off_catalog_marks.map((m) => (
                    <li
                      key={m.model}
                      className="flex items-center justify-between gap-3 rounded-2xl bg-muted/60 px-3 py-2"
                    >
                      <code className="truncate font-mono text-xs">{m.model}</code>
                      <Badge variant="warning" title={t('accounts.models.markedHint')}>
                        {formatMarkTtl(m.remaining_secs)}
                      </Badge>
                    </li>
                  ))}
                </ul>
              </div>
            )}

            {editable && query.data.models.length > 0 && (
              <>
                {error !== null && <p className="mt-3 text-sm text-destructive">{error}</p>}
                <div className="mt-4 flex justify-end gap-2">
                  <Button variant="ghost" onClick={onClose}>
                    {t('common.cancel')}
                  </Button>
                  <Button type="button" disabled={busy || initial === null} onClick={handleSave}>
                    {busy && <Loader2 className="h-4 w-4 animate-spin" />}
                    {busy ? t('accounts.edit.saving') : t('accounts.edit.submit')}
                  </Button>
                </div>
              </>
            )}
          </>
        )}
      </div>
    </Modal>
  )
}
