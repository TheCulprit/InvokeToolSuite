import { useEffect, useRef, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { ArrowRight, KeyRound, LoaderCircle, RotateCcw, Sparkles, Square, Trash2 } from 'lucide-react'

type CacheInfo = { cache_path: string; cache_exists: boolean; entries: number; matched: number; not_found: number; read_error?: string | null }
type Progress = { current: number; total: number; percent: number | null; model_name: string; status: string }
type Issue = { model_name: string; reason: string }
type Report = {
  models_scanned: number; models_processed: number; cancelled: boolean; cache_hits: number; cached_not_found: number; lookup_attempts: number; not_found: number
  no_trigger_phrases: number; no_preview_image: number; trigger_phrases_added: number; thumbnails_added: number
  already_had_triggers: number; already_had_thumbnails: number; skipped: number; errors: number
  backup_path: string; backup_created: boolean; issues: Issue[]
}

function readSavedToggle(key: string, oldKey: string, defaultValue: boolean) {
  try {
    let value = localStorage.getItem(key)
    if (value === null) {
      value = localStorage.getItem(oldKey)
      if (value !== null) localStorage.setItem(key, value)
    }
    localStorage.removeItem(oldKey)
    return value === null ? defaultValue : value === 'true'
  } catch { return defaultValue }
}

export function CivitaiPanel({ database, onBusyChange }: { database: string; onBusyChange: (busy: boolean) => void }) {
  const [apiKey, setApiKey] = useState('')
  const [keySaved, setKeySaved] = useState(false)
  const [includeLoras, setIncludeLoras] = useState(() => readSavedToggle('invoke-tool-suite.civitai-loras', 'invoke-tools.civitai-loras', true))
  const [includeCheckpoints, setIncludeCheckpoints] = useState(() => readSavedToggle('invoke-tool-suite.civitai-checkpoints', 'invoke-tools.civitai-checkpoints-v2', false))
  const [cache, setCache] = useState<CacheInfo | null>(null)
  const [report, setReport] = useState<Report | null>(null)
  const [progress, setProgress] = useState<Progress | null>(null)
  const [message, setMessage] = useState('')
  const [busy, setBusy] = useState(false)
  const [cancelable, setCancelable] = useState(false)
  const [cancelRequested, setCancelRequested] = useState(false)
  const taskId = useRef<string | null>(null)

  useEffect(() => { try { localStorage.setItem('invoke-tool-suite.civitai-loras', String(includeLoras)) } catch {} }, [includeLoras])
  useEffect(() => { try { localStorage.setItem('invoke-tool-suite.civitai-checkpoints', String(includeCheckpoints)) } catch {} }, [includeCheckpoints])

  useEffect(() => {
    let active = true
    invoke<boolean>('civitai_api_key_saved')
      .then(saved => { if (active) setKeySaved(saved) })
      .catch(error => { if (active) setMessage(String(error)) })
    return () => { active = false }
  }, [])

  const saveKey = async () => {
    try {
      await invoke('save_civitai_api_key', { apiKey })
      setApiKey('')
      setKeySaved(true)
      setMessage('CivitAI key saved in the system credential store.')
    } catch (error) { setMessage(String(error)) }
  }

  const clearKey = async () => {
    try {
      await invoke('clear_civitai_api_key')
      setApiKey('')
      setKeySaved(false)
      setMessage('Saved CivitAI key removed.')
    } catch (error) { setMessage(String(error)) }
  }

  const refreshCache = async () => {
    if (!database) { setCache(null); return }
    try { setCache(await invoke<CacheInfo>('get_civitai_cache_info', { database })) }
    catch (error) { setMessage(String(error)) }
  }

  useEffect(() => {
    let active = true
    if (!database) { setCache(null); return }
    invoke<CacheInfo>('get_civitai_cache_info', { database })
      .then(info => { if (active) setCache(info) })
      .catch(error => { if (active) setMessage(String(error)) })
    return () => { active = false }
  }, [database])

  const clearCache = async () => {
    if (!database || busy) return
    try {
      setCache(await invoke<CacheInfo>('clear_civitai_cache', { database }))
      setMessage('Cache cleared. The next scan will look up the selected models again.')
      setReport(null)
    } catch (error) { setMessage(String(error)) }
  }

  const start = async (refresh = false) => {
    if (!database) { setMessage('Select the v7 database first.'); return }
    if (!includeLoras && !includeCheckpoints) { setMessage('Select LoRAs, checkpoints, or both.'); return }
    if (apiKey.trim()) {
      try {
        await invoke('save_civitai_api_key', { apiKey })
        setKeySaved(true)
        setApiKey('')
      } catch (error) { setMessage(String(error)); return }
    }
    setBusy(true)
    onBusyChange(true)
    setCancelable(false)
    setCancelRequested(false)
    setReport(null)
    setMessage('')
    setProgress({ current: 0, total: 0, percent: null, model_name: '', status: refresh ? 'Clearing cache and preparing scan' : 'Preparing model scan' })
    const id = crypto.randomUUID()
    taskId.current = id
    let unlisten = () => {}
    try {
      unlisten = await listen<Progress>('civitai-progress', event => setProgress(event.payload))
      const request = invoke<Report>('enrich_civitai_models', {
        database, includeLoras, includeCheckpoints, taskId: id, refreshCache: refresh,
      })
      setCancelable(true)
      setReport(await request)
    } catch (error) { setMessage(String(error)) }
    finally {
      taskId.current = null
      setCancelable(false)
      setCancelRequested(false)
      setBusy(false)
      onBusyChange(false)
      unlisten()
      await refreshCache()
    }
  }

  const cancel = async () => {
    const id = taskId.current
    if (!id || !cancelable || cancelRequested) return
    setCancelRequested(true)
    setCancelable(false)
    setProgress(current => current ? { ...current, status: 'Cancellation requested. Finishing the current model…' } : current)
    try {
      await invoke('cancel_civitai_enrichment', { taskId: id })
    } catch (error) {
      setCancelRequested(false)
      setCancelable(true)
      setMessage(String(error))
    }
  }

  return <div className="panel civitai-panel">
    <div className="panel-title"><div className="icon-tile violet"><Sparkles size={19}/></div><div><h2>CivitAI trigger words and thumbnails</h2><p>Match selected model files by hash and fill missing details.</p></div></div>
    <div className="civitai-content">
      {!database && <div className="inline-message">Choose the current v7 database on the Home tab to use this tool.</div>}
      <div className="civitai-settings">
        <div className="field-stack">
          <div className="civitai-options" aria-label="Model types to scan">
            <label><input type="checkbox" checked={includeLoras} onChange={event => setIncludeLoras(event.target.checked)}/> LoRAs</label>
            <label><input type="checkbox" checked={includeCheckpoints} onChange={event => setIncludeCheckpoints(event.target.checked)}/> Checkpoints</label>
          </div>
          <label className="field-label civitai-key-label">CIVITAI API KEY <span className="required">Optional</span></label>
          <div className="civitai-key"><input type="password" value={apiKey} onChange={event => setApiKey(event.target.value)} autoComplete="new-password" placeholder={keySaved ? 'A key is saved securely' : 'Only needed for restricted lookups'}/><button type="button" disabled={busy || !apiKey.trim()} onClick={() => void saveKey()}><KeyRound size={14}/> Save key</button></div>
          <div className="civitai-key-status">{keySaved ? <><span>Key saved in your system credential store</span><button type="button" disabled={busy} onClick={() => void clearKey()}>Remove saved key</button></> : <span>No key saved</span>}</div>
          <p className="hint"><i className="hint-dot"/> Existing trigger phrases and cover images are kept. Cached lookups are reused unless you refresh the cache.</p>
        </div>
        {cache && <div className="civitai-cache">
          <div className="civitai-cache-heading"><div><strong>CivitAI lookup cache</strong><span>{cache.entries} saved lookups · {cache.matched} matches · {cache.not_found} no matches</span></div><button type="button" className="secondary cache-clear" disabled={busy || !cache.cache_exists} onClick={() => void clearCache()}><Trash2 size={14}/> Clear cache</button></div>
          <small className="cache-path">{cache.cache_path}</small>
          {cache.read_error && <small className="cache-error">Could not read cache: {cache.read_error}</small>}
        </div>}
        <div className="civitai-actions">
          {busy ? <button type="button" className={`secondary cancel-job ${cancelRequested ? 'pending' : ''}`} disabled={!cancelable || cancelRequested} onClick={() => void cancel()}>{cancelRequested ? <LoaderCircle className="spin" size={14}/> : <Square size={14}/>} {cancelRequested ? 'Cancelling…' : 'Cancel job'}</button> : <>
            <button type="button" className="secondary" disabled={!database || (!includeLoras && !includeCheckpoints)} onClick={() => void start(true)}><RotateCcw size={14}/> Clear cache &amp; rescan</button>
            <button className="primary" disabled={!database || (!includeLoras && !includeCheckpoints)} onClick={() => void start(false)}>Check selected models <ArrowRight size={16}/></button>
          </>}
        </div>
      </div>
      {message && <div className="inline-message">{message}</div>}
      {cancelRequested && <div className="cancel-notice" role="status" aria-live="assertive"><LoaderCircle className="spin" size={16}/><span><strong>Cancellation requested</strong><small>The current model is finishing. The scan will stop before starting another.</small></span></div>}
      {busy && <div className="civitai-progress">
        <div className="status-label">{progress?.total ? 'Checking model ' + progress.current + ' of ' + progress.total : 'Preparing model scan'}<span>{progress?.model_name}</span></div>
        <div className="progress-track"><div className={progress?.total ? 'progress-fill determinate' : 'progress-fill'} style={{ width: progress?.percent != null ? progress.percent + '%' : '2%' }}/></div>
        <small role="status" aria-live="polite">{progress?.status ?? 'Reading selected models'}</small>
      </div>}
      {report && <div className="civitai-report">
        <h3 className="civitai-result-title">{report.cancelled ? 'Job cancelled' : 'Scan complete'} · {report.models_processed} of {report.models_scanned} models processed</h3>
        <div className="civitai-report-group"><h4>Lookup activity</h4><div className="civitai-stats">
          <div><strong>{report.cache_hits}</strong><span>reused from cache</span></div>
          <div><strong>{report.cached_not_found}</strong><span>cached no match</span></div>
          <div><strong>{report.lookup_attempts}</strong><span>new CivitAI lookups</span></div>
          <div><strong>{report.not_found}</strong><span>no match this session</span></div>
        </div></div>
        <div className="civitai-report-group"><h4>Changes this session</h4><div className="civitai-stats">
          <div><strong>{report.trigger_phrases_added}</strong><span>trigger lists added</span></div>
          <div><strong>{report.thumbnails_added}</strong><span>thumbnails added</span></div>
          <div><strong>{report.errors}</strong><span>issues</span></div>
        </div></div>
        <div className="civitai-report-group"><h4>Lookup results, including cached matches</h4><div className="civitai-stats">
          <div><strong>{report.no_trigger_phrases}</strong><span>matches without triggers</span></div>
          <div><strong>{report.no_preview_image}</strong><span>matches without previews</span></div>
        </div></div>
        <p className="backup-note">{report.backup_created ? 'Safety backup created' : 'Reusing the safety backup from before the first change'}: {report.backup_path}</p>
        {report.issues.length > 0 && <div className="issue-list"><h3>Models needing attention</h3>{report.issues.map((issue, index) => <div key={issue.model_name + '-' + index}><b>{issue.model_name}</b><span>{issue.reason}</span></div>)}</div>}
      </div>}
    </div>
  </div>
}
