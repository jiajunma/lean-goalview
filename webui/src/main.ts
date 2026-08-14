// Host page for the official @leanprover/infoview, driven over a WebSocket to
// the lean-goalview proxy. We implement EditorApi by faithfully relaying LSP
// requests/notifications to the proxy (which multiplexes them onto lake serve)
// and drive InfoviewApi from the proxy's pushes.
//
// Wire protocol (both directions, JSON lines over /ws):
//   client → proxy:
//     {t:"req", seq, uri, method, params}   expect {t:"res", seq, result|error}
//     {t:"not", uri, method, params}        client→server notification
//     {t:"sub"|"unsub", method}             (un)subscribe to server notifications
//   proxy → client:
//     {t:"res", seq, result?|error?}
//     {t:"hello", initResult, loc}          sent once on connect
//     {t:"srvNot", method, params}          a subscribed server notification
//     {t:"restart", initResult}             server (re)started
//     {t:"cursor", loc}                     cursor moved

import { loadRenderInfoview } from '@leanprover/infoview/loader'
import type { EditorApi, InfoviewApi } from '@leanprover/infoview-api'

const ws = new WebSocket(`ws://${location.host}/ws`)
let api: InfoviewApi | null = null
let seq = 0
const pending = new Map<number, { resolve: (v: any) => void; reject: (e: any) => void }>()
const subscribed = new Set<string>()

// Messages that arrive before the infoview module finishes loading are buffered.
let hello: { initResult: any; loc: any } | null = null
const preBuffer: Array<{ method: string; params: any }> = []

function request(uri: string, method: string, params: any): Promise<any> {
  return new Promise((resolve, reject) => {
    const s = ++seq
    pending.set(s, { resolve, reject })
    ws.send(JSON.stringify({ t: 'req', seq: s, uri, method, params }))
  })
}

const editorApi: EditorApi = {
  async saveConfig() {},
  sendClientRequest: (uri, method, params) => request(uri, method, params),
  async sendClientNotification(uri, method, params) {
    ws.send(JSON.stringify({ t: 'not', uri, method, params }))
  },
  async subscribeServerNotifications(method) {
    subscribed.add(method)
    ws.send(JSON.stringify({ t: 'sub', method }))
  },
  async unsubscribeServerNotifications(method) {
    subscribed.delete(method)
    ws.send(JSON.stringify({ t: 'unsub', method }))
  },
  async subscribeClientNotifications() {},
  async unsubscribeClientNotifications() {},
  async copyToClipboard(text) {
    try { await navigator.clipboard.writeText(text) } catch {}
  },
  async insertText() {},
  // "Try this" and other suggestions call applyEdit. Forward the WorkspaceEdit
  // to the proxy, which asks the editor to apply it via workspace/applyEdit.
  async applyEdit(te) {
    console.log('[lean-goalview] applyEdit', te)
    ws.send(JSON.stringify({ t: 'edit', edit: te }))
  },
  async showDocument() {},
  async restartFile() {},
  createRpcSession: async (uri) => (await request(uri, '$/lean/rpc/connect', { uri })).sessionId,
  async closeRpcSession() {},
} as unknown as EditorApi

async function applyHello() {
  if (!api || !hello) return
  await api.serverRestarted(hello.initResult)
  await api.initialize(hello.loc)
  for (const n of preBuffer.splice(0)) {
    if (subscribed.has(n.method)) api.gotServerNotification(n.method, n.params)
  }
}

ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data)
  switch (m.t) {
    case 'res': {
      const p = pending.get(m.seq)
      if (p) {
        pending.delete(m.seq)
        m.error ? p.reject(m.error) : p.resolve(m.result)
      }
      break
    }
    case 'hello':
      hello = { initResult: m.initResult, loc: m.loc }
      applyHello()
      break
    case 'restart':
      if (api) api.serverRestarted(m.initResult)
      break
    case 'cursor':
      if (api) api.changedCursorLocation(m.loc)
      break
    case 'srvNot':
      if (api && subscribed.has(m.method)) api.gotServerNotification(m.method, m.params)
      else if (!api) preBuffer.push({ method: m.method, params: m.params })
      break
  }
}

ws.onopen = () => {
  const div = document.getElementById('infoview')!
  const imports = {
    '@leanprover/infoview': '/imports/index.production.min.js',
    react: '/imports/react.production.min.js',
    'react/jsx-runtime': '/imports/react-jsx-runtime.production.min.js',
    'react-dom': '/imports/react-dom.production.min.js',
  }
  loadRenderInfoview(imports, [editorApi, div], (a) => {
    api = a
    applyHello()
  })
}

ws.onclose = () => {
  const div = document.getElementById('infoview')
  if (div && !api) div.innerHTML = '<div class="gv-status">Lean server disconnected.</div>'
}
