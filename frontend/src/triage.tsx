// Spec 006: the pieces the list, the ticket and the keyboard share — toasts
// with Undo, optimistic ticket changes, menus that filter as you type,
// avatars, times, the command palette and the shortcut sheet.

import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from 'react'
import { useQueryClient, type InfiniteData, type QueryClient } from '@tanstack/react-query'
import {
  PRIORITY_LABELS,
  STATUS_LABELS,
  VIEW_LABELS,
  api,
  avatarColor,
  errorCode,
  formatDate,
  formatSnooze,
  initials,
  snoozeTimes,
  useAgents,
  useCategories,
  useMe,
  useNow,
} from './client'
import type { AgentsResponse } from './api/types/AgentsResponse'
import type { CategoriesResponse } from './api/types/CategoriesResponse'
import type { PatchTicket } from './api/types/PatchTicket'
import type { PatchTicketsItem as Item } from './api/types/PatchTicketsItem'
import type { Ticket } from './api/types/Ticket'
import type { TicketDetail } from './api/types/TicketDetail'
import type { TicketList } from './api/types/TicketList'
import type { TicketsResponse } from './api/types/TicketsResponse'

// ---------------------------------------------------------------------------
// Toasts (§24): bottom-left, at most three, paused while hovered, announced.
// ---------------------------------------------------------------------------

type Toast = { id: number; text: string; undo?: () => void; ms: number }
let toasts: Toast[] = []
let nextId = 1
const listeners = new Set<() => void>()
const emit = () => listeners.forEach((f) => f())

export function toast(text: string, undo?: () => void, ms = 8000) {
  toasts = [...toasts, { id: nextId++, text, undo, ms }].slice(-3)
  emit()
}
const dismiss = (id: number) => {
  toasts = toasts.filter((t) => t.id !== id)
  emit()
}
/** `z`: undo the most recent change that can still be undone. */
export function undoLatest() {
  const t = [...toasts].reverse().find((t) => t.undo)
  if (!t) return
  dismiss(t.id)
  t.undo!()
}

export function Toasts() {
  const list = useSyncExternalStore(
    (f) => {
      listeners.add(f)
      return () => void listeners.delete(f)
    },
    () => toasts,
  )
  return (
    <div className="toasts" role="status" aria-live="polite">
      {list.map((t) => (
        <ToastView key={t.id} t={t} />
      ))}
    </div>
  )
}

function ToastView({ t }: { t: Toast }) {
  const [hovered, setHovered] = useState(false)
  useEffect(() => {
    if (hovered) return
    const timer = setTimeout(() => dismiss(t.id), t.ms)
    return () => clearTimeout(timer)
  }, [hovered, t.id, t.ms])
  return (
    <div className="toast" onMouseEnter={() => setHovered(true)} onMouseLeave={() => setHovered(false)}>
      <span>{t.text}</span>
      {t.undo && (
        <button
          className="link"
          onClick={() => {
            dismiss(t.id)
            t.undo!()
          }}
        >
          Undo
        </button>
      )}
      <button className="link muted" aria-label="Dismiss" onClick={() => dismiss(t.id)}>
        ×
      </button>
    </div>
  )
}

// ---------------------------------------------------------------------------
// Changing tickets (§23–24): shown at once, saved through PATCH /api/tickets,
// reverted on failure, undone by sending each ticket's previous values.
// ---------------------------------------------------------------------------


const CHANGE_ERRORS: Record<string, string> = {
  ticketClosed: 'A closed ticket cannot be snoozed.',
  invalidSnooze: 'Pick a time in the future, at most a year away.',
  unknownAgent: 'That agent is no longer on the team.',
  unknownCategory: 'That category is archived or gone.',
  notFound: 'One of the tickets no longer exists.',
  invalidBatch: 'At most 200 tickets at a time.',
}
export const changeError = (code: string | null) =>
  `Could not save. ${(code && CHANGE_ERRORS[code]) || 'Try again.'}`

