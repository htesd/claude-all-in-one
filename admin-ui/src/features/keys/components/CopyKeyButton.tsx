import { useEffect, useRef, useState } from 'react'
import { Check, Copy, X } from 'lucide-react'

import { copyText } from '@/lib/clipboard'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

type CopyState = 'idle' | 'copied' | 'failed'

/** 复制完整 key 的小图标按钮，复制结果用图标短暂反馈（成功对勾 / 失败叉）。 */
export function CopyKeyButton({ value, className }: { value: string; className?: string }) {
  const { t } = useI18n()
  const [state, setState] = useState<CopyState>('idle')
  const timerRef = useRef<number | null>(null)

  // 卸载时清掉回弹定时器，避免 setState 落在已卸载组件上
  useEffect(
    () => () => {
      if (timerRef.current !== null) window.clearTimeout(timerRef.current)
    },
    [],
  )

  const handleCopy = async () => {
    const ok = await copyText(value)
    setState(ok ? 'copied' : 'failed')
    if (timerRef.current !== null) window.clearTimeout(timerRef.current)
    timerRef.current = window.setTimeout(() => setState('idle'), 1500)
  }

  const Icon = state === 'copied' ? Check : state === 'failed' ? X : Copy
  const title =
    state === 'copied'
      ? t('keys.action.copied')
      : state === 'failed'
        ? t('keys.action.copyFailed')
        : t('keys.action.copy')

  return (
    <button
      type="button"
      onClick={() => void handleCopy()}
      title={title}
      aria-label={title}
      className={cn(
        'inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-muted-foreground transition-colors hover:bg-black/5 hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50 dark:hover:bg-white/10',
        state === 'copied' && 'text-success hover:text-success',
        state === 'failed' && 'text-destructive hover:text-destructive',
        className,
      )}
    >
      <Icon className="h-3.5 w-3.5" />
    </button>
  )
}
