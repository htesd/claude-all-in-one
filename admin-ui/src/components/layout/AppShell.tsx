import { useEffect, useState } from 'react'
import { AnimatePresence, motion } from 'framer-motion'
import { Menu } from 'lucide-react'
import { useLocation, useOutlet } from 'react-router-dom'

import { useI18n } from '@/lib/i18n'
import { useMediaQuery } from '@/lib/useMediaQuery'

import { Sidebar } from './Sidebar'

/** Authenticated app shell: dark flush sidebar + light paper content + page transitions. */
export function AppShell() {
  const location = useLocation()
  // Snapshot the outlet element so the exiting page keeps rendering its own
  // route content during the AnimatePresence exit transition.
  const outlet = useOutlet()
  const { t } = useI18n()
  const isDesktop = useMediaQuery('(min-width: 768px)')
  const [mobileNavOpen, setMobileNavOpen] = useState(false)

  // Close the mobile drawer whenever the route changes (e.g. nav tap, redirect).
  useEffect(() => {
    setMobileNavOpen(false)
  }, [location.pathname])

  // Once we cross into the desktop breakpoint, drop the open state so the drawer
  // never re-appears already-open when the viewport shrinks back to mobile.
  useEffect(() => {
    if (isDesktop) setMobileNavOpen(false)
  }, [isDesktop])

  // While the drawer is open on mobile: Escape closes it, and body scrolling is
  // locked so touch scrolling can't leak through the backdrop (iOS/Android).
  useEffect(() => {
    if (!mobileNavOpen) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMobileNavOpen(false)
    }
    window.addEventListener('keydown', onKey)
    const prevOverflow = document.body.style.overflow
    document.body.style.overflow = 'hidden'
    return () => {
      window.removeEventListener('keydown', onKey)
      document.body.style.overflow = prevOverflow
    }
  }, [mobileNavOpen])

  return (
    <div className="flex h-screen overflow-hidden">
      <Sidebar mobileOpen={mobileNavOpen} onMobileClose={() => setMobileNavOpen(false)} />

      {/* Mobile drawer backdrop (below the sidebar's z-50, above content). */}
      <AnimatePresence>
        {mobileNavOpen && (
          <motion.div
            key="nav-backdrop"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.2 }}
            onClick={() => setMobileNavOpen(false)}
            className="fixed inset-0 z-40 bg-black/50 backdrop-blur-sm md:hidden"
            aria-hidden="true"
          />
        )}
      </AnimatePresence>

      <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
        {/* Mobile top bar: hamburger + wordmark. Hidden once the sidebar is inline. */}
        <header className="glass-sidebar flex h-14 shrink-0 items-center gap-3 border-b border-white/10 px-4 md:hidden">
          <button
            type="button"
            onClick={() => setMobileNavOpen(true)}
            aria-label={t('nav.menu')}
            className="flex h-9 w-9 items-center justify-center rounded-xl text-white/70 transition-all hover:bg-white/10 hover:text-white focus:outline-none focus-visible:ring-2 focus-visible:ring-acid/50"
          >
            <Menu className="h-5 w-5" />
          </button>
          <span className="font-display text-lg font-black leading-none tracking-[-0.04em] text-white">
            CA<span className="text-acid">IO</span>
          </span>
        </header>

        <main className="flex-1 overflow-y-auto">
          <AnimatePresence mode="wait" initial={false}>
            <motion.div
              key={location.pathname}
              initial={{ opacity: 0, y: 12 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, y: -8 }}
              transition={{ duration: 0.18, ease: 'easeOut' }}
              className="mx-auto min-h-full w-full max-w-[1440px] p-4 sm:p-6 md:p-8"
            >
              {outlet}
            </motion.div>
          </AnimatePresence>
        </main>
      </div>
    </div>
  )
}
