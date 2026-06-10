// @vitest-environment happy-dom
import type { InternalAxiosRequestConfig } from 'axios'
import { beforeEach, describe, expect, it } from 'vitest'

import { api, TOKEN_STORAGE_KEY } from './api'

/** 经 mock adapter 发一次请求,捕获拦截器加工后的 x-api-key 请求头(不出网)。 */
async function captureApiKeyHeader(): Promise<string | undefined> {
  let seen: string | undefined
  await api.get('/ping', {
    adapter: async (config: InternalAxiosRequestConfig) => {
      const value = config.headers.get('x-api-key')
      seen = typeof value === 'string' ? value : undefined
      return { data: {}, status: 200, statusText: 'OK', headers: {}, config }
    },
  })
  return seen
}

describe('admin api 拦截器与会话级密钥的集成', () => {
  beforeEach(() => {
    sessionStorage.clear()
    localStorage.clear()
  })

  it('sessionStorage 里的密钥自动落到每个请求的 x-api-key', async () => {
    sessionStorage.setItem(TOKEN_STORAGE_KEY, 'admt-secret')
    expect(await captureApiKeyHeader()).toBe('admt-secret')
  })

  it('localStorage 遗留旧密钥不进请求头(不迁移),且被清扫', async () => {
    localStorage.setItem(TOKEN_STORAGE_KEY, 'legacy-plaintext')
    expect(await captureApiKeyHeader()).toBeUndefined()
    expect(localStorage.getItem(TOKEN_STORAGE_KEY)).toBeNull()
  })
})
