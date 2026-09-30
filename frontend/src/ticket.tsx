// Spec 002 and 006: the open ticket — header, thread, composer, sidebar.

import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link, useParams, useSearch } from '@tanstack/react-router'
import {
  PRIORITY_LABELS,
  STATUS_LABELS,
  ago,
  api,
  errorCode,
  formatDate,
  formatSize,
  formatSnooze,
  pct,
  useAgents,
  useCategories,
  useMe,
  useSnippets,
} from './client'
import { CopyButton } from './auth'
import { Copilot } from './copilot'
import { POLL, Waiting, useInbox, type InboxSearch } from './inbox'
import {
  ActionMenu,
  AgentAvatar,
  Avatar,
  Dropdown,
  PickList,
  RelTime,
  composerBus,
  rewrite,
  toast,
  useChangeTickets,
} from './triage'
import type { Message } from './api/types/Message'
import type { PatchTicketsItem } from './api/types/PatchTicketsItem'
import type { ReplyRequest } from './api/types/ReplyRequest'
import type { Snippet } from './api/types/Snippet'
import type { Ticket } from './api/types/Ticket'
import type { TicketDetail } from './api/types/TicketDetail'

export function NoTicket() {
  return <div className="empty-pane muted">Select a ticket.</div>
}

export function TicketPane() {
  const { view = 'open', ticketId = '' } = useParams({ strict: false })
  const { from } = useSearch({ strict: false }) as InboxSearch
  const ticket = useQuery({
    queryKey: ['ticket', ticketId],
    queryFn: () => api.get<TicketDetail>(`/tickets/${ticketId}`),
    refetchInterval: POLL,
  })
  const t = ticket.data
  useEffect(() => {
    if (t) document.title = `#${t.number} ${t.subject} · Muninn`
  }, [t?.number, t?.subject])
  // Opening it read it (§17): the list shows that now, not at its next poll.
  const qc = useQueryClient()
  const loaded = !!t
  useEffect(() => {
    if (loaded) rewrite(qc, (x) => (x.id === ticketId ? { ...x, unread: false, snoozeEnded: false } : x))
  }, [loaded, ticketId, qc])

  if (ticket.isError && !t)
    return <div className="empty-pane muted">{errorCode(ticket.error) === 'notFound' ? 'Ticket not found.' : 'Could not load the ticket.'}</div>
  if (!t) return <div className="empty-pane muted">Loading…</div>

  return (
    <>
      <section className="thread" aria-label="Conversation">
        <Header t={t} from={from} view={view} />
        <Thread key={t.id} t={t} />
        {/* Spec 007 §25: a HubSpot ticket is answered in HubSpot. */}
        {t.hubspot ? (
          <p className="composer muted">Synced from HubSpot. Reply there, and change its status, owner and priority there.</p>
        ) : (
          <Composer key={t.id} t={t} />
        )}
      </section>
      <Sidebar t={t} view={view} />
    </>
  )
}

// ---------------------------------------------------------------------------
// §28: number, subject, status, Assign to me, up/down, presence, ⋯
// ---------------------------------------------------------------------------

