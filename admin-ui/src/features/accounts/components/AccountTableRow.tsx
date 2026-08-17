import { useState } from 'react'
import { HeartPulse, ListChecks, Pencil, RefreshCw, Trash2 } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Skeleton } from '@/components/ui/skeleton'
import { TD, TR } from '@/components/ui/table'
import { GroupChip } from '@/features/groups/components/GroupChip'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import {
  deriveAccountStatus,
  formatCredits,
  formatMarkTtl,
  formatUsd,
  isOnDemandHigh,
  isQuotaLow,
  onDemandState,
  priorityToTier,
  providerTabLabel,
  type QuotaKind,
} from '../lib'
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
  /** 分组名 -> 颜色（来自 /groups）。一个号可属多个组，所以传整张表而不是单个颜色。 */
  groupColors: Map<string, string>
  /** 配额列口径(随所属 tab):'credits'=积分/美元(Kiro·Cursor);'windows'=5h/7d(ccmax/dario);'none'=无配额概念。 */
  quotaKind: QuotaKind
  /** 是否渲染超额（on-demand）列 —— 与表头一致，仅支持的 provider 为 true。 */
  showOnDemand: boolean
  /** 本行是否有进行中的 mutation（按钮置灰防连点）。 */
  busy: boolean
  onToggleDisabled: (row: AccountRow) => void
  onEdit: (row: AccountRow) => void
  onDelete: (id: string) => void
  onReset: (id: string) => void
  onRefresh: (id: string) => void
  /** 打开「查看模型」弹窗（仅 kiro 行会渲染入口，纯本地零上游）。 */
  onViewModels: (row: AccountRow) => void
  /** 打开超额额度设置弹窗。 */
  onEditOnDemand: (row: AccountRow) => void
}

