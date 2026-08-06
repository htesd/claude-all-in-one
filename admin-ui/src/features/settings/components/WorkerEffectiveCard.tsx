import { AlertTriangle, CheckCircle2, XCircle } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import type { SystemSettings, WorkerSettingsView } from '../types'

/**
 * 同步被视为「新鲜」的秒数上限。
 *
 * worker 轮询周期是 30s，但每轮还要先跑 `sync_accounts_from_db()`（且要和 `/sync`
 * 抢同一把锁），账号多时一轮明显超过 30s 很正常。取 90s 是留三倍余量 ——
 * 这个阈值宁可迟报也不能误报：一个天天变红的健康 worker 会让整张卡失去可信度
 * （对抗审查 Skeptic#5）。
 */
const FRESH_SECS = 90

/** 字段的中文名。没列到的字段照样参与比对，只是显示原始 key。 */
const LABELS: Record<string, string> = {
  cache_floor_ratio: '缓存下限',
  cache_cap_ratio: '缓存上限',
  cache_read_multiplier: '缓存倍率',
  cache_sim_ttl_secs: '缓存模拟 TTL',
  cache_max_sessions: '最大缓存会话',
  max_failures: '连续失败上限',
  rate_limit_cooldown_secs: '限流冷却',
  suspended_cooldown_secs: '封禁冷却',
  affinity_ttl_secs: '会话亲和 TTL',
}

/** 浮点比较：JSON 往返后 0.9 可能变成 0.9000000000000001。 */
const same = (a: unknown, b: unknown) =>
  typeof a === 'number' && typeof b === 'number' ? Math.abs(a - b) < 1e-9 : a === b

/**
 * 「worker 到底在用什么值」。
 *
 * ## 为什么要有这张卡
 *
 * 这一页上其余所有输入框显示的都是**应然值**：库里的 overlay 叠上 YAML 基线算出来的。
 * 而真正决定计费和调度的是各 worker 自己 30s 轮询、应用之后的**实然值**。两者之间有
 * 两处**静默**失败：
 *
 * 1. overlay 解析失败 → worker 每 30s 跳过一轮并保持旧配置，只留一行日志；
 * 2. 本版本不认识的字段 → 被无声忽略（该 worker 镜像比写库的那个旧时发生），
 *    表现为「别的设置都生效，就这一个不生效」。
 *
 * 在这张卡出现之前，这两种情况在面板上与「保存成功」完全无法区分 —— 只能 SSH 上去
 * 翻库、或者盯着计费数据等几分钟看有没有跳变。这就是「我保存了不生效」查不下去的原因。
 *
 * ## 比对范围
 *
 * 逐字段比的是 worker 回报的**全部** `effective` 键，而不是一份手写清单 ——
 * 手写清单漏掉的字段会得到「一致」这个错误结论，而漏掉的恰恰是没人想到的那个
 * （对抗审查 Skeptic#9）。
 */