function Header({ t, from, view }: { t: TicketDetail; from?: string; view: string }) {
  const me = useMe()
  const inbox = useInbox()
  const change = useChangeTickets()
  const meId = me.data?.agent.id
  const prev = inbox.neighbor(t.id, -1)
  const next = inbox.neighbor(t.id, 1)
  const link = `${location.origin}/inbox/${inbox.view}/${t.id}`
  return (
    <header className="thread-head">
      {from && (
        <Link to="/inbox/$view/$ticketId" params={{ view, ticketId: from }} search={{ ...inbox.search, from: undefined }} className="back">
          ← Back to ticket
        </Link>
      )}
      <span className="number">#{t.number}</span>
      <h1>{t.subject}</h1>
      <span className={`pill status-${t.status}`}>{STATUS_LABELS[t.status]}</span>
      {t.hubspot && (
        <>
          <span className="pill">HubSpot</span>
          <a href={t.hubspot.url} target="_blank" rel="noopener noreferrer">
            Open in HubSpot
          </a>
        </>
      )}
      {t.viewers.length > 0 && (
        <span className="viewers">
          {t.viewers.map((v) => (
            <AgentAvatar key={v.id} id={v.id} name={v.name} small />
          ))}
          <span className="muted small">
            {t.viewers.map((v) => v.name).join(', ')} {t.viewers.length === 1 ? 'is' : 'are'} viewing
          </span>
        </span>
      )}
      {meId && !t.hubspot && t.owner?.id !== meId && <button onClick={() => change([t], { ownerId: meId })}>Assign to me</button>}
      <span className="nav-arrows">
        <button aria-label="Previous ticket (k)" disabled={!prev} onClick={() => prev && inbox.open(prev)}>
          ↑
        </button>
        <button aria-label="Next ticket (j)" disabled={!next} onClick={() => next && inbox.open(next)}>
          ↓
        </button>
        {inbox.position(t.id) && <span className="muted small">{inbox.position(t.id)}</span>}
      </span>
      <Dropdown label="⋯" title="More actions">
        {(close) => (
          <div className="menu">
            <CopyLink text={link} />
            <button onClick={() => (close(), inbox.markUnread([t]))}>Mark as unread</button>
            {t.snoozedUntil ? (
              <button onClick={() => (close(), change([t], { snoozedUntil: null }))}>Unsnooze</button>
            ) : (
              t.status !== 'closed' && (
                <Dropdown label="Snooze…">{(c2) => <ActionMenu kind="snooze" tickets={[t]} onClose={c2} onDone={() => (c2(), close())} />}</Dropdown>
              )
            )}
          </div>
        )}
      </Dropdown>
      {t.snoozedUntil && (
        <p className="snooze-note">
          Snoozed until {formatSnooze(t.snoozedUntil)} ·{' '}
          <button className="link" onClick={() => change([t], { snoozedUntil: null })}>
            Unsnooze
          </button>
        </p>
      )}
    </header>
  )
}

function CopyLink({ text }: { text: string }) {
  const [copied, setCopied] = useState(false)
  return (
    <button onClick={() => navigator.clipboard.writeText(text).then(() => setCopied(true))}>{copied ? 'Copied' : 'Copy link'}</button>
  )
}

// ---------------------------------------------------------------------------
// §30: newest in view, the middle collapsed, day separators, a pill for news.
// ---------------------------------------------------------------------------

function dayLabel(iso: string) {
  const d = new Date(iso)
  const today = new Date()
  const yesterday = new Date()
  yesterday.setDate(today.getDate() - 1)
  if (d.toDateString() === today.toDateString()) return 'Today'
  if (d.toDateString() === yesterday.toDateString()) return 'Yesterday'
  return d.toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short' })
}

function Thread({ t }: { t: TicketDetail }) {
  const [expanded, setExpanded] = useState(false)
  const [unseen, setUnseen] = useState(0)
  const box = useRef<HTMLDivElement>(null)
  const count = useRef(0)
  const all = t.messages
  const hidden = !expanded && all.length > 6 ? all.length - 4 : 0
  const shown = hidden ? [all[0]!, ...all.slice(-3)] : all

  const atBottom = () => {
    const el = box.current
    return !el || el.scrollHeight - el.scrollTop - el.clientHeight < 40
  }
  const toBottom = () => {
    box.current?.scrollTo({ top: box.current.scrollHeight })
    setUnseen(0)
  }
  useLayoutEffect(() => {
    const added = all.length - count.current
    if (count.current === 0 || atBottom()) toBottom()
    else if (added > 0) setUnseen((n) => n + added)
    count.current = all.length
  }, [all.length])

  return (
    <div className="messages" ref={box} onScroll={() => atBottom() && setUnseen(0)}>
      {shown.map((m, i) => {
        const before = shown[i - 1]
        const newDay = !before || new Date(before.at).toDateString() !== new Date(m.at).toDateString()
        return (
          <div key={m.id} className="stack-tight">
            {newDay && <div className="day">{dayLabel(m.at)}</div>}
            <MessageView m={m} ticketId={t.id} />
            {hidden > 0 && i === 0 && (
              <button className="link collapsed" onClick={() => setExpanded(true)}>
                Show {hidden} earlier messages
              </button>
            )}
          </div>
        )
      })}
      {unseen > 0 && (
        <button className="arrived bottom" onClick={toBottom}>
          {unseen} new {unseen === 1 ? 'message' : 'messages'} ↓
        </button>
      )}
    </div>
  )
}