function patched(t: Ticket, p: PatchTicket, qc: QueryClient): Ticket {
  const agents = qc.getQueryData<AgentsResponse>(['agents'])?.agents ?? []
  const categories = qc.getQueryData<CategoriesResponse>(['categories'])?.categories ?? []
  const next = { ...t }
  if (p.status !== undefined) {
    next.status = p.status
    if (p.status === 'closed') next.snoozedUntil = null
  }
  if (p.ownerId !== undefined) {
    const a = agents.find((a) => a.id === p.ownerId)
    next.owner = p.ownerId ? { id: p.ownerId, name: a?.name ?? t.owner?.name ?? '' } : null
  }
  if (p.priority !== undefined) next.priority = p.priority
  if (p.categoryId !== undefined) {
    const c = categories.find((c) => c.id === p.categoryId)
    next.category = p.categoryId ? { id: p.categoryId, name: c?.name ?? '', source: 'agent', probability: null } : null
  }
  if (p.snoozedUntil !== undefined) next.snoozedUntil = p.snoozedUntil
  return next
}

/** Rewrites the tickets in every cached list and detail; returns the undo of that. */
export function rewrite(qc: QueryClient, f: (t: Ticket) => Ticket) {
  const saved = [...qc.getQueriesData({ queryKey: ['tickets'] }), ...qc.getQueriesData({ queryKey: ['ticket'] })]
  qc.setQueriesData<InfiniteData<TicketList>>({ queryKey: ['tickets'] }, (d) =>
    d?.pages ? { ...d, pages: d.pages.map((p) => ({ ...p, tickets: p.tickets.map(f) })) } : d,
  )
  qc.setQueriesData<TicketDetail>({ queryKey: ['ticket'] }, (d) => (d?.id ? { ...d, ...f(d) } : d))
  return () => saved.forEach(([key, data]) => qc.setQueryData(key, data))
}

async function save(qc: QueryClient, items: Item[]): Promise<string | null> {
  await qc.cancelQueries({ queryKey: ['tickets'] })
  await qc.cancelQueries({ queryKey: ['ticket'] })
  const byId = new Map(items.map((i) => [i.id, i]))
  const revert = rewrite(qc, (t) => {
    const i = byId.get(t.id)
    return i ? patched(t, i, qc) : t
  })
  try {
    const res = await api.patch<TicketsResponse>('/tickets', { tickets: items })
    const fresh = new Map(res.tickets.map((t) => [t.id, t]))
    rewrite(qc, (t) => fresh.get(t.id) ?? t)
    return null
  } catch (e) {
    revert()
    return errorCode(e)
  } finally {
    qc.invalidateQueries({ queryKey: ['tickets'] })
    qc.invalidateQueries({ queryKey: ['ticket'] })
  }
}

function previous(t: Ticket, p: PatchTicket): Item {
  const item: Item = { id: t.id }
  if (p.status !== undefined || p.snoozedUntil !== undefined) {
    item.status = t.status
    item.snoozedUntil = t.snoozedUntil
  }
  if (p.ownerId !== undefined) item.ownerId = t.owner?.id ?? null
  if (p.priority !== undefined) item.priority = t.priority
  if (p.categoryId !== undefined) item.categoryId = t.category?.id ?? null
  return item
}

const which = (ts: Ticket[]) => (ts.length === 1 ? `#${ts[0]!.number}` : `${ts.length} tickets`)

