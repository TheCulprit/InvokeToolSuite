import { useEffect, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { ArrowLeft, ArrowRight, Check, ChevronLeft, ChevronRight, CircleAlert, Database, FolderOpen, FolderTree, Home, Images, Layers3, LoaderCircle, Search, Sparkles, Upload, X } from 'lucide-react'
import { CivitaiPanel } from './CivitaiPanel'

type Stage = 'source' | 'browse' | 'destination' | 'review'
type Board = { board_id: string; board_name: string; image_count: number }
type SourceImage = { image_name: string; image_subfolder: string; created_at: string; width: number; height: number; preview?: string }
type Project = { project_id: string; name: string; board_id: string; user_id: string }
type TargetBoard = { board_id: string; board_name: string; project_id?: string | null }
type User = { user_id: string }
type Report = { imported: number; skipped: number; thumbnails_repaired: number; issues: {image_name:string;reason:string}[]; backup_path: string; backup_created: boolean }
type Progress = { phase: string; current: number; total: number; imported: number; skipped: number; image_name: string }
type PreviewProgress = { request_id: string; current: number; total: number; image_name: string }
type PreviewImageEvent = { request_id: string; image: SourceImage }

function readSavedPath(key: string, oldKey: string) {
  try {
    let value = localStorage.getItem(key)
    if (value === null) {
      value = localStorage.getItem(oldKey)
      if (value !== null) localStorage.setItem(key, value)
    }
    localStorage.removeItem(oldKey)
    return value ?? ''
  } catch { return '' }
}

function migrateSavedPreference(key: string, oldKey: string) {
  try {
    if (localStorage.getItem(key) === null) {
      const oldValue = localStorage.getItem(oldKey)
      if (oldValue !== null) localStorage.setItem(key, oldValue)
    }
    localStorage.removeItem(oldKey)
  } catch {}
}

export default function App() {
  const [activeTool, setActiveTool] = useState<'home' | 'transfer' | 'civitai'>('home')
  const [stage, setStage] = useState<Stage>('source')
  const [database, setDatabase] = useState(() => readSavedPath('invoke-tool-suite.source-database', 'invoke-transfer.source-database'))
  const [outputs, setOutputs] = useState(() => readSavedPath('invoke-tool-suite.source-outputs', 'invoke-transfer.source-outputs'))
  const [destinationDb, setDestinationDb] = useState(() => readSavedPath('invoke-tool-suite.destination-database', 'invoke-transfer.destination-database'))
  const [destinationOutputs, setDestinationOutputs] = useState(() => readSavedPath('invoke-tool-suite.destination-outputs', 'invoke-transfer.destination-outputs'))
  const [filter, setFilter] = useState('')
  const [selectedBoard, setSelectedBoard] = useState('')
  const [boards, setBoards] = useState<Board[]>([])
  const [images, setImages] = useState<SourceImage[]>([])
  const [previewProgress, setPreviewProgress] = useState<PreviewProgress | null>(null)
  const [previewLoading, setPreviewLoading] = useState(false)
  const activePreview = useRef<string | null>(null)
  const previewListeners = useRef<(() => void)[]>([])
  const [projects, setProjects] = useState<Project[]>([])
  const [destinationUsers, setDestinationUsers] = useState<User[]>([])
  const [targetUser, setTargetUser] = useState('')
  const [targetProject, setTargetProject] = useState('')
  const [targetBoards, setTargetBoards] = useState<TargetBoard[]>([])
  const [targetBoard, setTargetBoard] = useState('')
  const [report, setReport] = useState<Report | null>(null)
  const [progress, setProgress] = useState<Progress | null>(null)
  const [message, setMessage] = useState('')
  const [busy, setBusy] = useState(false)
  const [civitaiBusy, setCivitaiBusy] = useState(false)
  const [lightbox, setLightbox] = useState<{ image: SourceImage; url: string | null; loading: boolean; error: string | null } | null>(null)
  useEffect(() => { try { localStorage.setItem('invoke-tool-suite.source-database', database) } catch {} }, [database])
  useEffect(() => { try { localStorage.setItem('invoke-tool-suite.source-outputs', outputs) } catch {} }, [outputs])
  useEffect(() => { try { localStorage.setItem('invoke-tool-suite.destination-database', destinationDb) } catch {} }, [destinationDb])
  useEffect(() => { try { localStorage.setItem('invoke-tool-suite.destination-outputs', destinationOutputs) } catch {} }, [destinationOutputs])
  useEffect(() => {
    migrateSavedPreference('invoke-tool-suite.civitai-loras', 'invoke-tools.civitai-loras')
    migrateSavedPreference('invoke-tool-suite.civitai-checkpoints', 'invoke-tools.civitai-checkpoints-v2')
  }, [])
  useEffect(() => {
    if (!destinationDb) return
    let active = true
    Promise.all([
      invoke<Project[]>('list_destination_projects', { path: destinationDb }),
      invoke<User[]>('list_destination_users', { path: destinationDb }),
    ]).then(([projectList, userList]) => {
      if (!active) return
      setProjects(projectList)
      setDestinationUsers(userList)
      setTargetUser(userList[0]?.user_id ?? '')
      setMessage('v7 database connected. Choose a project.')
    }).catch(error => {
      if (active) setMessage(`Could not load the saved v7 database: ${String(error)}`)
    })
    return () => { active = false }
  }, [destinationDb])
  useEffect(() => {
    if (!lightbox) return
    const navigate = (event: KeyboardEvent) => {
      const index = images.findIndex(image => image.image_name === lightbox.image.image_name)
      if (event.key === 'Escape') setLightbox(null)
      if (event.key === 'ArrowLeft' && index > 0) { event.preventDefault(); void openFullImage(images[index - 1]) }
      if (event.key === 'ArrowRight' && index >= 0 && index < images.length - 1) { event.preventDefault(); void openFullImage(images[index + 1]) }
    }
    window.addEventListener('keydown', navigate)
    return () => window.removeEventListener('keydown', navigate)
  }, [lightbox, images])
  const filteredBoards = useMemo(() => boards.filter(board => board.board_name.toLowerCase().includes(filter.toLowerCase())), [boards, filter])
  const selectedSource = boards.find(board => board.board_id === selectedBoard)
  const selectedProject = projects.find(project => project.project_id === targetProject)
  const selectedTarget = targetBoards.find(board => board.board_id === targetBoard)
  const targetReady = Boolean(destinationDb && destinationOutputs && targetProject && (targetProject === 'uncategorized' ? targetUser : targetBoard))
  const navigationLocked = busy || civitaiBusy
  const changeTool = (tool: 'home' | 'transfer' | 'civitai') => {
    if (!navigationLocked) setActiveTool(tool)
  }

  const chooseFile = async (kind: 'database' | 'outputs' | 'destinationDb' | 'destinationOutputs') => {
    try {
      const { open } = await import('@tauri-apps/plugin-dialog')
      const isDb = kind === 'database' || kind === 'destinationDb'
      const value = await open({ multiple: false, directory: !isDb, filters: isDb ? [{ name: 'SQLite database', extensions: ['db', 'sqlite', 'sqlite3'] }] : undefined })
      if (typeof value !== 'string') return
      if (kind === 'database') { setDatabase(value); await invoke('inspect_database', { path: value }); setMessage('v6 database connected.') }
      else if (kind === 'outputs') { setOutputs(value); setMessage('v6 outputs folder selected.') }
      else if (kind === 'destinationDb') {
        setDestinationDb(value)
        setProjects([]); setDestinationUsers([]); setTargetUser('')
        setTargetProject(''); setTargetBoards([]); setTargetBoard(''); setMessage('v7 database selected. Loading projects…')
      } else if (kind === 'destinationOutputs') { setDestinationOutputs(value); setMessage('v7 outputs folder selected.') }
    } catch (error) { setMessage(String(error)) }
  }

  const loadSourceBoards = async () => {
    setBusy(true); setMessage('')
    try { setBoards(await invoke<Board[]>('list_source_boards', { path: database })); setSelectedBoard(''); setImages([]); setStage('browse') }
    catch (error) { setMessage(String(error)) }
    finally { setBusy(false) }
  }
  const selectSourceBoard = async (board: Board) => {
    const previousId = activePreview.current
    previewListeners.current.forEach(unlisten => unlisten())
    previewListeners.current = []
    if (previousId) void invoke('cancel_source_preview', { requestId: previousId }).catch(() => {})
    const requestId = crypto.randomUUID()
    activePreview.current = requestId
    setSelectedBoard(board.board_id); setImages([]); setPreviewProgress({ request_id: requestId, current: 0, total: board.image_count, image_name: '' }); setPreviewLoading(true); setMessage('')
    try {
      const unlistenImage = await listen<PreviewImageEvent>('source-preview-image', event => {
        if (activePreview.current === requestId && event.payload.request_id === requestId) setImages(images => [...images, event.payload.image])
      })
      if (activePreview.current !== requestId) { unlistenImage(); return }
      const unlistenProgress = await listen<PreviewProgress>('source-preview-progress', event => {
        if (activePreview.current === requestId && event.payload.request_id === requestId) setPreviewProgress(event.payload)
      })
      if (activePreview.current !== requestId) { unlistenImage(); unlistenProgress(); return }
      previewListeners.current = [unlistenImage, unlistenProgress]
      await invoke('list_board_images', { database, outputs, boardId: board.board_id, requestId })
    }
    catch (error) { if (activePreview.current === requestId) setMessage(String(error)) }
    finally {
      if (activePreview.current === requestId) { setPreviewLoading(false); previewListeners.current.forEach(unlisten => unlisten()); previewListeners.current = [] }
    }
  }
  const cancelPreview = () => {
    const requestId = activePreview.current
    activePreview.current = null
    previewListeners.current.forEach(unlisten => unlisten())
    previewListeners.current = []
    if (requestId) void invoke('cancel_source_preview', { requestId }).catch(() => {})
    setPreviewLoading(false)
  }
  const openFullImage = async (image: SourceImage) => {
    setLightbox({ image, url: null, loading: true, error: null })
    try {
      const url = await invoke<string>('load_source_image', { outputs, imageName: image.image_name, imageSubfolder: image.image_subfolder })
      setLightbox(current => current?.image.image_name === image.image_name ? { ...current, url, loading: false } : current)
    } catch (error) {
      setLightbox(current => current?.image.image_name === image.image_name ? { ...current, loading: false, error: String(error) } : current)
    }
  }
  const lightboxIndex = lightbox ? images.findIndex(image => image.image_name === lightbox.image.image_name) : -1
  const navigateLightbox = (offset: number) => {
    const image = images[lightboxIndex + offset]
    if (image) void openFullImage(image)
  }
  const selectProject = async (id: string) => {
    setTargetProject(id); setTargetBoard(''); setTargetBoards([])
    if (id === 'uncategorized') { if (!targetUser && destinationUsers[0]) setTargetUser(destinationUsers[0].user_id); return }
    try { setTargetBoards(await invoke<TargetBoard[]>('list_destination_boards', { path: destinationDb, projectId: id })) }
    catch (error) { setMessage(String(error)) }
  }
  const startTransfer = async () => {
    setBusy(true); setReport(null); setProgress({ phase: 'copy', current: 0, total: selectedSource?.image_count ?? 0, imported: 0, skipped: 0, image_name: '' }); setMessage('')
    let unlisten = () => {}
    try {
      unlisten = await listen<Progress>('transfer-progress', event => setProgress(event.payload))
      const result = await invoke<Report>('transfer_board', {
        sourceDb: database, sourceOutputs: outputs, sourceBoardId: selectedBoard,
        destinationDb, destinationOutputs, destinationProjectId: targetProject === 'uncategorized' ? null : targetProject,
        destinationBoardId: targetProject === 'uncategorized' ? null : targetBoard,
        destinationUserId: targetProject === 'uncategorized' ? targetUser : null,
      })
      setReport(result)
    } catch (error) { setMessage(String(error)) }
    finally { setBusy(false); unlisten() }
  }

  const step = stage === 'source' ? 0 : stage === 'browse' ? 1 : stage === 'destination' ? 2 : 3
  return <div className="app-shell">
    <aside className="rail"><div className="brand-mark"><Layers3 size={19}/></div><div className="rail-line"/><button disabled={navigationLocked} className={`rail-icon ${activeTool === 'home' ? 'active' : ''}`} aria-label="Home" title={navigationLocked ? 'Finish or cancel the running job to switch tools' : 'Home'} onClick={()=>changeTool('home')}><Home size={19}/></button><button disabled={navigationLocked} className={`rail-icon ${activeTool === 'transfer' ? 'active' : ''}`} aria-label="Image transfer" title={navigationLocked ? 'Finish or cancel the running job to switch tools' : 'Image transfer'} onClick={()=>changeTool('transfer')}><Images size={19}/></button><button disabled={navigationLocked} className={`rail-icon ${activeTool === 'civitai' ? 'active' : ''}`} aria-label="CivitAI tools" title={navigationLocked ? 'Finish or cancel the running job to switch tools' : 'CivitAI tools'} onClick={()=>changeTool('civitai')}><Sparkles size={19}/></button></aside>
    <main className="main-area">
      <header className="topbar"><div className="crumb"><span>Invoke Tool Suite</span></div></header>
      <section className="content">
        <div className="heading-row"><div><h1>{activeTool === 'home' ? 'Invoke Tool Suite' : activeTool === 'transfer' ? 'Image transfer' : 'CivitAI'}</h1><p className="subtitle">{activeTool === 'home' ? 'Tools for managing your InvokeAI setup.' : activeTool === 'transfer' ? 'Move generated images from a v6 board to a v7 project and board.' : 'Pull trigger words and thumbnails from CivitAI.'}</p></div>{navigationLocked && <span className="job-lock" role="status"><LoaderCircle className="spin" size={14}/> Finish or cancel the active job to switch tools</span>}</div>
        {activeTool === 'home' ? <div className="home-panel"><div className="home-warning"><CircleAlert size={21}/><div><strong>Close InvokeAI before using these tools</strong><p>Both tools write directly to the v7 database. Keep InvokeAI closed until the operation finishes.</p></div></div><div className="home-shared-settings"><div className="home-config-heading"><strong>Current InvokeAI setup</strong><span>Shared v7 locations used by the tools</span></div><div className="home-config-paths"><PathField title="V7 DATABASE FILE" value={destinationDb} placeholder="Select the database used by InvokeAI v7" icon={<Database size={17}/>} onBrowse={()=>chooseFile('destinationDb')}/><PathField title="V7 OUTPUTS FOLDER" value={destinationOutputs} placeholder="Select the outputs folder used by InvokeAI v7" icon={<FolderTree size={17}/>} onBrowse={()=>chooseFile('destinationOutputs')}/></div><BackupRetentionSetting database={destinationDb}/></div><div className="home-tools"><button disabled={navigationLocked} className="home-tool-card" onClick={()=>changeTool('transfer')}><span className="home-tool-icon"><Images size={21}/></span><strong>Image transfer</strong><p>Move generated images from a v6 backup into a v7 project and board.</p><span className="home-tool-link">Open tool <ArrowRight size={15}/></span></button><button disabled={navigationLocked} className="home-tool-card" onClick={()=>changeTool('civitai')}><span className="home-tool-icon"><Sparkles size={21}/></span><strong>CivitAI details</strong><p>Find missing trigger phrases and thumbnails for installed models.</p><span className="home-tool-link">Open tool <ArrowRight size={15}/></span></button></div></div> : activeTool === 'transfer' ? <>
        <div className="steps">{[['Source','01'],['Browse','02'],['Destination','03'],['Review & transfer','04']].map(([label,n],i)=><ReactStep key={n} n={n} label={label} active={step===i} done={step>i}/>)}</div>
        {message && <div className="inline-message">{message}</div>}
        {stage==='source' && <div className="panel"><PanelTitle icon={<Database size={19}/>} title="Connect your v6 archive" subtitle="Choose the database backup and matching outputs folder."/><div className="field-stack"><PathField title="V6 DATABASE FILE" value={database} placeholder="Select your InvokeAI database file" icon={<Database size={17}/>} onBrowse={()=>chooseFile('database')}/><PathField title="V6 OUTPUTS FOLDER" value={outputs} placeholder="Select the matching outputs folder" icon={<FolderTree size={17}/>} onBrowse={()=>chooseFile('outputs')}/><p className="hint"><i className="hint-dot"/> The backup database is opened read-only. Nothing in your archive is changed.</p></div><div className="panel-footer"><div className="privacy-note"><span className="shield">✓</span> Files stay on this computer</div><button className="primary" disabled={!database||!outputs||busy} onClick={loadSourceBoards}>{busy?<><LoaderCircle className="spin" size={16}/> Reading boards…</>:<>Find my boards <ArrowRight size={16}/></>}</button></div></div>}
        {stage === 'browse' && <div className="panel destination-panel">
          <PanelTitle icon={<Images size={19}/>} title="Choose the source board" subtitle="Filter boards by name, then preview the generated images inside."/>
          <div className="split-browser">
            <div className="board-column">
              <div className="column-heading">V6 SOURCE BOARDS <span className="count">{filteredBoards.length}</span></div>
              <div className="searchbox"><Search size={16}/><input placeholder="Filter boards…" value={filter} onChange={e=>setFilter(e.target.value)}/>{filter && <button type="button" className="clear-filter" aria-label="Clear board filter" title="Clear filter" onClick={()=>setFilter('')}><X size={15}/></button>}</div>
              <div className="board-list">
                {filteredBoards.length ? filteredBoards.map(board => <button key={board.board_id} disabled={busy} aria-busy={previewLoading && selectedBoard === board.board_id} className={`board-item ${selectedBoard === board.board_id ? 'selected' : ''}`} onClick={() => selectSourceBoard(board)}>
                  <span className="board-symbol">{previewLoading && selectedBoard === board.board_id ? <LoaderCircle className="spin" size={17}/> : <Images size={16}/>}</span>
                  <span className="board-name">{board.board_name}<small>{board.image_count} images</small></span><span className="board-chevron">›</span>
                </button>) : <div className="empty-state compact"><div className="empty-icon"><Database size={19}/></div><strong>No boards found</strong><span>Check that this is a v6 database backup.</span></div>}
              </div>
            </div>
            <div className="image-column">
              <div className="column-heading">{selectedSource?.board_name ?? 'IMAGE PREVIEW'} <span className="count">{previewLoading ? `${images.length} of ${previewProgress?.total ?? selectedSource?.image_count ?? 0} loaded` : `${images.length} images`}</span></div>
              {previewLoading && <div className="preview-status" role="status" aria-live="polite"><LoaderCircle className="spin" size={17}/><span>{previewProgress?.image_name ? `Loading ${previewProgress.image_name}` : 'Scanning this board…'}</span><div className="preview-progress"><i style={{width: previewProgress?.total ? `${previewProgress.current / previewProgress.total * 100}%` : '3%'}}/></div></div>}
              {images.length ? <div className="image-grid">{images.map(image => <button type="button" className="image-card image-open" key={image.image_name} aria-label={`Open ${image.image_name} full size`} onClick={() => void openFullImage(image)}>
                {image.preview ? <img src={image.preview} alt=""/> : <div className="missing-preview"><Images size={18}/></div>}
                <div className="image-date">{new Date(image.created_at + 'Z').toLocaleDateString()}</div>
              </button>)}</div> : <div className="preview-empty"><Images size={28}/><strong>{previewLoading ? 'Waiting for the first image…' : selectedBoard ? 'No eligible images found' : 'Choose a board to preview'}</strong></div>}
            </div>
          </div>
          <div className="panel-footer"><button className="secondary" disabled={busy} onClick={()=>{cancelPreview();setStage('source')}}><ArrowLeft size={14}/> Back</button><button className="primary" disabled={!selectedBoard || images.length === 0 || busy} onClick={()=>{cancelPreview();setStage('destination')}}>Choose destination <ArrowRight size={16}/></button></div>
        </div>}        {stage==='destination' && <div className="panel"><PanelTitle icon={<FolderTree size={19}/>} title="Choose the v7 destination" subtitle="Select the project and board for this transfer."/><div className="field-stack"><div className="source-reminder"><span>SOURCE BOARD</span><strong>{selectedSource?.board_name ?? 'No board selected'}</strong><small>{selectedSource?.image_count ?? 0} images in this board</small></div>{(!destinationDb||!destinationOutputs)&&<div className="inline-message">Choose the current v7 database and outputs folder on the Home tab first.</div>}<div className="select-grid"><div><label className="field-label">PROJECT</label><select value={targetProject} onChange={e=>selectProject(e.target.value)} disabled={!destinationDb}><option value="">Select a project…</option>{projects.map(project=><option key={project.project_id} value={project.project_id}>{project.name}</option>)}<option value="uncategorized">Uncategorized</option></select></div><div><label className="field-label">BOARD</label><select value={targetProject==='uncategorized'?'uncategorized':targetBoard} onChange={e=>setTargetBoard(e.target.value)} disabled={!targetProject||targetProject==='uncategorized'}><option value="">Select a board…</option>{targetBoards.map(board=><option key={board.board_id} value={board.board_id}>{board.board_name}</option>)}</select></div></div>{targetProject==='uncategorized'&&<div className="owner-select"><label className="field-label">ACCOUNT FOR UNCATEGORIZED IMAGES</label><select value={targetUser} onChange={e=>setTargetUser(e.target.value)}><option value="">Select account…</option>{destinationUsers.map(user=><option key={user.user_id} value={user.user_id}>{user.user_id}</option>)}</select></div>}</div><div className="panel-footer"><button className="secondary" onClick={()=>setStage('browse')}><ArrowLeft size={14}/> Back</button><button className="primary" disabled={!targetReady} onClick={()=>setStage('review')}>Review transfer <ArrowRight size={16}/></button></div></div>}
        {stage==='review' && <div className="panel"><PanelTitle icon={<Check size={19}/>} title={report?'Transfer complete':'Review your transfer'} subtitle="Confirm the destination before writing to the v7 database."/><div className="review-summary"><div><span>FROM</span><strong>{selectedSource?.board_name}</strong><small>{selectedSource?.image_count ?? 0} images from this board</small></div><ArrowRight size={18}/><div><span>TO</span><strong>{targetProject==='uncategorized'?'Uncategorized':selectedProject?.name}</strong><small>{targetProject==='uncategorized'?'No board':selectedTarget?.board_name}</small></div></div>{report?<div className="report"><div className="report-stats"><strong>{report.imported}</strong><span>imported</span><strong>{report.skipped}</strong><span>skipped</span></div>{report.thumbnails_repaired>0&&<p className="backup-note">Repaired {report.thumbnails_repaired} existing thumbnail{report.thumbnails_repaired===1?'':'s'}.</p>}<p className="backup-note">{report.backup_created?'Safety backup created':'Reusing the safety backup from before the first transfer'}: {report.backup_path}</p>{report.issues.length>0&&<div className="issue-list"><h3>Images needing attention</h3>{report.issues.map((issue,i)=><div key={`${issue.image_name}-${i}`}><b>{issue.image_name}</b><span>{issue.reason}</span></div>)}</div>}</div>:null}{message&&<div className="inline-message">{message}</div>}{busy&&<div className="transfer-status"><div className="status-label">{progress?.phase==='backup'?'Creating safety backup':progress?.phase==='backup-reused'?'Using existing safety backup':progress?.phase==='copy'?`Copying image ${progress.current} of ${progress.total}`:'Preparing transfer…'}{progress?.phase==='backup'&&progress.total>0&&<span>{Math.round(progress.current/progress.total*100)}%</span>}{progress?.phase==='copy'&&progress.image_name&&<span>{progress.image_name}</span>}</div><div className="progress-track"><div className={`progress-fill ${progress?.total?'determinate':''}`} style={{width:`${progress&&progress.total?progress.current/progress.total*100:2}%`}}/></div><small>{progress?.phase==='backup'?'Copying database pages for the safety backup.':progress?.phase==='backup-reused'?'Reusing the existing backup for this destination database.':`${progress?.imported??0} imported · ${progress?.skipped??0} skipped`}</small></div>}<div className="panel-footer"><button className="secondary" disabled={busy} onClick={()=>setStage('destination')}><ArrowLeft size={14}/> Back</button><button className="primary" disabled={!report&&busy} onClick={report?()=>{setReport(null);setProgress(null);setMessage('');setStage('browse')}:startTransfer}>{busy?'Transferring…':report?'Done':'Start transfer'} {!busy&&!report&&<Upload size={15}/>}</button></div></div>}
        </> : <CivitaiPanel database={destinationDb} onBusyChange={setCivitaiBusy}/>}
      </section>
    </main>
    {lightbox && <div className="image-lightbox" role="dialog" aria-modal="true" aria-label="Image preview" onMouseDown={event=>{if(event.target===event.currentTarget)setLightbox(null)}}>
      <div className="lightbox-panel">
        <div className="lightbox-header"><div className="lightbox-caption"><strong>{lightbox.image.image_name}</strong><span>{lightbox.image.width} × {lightbox.image.height} · {new Date(lightbox.image.created_at + 'Z').toLocaleDateString()}</span></div><button className="lightbox-close" type="button" aria-label="Close image preview" onClick={()=>setLightbox(null)}><X size={21}/></button></div>
        <div className="lightbox-stage">
          <button className="lightbox-nav previous" type="button" aria-label="Previous image" disabled={lightboxIndex<=0} onClick={()=>navigateLightbox(-1)}><ChevronLeft size={27}/></button>
          {lightbox.loading && <div className="lightbox-message"><LoaderCircle className="spin" size={30}/>Loading full image…</div>}
          {lightbox.error && <div className="lightbox-message lightbox-error">{lightbox.error}</div>}
          {lightbox.url && <img className="lightbox-image" src={lightbox.url} alt={lightbox.image.image_name}/>}
          <button className="lightbox-nav next" type="button" aria-label="Next image" disabled={lightboxIndex<0||lightboxIndex>=images.length-1} onClick={()=>navigateLightbox(1)}><ChevronRight size={27}/></button>
        </div>
      </div>
    </div>}
  </div>
}

function BackupRetentionSetting({ database }: { database: string }) {
  const [keep, setKeep] = useState(3)
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [message, setMessage] = useState('')

  useEffect(() => {
    let active = true
    invoke<number>('get_backup_retention', { database: database || null })
      .then(value => { if (active) setKeep(value) })
      .catch(error => { if (active) setMessage(String(error)) })
      .finally(() => { if (active) setLoading(false) })
    return () => { active = false }
  }, [])

  const save = async (value: number) => {
    setSaving(true)
    setMessage('')
    try {
      await invoke('set_backup_retention', { keep: value, database: database || null })
      setKeep(value)
      setMessage(database ? `Keeping the newest ${value} backups. Older backups for this database were removed.` : `Keeping the newest ${value} backups. The setting will apply the next time you select a database.`)
    } catch (error) { setMessage(String(error)) }
    finally { setSaving(false) }
  }

  return <div className="backup-retention-setting">
    <div className="option-row"><div><strong>Safety backups to keep</strong><p>Shared by Image transfer and CivitAI. Older backups are removed when a new one is created.</p></div><label className="retention-select"><span className="sr-only">Safety backups to keep</span><select value={keep} disabled={loading || saving} onChange={event => void save(Number(event.target.value))}>{[1, 2, 3, 5, 10, 20].map(count => <option key={count} value={count}>{count} backups</option>)}</select></label></div>
    {message && <div className="inline-message">{message}</div>}
  </div>
}

function PanelTitle({icon,title,subtitle}:{icon:ReactNode;title:string;subtitle:string}) { return <div className="panel-title"><div className="icon-tile violet">{icon}</div><div><h2>{title}</h2><p>{subtitle}</p></div></div> }
function PathField({title,value,placeholder,icon,onBrowse}:{title:string;value:string;placeholder:string;icon:ReactNode;onBrowse:()=>void}) { return <><label className="field-label">{title} <span className="required">Required</span></label><div className="path-input">{icon}<input readOnly value={value} placeholder={placeholder}/><button onClick={onBrowse}><FolderOpen size={15}/> Browse</button></div></> }
function ReactStep({n,label,active,done}:{n:string;label:string;active:boolean;done:boolean}) { return <><div className={`step ${active?'current':''} ${done?'complete':''}`}><span className="step-number">{done?<Check size={13}/>:n}</span><span>{label}</span></div>{n!=='04'&&<div className="step-rule"/>}</> }
