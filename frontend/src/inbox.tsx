// Spec 002: views, the ticket list, the thread, the sidebar — laid out like
// HubSpot Help Desk.

import { useState } from 'react'
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link, Outlet, useParams, useSearch } from '@tanstack/react-router'
import {
  PRIORITY_LABELS,
  STATUS_LABELS,
  ago,
  api,
  errorCode,
  formatDate,
  formatSize,
  pct,
  useAgents,
  useCategories,
  useMe,
} from './client'
import { ForwardingInstructions } from './auth'
import { Copilot } from './copilot'
import type { Message } from './api/types/Message'
import type { PatchTicket } from './api/types/PatchTicket'
import type { Ticket } from './api/types/Ticket'
import type { TicketDetail } from './api/types/TicketDetail'
import type { TicketList } from './api/types/TicketList'

const POLL = 10_000 // ADR 0007

const VIEWS = [
  { view: 'unassigned', label: 'Unassigned', count: 'unassigned' },
  { view: 'mine', label: 'Assigned to me', count: 'mine' },
  { view: 'open', label: 'All open', count: 'open' },
  { view: 'closed', label: 'Closed', count: null },
] as const

const EMPTY: Record<string, string> = {
  unassigned: 'Nothing unassigned. Nice.',
  mine: 'Nothing assigned to you.',
  open: 'No open tickets.',
  closed: 'No closed tickets yet.',
}

function StatusPill({ status }: { status: Ticket['status'] }) {
  return <span className={`pill status-${status}`}>{STATUS_LABELS[status]}</span>
}

export function Inbox() {
  const { view = 'unassigned', ticketId } = useParams({ strict: false })
  const me = useMe()
  const list = useInfiniteQuery({
    queryKey: ['tickets', view],
    queryFn: ({ pageParam }) =>
      api.get<TicketList>(`/tickets?view=${view}${pageParam ? `&cursor=${encodeURIComponent(pageParam)}` : ''}`),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.nextCursor,
    refetchInterval: POLL,
  })
  const first = list.data?.pages[0]
  const tickets = list.data?.pages.flatMap((p) => p.tickets) ?? []

  return (
    <div className={`inbox ${ticketId ? 'has-ticket' : ''}`}>
      <nav className="views">
        <h2>Inbox</h2>
        {VIEWS.map((v) => (
          <Link key={v.view} to="/inbox/$view" params={{ view: v.view }} className={v.view === view ? 'active' : ''}>
            <span>{v.label}</span>
            {v.count && first && <span className="count">{first.counts[v.count]}</span>}
          </Link>
        ))}
      </nav>
      <section className="list" aria-label="Tickets">
        {list.isError && <p className="pad error">Could not load tickets.</p>}
        {first && !first.hasTickets && me.data ? (
          <div className="pad">
            <h3>No mail yet</h3>
            <ForwardingInstructions address={me.data.workspace.inboundAddress} />
          </div>
        ) : (
          first && tickets.length === 0 && <p className="pad muted">{EMPTY[view] ?? 'Nothing here.'}</p>
        )}
        {tickets.map((t) => (
          <Link
            key={t.id}
            to="/inbox/$view/$ticketId"
            params={{ view, ticketId: t.id }}
            className={`row ${t.id === ticketId ? 'active' : ''}`}
          >
            <div className="row-top">
              <strong>{t.contact.name || t.contact.email}</strong>
              <time dateTime={t.lastActivityAt}>{ago(t.lastActivityAt)}</time>
            </div>
            <div className="subject">
              {t.seenBefore && (
                <span className="seen" title="Seen before: the brain has similar solved cases">
                  ◆
                </span>
              )}
              {t.subject}
            </div>
            <div className="snippet">{t.lastMessage.snippet}</div>
            <div className="meta">
              <StatusPill status={t.status} />
              {t.priority && <span className={`pill priority-${t.priority}`}>{PRIORITY_LABELS[t.priority]}</span>}
              <span>{t.owner?.name ?? 'Unassigned'}</span>
              {t.category && <span className="pill">{t.category.name}</span>}
            </div>
          </Link>
        ))}
        {list.hasNextPage && (
          <button className="load-more" disabled={list.isFetchingNextPage} onClick={() => list.fetchNextPage()}>
            Load more
          </button>
        )}
      </section>
      <Outlet />
    </div>
  )
}

export function NoTicket() {
  return <div className="empty-pane muted">Select a ticket.</div>
}

export function TicketPane() {
  const { view = 'open', ticketId = '' } = useParams({ strict: false })
  const { from } = useSearch({ strict: false })
  const ticket = useQuery({
    queryKey: ['ticket', ticketId],
    queryFn: () => api.get<TicketDetail>(`/tickets/${ticketId}`),
    refetchInterval: POLL,
  })

  if (ticket.isError)
    return <div className="empty-pane muted">{errorCode(ticket.error) === 'notFound' ? 'Ticket not found.' : 'Could not load the ticket.'}</div>
  if (!ticket.data) return <div className="empty-pane muted">Loading…</div>
  const t = ticket.data

  return (
    <>
      <section className="thread" aria-label="Conversation">
        <header className="thread-head">
          {from && (
            <Link to="/inbox/$view/$ticketId" params={{ view, ticketId: from }} className="back">
              ← Back to ticket
            </Link>
          )}
          <h1>{t.subject}</h1>
          <StatusPill status={t.status} />
        </header>
        <div className="messages">
          {t.messages.map((m) => (
            <MessageView key={m.id} m={m} ticketId={t.id} />
          ))}
        </div>
        <Composer ticketId={t.id} />
      </section>
      <Sidebar t={t} view={view} />
    </>
  )
}