function describe(ts: Ticket[], p: PatchTicket, qc: QueryClient) {
  const agents = qc.getQueryData<AgentsResponse>(['agents'])?.agents ?? []
  const categories = qc.getQueryData<CategoriesResponse>(['categories'])?.categories ?? []
  if (p.snoozedUntil !== undefined)
    return p.snoozedUntil ? `Snoozed ${which(ts)} until ${formatSnooze(p.snoozedUntil)}` : `Unsnoozed ${which(ts)}`
  if (p.status === 'closed') return `Closed ${which(ts)}`
  if (p.status) return `${STATUS_LABELS[p.status]}: ${which(ts)}`
  if (p.ownerId !== undefined)
    return p.ownerId ? `Assigned ${which(ts)} to ${agents.find((a) => a.id === p.ownerId)?.name ?? 'them'}` : `Unassigned ${which(ts)}`
  if (p.priority !== undefined) return `Priority ${p.priority ? PRIORITY_LABELS[p.priority] : 'none'}: ${which(ts)}`
  if (p.categoryId !== undefined)
    return `Category ${categories.find((c) => c.id === p.categoryId)?.name ?? 'none'}: ${which(ts)}`
  return `Changed ${which(ts)}`
}

/** One change to one or many tickets. Resolves to the error code, or null. */
export function useChangeTickets() {
  const qc = useQueryClient()
  return useCallback(
    async (ts: Ticket[], p: PatchTicket) => {
      if (!ts.length) return null
      const before = ts.map((t) => previous(t, p))
      const error = await save(
        qc,
        ts.map((t) => ({ id: t.id, ...p })),
      )
      if (error) toast(changeError(error))
      else
        toast(describe(ts, p, qc), async () => {
          const e = await save(qc, before)
          if (e) toast(changeError(e))
        })
      return error
    },
    [qc],
  )
}

// ---------------------------------------------------------------------------
// People and times
// ---------------------------------------------------------------------------

export function Avatar({ name, email, small }: { name: string; email: string; small?: boolean }) {
  return (
    <span className={`avatar ${small ? 'small' : ''}`} style={{ background: avatarColor(email) }} title={name} aria-hidden>
      {initials(name || email)}
    </span>
  )
}

/** An agent's avatar, coloured by their email like everywhere else. */
export function AgentAvatar({ id, name, small }: { id: string; name: string; small?: boolean }) {
  const agents = useAgents()
  const email = agents.data?.agents.find((a) => a.id === id)?.email ?? id
  return <Avatar name={name} email={email} small={small} />
}

/** §43: ticks every minute; the exact time on hover. */
export function RelTime({ iso, children }: { iso: string; children: (now: number) => string }) {
  const now = useNow()
  return (
    <time dateTime={iso} title={formatDate(iso)}>
      {children(now)}
    </time>
  )
}

/** Search matches marked by splitting the text — mail stays a text node. */
export function Highlight({ text, q }: { text: string; q?: string | null }) {
  const needle = q?.trim().toLowerCase()
  if (!needle) return <>{text}</>
  const parts: ReactNode[] = []
  const lower = text.toLowerCase()
  let at = 0
  for (let i = lower.indexOf(needle); i >= 0; i = lower.indexOf(needle, at)) {
    if (i > at) parts.push(text.slice(at, i))
    parts.push(<mark key={i}>{text.slice(i, i + needle.length)}</mark>)
    at = i + needle.length
  }
  parts.push(text.slice(at))
  return <>{parts}</>
}

// ---------------------------------------------------------------------------
// Menus: filter as you type, arrows move, Enter picks, Esc closes (§39).
// ---------------------------------------------------------------------------

export type Option = { value: string; label: string; hint?: string; divider?: boolean; keywords?: string }