function MessageView({ m, ticketId }: { m: Message; ticketId: string }) {
  const qc = useQueryClient()
  const refresh = () => qc.invalidateQueries({ queryKey: ['ticket', ticketId] })
  const retry = useMutation({ mutationFn: () => api.post(`/messages/${m.id}/retry`), onSettled: refresh })
  // §35: Discard puts the text back in an empty composer.
  const discard = useMutation({
    mutationFn: () => api.del(`/messages/${m.id}`),
    onSuccess: () => composerBus.restore(ticketId, m.text, true),
    onSettled: refresh,
  })
  const d = m.delivery
  return (
    <article className={`msg msg-${m.kind}`}>
      <header>
        <strong>{m.author.name}</strong>
        {m.author.email !== m.author.name && <span className="muted"> {m.author.email}</span>}
        {m.kind === 'comment' && <span className="tag">Internal comment</span>}
        <RelTime iso={m.at}>{() => formatDate(m.at)}</RelTime>
      </header>
      {/* Mail is attacker-controlled: a plain text node, never HTML (spec 002). */}
      <div className="msg-text">{m.text}</div>
      {m.attachments.length > 0 && (
        <ul className="attachments">
          {m.attachments.map((a) => (
            <li key={a.id}>
              <a href={`/api/attachments/${a.id}`} download>
                {a.name}
              </a>{' '}
              <span className="muted">{formatSize(a.size)}</span>
            </li>
          ))}
        </ul>
      )}
      {d && d.status !== 'sent' && (
        <div className={`delivery delivery-${d.status}`}>
          {d.status === 'queued' && 'Sending…'}
          {d.status === 'failed' && <>Not sent{d.error ? ` — ${d.error}` : ''}. </>}
          {d.status === 'held' && 'Held — trial limit of 100 emails a day. Retry later. '}
          {(d.status === 'failed' || d.status === 'held') && (
            <>
              <button className="link" disabled={retry.isPending} onClick={() => retry.mutate()}>
                Retry
              </button>{' '}
              ·{' '}
              <button className="link" disabled={discard.isPending} onClick={() => discard.mutate()}>
                Discard
              </button>
            </>
          )}
          {discard.isError && <span className="error"> Could not discard it.</span>}
        </div>
      )}
    </article>
  )
}

// ---------------------------------------------------------------------------
// §32–37: drafts, send and close, undo send, new activity, snippets.
// ---------------------------------------------------------------------------

type DraftState = 'idle' | 'dirty' | 'saving' | 'saved' | 'failed'

/** §37: `{{contact.firstName}}` and friends; a missing value becomes empty. */
function fill(text: string, contact: string | null, agent: string) {
  const first = (n: string | null) => n?.trim().split(/\s+/)[0] ?? ''
  const values: Record<string, string> = {
    'contact.firstName': first(contact),
    'contact.name': contact ?? '',
    'agent.firstName': first(agent),
    'agent.name': agent,
  }
  return text.replace(/\{\{\s*([\w.]+)\s*\}\}/g, (_, k: string) => values[k] ?? '')
}