export function WorkerEffectiveCard({
  workers,
  desired,
}: {
  workers?: WorkerSettingsView[]
  desired?: SystemSettings
}) {
  if (!workers || workers.length === 0) return null

  return (
    <Card>
      <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-2">
        <CardTitle className="text-base">worker 实际生效值</CardTitle>
        <span className="text-xs text-muted-foreground">
          上面填的是<b>要设成什么</b>，这里是各 worker <b>正在用什么</b>。刚保存后最多 30 秒才一致
        </span>
      </CardHeader>
      <CardContent className="space-y-3">
        {workers.map((w) => {
          const s = w.settings
          // age_secs < 0 有两种含义：-1 = 从未成功同步；其余负数 = 系统时钟被回拨
          // （NTP 校时）。后者配置其实在生效，不该甩一句「从未同步」吓人。
          const neverSynced = !!s && s.applied_at === 0
          const clockSkew = !!s && s.age_secs < 0 && !neverSynced
          const stale = !!s && !clockSkew && (neverSynced || s.age_secs > FRESH_SECS)

          // 比对 worker 回报的每一个字段（只比 desired 里也有的那些）。
          const drift = Object.keys(s?.effective ?? {}).filter(
            (k) =>
              desired != null &&
              k in desired &&
              !same(s?.effective?.[k], (desired as unknown as Record<string, unknown>)[k]),
          )

          // provider 不热应用 provider 级设置（dario）：那半边的「一致」不成立。
          const providerCold = !!s && s.provider_hot === false
          // 「刚保存、还没轮询到」是每次日常操作后的必经状态，不该报红 ——
          // 每次保存都变红等于训练用户忽略红色，与 FRESH_SECS 反对的误报同病
          //（对抗审查 Minimalist#6）。同步新鲜时的 drift 只标黄「收敛中」。
          const converging = drift.length > 0 && !stale
          const bad =
            !w.online || !!w.stale_image || !!s?.error || stale || providerCold ||
            !!s?.unknown?.length || (drift.length > 0 && stale)

          return (
            <div key={`${w.group}#${w.instance}`} className="rounded-2xl border p-3">
              <div className="flex flex-wrap items-center gap-2">
                {bad ? (
                  <XCircle className="h-4 w-4 text-destructive" />
                ) : converging ? (
                  <AlertTriangle className="h-4 w-4 text-amber-600 dark:text-amber-400" />
                ) : (
                  <CheckCircle2 className="h-4 w-4 text-emerald-600 dark:text-emerald-400" />
                )}
                <b className="text-sm">worker {w.instance}</b>
                <Badge variant="muted">{w.group}</Badge>
                {!w.online && <Badge variant="destructive">离线</Badge>}
                {w.stale_image && <Badge variant="destructive">镜像过旧</Badge>}
                {s && !clockSkew && (
                  <span className={`text-xs ${stale ? 'text-destructive' : 'text-muted-foreground'}`}>
                    {neverSynced ? '启动后从未成功同步' : `${s.age_secs} 秒前同步`}
                  </span>
                )}
                {clockSkew && (
                  <span className="text-xs text-amber-700 dark:text-amber-300">
                    系统时钟异常，同步时间不可读
                  </span>
                )}
              </div>

              {/* 在线但没有 settings 字段 = 这个 worker 的镜像还不带回显。
                  它对新增设置字段是**只字不认**的，所以任何「一致」结论都不成立。 */}
              {w.stale_image && (
                <p className="mt-2 rounded-lg bg-destructive/10 px-2 py-1 text-xs text-destructive">
                  这个 worker 没有回报实际生效值（多半是镜像太旧，也可能是端口被别的服务占了）
                  —— 无法确认设置对它是否生效；若确为旧镜像，它还会静默忽略本版本新增的设置字段。
                  先确认端口对应的确实是这个 worker，再用当前镜像重建它。
                </p>
              )}

              {providerCold && (
                <p className="mt-2 rounded-lg bg-destructive/10 px-2 py-1 text-xs text-destructive">
                  这个 worker 的 provider 不热应用 <b>provider 级设置</b>（缓存计费、图像压缩、
                  实验开关）—— 这些改完必须<b>重启该 worker</b> 才生效，下面显示的是「算出来的值」
                  而不是它真正在用的值。调度类参数（冷却、失败上限等）不受影响，一直是热的。
                </p>
              )}

              {/* 同步报错：配置已经僵住。库里 JSON 恢复可解析后下一轮就自愈，不必重启。 */}
              {s?.error && (
                <p className="mt-2 flex items-start gap-1.5 break-words rounded-lg bg-destructive/10 px-2 py-1 text-xs text-destructive">
                  <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  <span>
                    同步失败，配置停在上一次成功的值上（每 30 秒重试，库里的设置恢复正常后会自动追上）：
                    {s.error}
                  </span>
                </p>
              )}

              {/* 未知字段 = 这个 worker 的镜像比写库的那个旧。 */}
              {!!s?.unknown?.length && (
                <p className="mt-2 break-words rounded-lg bg-amber-500/10 px-2 py-1 text-xs text-amber-700 dark:text-amber-300">
                  这些字段本 worker 不认识、已被忽略（说明它的镜像比后台旧，需重建该 worker）：
                  {s.unknown.join('、')}
                </p>
              )}

              {drift.length > 0 ? (
                <div className="mt-2 space-y-1">
                  {drift.map((k) => (
                    <div key={k} className="text-xs">
                      <span className="text-muted-foreground">{LABELS[k] ?? k}：</span>
                      <span className={converging ? 'text-amber-700 dark:text-amber-300' : 'text-destructive'}>
                        worker 在用 <b>{String(s?.effective?.[k] ?? '—')}</b>
                      </span>
                      <span className="text-muted-foreground">
                        {' '}
                        / 你设的是{' '}
                        <b>{String((desired as unknown as Record<string, unknown>)?.[k] ?? '—')}</b>
                      </span>
                    </div>
                  ))}
                  <p className="text-[11px] text-muted-foreground">
                    刚保存的话属正常，等下一轮同步（≤30 秒）再看；一直不消失才是真没生效。
                  </p>
                </div>
              ) : (
                w.online &&
                !w.stale_image &&
                !s?.error &&
                !stale &&
                !providerCold &&
                !s?.unknown?.length && (
                  <p className="mt-2 text-xs text-muted-foreground">
                    回报的 {Object.keys(s?.effective ?? {}).length} 项全部与设置一致
                    {s?.effective?.cache_floor_ratio != null && (
                      <>
                        {' · '}缓存下限{' '}
                        <b className="text-foreground">{String(s.effective.cache_floor_ratio)}</b>
                      </>
                    )}
                  </p>
                )
              )}
            </div>
          )
        })}
      </CardContent>
    </Card>
  )
}