export function PickList({
  options,
  selected,
  onPick,
  onClose,
  placeholder = 'Search…',
}: {
  options: Option[]
  selected?: string[]
  onPick: (value: string) => void
  onClose: () => void
  placeholder?: string
}) {
  const [filter, setFilter] = useState('')
  const [at, setAt] = useState(0)
  const shown = options.filter((o) => `${o.label} ${o.keywords ?? ''}`.toLowerCase().includes(filter.toLowerCase()))
  const pick = (o?: Option) => o && onPick(o.value)
  return (
    <div className="picklist" onClick={(e) => e.stopPropagation()}>
      <input
        autoFocus
        aria-label={placeholder}
        placeholder={placeholder}
        value={filter}
        onChange={(e) => {
          setFilter(e.target.value)
          setAt(0)
        }}
        onKeyDown={(e) => {
          if (e.key === 'ArrowDown') setAt((a) => Math.min(a + 1, shown.length - 1))
          else if (e.key === 'ArrowUp') setAt((a) => Math.max(a - 1, 0))
          else if (e.key === 'Enter') pick(shown[at])
          else if (e.key === 'Escape') onClose()
          else return
          e.preventDefault()
          e.stopPropagation()
        }}
      />
      <ul role="listbox" aria-multiselectable={!!selected}>
        {shown.map((o, i) => (
          <li
            key={o.value}
            role="option"
            aria-selected={selected ? selected.includes(o.value) : i === at}
            className={`${i === at ? 'at' : ''} ${o.divider ? 'divider' : ''}`}
            onMouseEnter={() => setAt(i)}
            onClick={() => pick(o)}
          >
            {selected && <input type="checkbox" readOnly tabIndex={-1} checked={selected.includes(o.value)} />}
            <span>{o.label}</span>
            {o.hint && <kbd>{o.hint}</kbd>}
          </li>
        ))}
        {shown.length === 0 && <li className="muted">No matches</li>}
      </ul>
    </div>
  )
}

/** A button and its popover; closes on outside click. */
export function Dropdown({
  label,
  className = '',
  disabled,
  title,
  children,
}: {
  label: ReactNode
  className?: string
  disabled?: boolean
  title?: string
  children: (close: () => void) => ReactNode
}) {
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (!open) return
    const out = (e: MouseEvent) => ref.current && !ref.current.contains(e.target as Node) && setOpen(false)
    document.addEventListener('mousedown', out)
    return () => document.removeEventListener('mousedown', out)
  }, [open])
  return (
    <div className="dropdown" ref={ref}>
      <button
        type="button"
        className={className}
        disabled={disabled}
        title={title}
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        {label}
      </button>
      {open && <div className="popover">{children(() => setOpen(false))}</div>}
    </div>
  )
}

export type ActionKind = 'assign' | 'status' | 'priority' | 'category' | 'snooze'

export const ACTION_LABELS: Record<ActionKind, string> = {
  assign: 'Assign',
  status: 'Status',
  priority: 'Priority',
  category: 'Category',
  snooze: 'Snooze',
}

/** Assign, status, priority, category or snooze for these tickets. */
export function ActionMenu({
  kind,
  tickets,
  onClose,
  onDone,
}: {
  kind: ActionKind
  tickets: Ticket[]
  onClose: () => void
  onDone: (error: string | null) => void
}) {
  const me = useMe()
  const agents = useAgents()
  const categories = useCategories()
  const change = useChangeTickets()
  const [picking, setPicking] = useState(false)
  const [when, setWhen] = useState('')
  const apply = async (p: PatchTicket) => onDone(await change(tickets, p))

  if (kind === 'snooze') {
    if (tickets.some((t) => t.status === 'closed'))
      return <p className="pad muted small">A closed ticket cannot be snoozed.</p>
    if (picking)
      return (
        <form
          className="stack pad"
          onSubmit={(e) => {
            e.preventDefault()
            if (when) apply({ snoozedUntil: new Date(when).toISOString() })
          }}
        >
          <label>
            Snooze until
            <input type="datetime-local" autoFocus required value={when} onChange={(e) => setWhen(e.target.value)} />
          </label>
          <button className="primary">Snooze</button>
        </form>
      )
    const options: Option[] = snoozeTimes().map((s) => ({
      value: s.at.toISOString(),
      label: s.label,
      hint: formatSnooze(s.at.toISOString()),
    }))
    if (tickets.some((t) => t.snoozedUntil)) options.push({ value: 'unsnooze', label: 'Unsnooze' })
    options.push({ value: 'pick', label: 'Pick a date and time', divider: true })
    return (
      <PickList
        options={options}
        onClose={onClose}
        onPick={(v) => (v === 'pick' ? setPicking(true) : apply({ snoozedUntil: v === 'unsnooze' ? null : v }))}
      />
    )
  }

  let options: Option[] = []
  if (kind === 'assign') {
    const myId = me.data?.agent.id
    const others = (agents.data?.agents ?? []).filter((a) => a.id !== myId)
    options = [
      ...(myId ? [{ value: myId, label: `Me (${me.data!.agent.name})` }] : []),
      { value: '', label: 'Unassigned' },
      ...others.map((a) => ({ value: a.id, label: a.name })),
    ]
  } else if (kind === 'status') {
    options = Object.entries(STATUS_LABELS).map(([value, label]) => ({ value, label }))
  } else if (kind === 'priority') {
    options = [
      ...(['urgent', 'high', 'medium', 'low'] as const).map((value) => ({ value, label: PRIORITY_LABELS[value] })),
      { value: '', label: 'None' },
    ]
  } else {
    options = [
      ...(categories.data?.categories ?? []).filter((c) => !c.archived).map((c) => ({ value: c.id, label: c.name })),
      { value: '', label: 'None' },
    ]
  }
  return (
    <PickList
      options={options}
      onClose={onClose}
      onPick={(v) =>
        apply(
          kind === 'assign'
            ? { ownerId: v || null }
            : kind === 'status'
              ? { status: v as Ticket['status'] }
              : kind === 'priority'
                ? { priority: (v || null) as Ticket['priority'] }
                : { categoryId: v || null },
        )
      }
    />
  )
}

