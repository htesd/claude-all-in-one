import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom'

import { AppShell } from '@/components/layout/AppShell'
import { RequireAuth } from '@/features/auth/RequireAuth'
import AccountsPage from '@/pages/AccountsPage'
import ApiKeysPage from '@/pages/ApiKeysPage'
import DashboardPage from '@/pages/DashboardPage'
import GroupsPage from '@/pages/GroupsPage'
import LoginPage from '@/pages/LoginPage'
import RequestLogsPage from '@/pages/RequestLogsPage'
import SettingsPage from '@/pages/SettingsPage'
import UsagePage from '@/pages/UsagePage'

export default function App() {
  return (
    <BrowserRouter basename="/admin">
      <Routes>
        <Route path="/login" element={<LoginPage />} />
        <Route
          element={
            <RequireAuth>
              <AppShell />
            </RequireAuth>
          }
        >
          <Route index element={<DashboardPage />} />
          <Route path="usage" element={<UsagePage />} />
          <Route path="accounts" element={<AccountsPage />} />
          <Route path="keys" element={<ApiKeysPage />} />
          <Route path="groups" element={<GroupsPage />} />
          <Route path="settings" element={<SettingsPage />} />
          <Route path="logs" element={<RequestLogsPage />} />
        </Route>
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </BrowserRouter>
  )
}
