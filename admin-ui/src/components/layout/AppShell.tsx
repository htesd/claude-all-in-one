import { AnimatePresence, motion } from 'framer-motion'
import { useLocation, useOutlet } from 'react-router-dom'

import { Sidebar } from './Sidebar'

/** Authenticated app shell: dark flush sidebar + light paper content + page transitions. */
export function AppShell() {
  const location = useLocation()
  // Snapshot the outlet element so the exiting page keeps rendering its own
  // route content during the AnimatePresence exit transition.
  const outlet = useOutlet()

  return (
    <div className="flex h-screen overflow-hidden">
      <Sidebar />
      <main className="flex-1 overflow-y-auto">
        <AnimatePresence mode="wait" initial={false}>
          <motion.div
            key={location.pathname}
            initial={{ opacity: 0, y: 12 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -8 }}
            transition={{ duration: 0.18, ease: 'easeOut' }}
            className="mx-auto min-h-full w-full max-w-[1440px] p-6 md:p-8"
          >
            {outlet}
          </motion.div>
        </AnimatePresence>
      </main>
    </div>
  )
}