/** A floating panel for menus opened from the keyboard or the palette. */
export function Overlay({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  return (
    <div className="overlay" onMouseDown={onClose}>
      <div
        className="overlay-panel"
        role="dialog"
        aria-label={title}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === 'Escape') {
            e.stopPropagation()
            onClose()
          }
        }}
      >
        <h2>{title}</h2>
        {children}
      </div>
    </div>
  )
}

// ---------------------------------------------------------------------------
// The composer, reachable from the keyboard and from Undo (§34–35, §39).
// ---------------------------------------------------------------------------

type ComposerHandle = { open: (mode: 'reply' | 'comment') => void; restore: (text: string, onlyIfEmpty: boolean) => void }
const composers = new Map<string, ComposerHandle>()

export const composerBus = {
  register(ticketId: string, h: ComposerHandle) {
    composers.set(ticketId, h)
    return () => void (composers.get(ticketId) === h && composers.delete(ticketId))
  },
  open: (ticketId: string, mode: 'reply' | 'comment') => composers.get(ticketId)?.open(mode),
  /** False when that ticket's composer is not on screen. */
  restore(ticketId: string, text: string, onlyIfEmpty: boolean) {
    const h = composers.get(ticketId)
    h?.restore(text, onlyIfEmpty)
    return !!h
  },
}

// ---------------------------------------------------------------------------
// Keyboard sheet (§39) and command palette (§40)
// ---------------------------------------------------------------------------

export const SHORTCUTS: [string, string][] = [
  ['?', 'Show all shortcuts'],
  ['/', 'Search'],
  ['Ctrl/⌘ K', 'Command palette'],
  ['g then u m o s d c a', 'Go to Unassigned, Assigned to me, All open, Snoozed, Drafts, Closed, All tickets'],
  ['j / k, ↓ / ↑', 'Next / previous ticket; with a ticket open, opens it'],
  ['Enter, o', 'Open the focused ticket'],
  ['Esc', 'Close the open menu, else leave the composer, else back to the list'],
  ['x', 'Select or deselect the focused ticket'],
  ['r', 'Reply'],
  ['n', 'Internal comment'],
  ['a', 'Assign… (Me first)'],
  ['e', 'Close'],
  ['s', 'Status…'],
  ['p', 'Priority…'],
  ['c', 'Category…'],
  ['b', 'Snooze…'],
  ['Shift U', 'Mark as unread'],
  ['z', 'Undo'],
  ['Ctrl/⌘ Enter', 'Send'],
  ['Ctrl/⌘ Shift Enter', 'Send and close'],
]

