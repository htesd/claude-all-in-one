/**
 * 管理密钥的会话级存储(公网部署安全要求):
 * - 只存 sessionStorage —— 关浏览器/关标签页即失效,密钥不长期落盘;
 * - 历史版本曾存 localStorage(明文持久化),三个入口都顺手清扫遗留值,
 *   且**不迁移**——旧密钥一律作废,强制重输一次。
 */
export const TOKEN_STORAGE_KEY = 'caioAdminToken'

/** 清扫旧版遗留在 localStorage 的明文密钥(隐私模式下存储可能抛异常,吞掉)。 */
function purgeLegacyToken(): void {
  try {
    localStorage.removeItem(TOKEN_STORAGE_KEY)
  } catch {
    // 存储不可用时无可清扫
  }
}

export function getToken(): string | null {
  purgeLegacyToken()
  try {
    return sessionStorage.getItem(TOKEN_STORAGE_KEY)
  } catch {
    return null
  }
}

export function setToken(token: string): void {
  purgeLegacyToken()
  sessionStorage.setItem(TOKEN_STORAGE_KEY, token)
}

export function clearToken(): void {
  purgeLegacyToken()
  try {
    sessionStorage.removeItem(TOKEN_STORAGE_KEY)
  } catch {
    // 存储不可用时无可清除
  }
}