function Composer({ t }: { t: TicketDetail }) {
  const qc = useQueryClient()
  const me = useMe()
  const snippets = useSnippets()
  const [mode, setMode] = useState<'reply' | 'comment'>(t.draft?.mode ?? 'reply')
  const [text, setText] = useState(t.draft?.text ?? '')
  const [draft, setDraft] = useState<DraftState>('idle')
  const [conflict, setConflict] = useState(false)
  const [sending, setSending] = useState(false)
  const [sendError, setSendError] = useState<string | null>(null)
  const [picker, setPicker] = useState<{ query: string; start: number } | null>(null)
  const [pickAt, setPickAt] = useState(0)
  const area = useRef<HTMLTextAreaElement>(null)
  const pending = useRef({ mode, text, dirty: false })

  const saveDraft = () => {
    const body = { mode: pending.current.mode, text: pending.current.text }
    pending.current.dirty = false
    setDraft('saving')
    return api
      .put(`/tickets/${t.id}/draft`, body)
      .then(() => {
        setDraft(pending.current.dirty ? 'dirty' : 'saved')
        qc.invalidateQueries({ queryKey: ['tickets'] })
      })
      .catch(() => {
        pending.current.dirty = true
        setDraft('failed')
      })
  }
  const edit = (next: { mode?: 'reply' | 'comment'; text?: string }) => {
    if (next.mode) setMode(next.mode)
    if (next.text !== undefined) setText(next.text)
    pending.current = { mode: next.mode ?? pending.current.mode, text: next.text ?? pending.current.text, dirty: true }
    setDraft((d) => (d === 'failed' ? 'failed' : 'dirty'))
  }
  // Saved a second after typing stops; a failed save is retried on the next keystroke.
  useEffect(() => {
    if (!pending.current.dirty) return
    const timer = setTimeout(saveDraft, 1000)
    return () => clearTimeout(timer)
  }, [text, mode])
  // Moving to another ticket saves what is unsaved; leaving the page asks.
  useEffect(() => {
    const leave = (e: BeforeUnloadEvent) => {
      if (pending.current.dirty) e.preventDefault()
    }
    window.addEventListener('beforeunload', leave)
    return () => {
      window.removeEventListener('beforeunload', leave)
      if (pending.current.dirty) api.put(`/tickets/${t.id}/draft`, { mode: pending.current.mode, text: pending.current.text }).catch(() => {})
    }
  }, [t.id])
  useEffect(
    () =>
      composerBus.register(t.id, {
        open: (m) => {
          edit({ mode: m })
          area.current?.focus()
        },
        restore: (value, onlyIfEmpty) => {
          if (onlyIfEmpty && pending.current.text.trim()) return
          edit({ mode: 'reply', text: value })
        },
      }),
    [t.id],
  )

  const newest = t.messages.at(-1)?.id
  const send = async (close: boolean) => {
    const body = text.trim()
    if (!body || sending) return
    setSending(true)
    setSendError(null)
    // A reply only ever assigns an unassigned ticket; an owner it kept (even
    // one since removed from the team) needs no putting back.
    const before = { status: t.status, ...(t.owner ? {} : { ownerId: null }) }
    try {
      if (mode === 'comment') {
        await api.post(`/tickets/${t.id}/comments`, { text: body })
      } else {
        const req: ReplyRequest = { text: body, ...(close ? { status: 'closed' } : {}), ...(conflict ? {} : { after: newest }) }
        const m = await api.post<Message>(`/tickets/${t.id}/replies`, req)
        toast('Sending…', () => undoSend(m.id, body, before), 10_000)
      }
      pending.current = { mode, text: '', dirty: false }
      setText('')
      setDraft('idle')
      setConflict(false)
    } catch (e) {
      const code = errorCode(e)
      if (code === 'newActivity') setConflict(true)
      else setSendError(code === 'emptyText' ? 'Write something first.' : 'Could not send. Try again.')
    } finally {
      setSending(false)
      qc.invalidateQueries({ queryKey: ['ticket', t.id] })
      qc.invalidateQueries({ queryKey: ['tickets'] })
    }
  }
  // §34: the reply, its text back in the composer, the status and owner as they were.
  const undoSend = async (id: string, body: string, before: Pick<PatchTicketsItem, 'status' | 'ownerId'>) => {
    try {
      await api.del(`/messages/${id}`)
    } catch (e) {
      toast(errorCode(e) === 'notDeletable' ? 'Too late — the reply is already on its way.' : 'Could not undo. Try again.')
      return
    }
    if (!composerBus.restore(t.id, body, false)) await api.put(`/tickets/${t.id}/draft`, { mode: 'reply', text: body }).catch(() => {})
    await api.patch('/tickets', { tickets: [{ id: t.id, ...before }] }).catch(() => toast('Could not put the status back.'))
    qc.invalidateQueries({ queryKey: ['ticket', t.id] })
    qc.invalidateQueries({ queryKey: ['tickets'] })
  }

  // §37: `#` at the start of a word, or the Snippets button.
  const agentName = me.data?.agent.name ?? ''
  const matches = (snippets.data?.snippets ?? []).filter((s) =>
    `${s.name} ${s.text}`.toLowerCase().includes((picker?.query ?? '').toLowerCase()),
  )
  const insert = (s: Snippet) => {
    const el = area.current
    const at = el?.selectionStart ?? text.length
    const start = picker && picker.start >= 0 ? picker.start : at
    const value = fill(s.text, t.contact.name, agentName)
    edit({ text: text.slice(0, start) + value + text.slice(at) })
    setPicker(null)
    requestAnimationFrame(() => {
      el?.focus()
      el?.setSelectionRange(start + value.length, start + value.length)
    })
  }
  const onType = (value: string, caret: number) => {
    edit({ text: value })
    const m = /(^|\s)#([^\s#]*)$/.exec(value.slice(0, caret))
    setPicker(m ? { query: m[2]!, start: caret - m[2]!.length - 1 } : null)
    setPickAt(0)
  }

  const replying = t.viewers.filter((v) => v.replying)
  const label = mode === 'reply' ? (conflict ? 'Send anyway' : 'Send') : 'Add comment'
  return (
    <form
      className={`composer composer-${mode}`}
      onSubmit={(e) => {
        e.preventDefault()
        send(false)
      }}
    >
      {replying.length > 0 && (
        <p className="muted small">
          {replying.map((v) => v.name).join(', ')} {replying.length === 1 ? 'is' : 'are'} replying
        </p>
      )}
      {conflict && (
        <p className="conflict" role="alert">
          New activity on this ticket. Read the new messages above, then send anyway or edit your reply.
        </p>
      )}
      <div className="tabs" role="tablist">
        <button type="button" role="tab" aria-selected={mode === 'reply'} onClick={() => edit({ mode: 'reply' })}>
          Reply
        </button>
        <button type="button" role="tab" aria-selected={mode === 'comment'} onClick={() => edit({ mode: 'comment' })}>
          Internal comment
        </button>
        <span className="spacer" />
        <Dropdown label="Snippets" className="link">
          {(close) => (
            <PickList
              placeholder="Find a snippet…"
              options={(snippets.data?.snippets ?? []).map((s) => ({ value: s.id, label: s.name, keywords: s.text }))}
              onClose={close}
              onPick={(id) => {
                close()
                const s = snippets.data?.snippets.find((s) => s.id === id)
                if (s) insert(s)
              }}
            />
          )}
        </Dropdown>
      </div>
      <div className="composer-box">
        <textarea
          ref={area}
          aria-label={mode === 'reply' ? 'Reply to the contact' : 'Internal comment, never emailed'}
          placeholder={mode === 'reply' ? 'Write a reply… Type # for snippets.' : 'Only your team sees this.'}
          value={text}
          rows={5}
          onChange={(e) => onType(e.target.value, e.target.selectionStart)}
          onKeyDown={(e) => {
            if (picker && matches.length) {
              if (e.key === 'ArrowDown') setPickAt((a) => Math.min(a + 1, matches.length - 1))
              else if (e.key === 'ArrowUp') setPickAt((a) => Math.max(a - 1, 0))
              else if (e.key === 'Enter' && !(e.metaKey || e.ctrlKey)) insert(matches[pickAt]!)
              else if (e.key === 'Escape') setPicker(null)
              else return
              e.preventDefault()
              e.stopPropagation()
              return
            }
            if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
              e.preventDefault()
              send(e.shiftKey && mode === 'reply')
            } else if (e.key === 'Escape') {
              e.stopPropagation()
              e.currentTarget.blur()
            }
          }}
        />
        {picker && matches.length > 0 && (
          <ul className="picklist snippet-picker" role="listbox" aria-label="Snippets">
            {matches.slice(0, 8).map((s, i) => (
              <li
                key={s.id}
                role="option"
                aria-selected={i === pickAt}
                className={i === pickAt ? 'at' : ''}
                onMouseDown={(e) => {
                  e.preventDefault()
                  insert(s)
                }}
              >
                <strong>{s.name}</strong> <span className="muted small">{s.text.slice(0, 60)}</span>
              </li>
            ))}
          </ul>
        )}
      </div>
      <div className="composer-foot">
        <span className="muted small" aria-live="polite">
          {draft === 'saved' && 'Draft saved'}
          {draft === 'failed' && <span className="error">Draft not saved</span>}
        </span>
        <span className="spacer" />
        {sendError && <span className="error">{sendError}</span>}
        <span className="split-button">
          <button className="primary" disabled={sending || !text.trim()} title={mode === 'reply' ? 'Ctrl/⌘ Enter' : undefined}>
            {label}
          </button>
          {mode === 'reply' && (
            <Dropdown label="▾" className="primary" title="More send options" disabled={sending || !text.trim()}>
              {(close) => (
                <div className="menu">
                  <button
                    type="button"
                    onClick={() => {
                      close()
                      send(true)
                    }}
                  >
                    Send and close <kbd>Ctrl/⌘ Shift Enter</kbd>
                  </button>
                </div>
              )}
            </Dropdown>
          )}
        </span>
      </div>
    </form>
  )
}