export function ShortcutsHelp({ onClose }: { onClose: () => void }) {
  return (
    <Overlay title="Keyboard shortcuts" onClose={onClose}>
      <table className="shortcuts">
        <tbody>
          {SHORTCUTS.map(([k, v]) => (
            <tr key={k}>
              <td>
                <kbd>{k}</kbd>
              </td>
              <td>{v}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="muted small">Single-key shortcuts can be turned off in Settings → Profile.</p>
      <button autoFocus onClick={onClose}>
        Close
      </button>
    </Overlay>
  )
}

export type PaletteItem = { key: string; label: string; hint?: string; group: 'Tickets' | 'Views' | 'Actions'; run: () => void }

/** One box for tickets (over All tickets, first eight), views and actions. */
export function Palette({
  items,
  openTicket,
  onClose,
}: {
  items: PaletteItem[]
  openTicket: (id: string) => void
  onClose: () => void
}) {
  const [q, setQ] = useState('')
  const [found, setFound] = useState<Ticket[]>([])
  const [at, setAt] = useState(0)
  useEffect(() => {
    const query = q.trim()
    if (!query) return setFound([])
    const timer = setTimeout(() => {
      api
        .get<TicketList>(`/tickets?view=all&q=${encodeURIComponent(query)}`)
        // §40: by number, subject or contact — not message text, which `q` also searches.
        .then((l) => {
          const n = query.toLowerCase().replace(/^#/, '')
          const hit = (t: Ticket) =>
            String(t.number) === n || [t.subject, t.contact.name ?? '', t.contact.email].some((s) => s.toLowerCase().includes(query.toLowerCase()))
          setFound(l.tickets.filter(hit).slice(0, 8))
        })
        .catch(() => setFound([]))
    }, 250)
    return () => clearTimeout(timer)
  }, [q])
  const needle = q.trim().toLowerCase()
  const tickets: PaletteItem[] = found.map((t) => ({
    key: t.id,
    group: 'Tickets',
    label: `#${t.number} ${t.subject} · ${t.contact.name || t.contact.email}`,
    run: () => openTicket(t.id),
  }))
  const shown = [...tickets, ...items.filter((i) => i.label.toLowerCase().includes(needle))]
  const pick = (i?: PaletteItem) => {
    if (!i) return
    onClose()
    i.run()
  }
  return (
    <Overlay title="Command palette" onClose={onClose}>
      <input
        autoFocus
        aria-label="Find tickets, views and actions"
        placeholder="Find tickets, views and actions…"
        value={q}
        onChange={(e) => {
          setQ(e.target.value)
          setAt(0)
        }}
        onKeyDown={(e) => {
          if (e.key === 'ArrowDown') setAt((a) => Math.min(a + 1, shown.length - 1))
          else if (e.key === 'ArrowUp') setAt((a) => Math.max(a - 1, 0))
          else if (e.key === 'Enter') pick(shown[at])
          else return
          e.preventDefault()
        }}
      />
      <ul className="palette" role="listbox">
        {shown.map((i, n) => (
          <li
            key={`${i.group}-${i.key}`}
            role="option"
            aria-selected={n === at}
            className={n === at ? 'at' : ''}
            onMouseEnter={() => setAt(n)}
            onClick={() => pick(i)}
          >
            <span className="muted small">{i.group}</span> <span>{i.label}</span>
            {i.hint && <kbd>{i.hint}</kbd>}
          </li>
        ))}
        {shown.length === 0 && <li className="muted">Nothing found</li>}
      </ul>
    </Overlay>
  )
}

export const viewItems = (go: (view: string) => void, saved: { id: string; name: string }[]): PaletteItem[] => [
  ...Object.entries(VIEW_LABELS).map(([view, label]) => ({ key: view, group: 'Views' as const, label, run: () => go(view) })),
  ...saved.map((v) => ({ key: v.id, group: 'Views' as const, label: v.name, run: () => go(v.id) })),
]
