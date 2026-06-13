import { useState, type ComponentType } from 'react'
import { AnimatePresence, motion } from 'framer-motion'
import {
  BarChart3,
  ChevronRight,
  FolderKanban,
  KeyRound,
  Languages,
  LayoutDashboard,
  LogOut,
  Moon,
  ScrollText,
  Settings,
  Sun,
  Users,
  Zap,
  type LucideIcon,
} from 'lucide-react'
import { useLocation, useNavigate } from 'react-router-dom'

import { clearToken } from '@/lib/api'
import { useI18n, type I18nKey } from '@/lib/i18n'
import { useTheme } from '@/lib/theme'
import { cn } from '@/lib/utils'

const COLLAPSE_STORAGE_KEY = 'caioSidebarCollapsed'

interface NavItem {
  to: string
  labelKey: I18nKey
  icon: LucideIcon
}

const navItems: NavItem[] = [
  { to: '/', labelKey: 'nav.dashboard', icon: LayoutDashboard },
  { to: '/usage', labelKey: 'nav.usage', icon: BarChart3 },
  { to: '/accounts', labelKey: 'nav.accounts', icon: Users },
  { to: '/keys', labelKey: 'nav.apiKeys', icon: KeyRound },
  { to: '/groups', labelKey: 'nav.groups', icon: FolderKanban },
  { to: '/settings', labelKey: 'nav.settings', icon: Settings },
  { to: '/logs', labelKey: 'nav.logs', icon: ScrollText },
]

function readInitialCollapsed(): boolean {
  try {
    return localStorage.getItem(COLLAPSE_STORAGE_KEY) === '1'
  } catch {
    return false
  }
}

interface FooterButtonProps {
  icon: ComponentType<{ className?: string }>
  label: string
  collapsed: boolean
  onClick: () => void
}

function FooterButton({ icon: Icon, label, collapsed, onClick }: FooterButtonProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={label}
      className={cn(
        'w-full flex items-center rounded-xl text-xs font-medium text-white/55 transition-all hover:bg-white/10 hover:text-white focus:outline-none focus-visible:ring-2 focus-visible:ring-acid/50',
        collapsed ? 'justify-center p-2.5' : 'gap-3 px-3 py-2',
      )}
    >
      <Icon className="h-4 w-4 shrink-0" />
      {!collapsed && <span className="whitespace-nowrap">{label}</span>}
    </button>
  )
}