// ---------------------------------------------------------------------------
// Sidebar (spec 002 §15): every change goes through the undoable path (§24).
// ---------------------------------------------------------------------------

function Sidebar({ t, view }: { t: TicketDetail; view: string }) {
  const agents = useAgents()
  const categories = useCategories()
  const change = useChangeTickets()
  const inbox = useInbox()
  // An archived category is not offered, but a ticket that has it keeps showing it (spec 004 §3).
  const options = (categories.data?.categories ?? []).filter((c) => !c.archived || c.id === t.category?.id)
  // Spec 007 §25–26: HubSpot owns these; the category stays Muninn's own.
  const fromHubspot = t.hubspot !== null

  return (
    <aside className="sidebar" aria-label="Ticket details">
      <section>
        <h2>Ticket</h2>
        <label>
          Status
          <select value={t.status} disabled={fromHubspot} onChange={(e) => change([t], { status: e.target.value as Ticket['status'] })}>
            {Object.entries(STATUS_LABELS).map(([k, v]) => (
              <option key={k} value={k}>
                {v}
              </option>
            ))}
          </select>
        </label>
        <label>
          Owner
          <select value={t.owner?.id ?? ''} disabled={fromHubspot} onChange={(e) => change([t], { ownerId: e.target.value || null })}>
            <option value="">Unassigned</option>
            {agents.data?.agents.map((a) => (
              <option key={a.id} value={a.id}>
                {a.name}
              </option>
            ))}
            {t.owner && !agents.data?.agents.some((a) => a.id === t.owner?.id) && <option value={t.owner.id}>{t.owner.name}</option>}
          </select>
        </label>
        <label>
          Priority
          <select value={t.priority ?? ''} disabled={fromHubspot} onChange={(e) => change([t], { priority: (e.target.value || null) as Ticket['priority'] })}>
            <option value="">None</option>
            {Object.entries(PRIORITY_LABELS).map(([k, v]) => (
              <option key={k} value={k}>
                {v}
              </option>
            ))}
          </select>
        </label>
        <label>
          Category
          <select value={t.category?.id ?? ''} onChange={(e) => change([t], { categoryId: e.target.value || null })}>
            <option value="">None</option>
            {options.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </select>
        </label>
        {fromHubspot && <p className="hint">Status, owner and priority come from HubSpot.</p>}
        {t.category?.source === 'jev' && t.category.probability !== null && <p className="hint">Set by Jev · {pct(t.category.probability)}</p>}
        {!t.category && t.categorySuggestions.length > 0 && (
          <p className="chips">
            {t.categorySuggestions.map((s, i) => (
              <span key={s.id}>
                {i > 0 && ' · '}
                <button className="chip" onClick={() => change([t], { categoryId: s.id })}>
                  {s.name} {pct(s.probability)}
                </button>
              </span>
            ))}
          </p>
        )}
        <Waiting t={t} />
      </section>
      <section>
        <h2>Contact</h2>
        <div className="contact-card">
          <Avatar name={t.contact.name || t.contact.email} email={t.contact.email} />
          <div>
            {t.contact.name && <div>{t.contact.name}</div>}
            <div className="muted">
              {t.contact.email} <CopyButton text={t.contact.email} />
            </div>
          </div>
        </div>
        {t.contactTickets.length > 0 && (
          <>
            <h3>Other tickets</h3>
            <ul className="plain">
              {t.contactTickets.map((c) => (
                <li key={c.id}>
                  <Link to="/inbox/$view/$ticketId" params={{ view, ticketId: c.id }} search={{ ...inbox.search, from: undefined }}>
                    {c.subject}
                  </Link>{' '}
                  <span className="muted">
                    {STATUS_LABELS[c.status]} · {ago(c.createdAt)}
                  </span>
                </li>
              ))}
            </ul>
          </>
        )}
      </section>
      <Copilot ticketId={t.id} view={view} />
    </aside>
  )
}
