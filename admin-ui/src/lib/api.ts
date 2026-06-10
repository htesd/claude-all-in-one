import axios from 'axios'

export const TOKEN_STORAGE_KEY = 'kiroGwAdminToken'

export function getToken(): string | null {
  try {
    return localStorage.getItem(TOKEN_STORAGE_KEY)
  } catch {
    return null
  }
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_STORAGE_KEY, token)
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_STORAGE_KEY)
}

/** Shared admin API client. Base path matches the gateway's mount point. */
export const api = axios.create({
  baseURL: '/admin/api',
  timeout: 30_000,
})

api.interceptors.request.use((config) => {
  const token = getToken()
  if (token) {
    config.headers.set('x-api-key', token)
  }
  return config
})

api.interceptors.response.use(
  (response) => response,
  (error: unknown) => {
    if (axios.isAxiosError(error) && error.response?.status === 401) {
      clearToken()
      const loginUrl = `${import.meta.env.BASE_URL}login`
      if (!window.location.pathname.startsWith(loginUrl)) {
        window.location.assign(loginUrl)
      }
    }
    return Promise.reject(error)
  },
)

interface ApiErrorEnvelope {
  type?: string
  error?: { message?: string }
}

/** Extract a human-readable message from the gateway error envelope or transport error. */
export function extractErrorMessage(error: unknown): string {
  if (axios.isAxiosError(error)) {
    const data = error.response?.data as ApiErrorEnvelope | undefined
    if (data?.error?.message) return data.error.message
    if (error.response) return `HTTP ${error.response.status}`
    return error.message
  }
  return error instanceof Error ? error.message : String(error)
}

export function isUnauthorizedError(error: unknown): boolean {
  return axios.isAxiosError(error) && error.response?.status === 401
}

/** 取 HTTP 状态码（非 axios 错误返回 undefined），用于 409/400 的友好文案映射。 */
export function getErrorStatus(error: unknown): number | undefined {
  return axios.isAxiosError(error) ? error.response?.status : undefined
}

interface PingResponse {
  ok?: boolean
  role?: string
}

/**
 * Validate an admin token against the gateway.
 * Uses a raw axios call (not the shared instance) so the 401 redirect
 * interceptor does not fire while the user is still on the login page.
 */
export async function pingAdmin(token: string): Promise<boolean> {
  const response = await axios.get<PingResponse>('/admin/api/ping', {
    headers: { 'x-api-key': token },
    timeout: 15_000,
  })
  return response.data?.ok === true
}
