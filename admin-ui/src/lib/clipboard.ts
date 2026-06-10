/**
 * 复制文本到剪贴板，返回是否成功。
 * 优先 Clipboard API；在非安全上下文（如 http 内网部署的网关后台）该 API 不可用，
 * 回退到隐藏 textarea + execCommand('copy')。
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    /* 继续走 execCommand 回退 */
  }
  try {
    const textarea = document.createElement('textarea')
    textarea.value = text
    textarea.setAttribute('readonly', '')
    // 移出视口避免闪烁，但保持可选中
    textarea.style.position = 'fixed'
    textarea.style.opacity = '0'
    document.body.appendChild(textarea)
    textarea.select()
    const ok = document.execCommand('copy')
    document.body.removeChild(textarea)
    return ok
  } catch {
    return false
  }
}
