import { useState } from 'react'
import { HeartPulse, Pencil, RefreshCw, Trash2 } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Skeleton } from '@/components/ui/skeleton'
import { TD, TR } from '@/components/ui/table'
import { GroupChip } from '@/features/groups/components/GroupChip'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import { deriveAccountStatus, formatCredits, isQuotaLow } from '../lib'
import type { AccountRow, AccountRuntimeEntry } from '../types'
import { AccountStatusBadge } from './AccountStatusBadge'

/** 行内小图标按钮的统一样式（编辑铅笔 / 删除等）。 */
const iconButtonClass =
  'inline-flex h-6 w-6 items-center justify-center rounded-full text-muted-foreground transition-colors hover:bg-black/5 hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50 disabled:pointer-events-none disabled:opacity-50 dark:hover:bg-white/10'

/** runtime 查询的三态：初载 / 失败（无数据可用）/ 就绪。 */
export type RuntimeQueryState = 'loading' | 'error' | 'ready'

interface AccountTableRowProps {
  row: AccountRow
  /** 按 account_id merge 出的运行态；worker 未上报时为 undefined。 */
  runtime: AccountRuntimeEntry | undefined
  runtimeState: RuntimeQueryState
  /** 分组颜色（来自 /groups），未命中用默认色。 */
  groupColor: string | undefined
  /** 本行是否有进行中的 mutation（按钮置灰防连点）。 */
  busy: boolean
  onToggleDisabled: (row: AccountRow) => void
  onEdit: (row: AccountRow) => void
  onDelete: (id: string) => void
  onReset: (id: string) => void
  onRefresh: (id: string) => void
}

