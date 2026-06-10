// @vitest-environment happy-dom
import { beforeEach, describe, expect, it, vi } from 'vitest'

import { clearToken, getToken, setToken, TOKEN_STORAGE_KEY } from './token-storage'

describe('admin token storage(公网部署:仅会话级,不落盘)', () => {
  beforeEach(() => {
    sessionStorage.clear()
    localStorage.clear()
    vi.unstubAllGlobals()
  })

  it('setToken 只写 sessionStorage,绝不写 localStorage', () => {
    setToken('admt-secret')
    expect(sessionStorage.getItem(TOKEN_STORAGE_KEY)).toBe('admt-secret')
    expect(localStorage.getItem(TOKEN_STORAGE_KEY)).toBeNull()
  })

  it('getToken 读 sessionStorage', () => {
    sessionStorage.setItem(TOKEN_STORAGE_KEY, 'admt-secret')
    expect(getToken()).toBe('admt-secret')
  })

  it('getToken 无密钥时返回 null', () => {
    expect(getToken()).toBeNull()
  })

  it('clearToken 清掉 sessionStorage 里的密钥', () => {
    sessionStorage.setItem(TOKEN_STORAGE_KEY, 'admt-secret')
    clearToken()
    expect(sessionStorage.getItem(TOKEN_STORAGE_KEY)).toBeNull()
  })

  it('旧版遗留在 localStorage 的明文密钥:getToken 不迁移、就地清扫(强制重登一次)', () => {
    localStorage.setItem(TOKEN_STORAGE_KEY, 'legacy-plaintext')
    expect(getToken()).toBeNull()
    expect(localStorage.getItem(TOKEN_STORAGE_KEY)).toBeNull()
    expect(sessionStorage.getItem(TOKEN_STORAGE_KEY)).toBeNull()
  })

  it('setToken / clearToken 同样清扫 localStorage 遗留密钥', () => {
    localStorage.setItem(TOKEN_STORAGE_KEY, 'legacy-plaintext')
    setToken('new-secret')
    expect(localStorage.getItem(TOKEN_STORAGE_KEY)).toBeNull()

    localStorage.setItem(TOKEN_STORAGE_KEY, 'legacy-plaintext')
    clearToken()
    expect(localStorage.getItem(TOKEN_STORAGE_KEY)).toBeNull()
  })

  it('存储不可用(隐私模式抛异常)时 getToken 安全返回 null', () => {
    vi.stubGlobal('sessionStorage', {
      getItem() {
        throw new Error('storage disabled')
      },
    })
    expect(getToken()).toBeNull()
  })
})
