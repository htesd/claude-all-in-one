import type { ReactNode } from 'react'
import { Navigate } from 'react-router-dom'

import { getToken } from '@/lib/api'

/** Redirect to /login when no admin token is stored. */
export function RequireAuth({ children }: { children: ReactNode }) {
  if (!getToken()) {
    return <Navigate to="/login" replace />
  }
  return <>{children}</>
}
