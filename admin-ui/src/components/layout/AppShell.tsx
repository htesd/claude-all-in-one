import { AnimatePresence, motion } from 'framer-motion'
import { useLocation, useOutlet } from 'react-router-dom'

import { Sidebar } from './Sidebar'

/** Authenticated app shell: ambient background + glass sidebar + page transitions. */
export function AppShell() {
  const location = useLocation()
  // Snapshot the outlet element so the exiting page keeps rendering its own
  // route content during the AnimatePresence exit transition.
  const outlet = useOutlet()

  return (
    <div className="ambient-bg flex h-screen gap-4 overflow-hidden p-4">
      <Sidebar />
      <main className="page-surface glass-card-subtle flex-1 overflow-y-auto rounded-3xl">
        <AnimatePresence mode="wait" initial={false}>
          <motion.div
            key={location.pathname}
            initial={{ opacity: 0, y: 12 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -8 }}
            transition={{ duration: 0.18, ease: 'easeOut' }}
            className="min-h-full p-6"
          >
            {outlet}
          </motion.div>
        </AnimatePresence>
      </main>
    </div>
  )
}
