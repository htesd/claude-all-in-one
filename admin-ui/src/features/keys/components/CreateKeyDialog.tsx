import { useEffect, useState, type FormEvent } from 'react'
import { AnimatePresence, motion } from 'framer-motion'
import { CheckCircle2, ChevronRight, Loader2, X } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { extractErrorMessage } from '@/lib/api'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

import { getErrorStatus } from '../api'
import { useCreateKey } from '../hooks'
import { CUSTOM_KEY_PATTERN, type CreateKeyPayload } from '../types'
import { CopyKeyButton } from './CopyKeyButton'

interface CreateKeyDialogProps {
  open: boolean
  onClose: () => void
}

/**
 * 新建 Key 对话框，两个阶段：
 * 1. 表单态：备注（可选）+ 折叠的自定义 key 输入（留空 = 服务端自动生成 sk-gw-<32hex>）
 * 2. 成功态：展示完整 key + 复制按钮，提示妥善保存
 */
export function CreateKeyDialog({ open, onClose }: CreateKeyDialogProps) {
  const { t } = useI18n()
  const mutation = useCreateKey()

  const [label, setLabel] = useState('')
  const [customOpen, setCustomOpen] = useState(false)
  const [customKey, setCustomKey] = useState('')
  const [error, setError] = useState<string | null>(null)
  /** 创建成功后保存完整 key，非 null 即切到成功确认态。 */
  const [createdKey, setCreatedKey] = useState<string | null>(null)

  // 每次打开都从干净的表单态开始（上一次的成功态/错误不残留）
  useEffect(() => {
    if (open) {
      setLabel('')
      setCustomOpen(false)
      setCustomKey('')
      setError(null)
      setCreatedKey(null)
    }
  }, [open])

  // Esc 关闭
  useEffect(() => {
    if (!open) return
    const handleKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', handleKeyDown)
    return () => window.removeEventListener('keydown', handleKeyDown)
  }, [open, onClose])

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (mutation.isPending) return

    // 自定义 key 按原始输入校验，绝不 trim 改写：尾随空格/全空格输入都会
    // 直接报格式错误，而不是被静默改写成另一个 key 或静默落回自动生成。
    // 只有真正的空字符串（''）才走服务端自动生成。
    if (customKey !== '' && !CUSTOM_KEY_PATTERN.test(customKey)) {
      setError(t('keys.error.invalidKey'))
      return
    }
    setError(null)

    const payload: CreateKeyPayload = {}
    const trimmedLabel = label.trim()
    if (trimmedLabel !== '') payload.label = trimmedLabel
    if (customKey !== '') payload.key = customKey

    mutation.mutate(payload, {
      onSuccess: (row) => setCreatedKey(row.key),
      onError: (err) => {
        // 409 = key 重复，400 = 格式非法，其余透出服务端 message
        const status = getErrorStatus(err)
        if (status === 409) setError(t('keys.error.duplicate'))
        else if (status === 400) setError(t('keys.error.invalidKey'))
        else setError(extractErrorMessage(err))
      },
    })
  }

  return (
    <AnimatePresence>
      {open && (
        <motion.div
          key="create-key-dialog"
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.15 }}
          className="fixed inset-0 z-50 flex items-center justify-center p-4"
        >
          {/* 遮罩：点击关闭 */}
          <div className="absolute inset-0 bg-black/50" onClick={onClose} />

          <motion.div
            initial={{ scale: 0.96, y: 8 }}
            animate={{ scale: 1, y: 0 }}
            exit={{ scale: 0.96, y: 8 }}
            transition={{ duration: 0.18, ease: [0.4, 0, 0.2, 1] }}
            className="relative w-full max-w-md"
          >
            <Card variant="glass-strong" className="p-6" role="dialog" aria-modal="true">
              <div className="flex items-center justify-between">
                <h2 className="text-base font-semibold">
                  {createdKey !== null ? t('keys.create.successTitle') : t('keys.create.title')}
                </h2>
                <button
                  type="button"
                  onClick={onClose}
                  title={t('common.cancel')}
                  className="inline-flex h-7 w-7 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-black/5 hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50 dark:hover:bg-white/10"
                >
                  <X className="h-4 w-4" />
                </button>
              </div>

              {createdKey !== null ? (
                /* 成功确认态：展示完整 key + 复制 */
                <div className="mt-4 space-y-4">
                  <div className="flex items-start gap-2 rounded-xl border bg-input px-3 py-2.5">
                    <code className="min-w-0 flex-1 break-all font-mono text-xs leading-5">
                      {createdKey}
                    </code>
                    <CopyKeyButton value={createdKey} />
                  </div>
                  <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
                    <CheckCircle2 className="h-3.5 w-3.5 shrink-0 text-success" />
                    {t('keys.create.successHint')}
                  </p>
                  <Button onClick={onClose} className="w-full">
                    {t('keys.create.done')}
                  </Button>
                </div>
              ) : (
                <form onSubmit={handleSubmit} className="mt-4 space-y-4">
                  {/* 备注（可选） */}
                  <div className="space-y-1.5">
                    <label
                      htmlFor="create-key-label"
                      className="text-xs font-medium text-muted-foreground"
                    >
                      {t('keys.create.label')}
                    </label>
                    <input
                      id="create-key-label"
                      value={label}
                      onChange={(event) => setLabel(event.target.value)}
                      placeholder={t('keys.create.labelPlaceholder')}
                      autoFocus
                      className="w-full rounded-xl border bg-input px-3 py-2 text-sm text-foreground transition-colors placeholder:text-muted-foreground focus:outline-none"
                    />
                  </div>

                  {/* 自定义 key：默认折叠，展开后留空仍走自动生成 */}
                  <div className="space-y-1.5">
                    <button
                      type="button"
                      onClick={() => setCustomOpen((prev) => !prev)}
                      className="inline-flex items-center gap-1 rounded text-xs font-medium text-muted-foreground transition-colors hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
                    >
                      <ChevronRight
                        className={cn('h-3.5 w-3.5 transition-transform', customOpen && 'rotate-90')}
                      />
                      {t('keys.create.customToggle')}
                    </button>
                    {customOpen && (
                      <div className="space-y-1">
                        <input
                          value={customKey}
                          onChange={(event) => setCustomKey(event.target.value)}
                          placeholder={t('keys.create.customPlaceholder')}
                          spellCheck={false}
                          autoComplete="off"
                          className="w-full rounded-xl border bg-input px-3 py-2 font-mono text-sm text-foreground transition-colors placeholder:text-muted-foreground focus:outline-none"
                        />
                        <p className="text-xs text-muted-foreground">
                          {t('keys.create.customRule')}
                        </p>
                      </div>
                    )}
                  </div>

                  {/* 409 / 400 / 其他错误的内联展示 */}
                  {error !== null && <p className="text-sm text-destructive">{error}</p>}

                  <div className="flex justify-end gap-2 pt-1">
                    <Button variant="ghost" onClick={onClose}>
                      {t('common.cancel')}
                    </Button>
                    <Button type="submit" disabled={mutation.isPending}>
                      {mutation.isPending && <Loader2 className="h-4 w-4 animate-spin" />}
                      {mutation.isPending ? t('keys.create.creating') : t('keys.create.submit')}
                    </Button>
                  </div>
                </form>
              )}
            </Card>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  )
}