function MessageView({ m, ticketId }: { m: Message; ticketId: string }) {
  const qc = useQueryClient()
  const retry = useMutation({
    mutationFn: () => api.post(`/messages/${m.id}/retry`),
    onSettled: () => qc.invalidateQueries({ queryKey: ['ticket', ticketId] }),
  })
  const d = m.delivery
  return (
    <article className={`msg msg-${m.kind}`}>
      <header>
        <strong>{m.author.name}</strong>
        {m.author.email !== m.author.name && <span className="muted"> {m.author.email}</span>}
        {m.kind === 'comment' && <span className="tag">Internal comment</span>}
        <time dateTime={m.at}>{formatDate(m.at)}</time>
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
            <button className="link" disabled={retry.isPending} onClick={() => retry.mutate()}>
              Retry
            </button>
          )}
        </div>
      )}
    </article>
  )
}

function Composer({ ticketId }: { ticketId: string }) {
  const [mode, setMode] = useState<'reply' | 'comment'>('reply')
  const [text, setText] = useState('')
  const qc = useQueryClient()
  const send = useMutation({
    mutationFn: () => api.post(`/tickets/${ticketId}/${mode === 'reply' ? 'replies' : 'comments'}`, { text }),
    onSuccess: () => {
      setText('')
      qc.invalidateQueries({ queryKey: ['ticket', ticketId] })
      qc.invalidateQueries({ queryKey: ['tickets'] })
    },
  })
  return (
    <form
      className={`composer composer-${mode}`}
      onSubmit={(e) => {
        e.preventDefault()
        if (text.trim()) send.mutate()
      }}
    >
      <div className="tabs" role="tablist">
        <button type="button" role="tab" aria-selected={mode === 'reply'} onClick={() => setMode('reply')}>
          Reply
        </button>
        <button type="button" role="tab" aria-selected={mode === 'comment'} onClick={() => setMode('comment')}>
          Internal comment
        </button>
      </div>
      <textarea
        aria-label={mode === 'reply' ? 'Reply to the contact' : 'Internal comment, never emailed'}
        placeholder={mode === 'reply' ? 'Write a reply…' : 'Only your team sees this.'}
        value={text}
        rows={5}
        onChange={(e) => setText(e.target.value)}
      />
      <div className="composer-foot">
        {send.isError && <span className="error">Could not send. Try again.</span>}
        <button className="primary" disabled={send.isPending || !text.trim()}>
          {mode === 'reply' ? 'Send' : 'Add comment'}
        </button>
      </div>
    </form>
  )
}

function Sidebar({ t, view }: { t: TicketDetail; view: string }) {
  const qc = useQueryClient()
  const agents = useAgents()
  const categories = useCategories()
  const patch = useMutation({
    mutationFn: (body: PatchTicket) => api.patch<Ticket>(`/tickets/${t.id}`, body),
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ['ticket', t.id] })
      qc.invalidateQueries({ queryKey: ['tickets'] })
      qc.invalidateQueries({ queryKey: ['suggestions'] })
    },
  })
  // An archived category is not offered, but a ticket that has it keeps showing it (spec 004 §3).
  const options = (categories.data?.categories ?? []).filter((c) => !c.archived || c.id === t.category?.id)

  return (
    <aside className="sidebar" aria-label="Ticket details">
      <section>
        <h2>Ticket</h2>
        <label>
          Status
          <select value={t.status} onChange={(e) => patch.mutate({ status: e.target.value as Ticket['status'] })}>
            {Object.entries(STATUS_LABELS).map(([k, v]) => (
              <option key={k} value={k}>
                {v}
              </option>
            ))}
          </select>
        </label>
        <label>
          Owner
          <select value={t.owner?.id ?? ''} onChange={(e) => patch.mutate({ ownerId: e.target.value || null })}>
            <option value="">Unassigned</option>
            {agents.data?.agents.map((a) => (
              <option key={a.id} value={a.id}>
                {a.name}
              </option>
            ))}
            {t.owner && !agents.data?.agents.some((a) => a.id === t.owner?.id) && (
              <option value={t.owner.id}>{t.owner.name}</option>
            )}
          </select>
        </label>
        <label>
          Priority
          <select
            value={t.priority ?? ''}
            onChange={(e) => patch.mutate({ priority: (e.target.value || null) as Ticket['priority'] })}
          >
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
          <select value={t.category?.id ?? ''} onChange={(e) => patch.mutate({ categoryId: e.target.value || null })}>
            <option value="">None</option>
            {options.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </select>
        </label>
        {t.category?.source === 'jev' && t.category.probability !== null && (
          <p className="hint">Set by Jev · {pct(t.category.probability)}</p>
        )}
        {!t.category && t.categorySuggestions.length > 0 && (
          <p className="chips">
            {t.categorySuggestions.map((s, i) => (
              <span key={s.id}>
                {i > 0 && ' · '}
                <button className="chip" onClick={() => patch.mutate({ categoryId: s.id })}>
                  {s.name} {pct(s.probability)}
                </button>
              </span>
            ))}
          </p>
        )}
        {patch.isError && <p className="error">Could not save. Try again.</p>}
      </section>
      <section>
        <h2>Contact</h2>
        {t.contact.name && <div>{t.contact.name}</div>}
        <div className="muted">{t.contact.email}</div>
        {t.contactTickets.length > 0 && (
          <>
            <h3>Other tickets</h3>
            <ul className="plain">
              {t.contactTickets.map((c) => (
                <li key={c.id}>
                  <Link to="/inbox/$view/$ticketId" params={{ view, ticketId: c.id }}>
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