export function Sidebar() {
  const [collapsed, setCollapsed] = useState(readInitialCollapsed)
  const { t, lang, setLang } = useI18n()
  const { dark, toggleTheme } = useTheme()
  const navigate = useNavigate()
  const location = useLocation()

  const toggleCollapsed = () => {
    setCollapsed((prev) => {
      const next = !prev
      try {
        localStorage.setItem(COLLAPSE_STORAGE_KEY, next ? '1' : '0')
      } catch {
        /* ignore storage errors */
      }
      return next
    })
  }

  const handleLogout = () => {
    clearToken()
    navigate('/login', { replace: true })
  }

  const isActive = (to: string) =>
    to === '/' ? location.pathname === '/' : location.pathname.startsWith(to)

  return (
    <motion.aside
      initial={false}
      animate={{ width: collapsed ? 64 : 240 }}
      transition={{ type: 'spring', stiffness: 320, damping: 30 }}
      className="glass-sidebar flex h-full shrink-0 flex-col overflow-hidden border-r border-white/10"
    >
      {/* Wordmark */}
      <div
        className={cn(
          'flex h-20 items-center overflow-hidden border-b border-white/10',
          collapsed ? 'justify-center px-2' : 'px-5',
        )}
      >
        {collapsed ? (
          <span
            className="flex h-9 w-9 shrink-0 items-center justify-center rounded-xl bg-acid"
            title={t('app.title')}
          >
            <Zap className="h-5 w-5 text-ink" />
          </span>
        ) : (
          <AnimatePresence initial={false}>
            <motion.div
              key="wordmark"
              initial={{ opacity: 0, x: -10 }}
              animate={{ opacity: 1, x: 0 }}
              transition={{ duration: 0.2 }}
              className="min-w-0"
              title={t('app.title')}
            >
              <div className="font-display text-[26px] font-black leading-none tracking-[-0.06em] text-white">
                CA<span className="text-acid">IO</span>
              </div>
              <div className="mt-1.5 whitespace-nowrap text-[10px] font-semibold uppercase tracking-[0.28em] text-white/35">
                Admin Console
              </div>
            </motion.div>
          </AnimatePresence>
        )}
      </div>

      {/* Menu items */}
      <nav className="flex-1 space-y-1.5 overflow-y-auto px-2.5 py-4">
        {navItems.map((item) => {
          const Icon = item.icon
          const active = isActive(item.to)
          const label = t(item.labelKey)
          return (
            <button
              key={item.to}
              type="button"
              onClick={() => navigate(item.to)}
              className={cn(
                'group relative w-full flex items-center overflow-hidden rounded-xl text-sm font-medium transition-all focus:outline-none focus-visible:ring-2 focus-visible:ring-acid/50',
                active
                  ? 'text-ink'
                  : 'text-white/60 hover:bg-white/10 hover:text-white',
                collapsed ? 'justify-center p-2.5' : 'gap-3 px-4 py-2.5',
              )}
              title={collapsed ? label : undefined}
            >
              {/* Active pill: acid block, the brand signature */}
              {active && (
                <motion.span
                  layoutId="sidebar-active-pill"
                  className="absolute inset-0 rounded-xl bg-acid"
                  transition={{ type: 'spring', stiffness: 380, damping: 32 }}
                />
              )}
              <Icon className={cn('relative z-10 h-5 w-5 shrink-0', active && 'text-ink')} />
              <AnimatePresence initial={false}>
                {!collapsed && (
                  <motion.span
                    key="label"
                    initial={{ opacity: 0, x: -8 }}
                    animate={{ opacity: 1, x: 0 }}
                    exit={{ opacity: 0, x: -8 }}
                    transition={{ duration: 0.15 }}
                    className={cn(
                      'relative z-10 whitespace-nowrap',
                      active && 'font-semibold text-ink',
                    )}
                  >
                    {label}
                  </motion.span>
                )}
              </AnimatePresence>
            </button>
          )
        })}
      </nav>

      {/* Footer: theme / language / logout */}
      <div className="space-y-1 border-t border-white/10 p-2">
        <FooterButton
          icon={dark ? Sun : Moon}
          label={dark ? t('theme.toLight') : t('theme.toDark')}
          collapsed={collapsed}
          onClick={toggleTheme}
        />
        <FooterButton
          icon={Languages}
          label={lang === 'zh' ? 'English' : '中文'}
          collapsed={collapsed}
          onClick={() => setLang(lang === 'zh' ? 'en' : 'zh')}
        />
        <FooterButton
          icon={LogOut}
          label={t('nav.logout')}
          collapsed={collapsed}
          onClick={handleLogout}
        />
      </div>

      {/* Collapse toggle */}
      <div className="border-t border-white/10 p-2">
        <button
          type="button"
          onClick={toggleCollapsed}
          title={collapsed ? t('nav.expand') : t('nav.collapse')}
          className="group w-full flex items-center justify-center gap-2 overflow-hidden rounded-xl px-3 py-2 text-sm text-white/55 transition-all hover:bg-white/10 hover:text-white focus:outline-none focus-visible:ring-2 focus-visible:ring-acid/50"
        >
          <motion.div
            animate={{ rotate: collapsed ? 0 : 180 }}
            transition={{ duration: 0.3, ease: [0.4, 0, 0.2, 1] }}
            className="shrink-0"
          >
            <ChevronRight className="h-4 w-4" />
          </motion.div>
          <AnimatePresence initial={false}>
            {!collapsed && (
              <motion.span
                key="collapse-label"
                initial={{ opacity: 0, width: 0 }}
                animate={{ opacity: 1, width: 'auto' }}
                exit={{ opacity: 0, width: 0 }}
                transition={{ duration: 0.15 }}
                className="overflow-hidden whitespace-nowrap text-xs"
              >
                {t('nav.collapse')}
              </motion.span>
            )}
          </AnimatePresence>
        </button>
      </div>
    </motion.aside>
  )
}
