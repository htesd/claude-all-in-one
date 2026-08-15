import { useEffect, useState } from 'react'

import { Button } from '@/components/ui/button'
import { ErrorNote } from '@/components/ui/error-note'
import { Modal } from '@/components/ui/modal'
import { useI18n } from '@/lib/i18n'

import { useSetAccountOnDemand } from '../hooks'
import { formatUsd, onDemandState } from '../lib'
import type { AccountRow, OnDemandQuota } from '../types'

/** 客户端的预设档（按订阅档位）；这里取并集做快捷按钮，仍允许手填任意值。 */
const PRESETS = [20, 50, 75, 100, 150, 300]

interface OnDemandDialogProps {
  open: boolean
  /** 目标账号；open 为 true 时必有值。 */
  row: AccountRow | null
  /** 当前超额快照（来自 runtime 的配额缓存）；未查到时为 null。 */
  current: OnDemandQuota | null | undefined
  onClose: () => void
}

/**
 * 超额（on-demand / usage-based）额度设置弹窗。
 *
 * ⚠️ 这是**写**操作：改的是上游账号的计费设置。开启后套餐内额度用尽会继续走量并
 * 产生**真实费用**，所以弹窗里明确写出这一点，而不是只给一个输入框。
 *
 * 未绑支付方式的号上游会直接拒（`Payment method required`）—— 该原文由后端透传到
 * ErrorNote，运维据此知道要去 cursor.com/dashboard 绑卡，而不是怀疑面板坏了。
 */
export function OnDemandDialog({ open, row, current, onClose }: OnDemandDialogProps) {
  const { t } = useI18n()
  const setOnDemand = useSetAccountOnDemand()
  // 输入框存字符串：空串与 "0" 语义不同（前者=没填，后者=显式关闭）。
  const [value, setValue] = useState('')

  // 每次打开时用当前上限预填（未开启/不限额则留空），并清掉上一次的错误。
  useEffect(() => {
    if (!open) return
    setValue(current?.limit != null && current.limit > 0 ? String(current.limit) : '')
    setOnDemand.reset()
    // setOnDemand 每次渲染都是新对象，不入依赖（只在开关/目标变化时重置）。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, row?.account_id])

  const trimmed = value.trim().replace(/^\$/, '').trim()
  const parsed = trimmed === '' ? null : Number(trimmed)
  const invalid =
    parsed !== null && (!Number.isFinite(parsed) || parsed < 0 || !Number.isInteger(parsed))

  const submit = (limitUsd: number | null) => {
    if (!row) return
    setOnDemand.mutate(
      { id: row.account_id, limitUsd },
      { onSuccess: () => onClose() },
    )
  }

  const state = current ? onDemandState(current) : null

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={`${t('accounts.onDemand.title')} · ${row?.account_id ?? ''}`}
      className="max-w-md"
    >
      {/* 当前状态：已用超额 / 上限。运维改额度前要先看到现状。 */}
      <div className="mt-3 rounded-lg bg-muted/50 px-3 py-2 text-sm">
        {current == null ? (
          <span className="text-muted-foreground">{t('accounts.onDemand.unknown')}</span>
        ) : state === 'off' ? (
          <span className="text-muted-foreground">{t('accounts.onDemand.currentOff')}</span>
        ) : (
          <span className="tabular-nums">
            {t('accounts.onDemand.currentUsed')}{' '}
            <span className="font-medium">{formatUsd(current.used)}</span>
            <span className="text-muted-foreground">
              {' / '}
              {state === 'unlimited'
                ? t('accounts.onDemand.unlimited')
                : formatUsd(current.limit ?? 0)}
            </span>
          </span>
        )}
      </div>

      <p className="mt-3 text-xs text-muted-foreground">{t('accounts.onDemand.costWarning')}</p>

      {/* 快捷档 */}
      <div className="mt-4">
        <span className="text-xs font-medium">{t('accounts.onDemand.presets')}</span>
        <div className="mt-1.5 flex flex-wrap gap-1.5">
          {PRESETS.map((p) => (
            <Button
              key={p}
              variant="outline"
              size="sm"
              disabled={setOnDemand.isPending}
              onClick={() => setValue(String(p))}
            >
              ${p}
            </Button>
          ))}
        </div>
      </div>

      {/* 自定义额度 */}
      <label className="mt-4 block">
        <span className="text-xs font-medium">{t('accounts.onDemand.limitLabel')}</span>
        <input
          type="text"
          inputMode="numeric"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder={t('accounts.onDemand.limitPlaceholder')}
          disabled={setOnDemand.isPending}
          className="mt-1 w-full rounded-lg border border-black/10 bg-transparent px-3 py-1.5 text-sm outline-none focus-visible:ring-2 focus-visible:ring-ring/50 disabled:opacity-50 dark:border-white/10"
        />
      </label>
      {invalid && (
        <p className="mt-1 text-xs text-destructive">{t('accounts.onDemand.invalid')}</p>
      )}

      {setOnDemand.isError && (
        <div className="mt-3">
          <ErrorNote error={setOnDemand.error} />
        </div>
      )}

      <div className="mt-5 flex items-center justify-between gap-2">
        {/* 关闭超额：与「设额度」分开成独立动作，避免运维靠"填 0"猜语义。 */}
        <Button
          variant="outline"
          size="sm"
          disabled={setOnDemand.isPending || state === 'off'}
          onClick={() => submit(0)}
        >
          {t('accounts.onDemand.disable')}
        </Button>
        <span className="flex items-center gap-2">
          <Button variant="ghost" size="sm" onClick={onClose} disabled={setOnDemand.isPending}>
            {t('common.cancel')}
          </Button>
          <Button
            size="sm"
            disabled={setOnDemand.isPending || invalid || parsed === null}
            onClick={() => submit(parsed)}
          >
            {setOnDemand.isPending ? t('accounts.onDemand.saving') : t('accounts.onDemand.save')}
          </Button>
        </span>
      </div>
    </Modal>
  )
}