export function AccountTableRow({
  row,
  runtime,
  runtimeState,
  groupColors,
  quotaKind,
  showOnDemand,
  busy,
  onToggleDisabled,
  onEdit,
  onDelete,
  onReset,
  onRefresh,
  onViewModels,
  onEditOnDemand,
}: AccountTableRowProps) {
  const { t } = useI18n()
  const [confirmingDelete, setConfirmingDelete] = useState(false)

  const failureCount = runtime?.status.failure_count ?? 0
  // (账号,模型) 不可用标记:号整体"正常"但被上游逐模型拒绝(INVALID_MODEL_ID)时,
  // 状态徽章旁必须能看出"半死",否则面板上显示正常却不吃流量(2026-08-10 排障实录)。
  const modelMarks = runtime?.online === true ? (runtime.status.model_unavailable ?? []) : []
  // 救号按钮：worker 在线且账号处于「运行时」禁用（冷却/封禁,非 admin 配置开关）,
  // 或攒了失败计数,或挂着模型不可用标记 —— reset 可立即清掉,不必等冷却/TTL 自然到期。
  const runtimeReason = runtime?.online === true ? runtime.status.reason : ''
  const canReset =
    runtime?.online === true &&
    ((runtime.status.disabled && runtimeReason !== '' && runtimeReason !== 'config') ||
      failureCount > 0 ||
      modelMarks.length > 0)
  /** 并发列只在 worker 在线上报时显示「空闲/上限」，否则回落到配置值。 */
  const hasLivePermits = runtimeState === 'ready' && runtime !== undefined && runtime.online
  /**
   * 成员边（决定谁能用它 + 组内排第几）。
   * `undefined` = 旧缓存响应还没带这个字段（**未知**，不是"没有"）——此时显示 —，
   * 绝不能报红"无分组"，那是在拿一次升级窗口期的空档吓运维。
   */
  const memberships = row.groups
  const membershipsUnknown = memberships === undefined
  const highTierCount = (memberships ?? []).filter(
    (m) => priorityToTier(m.priority) === 'high',
  ).length
  const lowTierCount = (memberships ?? []).length - highTierCount
  // 超额快照只有在线 worker 才有（离线时 status 无配额缓存）→ 显示「—」而非假的 0。
  const onDemand = runtime?.online ? runtime.status.quota?.on_demand : undefined
  // 只有 cursor 支持超额（与后端 on_demand_supported() 一致）。混部机器上这一列会
  // 同时出现 kiro 行，它们不该是可点的。
  const onDemandSupported = row.provider === 'cursor'

  return (
    <TR>
      {/* account_id：代码字体 */}
      <TD>
        <code className="rounded-md bg-muted px-2 py-0.5 font-mono text-xs">
          {row.account_id}
        </code>
      </TD>

      {/* 分组：**成员边**（谁能用到它），一个号可属多个组。归属组见编辑弹窗。
          一条边都没有 = 库里有号但永远选不中，必须一眼可见。 */}
      <TD>
        {membershipsUnknown ? (
          <span className="text-muted-foreground">—</span>
        ) : memberships.length === 0 ? (
          <span className="text-xs font-medium text-destructive" title={t('table.groupNoneHint')}>
            {t('table.groupNone')}
          </span>
        ) : (
          <span className="flex flex-wrap items-center gap-1">
            {memberships.map((m) => (
              <GroupChip key={m.name} name={m.name} color={groupColors.get(m.name)} />
            ))}
          </span>
        )}
      </TD>

      {/* provider：走 providerTabLabel,与筛选器选项同一处维护(kiro→Kiro / claude-dario→ccmax / cursor→Cursor)
          CLI 驱动的号在这里挂一枚徽章 —— 同一个 provider 下两种上游形态,
          出问题时第一件要分辨的就是"这号走的哪条路",不该只能进弹窗才看得到。 */}
      <TD className="text-muted-foreground">
        <span className="flex flex-wrap items-center gap-1.5">
          {providerTabLabel(row.provider)}
          {row.driver === 'cli' && (
            <Badge variant="default" title={t('table.driverCliHint')}>
              {t('table.driverCli')}
            </Badge>
          )}
        </span>
      </TD>

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
          <span className="inline-flex flex-wrap items-center gap-1">
            <AccountStatusBadge status={deriveAccountStatus(row, runtime)} />
            {modelMarks.length > 0 && (
              <Badge
                variant="warning"
                title={`${t('accounts.status.modelMarkedHint')}\n${modelMarks
                  .map((m) => `${m.model} · ${formatMarkTtl(m.remaining_secs)}`)
                  .join('\n')}`}
              >
                {t('accounts.status.modelMarked')}×{modelMarks.length}
              </Badge>
            )}
          </span>
        )}
      </TD>

      {/* 配额列:口径随所属 tab。
          - windows(ccmax/dario):5h/7d 滚动窗口利用率%;无数据(未跑过流量/查询中)显示 —
          - credits(Kiro·Cursor):积分/美元剩余/上限。Cursor 额外带 auto(自家模型)/
            api(第三方模型)两条百分比窗口 —— 上游只给 %、不给金额拆分,
            与美元账期并排展示(用户点名要三条用量齐:auto、api、超额)
          - none:订阅制无配额概念、后端不采集,恒为 — */}
      <TD className="text-right">
        {(() => {
          if (runtimeState === 'loading') return <Skeleton className="ml-auto h-4 w-16" />
          if (quotaKind === 'none') {
            return <span className="text-muted-foreground">—</span>
          }
          const quota = runtime?.online ? runtime.status.quota : undefined

          if (quotaKind === 'windows') {
            // ccmax:只展示利用率窗口;Anthropic 无只读用量接口,须先跑过流量才有数。
            if (!quota?.windows || quota.windows.length === 0) {
              return <span className="text-muted-foreground">—</span>
            }
            return (
              <span className="tabular-nums" title={quota.label ?? undefined}>
                {quota.windows.map((w, i) => (
                  <span key={w.label}>
                    {i > 0 && <span className="text-muted-foreground"> · </span>}
                    <span className="text-muted-foreground">{w.label} </span>
                    <span className={cn('font-medium', w.percent_used >= 90 && 'text-destructive')}>
                      {Math.round(w.percent_used)}%
                    </span>
                  </span>
                ))}
              </span>
            )
          }

          // credits(Kiro·Cursor):剩余/上限;Cursor 附带 auto/api 两条百分比窗口。
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
              {/* Cursor 的 auto(自家模型)/api(第三方模型)用量 —— 上游只给 %。
                  缺省(kiro / 旧快照)不渲染,不出空条。 */}
              {quota.windows && quota.windows.length > 0 && (
                <span className="block text-xs">
                  {quota.windows.map((w, i) => (
                    <span key={w.label}>
                      {i > 0 && <span className="text-muted-foreground"> · </span>}
                      <span className="text-muted-foreground">{w.label} </span>
                      <span
                        className={cn('font-medium', w.percent_used >= 90 && 'text-destructive')}
                      >
                        {Math.round(w.percent_used)}%
                      </span>
                    </span>
                  ))}
                </span>
              )}
            </span>
          )
        })()}
      </TD>

      {/* 超额(on-demand)列:「已用超额 / 超额上限」(美元)。
          - 该 provider 不支持(非 cursor)→ 不可点的「—」(点开弹窗只会被后端回 400)
          - 未查到(worker 离线/该号未采集)→ —
          - 未开启 → 灰「关」+ 点击可开
          - 已开启不限额 → 已用 / 不限
          - 已开启有上限 → 已用 / 上限,到 80% 标黄 */}
      {showOnDemand && (
        <TD className="text-right">
          {runtimeState === 'loading' ? (
            <Skeleton className="ml-auto h-4 w-16" />
          ) : !onDemandSupported ? (
            <span className="text-muted-foreground" title={t('accounts.onDemand.unsupported')}>
              —
            </span>
          ) : (
            <button
              type="button"
              onClick={() => onEditOnDemand(row)}
              disabled={busy}
              title={t('accounts.onDemand.editHint')}
              className="tabular-nums underline-offset-2 hover:underline focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50 disabled:pointer-events-none disabled:opacity-50"
            >
              {onDemand == null ? (
                <span className="text-muted-foreground">—</span>
              ) : onDemandState(onDemand) === 'off' ? (
                <span className="text-xs text-muted-foreground">{t('accounts.onDemand.off')}</span>
              ) : (
                <>
                  <span className={cn('font-medium', isOnDemandHigh(onDemand) && 'text-warning')}>
                    {formatUsd(onDemand.used)}
                  </span>
                  <span className="text-muted-foreground">
                    {' / '}
                    {onDemandState(onDemand) === 'unlimited'
                      ? t('accounts.onDemand.unlimited')
                      : formatUsd(onDemand.limit ?? 0)}
                  </span>
                </>
              )}
            </button>
          )}
        </TD>
      )}

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

      {/* 排队开关：runtime 值优先（worker 侧实时），回落到列表行的配置值。
          关着的号显示灰「关」而不是空白 —— 空白会被误读成"这列还没加载"。 */}
      <TD className="text-right">
        {(() => {
          const on = runtime?.online
            ? (runtime.status.queue_enabled ?? row.queue_enabled ?? false)
            : (row.queue_enabled ?? false)
          return on ? (
            <Badge variant="success" title={t('table.queueOnHint')}>
              {t('table.queueOn')}
            </Badge>
          ) : (
            <span className="text-xs text-muted-foreground" title={t('table.queueOffHint')}>
              {t('table.queueOff')}
            </span>
          )
        })()}
      </TD>

      {/* 调度优先级:**按组**两档(高/低) —— 同一个号在 A 组可以是主力、B 组是兜底,
          所以这里只给个汇总,逐组明细在 title 里(以及编辑弹窗)。 */}
      <TD className="text-right">
        {membershipsUnknown || memberships.length === 0 ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <span
            className={cn(highTierCount > 0 && 'font-medium text-primary')}
            title={`${t('table.priorityHint')}\n${memberships
              .map((m) => `${m.name}: ${t(`accounts.priorityTier.${priorityToTier(m.priority)}`)}`)
              .join('\n')}`}
          >
            {[
              highTierCount > 0 ? `${t('accounts.priorityTier.high')}×${highTierCount}` : null,
              lowTierCount > 0 ? `${t('accounts.priorityTier.low')}×${lowTierCount}` : null,
            ]
              .filter(Boolean)
              .join(' · ')}
          </span>
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

      {/* 累计成功/失败：来自账号行的 success_count / failure_count（后端新增，旧数据视为 0）。
          与「连续失败」列不同：这里是生命周期累计值，连续失败列是当前冷却期连续失败计数。 */}
      <TD className="text-right">
        <span className="tabular-nums">
          <span className="text-success">{row.success_count ?? 0}</span>
          <span className="text-muted-foreground"> / </span>
          <span className={cn((row.failure_count ?? 0) > 0 ? 'text-destructive' : 'text-muted-foreground')}>
            {row.failure_count ?? 0}
          </span>
        </span>
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
            {/* 查看模型：仅 kiro(模型清单来自 kiro 静态目录 × 档位 − 标记;
                其它 provider 无此概念,不显示入口)。纯本地零上游,不依赖 runtime.online。 */}
            {row.provider === 'kiro' && (
              <button
                type="button"
                onClick={() => onViewModels(row)}
                title={t('accounts.action.viewModels')}
                disabled={busy}
                className={iconButtonClass}
              >
                <ListChecks className="h-3.5 w-3.5" />
              </button>
            )}
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