export function AccountTableRow({
  row,
  runtime,
  runtimeState,
  groupColor,
  busy,
  onToggleDisabled,
  onEdit,
  onDelete,
  onReset,
  onRefresh,
}: AccountTableRowProps) {
  const { t } = useI18n()
  const [confirmingDelete, setConfirmingDelete] = useState(false)

  const failureCount = runtime?.status.failure_count ?? 0
  // 救号按钮：worker 在线且账号处于「运行时」禁用（冷却/封禁,非 admin 配置开关）,
  // 或攒了失败计数 —— reset 可立即清掉,不必等冷却自然到期。
  const runtimeReason = runtime?.online === true ? runtime.status.reason : ''
  const canReset =
    runtime?.online === true &&
    ((runtime.status.disabled && runtimeReason !== '' && runtimeReason !== 'config') ||
      failureCount > 0)
  /** 并发列只在 worker 在线上报时显示「空闲/上限」，否则回落到配置值。 */
  const hasLivePermits = runtimeState === 'ready' && runtime !== undefined && runtime.online

  return (
    <TR>
      {/* account_id：代码字体 */}
      <TD>
        <code className="rounded-md bg-muted px-2 py-0.5 font-mono text-xs">
          {row.account_id}
        </code>
      </TD>

      {/* 分组：色 chip */}
      <TD>
        <GroupChip name={row.group_name} color={groupColor} />
      </TD>

      {/* provider */}
      <TD className="text-muted-foreground">{row.provider || '—'}</TD>

      {/* 运行状态徽章（merge 配置 + runtime） */}
      <TD>
        {runtimeState === 'loading' ? (
          <Skeleton className="h-4 w-14" />
        ) : runtimeState === 'error' ? (
          // runtime 加载失败时不妄断「离线」，只按配置展示已停用，其余显示 —
          row.disabled ? (
            <Badge variant="muted">{t('accounts.status.disabled')}</Badge>
          ) : (
            <span className="text-muted-foreground">—</span>
          )
        ) : (
          <AccountStatusBadge status={deriveAccountStatus(row, runtime)} />
        )}
      </TD>

      {/* 积分:剩余/上限(来自 getUsageLimits);null = 后台查询中 */}
      <TD className="text-right">
        {(() => {
          if (runtimeState === 'loading') return <Skeleton className="ml-auto h-4 w-16" />
          const quota = runtime?.online ? runtime.status.quota : undefined
          if (quota === undefined || quota === null) {
            return <span className="text-muted-foreground">—</span>
          }
          const low = isQuotaLow(quota.remaining, quota.limit)
          return (
            <span className="tabular-nums" title={quota.label ?? undefined}>
              <span className={cn('font-medium', low && 'text-destructive')}>
                {formatCredits(quota.remaining)}
              </span>
              <span className="text-muted-foreground"> / {formatCredits(quota.limit)}</span>
            </span>
          )
        })()}
      </TD>

      {/* 并发：在用/上限（runtime）。available_permits 是空闲槽，在用 = 上限 - 空闲，
          故空闲账号显示 0/N(而非反直觉的 N/N)。无 runtime 数据时只显示配置上限。 */}
      <TD className="text-right">
        {runtimeState === 'loading' ? (
          <Skeleton className="ml-auto h-4 w-10" />
        ) : hasLivePermits ? (
          <span
            className="tabular-nums"
            title={t('table.concurrencyHint')}
          >
            {runtime.status.max_concurrency - runtime.status.available_permits}/
            {runtime.status.max_concurrency}
          </span>
        ) : (
          <span className="tabular-nums text-muted-foreground">{row.max_concurrency}</span>
        )}
      </TD>

      {/* 连续失败次数：> 0 才显示 */}
      <TD className="text-right">
        {runtimeState === 'loading' ? (
          <Skeleton className="ml-auto h-4 w-8" />
        ) : failureCount > 0 ? (
          <span className="font-medium tabular-nums text-destructive">{failureCount}</span>
        ) : (
          <span className="text-muted-foreground">—</span>
        )}
      </TD>

      {/* 操作：启停 + 编辑 + 删除（二次确认） */}
      <TD className="text-right">
        {confirmingDelete ? (
          <span className="inline-flex items-center gap-2">
            <span className="text-xs text-muted-foreground">{t('accounts.delete.hint')}</span>
            <Button
              variant="destructive"
              size="sm"
              disabled={busy}
              onClick={() => onDelete(row.account_id)}
            >
              {t('accounts.delete.confirm')}
            </Button>
            <Button variant="ghost" size="sm" onClick={() => setConfirmingDelete(false)}>
              {t('common.cancel')}
            </Button>
          </span>
        ) : (
          <span className="inline-flex items-center gap-1.5">
            {/* 刷新 token 始终可用(与编辑/删除一致)：不依赖 runtime.online——runtime 降级时
                仍要能手动刷新,后端会顺序扇出并回成功/失败/无人持有(审查 Skeptic#5/Minimalist#2)。 */}
            <button
              type="button"
              onClick={() => onRefresh(row.account_id)}
              title={t('accounts.action.refresh')}
              disabled={busy}
              className={cn(iconButtonClass, 'text-primary hover:text-primary')}
            >
              <RefreshCw className="h-3.5 w-3.5" />
            </button>
            {canReset && (
              <button
                type="button"
                onClick={() => onReset(row.account_id)}
                title={t('accounts.action.reset')}
                disabled={busy}
                className={cn(iconButtonClass, 'text-warning hover:text-warning')}
              >
                <HeartPulse className="h-3.5 w-3.5" />
              </button>
            )}
            <Button
              variant="outline"
              size="sm"
              disabled={busy}
              onClick={() => onToggleDisabled(row)}
            >
              {row.disabled ? t('accounts.action.enable') : t('accounts.action.disable')}
            </Button>
            <button
              type="button"
              onClick={() => onEdit(row)}
              title={t('accounts.action.edit')}
              disabled={busy}
              className={iconButtonClass}
            >
              <Pencil className="h-3 w-3" />
            </button>
            <button
              type="button"
              onClick={() => setConfirmingDelete(true)}
              title={t('accounts.action.delete')}
              disabled={busy}
              className={cn(iconButtonClass, 'hover:text-rose-600 dark:hover:text-rose-300')}
            >
              <Trash2 className="h-3.5 w-3.5" />
            </button>
          </span>
        )}
      </TD>
    </TR>
  )
}
