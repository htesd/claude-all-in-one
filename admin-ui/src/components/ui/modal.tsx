import { useEffect, type ReactNode } from 'react'
import { AnimatePresence, motion } from 'framer-motion'
import { X } from 'lucide-react'

import { Card } from '@/components/ui/card'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'

interface ModalProps {
  open: boolean
  onClose: () => void
  title: string
  children: ReactNode
  /** 外层容器附加样式（控制宽度等，默认 max-w-md）。 */
  className?: string
}

/** 通用对话框：统一遮罩点击 / Esc 关闭、标题栏与进出场动画。 */
export function Modal({ open, onClose, title, children, className }: ModalProps) {
  const { t } = useI18n()

  // Esc 关闭
  useEffect(() => {
    if (!open) return
    const handleKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', handleKeyDown)
    return () => window.removeEventListener('keydown', handleKeyDown)
  }, [open, onClose])

  return (
    <AnimatePresence>
      {open && (
        <motion.div
          key="modal"
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.15 }}
          className="fixed inset-0 z-50 flex items-center justify-center p-4"
        >
          {/* 遮罩：点击关闭 */}
          <div className="absolute inset-0 bg-black/55" onClick={onClose} />

          <motion.div
            initial={{ scale: 0.96, y: 8 }}
            animate={{ scale: 1, y: 0 }}
            exit={{ scale: 0.96, y: 8 }}
            transition={{ duration: 0.18, ease: [0.4, 0, 0.2, 1] }}
            className={cn('relative w-full max-w-md', className)}
          >
            {/* 卡片限高到视口内(max-h-[88vh])并改为纵向 flex:标题栏固定、内容区超出时自身滚动,
                避免高报文详情等长内容把弹窗撑出浏览器可视范围。 */}
            <Card
              variant="glass-strong"
              className="flex max-h-[88vh] flex-col rounded-3xl p-6"
              role="dialog"
              aria-modal="true"
            >
              <div className="flex shrink-0 items-center justify-between">
                <h2 className="text-lg font-black tracking-[-0.02em]">{title}</h2>
                <button
                  type="button"
                  onClick={onClose}
                  title={t('common.cancel')}
                  className="inline-flex h-7 w-7 items-center justify-center rounded-full text-muted-foreground transition-colors hover:bg-black/5 hover:text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-ring/40 dark:hover:bg-white/10"
                >
                  <X className="h-4 w-4" />
                </button>
              </div>
              {/* min-h-0 让 flex 子项可收缩从而触发滚动;pr-1 给滚动条一点内边距(不用负 margin,
                  避免在其它弹窗里把正文挤进卡片 padding 造成水平溢出/焦点环裁切)。 */}
              <div className="min-h-0 flex-1 overflow-y-auto pr-1">{children}</div>
            </Card>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  )
}
