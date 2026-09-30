// Spec 002 and 006: views, filters and search, the ticket list, selection and
// bulk actions, and the keyboard — laid out like HubSpot Help Desk.

import { createContext, useContext, useEffect, useReducer, useRef, useState } from 'react'
import { keepPreviousData, useInfiniteQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Link, Outlet, useNavigate, useParams, useSearch } from '@tanstack/react-router'
import {
  CREATED_LABELS,
  PRIORITY_LABELS,
  SORT_LABELS,
  STATUS_LABELS,
  VIEW_LABELS,
  ago,
  api,
  duration,
  errorCode,
  formatSnooze,
  isBuiltin,
  sameFilters,
  shortcutsOn,
  ticketsQuery,
  useAgents,
  useCategories,
  useMe,
  useNow,
  useViews,
} from './client'
import { ForwardingInstructions } from './auth'
import {
  ACTION_LABELS,
  ActionMenu,
  AgentAvatar,
  Avatar,
  Dropdown,
  Highlight,
  Overlay,
  Palette,
  PickList,
  RelTime,
  ShortcutsHelp,
  changeError,
  composerBus,
  rewrite,
  undoLatest,
  useChangeTickets,
  viewItems,
  type ActionKind,
  type Option,
  type PaletteItem,
} from './triage'
import type { Ticket } from './api/types/Ticket'
import type { TicketDetail } from './api/types/TicketDetail'
import type { TicketList } from './api/types/TicketList'
import type { View } from './api/types/View'
import type { ViewFilters } from './api/types/ViewFilters'

export const POLL = 10_000 // ADR 0007
const PAGE_LIMIT = 200 // spec 006 §23

const EMPTY: Record<string, string> = {
  unassigned: 'Nothing unassigned. Nice.',
  mine: 'Nothing assigned to you.',
  open: 'No open tickets.',
  snoozed: 'Nothing snoozed. Snooze a ticket to hide it until it needs you.',
  drafts: 'No drafts.',
  closed: 'No closed tickets yet.',
  all: 'No tickets yet.',
}

// ---------------------------------------------------------------------------
// Filters in the URL (§11): comma-separated, defaults left out.
// ---------------------------------------------------------------------------

export type InboxSearch = {
  q?: string
  status?: string
  owner?: string
  priority?: string
  category?: string
  created?: string
  unread?: boolean
  seenBefore?: boolean
  sort?: string
  /** A saved view's base view. */
  base?: string
  /** The ticket a copilot case was opened from, for "Back". */
  from?: string
}

export function validateInboxSearch(s: Record<string, unknown>): InboxSearch {
  const str = (v: unknown) => (typeof v === 'string' || typeof v === 'number') && String(v) !== '' ? String(v) : undefined
  const bool = (v: unknown) => (v === true || v === 'true' ? true : undefined)
  return {
    q: str(s.q),
    status: str(s.status),
    owner: str(s.owner),
    priority: str(s.priority),
    category: str(s.category),
    created: str(s.created),
    unread: bool(s.unread),
    seenBefore: bool(s.seenBefore),
    sort: str(s.sort),
    base: str(s.base),
    from: str(s.from),
  }
}

export function filtersOf(view: string, s: InboxSearch): ViewFilters {
  const list = (v?: string) => (v ? v.split(',').filter(Boolean) : [])
  return {
    view: isBuiltin(view) ? view : s.base && isBuiltin(s.base) ? s.base : 'open',
    q: s.q ?? null,
    status: list(s.status) as ViewFilters['status'],
    owner: list(s.owner),
    priority: list(s.priority) as ViewFilters['priority'],
    category: list(s.category),
    created: (s.created as ViewFilters['created']) ?? null,
    unread: !!s.unread,
    seenBefore: !!s.seenBefore,
    sort: (s.sort as ViewFilters['sort']) ?? 'recent',
  }
}

export function searchOf(f: ViewFilters, saved: boolean): InboxSearch {
  const join = (v: string[]) => (v.length ? v.join(',') : undefined)
  return {
    q: f.q || undefined,
    status: join(f.status),
    owner: join(f.owner),
    priority: join(f.priority),
    category: join(f.category),
    created: f.created ?? undefined,
    unread: f.unread || undefined,
    seenBefore: f.seenBefore || undefined,
    sort: f.sort === 'recent' ? undefined : f.sort,
    base: saved ? f.view : undefined,
  }
}

const hasFilters = (f: ViewFilters) =>
  !!(f.q || f.status.length || f.owner.length || f.priority.length || f.category.length || f.created || f.unread || f.seenBefore)

/** The filters in words, for a saved view's tooltip (§4). */
function useDescribe() {
  const agents = useAgents()
  const categories = useCategories()
  return (f: ViewFilters) => {
    const agent = (id: string) => (id === 'me' ? 'Me' : id === 'none' ? 'Unassigned' : agents.data?.agents.find((a) => a.id === id)?.name ?? 'Removed agent')
    const cat = (id: string) => (id === 'none' ? 'None' : categories.data?.categories.find((c) => c.id === id)?.name ?? 'Removed category')
    const parts = [VIEW_LABELS[f.view]]
    if (f.q) parts.push(`Search: "${f.q}"`)
    if (f.status.length) parts.push(`Status: ${f.status.map((s) => STATUS_LABELS[s]).join(', ')}`)
    if (f.owner.length) parts.push(`Owner: ${f.owner.map(agent).join(', ')}`)
    if (f.priority.length) parts.push(`Priority: ${f.priority.map((p) => (p === 'none' ? 'None' : PRIORITY_LABELS[p])).join(', ')}`)
    if (f.category.length) parts.push(`Category: ${f.category.map(cat).join(', ')}`)
    if (f.created) parts.push(`Created: ${CREATED_LABELS[f.created]}`)
    if (f.unread) parts.push('Unread only')
    if (f.seenBefore) parts.push('Seen before')
    parts.push(`Sort: ${SORT_LABELS[f.sort]}`)
    return parts.join(' · ')
  }
}

// ---------------------------------------------------------------------------
// What the ticket pane needs from the list: its neighbours (§28, §31).
// ---------------------------------------------------------------------------

type InboxApi = {
  view: string
  search: InboxSearch
  neighbor: (id: string, dir: 1 | -1) => string | undefined
  position: (id: string) => string | null
  open: (id: string) => void
  markUnread: (ts: Ticket[]) => void
}
const InboxContext = createContext<InboxApi | null>(null)
export const useInbox = () => useContext(InboxContext)!

const listSearch = (s: InboxSearch): InboxSearch => ({ ...s, from: undefined })

export function Inbox() {
  const { view = 'unassigned', ticketId } = useParams({ strict: false })
  const search = useSearch({ strict: false }) as InboxSearch
  const navigate = useNavigate()
  const qc = useQueryClient()
  const me = useMe()
  const views = useViews()
  const change = useChangeTickets()
  const saved = isBuiltin(view) ? undefined : views.data?.views.find((v) => v.id === view)
  const missing = !isBuiltin(view) && views.data && !saved

  // A saved view opened without its filters in the URL gets them.
  useEffect(() => {
    if (saved && !search.base)
      navigate({ to: '/inbox/$view', params: { view }, search: searchOf(saved.filters, true), replace: true })
  }, [saved, search.base, view, navigate])

  const filters = filtersOf(view, search)
  const query = ticketsQuery(filters)
  const list = useInfiniteQuery({
    queryKey: ['tickets', query],
    queryFn: ({ pageParam }) =>
      api.get<TicketList>(`/tickets?${query}${pageParam ? `&cursor=${encodeURIComponent(pageParam)}` : ''}`),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.nextCursor,
    refetchInterval: POLL,
    placeholderData: keepPreviousData,
    enabled: isBuiltin(view) || !!search.base,
  })
  const first = list.data?.pages[0]
  const fresh = list.data?.pages.flatMap((p) => p.tickets) ?? []

  // §19: the list does not move under the agent.
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const [focusedId, setFocusedId] = useState<string | null>(null)
  const [scrolled, setScrolled] = useState(false)
  const [keyed, setKeyed] = useState(false)
  const [, bump] = useReducer((n: number) => n + 1, 0)
  const lastKeyAt = useRef(0)
  const frozen = useRef<{ key: string; ids: string[] } | null>(null)
  const listRef = useRef<HTMLDivElement>(null)
  const holding = scrolled || selected.size > 0 || keyed
  if (!holding) frozen.current = null
  else if (frozen.current?.key !== query) frozen.current = { key: query, ids: fresh.map((t) => t.id) }
  let shown = fresh
  let arrived = 0
  if (frozen.current) {
    const byId = new Map(fresh.map((t) => [t.id, t]))
    const ids = frozen.current.ids
    const known = new Set(ids)
    // Older rows from "Load more" join at the end; newer ones wait.
    const lastKnown = fresh.reduce((m, t, i) => (known.has(t.id) ? i : m), -1)
    fresh.forEach((t, i) => {
      if (known.has(t.id)) return
      if (i > lastKnown) {
        ids.push(t.id)
        known.add(t.id)
      } else arrived++
    })
    shown = ids.flatMap((id) => byId.get(id) ?? [])
  }
  const showArrived = () => {
    frozen.current = null
    setKeyed(false)
    setScrolled(false)
    listRef.current?.scrollTo({ top: 0 })
    bump()
  }

  // A new view or new filters start a new list.
  const [listKey, setListKey] = useState(query)
  if (listKey !== query) {
    setListKey(query)
    setSelected(new Set())
    setKeyed(false)
  }

  // §31: the order the open ticket was last seen in, for when it leaves.
  const order = useRef<string[]>([])
  const ids = shown.map((t) => t.id)
  if (!ticketId || ids.includes(ticketId)) order.current = ids
  const neighbor = (id: string, dir: 1 | -1) => {
    const i = ids.indexOf(id)
    if (i >= 0) return ids[i + dir]
    const old = order.current
    const j = old.indexOf(id)
    if (j < 0) return dir > 0 ? ids[0] : undefined
    return dir > 0 ? old.slice(j + 1).find((x) => ids.includes(x)) : old.slice(0, j).reverse().find((x) => ids.includes(x))
  }
  const position = (id: string) => {
    const i = ids.indexOf(id)
    return i < 0 ? null : `${i + 1} of ${ids.length}${list.hasNextPage ? '+' : ''}`
  }
  const open = (id: string) => {
    setFocusedId(id)
    navigate({ to: '/inbox/$view/$ticketId', params: { view, ticketId: id }, search: listSearch(search) })
  }
  const toList = () => navigate({ to: '/inbox/$view', params: { view }, search: listSearch(search) })
  const markUnread = async (ts: Ticket[]) => {
    if (ticketId && ts.some((t) => t.id === ticketId)) {
      await qc.cancelQueries({ queryKey: ['ticket', ticketId] })
      toList()
    }
    rewrite(qc, (t) => (ts.some((x) => x.id === t.id) ? { ...t, unread: t.status !== 'closed' } : t))
    await Promise.all(ts.map((t) => api.del(`/tickets/${t.id}/read`).catch(() => {})))
    qc.invalidateQueries({ queryKey: ['tickets'] })
  }

  // The open ticket stays open while the list around it changes.
  const setFilters = (f: ViewFilters, replace = false) => {
    const next = searchOf(f, !isBuiltin(view))
    if (ticketId) navigate({ to: '/inbox/$view/$ticketId', params: { view, ticketId }, search: next, replace })
    else navigate({ to: '/inbox/$view', params: { view }, search: next, replace })
  }

  // §42: the tab title; the ticket pane sets its own.
  const counts = first?.counts
  const label = saved?.name ?? (isBuiltin(view) ? VIEW_LABELS[view] : 'Inbox')
  const count = saved
    ? counts?.views[saved.id]
    : view === 'unassigned' || view === 'mine' || view === 'open' || view === 'drafts'
      ? counts?.[view]
      : undefined
  useEffect(() => {
    if (!ticketId) document.title = `${count !== undefined ? `(${count}) ` : ''}${label} · Muninn`
  }, [ticketId, count, label])

  // Targets of a ticket action (§39): the selection, else the open ticket, else the focused row.
  const selection = shown.filter((t) => selected.has(t.id))
  const targets = (): Ticket[] => {
    if (selection.length) return selection
    if (ticketId) {
      const t = qc.getQueryData<TicketDetail>(['ticket', ticketId]) ?? shown.find((t) => t.id === ticketId)
      return t ? [t] : []
    }
    const f = shown.find((t) => t.id === focusedId)
    return f ? [f] : []
  }
  const [menu, setMenu] = useState<{ kind: ActionKind; tickets: Ticket[] } | null>(null)
  const [palette, setPalette] = useState(false)
  const [help, setHelp] = useState(false)
  const searchRef = useRef<HTMLInputElement>(null)
  const gAt = useRef(0)

  const act = (kind: ActionKind | 'close' | 'unread' | 'reply' | 'comment') => {
    const ts = targets()
    if (!ts.length) return
    if (kind === 'close') change(ts, { status: 'closed' })
    else if (kind === 'unread') markUnread(ts)
    else if (kind === 'reply' || kind === 'comment') ticketId && composerBus.open(ticketId, kind)
    else setMenu({ kind, tickets: ts })
  }
  const move = (dir: 1 | -1) => {
    lastKeyAt.current = Date.now()
    setKeyed(true)
    const from = ticketId ?? focusedId
    const next = from ? neighbor(from, dir) : ids[0]
    if (!next) return
    if (ticketId) open(next)
    else setFocusedId(next)
  }
  const go = (v: string) => navigate({ to: '/inbox/$view', params: { view: v }, search: {} })

  const onKey = useRef<(e: KeyboardEvent) => void>(() => {})
  onKey.current = (e) => {
    const mod = e.metaKey || e.ctrlKey
    if (mod && e.key.toLowerCase() === 'k') {
      e.preventDefault()
      setPalette(true)
      return
    }
    if (mod || e.altKey || menu || palette || help) return
    if (e.key === 'Escape') {
      if (ticketId) toList()
      return
    }
    const el = e.target as HTMLElement
    if (el.closest('input, textarea, select, [contenteditable="true"]') || !shortcutsOn()) return
    if (e.key === 'Enter' && el.closest('a, button')) return // the element's own Enter
    const g = Date.now() - gAt.current < 1500
    gAt.current = 0
    const keys: Record<string, () => void> = g
      ? { u: () => go('unassigned'), m: () => go('mine'), o: () => go('open'), s: () => go('snoozed'), d: () => go('drafts'), c: () => go('closed'), a: () => go('all') }
      : {
          '?': () => setHelp(true),
          '/': () => searchRef.current?.focus(),
          g: () => (gAt.current = Date.now()),
          j: () => move(1),
          k: () => move(-1),
          ArrowDown: () => move(1),
          ArrowUp: () => move(-1),
          Enter: () => focusedId && open(focusedId),
          o: () => focusedId && open(focusedId),
          x: () => {
            if (!focusedId) return
            const next = new Set(selected)
            if (next.has(focusedId)) next.delete(focusedId)
            else next.add(focusedId)
            setSelected(next)
          },
          r: () => act('reply'),
          n: () => act('comment'),
          a: () => act('assign'),
          e: () => act('close'),
          s: () => act('status'),
          p: () => act('priority'),
          c: () => act('category'),
          b: () => act('snooze'),
          U: () => act('unread'),
          z: () => undoLatest(),
        }
    const f = keys[e.key]
    if (!f || (e.key === 'U' && !e.shiftKey)) return
    e.preventDefault()
    f()
  }
  useEffect(() => {
    const f = (e: KeyboardEvent) => onKey.current(e)
    window.addEventListener('keydown', f)
    return () => window.removeEventListener('keydown', f)
  }, [])
  useEffect(() => {
    if (focusedId) document.getElementById(`row-${focusedId}`)?.scrollIntoView({ block: 'nearest' })
  }, [focusedId])

  const paletteItems: PaletteItem[] = [
    ...viewItems(go, views.data?.views ?? []),
    ...(ticketId
      ? ([
          ['reply', 'Reply', 'r'],
          ['comment', 'Internal comment', 'n'],
          ['assign', 'Assign…', 'a'],
          ['close', 'Close', 'e'],
          ['status', 'Status…', 's'],
          ['priority', 'Priority…', 'p'],
          ['category', 'Category…', 'c'],
          ['snooze', 'Snooze…', 'b'],
          ['unread', 'Mark as unread', 'Shift U'],
        ] as const
        ).map(([k, label, hint]) => ({ key: k, group: 'Actions' as const, label, hint, run: () => act(k) }))
      : []),
  ]

  const lastClicked = useRef<string | null>(null)
  const check = (id: string, shift: boolean) => {
    const next = new Set(selected)
    if (shift && lastClicked.current && ids.includes(lastClicked.current)) {
      const [a, b] = [ids.indexOf(lastClicked.current), ids.indexOf(id)].sort((x, y) => x - y)
      ids.slice(a, b + 1).forEach((x) => next.add(x))
    } else if (next.has(id)) next.delete(id)
    else next.add(id)
    lastClicked.current = id
    setSelected(next)
  }
  const reconnecting = list.isError && !!list.data
  const meId = me.data?.agent.id

  return (
    <InboxContext.Provider value={{ view, search, neighbor, position, open, markUnread }}>
      <div className={`inbox ${ticketId ? 'has-ticket' : ''}`}>
        <ViewsPane view={view} counts={counts} views={views.data?.views ?? []} meId={meId} />
        <section className="list-pane" aria-label="Tickets">
          {reconnecting && (
            <div className="reconnecting" role="status">
              Reconnecting…
            </div>
          )}
          {selection.length > 0 ? (
            <BulkBar
              tickets={selection}
              all={selection.length === shown.length}
              onAll={() => setSelected(selection.length === shown.length ? new Set() : new Set(ids))}
              onClear={() => setSelected(new Set())}
            />
          ) : (
            <FilterBar
              filters={filters}
              saved={saved}
              meId={meId}
              searchRef={searchRef}
              onChange={setFilters}
              onSelectAll={() => setSelected(new Set(ids))}
              anyRows={shown.length > 0}
            />
          )}
          <div
            className={`list ${selected.size ? 'selecting' : ''} ${list.isPlaceholderData ? 'dim' : ''}`}
            ref={listRef}
            onScroll={(e) => {
              const away = e.currentTarget.scrollTop > 0
              setScrolled(away)
              if (!away && selected.size === 0 && Date.now() - lastKeyAt.current > 500) setKeyed(false)
            }}
          >
            {arrived > 0 && (
              <button className="arrived" onClick={showArrived}>
                ↑ {arrived} new {arrived === 1 ? 'ticket' : 'tickets'}
              </button>
            )}
            {missing && <p className="pad muted">This view no longer exists.</p>}
            {list.isError && !list.data && <p className="pad error">Could not load tickets.</p>}
            {!first && !list.isError && !missing && <Skeleton />}
            {first && !first.hasTickets && me.data ? (
              <div className="pad">
                <h3>No mail yet</h3>
                <ForwardingInstructions address={me.data.workspace.inboundAddress} />
              </div>
            ) : (
              first &&
              shown.length === 0 && (
                <Empty
                  filters={filters}
                  label={label}
                  onClear={() => setFilters({ ...filtersOf(view, {}), view: filters.view, sort: filters.sort })}
                  onSearchAll={() => navigate({ to: '/inbox/$view', params: { view: 'all' }, search: { q: filters.q ?? undefined } })}
                />
              )
            )}
            {shown.map((t) => (
              <Row
                key={t.id}
                t={t}
                q={filters.q}
                sort={filters.sort}
                active={t.id === ticketId}
                focused={t.id === focusedId}
                selected={selected.has(t.id)}
                meId={meId}
                onCheck={check}
                onFocus={() => setFocusedId(t.id)}
              />
            ))}
            {list.hasNextPage && (
              <button className="load-more" disabled={list.isFetchingNextPage} onClick={() => list.fetchNextPage()}>
                Load more
              </button>
            )}
          </div>
        </section>
        <Outlet />
      </div>
      {menu && (
        <Overlay title={`${ACTION_LABELS[menu.kind]} ${menu.tickets.length === 1 ? `#${menu.tickets[0]!.number}` : `${menu.tickets.length} tickets`}`} onClose={() => setMenu(null)}>
          <ActionMenu
            kind={menu.kind}
            tickets={menu.tickets}
            onClose={() => setMenu(null)}
            onDone={(e) => {
              setMenu(null)
              if (!e && selection.length) setSelected(new Set())
            }}
          />
        </Overlay>
      )}
      {palette && <Palette items={paletteItems} openTicket={open} onClose={() => setPalette(false)} />}
      {help && <ShortcutsHelp onClose={() => setHelp(false)} />}
    </InboxContext.Provider>
  )
}

function Skeleton() {
  return (
    <div aria-busy="true" aria-label="Loading tickets">
      {Array.from({ length: 6 }, (_, i) => (
        <div key={i} className="row skeleton">
          <div className="bone short" />
          <div className="bone" />
          <div className="bone long" />
        </div>
      ))}
    </div>
  )
}

function Empty({
  filters,
  label,
  onClear,
  onSearchAll,
}: {
  filters: ViewFilters
  label: string
  onClear: () => void
  onSearchAll: () => void
}) {
  if (filters.q)
    return (
      <div className="pad">
        <p className="muted">
          No tickets match "{filters.q}" in {label}
        </p>
        {filters.view !== 'all' && <button onClick={onSearchAll}>Search all tickets</button>}
      </div>
    )
  if (hasFilters(filters))
    return (
      <div className="pad">
        <p className="muted">No tickets match these filters</p>
        <button onClick={onClear}>Clear all</button>
      </div>
    )
  return <p className="pad muted">{EMPTY[filters.view] ?? 'Nothing here.'}</p>
}

// ---------------------------------------------------------------------------
// The left pane (§2, §4)
// ---------------------------------------------------------------------------

function ViewsPane({ view, counts, views, meId }: { view: string; counts?: TicketList['counts']; views: View[]; meId?: string }) {
  const describe = useDescribe()
  const builtin: { view: ViewFilters['view']; count?: number }[] = [
    { view: 'unassigned', count: counts?.unassigned },
    { view: 'mine', count: counts?.mine },
    { view: 'open', count: counts?.open },
    { view: 'snoozed' },
    ...(counts?.drafts || view === 'drafts' ? [{ view: 'drafts' as const, count: counts?.drafts }] : []),
    { view: 'closed' },
    { view: 'all' },
  ]
  const mine = views.filter((v) => v.createdBy.id === meId)
  const team = views.filter((v) => v.createdBy.id !== meId)
  return (
    <nav className="views" aria-label="Views">
      <h2>Inbox</h2>
      {builtin.map((b) => (
        <Link key={b.view} to="/inbox/$view" params={{ view: b.view }} search={{}} className={b.view === view ? 'active' : ''}>
          <span>{VIEW_LABELS[b.view]}</span>
          {b.count !== undefined && <span className="count">{b.count}</span>}
        </Link>
      ))}
      {mine.length > 0 && <h2 className="views-group">My views</h2>}
      {mine.map((v) => (
        <SavedViewLink key={v.id} v={v} active={v.id === view} count={counts?.views[v.id]} title={describe(v.filters)} own />
      ))}
      {team.length > 0 && <h2 className="views-group">Team views</h2>}
      {team.map((v) => (
        <SavedViewLink key={v.id} v={v} active={v.id === view} count={counts?.views[v.id]} title={describe(v.filters)} />
      ))}
    </nav>
  )
}

function SavedViewLink({ v, active, count, title, own }: { v: View; active: boolean; count?: number; title: string; own?: boolean }) {
  const qc = useQueryClient()
  const navigate = useNavigate()
  const [renaming, setRenaming] = useState(false)
  const [name, setName] = useState(v.name)
  const done = () => qc.invalidateQueries({ queryKey: ['views'] })
  const patch = useMutation({
    mutationFn: (body: { name?: string; shared?: boolean }) => api.patch<View>(`/views/${v.id}`, body),
    onSuccess: () => {
      setRenaming(false)
      done()
    },
  })
  const remove = useMutation({
    mutationFn: () => api.del(`/views/${v.id}`),
    onSuccess: () => {
      done()
      if (active) navigate({ to: '/inbox/$view', params: { view: v.filters.view }, search: {} })
    },
  })
  if (renaming)
    return (
      <form
        className="view-rename"
        onSubmit={(e) => {
          e.preventDefault()
          patch.mutate({ name })
        }}
      >
        <input aria-label="View name" autoFocus maxLength={40} required value={name} onChange={(e) => setName(e.target.value)} />
        {patch.isError && <span className="error">Enter a name of at most 40 characters.</span>}
      </form>
    )
  return (
    <div className="saved-view">
      <Link to="/inbox/$view" params={{ view: v.id }} search={searchOf(v.filters, true)} className={active ? 'active' : ''} title={title}>
        <span>
          {v.name}
          {own && v.shared && <span className="muted small"> · Shared</span>}
          {!own && <span className="muted small"> · {v.createdBy.name}</span>}
        </span>
        {count !== undefined && <span className="count">{count}</span>}
      </Link>
      {own && (
        <Dropdown label="⋯" className="link view-menu" title={`${v.name} options`}>
          {(close) => (
            <div className="menu">
              <button onClick={() => (close(), setRenaming(true))}>Rename</button>
              <button onClick={() => (close(), patch.mutate({ shared: !v.shared }))}>{v.shared ? 'Stop sharing' : 'Share with team'}</button>
              <button className="danger" onClick={() => (close(), remove.mutate())}>
                Delete
              </button>
            </div>
          )}
        </Dropdown>
      )}
    </div>
  )
}

// ---------------------------------------------------------------------------
// Search, filter chips, sort and saving (§4–5, §7–14)
// ---------------------------------------------------------------------------

function Chip({ label, values, options, onChange }: { label: string; values: string[]; options: Option[]; onChange: (v: string[]) => void }) {
  const names = values.map((v) => options.find((o) => o.value === v)?.label ?? 'Removed')
  return (
    <div className={`filter-chip ${values.length ? 'active' : ''}`}>
      <Dropdown label={values.length ? `${label}: ${names.join(', ')}` : `${label} ▾`}>
        {(close) => (
          <PickList
            options={options}
            selected={values}
            onClose={close}
            onPick={(v) => onChange(values.includes(v) ? values.filter((x) => x !== v) : [...values, v])}
          />
        )}
      </Dropdown>
      {values.length > 0 && (
        <button className="link clear" aria-label={`Clear ${label}`} onClick={() => onChange([])}>
          ×
        </button>
      )}
    </div>
  )
}

const VIEW_ERRORS: Record<string, string> = {
  invalidName: 'Enter a name of at most 40 characters.',
  tooManyViews: 'You have 30 saved views, the most there can be. Delete one first.',
  invalidFilter: 'One of these filters cannot be saved.',
}

function FilterBar({
  filters,
  saved,
  meId,
  searchRef,
  onChange,
  onSelectAll,
  anyRows,
}: {
  filters: ViewFilters
  saved?: View
  meId?: string
  searchRef: React.RefObject<HTMLInputElement | null>
  onChange: (f: ViewFilters, replace?: boolean) => void
  onSelectAll: () => void
  anyRows: boolean
}) {
  const agents = useAgents()
  const categories = useCategories()
  const qc = useQueryClient()
  const navigate = useNavigate()
  const [q, setQ] = useState(filters.q ?? '')
  const [saving, setSaving] = useState(false)
  const [name, setName] = useState('')
  const [shared, setShared] = useState(false)

  // §13: 250 ms after typing stops.
  useEffect(() => {
    if ((filters.q ?? '') !== q.trim()) setQ(filters.q ?? '')
  }, [filters.q])
  useEffect(() => {
    if (q.trim() === (filters.q ?? '')) return
    const timer = setTimeout(() => onChange({ ...filters, q: q.trim() || null }, true), 250)
    return () => clearTimeout(timer)
  }, [q, filters.q])

  const create = useMutation({
    mutationFn: () => api.post<View>('/views', { name, shared, filters }),
    onSuccess: (v) => {
      setSaving(false)
      setName('')
      qc.invalidateQueries({ queryKey: ['views'] })
      navigate({ to: '/inbox/$view', params: { view: v.id }, search: searchOf(v.filters, true) })
    },
  })
  const update = useMutation({
    mutationFn: () => api.patch<View>(`/views/${saved!.id}`, { filters }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['views'] }),
  })
  const set = <K extends keyof ViewFilters>(k: K, v: ViewFilters[K]) => onChange({ ...filters, [k]: v })

  const owners: Option[] = [
    { value: 'me', label: 'Me' },
    { value: 'none', label: 'Unassigned' },
    ...(agents.data?.agents ?? []).filter((a) => a.id !== meId).map((a) => ({ value: a.id, label: a.name })),
  ]
  const all = categories.data?.categories ?? []
  const cats: Option[] = [
    { value: 'none', label: 'None' },
    ...all.filter((c) => !c.archived).map((c) => ({ value: c.id, label: c.name })),
    // §7: archived ones too, under a divider (spec 004 §3).
    ...all.filter((c) => c.archived).map((c, i) => ({ value: c.id, label: `${c.name} (archived)`, divider: i === 0 })),
  ]
  const more = [...(filters.unread ? ['unread'] : []), ...(filters.seenBefore ? ['seenBefore'] : [])]
  const changed = saved && !sameFilters(filters, saved.filters)
  const code = errorCode(create.error)

  return (
    <div className="filterbar">
      <div className="filter-row">
        <input
          ref={searchRef}
          className="search"
          type="search"
          aria-label="Search tickets"
          placeholder="Search number, subject, contact or message…"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key !== 'Escape') return
            e.stopPropagation()
            e.currentTarget.blur()
          }}
        />
      </div>
      <div className="filter-row">
        <Chip
          label="Status"
          values={filters.status}
          options={Object.entries(STATUS_LABELS).map(([value, label]) => ({ value, label }))}
          onChange={(v) => set('status', v as ViewFilters['status'])}
        />
        <Chip label="Owner" values={filters.owner} options={owners} onChange={(v) => set('owner', v)} />
        <Chip
          label="Priority"
          values={filters.priority}
          options={[
            ...(['urgent', 'high', 'medium', 'low'] as const).map((value) => ({ value, label: PRIORITY_LABELS[value] })),
            { value: 'none', label: 'None' },
          ]}
          onChange={(v) => set('priority', v as ViewFilters['priority'])}
        />
        <Chip label="Category" values={filters.category} options={cats} onChange={(v) => set('category', v)} />
        <Chip
          label="Created"
          values={filters.created ? [filters.created] : []}
          options={Object.entries(CREATED_LABELS).map(([value, label]) => ({ value, label }))}
          onChange={(v) => set('created', (v.at(-1) ?? null) as ViewFilters['created'])}
        />
        <Chip
          label="More"
          values={more}
          options={[
            { value: 'unread', label: 'Unread only' },
            { value: 'seenBefore', label: 'Seen before' },
          ]}
          onChange={(v) => onChange({ ...filters, unread: v.includes('unread'), seenBefore: v.includes('seenBefore') })}
        />
        {hasFilters(filters) && (
          <button className="link" onClick={() => onChange({ ...filters, q: null, status: [], owner: [], priority: [], category: [], created: null, unread: false, seenBefore: false })}>
            Clear all
          </button>
        )}
      </div>
      <div className="filter-row">
        {anyRows && <input type="checkbox" aria-label="Select every loaded ticket" checked={false} onChange={onSelectAll} />}
        <Dropdown label={`Sort: ${SORT_LABELS[filters.sort]} ▾`} className="link">
          {(close) => (
            <PickList
              options={Object.entries(SORT_LABELS).map(([value, label]) => ({ value, label }))}
              onClose={close}
              onPick={(v) => {
                close()
                set('sort', v as ViewFilters['sort'])
              }}
            />
          )}
        </Dropdown>
        <span className="spacer" />
        {!saving && (hasFilters(filters) || filters.sort !== 'recent') && !changed && !saved && (
          <button className="link" onClick={() => setSaving(true)}>
            Save view
          </button>
        )}
      </div>
      {changed && !saving && (
        <div className="filter-row view-changed">
          {saved.createdBy.id === meId && (
            <button className="link" disabled={update.isPending} onClick={() => update.mutate()}>
              Update view
            </button>
          )}
          <button className="link" onClick={() => setSaving(true)}>
            Save as new
          </button>
          <button className="link" onClick={() => navigate({ to: '/inbox/$view', params: { view: saved.id }, search: searchOf(saved.filters, true) })}>
            Reset
          </button>
          {update.isError && <span className="error">Could not update the view.</span>}
        </div>
      )}
      {saving && (
        <form
          className="filter-row"
          onSubmit={(e) => {
            e.preventDefault()
            create.mutate()
          }}
        >
          <input aria-label="View name" autoFocus required maxLength={40} placeholder="View name" value={name} onChange={(e) => setName(e.target.value)} />
          <label className="inline-check">
            <input type="checkbox" checked={shared} onChange={(e) => setShared(e.target.checked)} /> Share with team
          </label>
          <button className="primary" disabled={create.isPending}>
            Save
          </button>
          <button type="button" onClick={() => setSaving(false)}>
            Cancel
          </button>
          {create.isError && <span className="error">{(code && VIEW_ERRORS[code]) ?? 'Could not save the view.'}</span>}
        </form>
      )}
    </div>
  )
}

// ---------------------------------------------------------------------------
// Bulk actions (§22–23)
// ---------------------------------------------------------------------------

function BulkBar({ tickets, all, onAll, onClear }: { tickets: Ticket[]; all: boolean; onAll: () => void; onClear: () => void }) {
  const [error, setError] = useState<string | null>(null)
  const over = tickets.length > PAGE_LIMIT
  const closed = tickets.some((t) => t.status === 'closed')
  return (
    <div className="filterbar bulkbar">
      <div className="filter-row">
        <input type="checkbox" aria-label="Select every loaded ticket" checked={all} onChange={onAll} />
        <strong>{tickets.length} selected</strong>
        {(['assign', 'status', 'priority', 'category', 'snooze'] as const).map((k) => (
          <Dropdown
            key={k}
            label={ACTION_LABELS[k]}
            disabled={over || (k === 'snooze' && closed)}
            title={k === 'snooze' && closed ? 'A closed ticket cannot be snoozed' : undefined}
          >
            {(close) => (
              <ActionMenu
                kind={k}
                tickets={tickets}
                onClose={close}
                onDone={(e) => {
                  close()
                  setError(e ? changeError(e) : null)
                  if (!e) onClear()
                }}
              />
            )}
          </Dropdown>
        ))}
        <span className="spacer" />
        <button className="link" aria-label="Clear selection" onClick={onClear}>
          ×
        </button>
      </div>
      {over && <p className="error">At most 200 tickets at a time.</p>}
      {error && <p className="error">{error}</p>}
    </div>
  )
}

// ---------------------------------------------------------------------------
// A row (§15–18)
// ---------------------------------------------------------------------------

function Icon({ name }: { name: 'eye' | 'pencil' }) {
  return (
    <svg width="13" height="13" viewBox="0 0 16 16" aria-hidden fill="none" stroke="currentColor" strokeWidth="1.6">
      {name === 'eye' ? (
        <>
          <path d="M1 8s2.5-5 7-5 7 5 7 5-2.5 5-7 5-7-5-7-5z" />
          <circle cx="8" cy="8" r="2" />
        </>
      ) : (
        <path d="M11 2l3 3-8 8H3v-3z" />
      )}
    </svg>
  )
}

export function Presence({ viewers }: { viewers: Ticket['viewers'] }) {
  if (!viewers.length) return null
  const replying = viewers.filter((v) => v.replying)
  const text = replying.length
    ? `${replying.map((v) => v.name).join(', ')} ${replying.length === 1 ? 'is' : 'are'} replying`
    : `${viewers.map((v) => v.name).join(', ')} ${viewers.length === 1 ? 'is' : 'are'} viewing`
  return (
    <span className="presence" title={text}>
      <Icon name={replying.length ? 'pencil' : 'eye'} />
      <span className="sr-only">{text}</span>
    </span>
  )
}

export function Waiting({ t }: { t: Ticket }) {
  const now = useNow()
  if (!t.waitingSince || (t.status !== 'new' && t.status !== 'waitingOnUs')) return null
  const late = now - new Date(t.waitingSince).getTime() > 86_400_000
  return (
    <span className={`waiting ${late ? 'late' : ''}`} title={`Waiting since ${new Date(t.waitingSince).toLocaleString()}`}>
      Waiting {duration(t.waitingSince, now)}
    </span>
  )
}

function Row({
  t,
  q,
  sort,
  active,
  focused,
  selected,
  meId,
  onCheck,
  onFocus,
}: {
  t: Ticket
  q: string | null
  sort: ViewFilters['sort']
  active: boolean
  focused: boolean
  selected: boolean
  meId?: string
  onCheck: (id: string, shift: boolean) => void
  onFocus: () => void
}) {
  const { view, search } = useInbox()
  const qc = useQueryClient()
  const change = useChangeTickets()
  const m = t.searchMatch ?? t.lastMessage
  const prefix = m.kind === 'agent' ? `${m.author}: ` : m.kind === 'comment' ? `Note · ${m.author}: ` : ''
  const at = sort === 'waiting' ? (t.waitingSince ?? t.lastActivityAt) : sort === 'created' ? t.createdAt : t.lastActivityAt
  const toggleRead = async () => {
    rewrite(qc, (x) => (x.id === t.id ? { ...x, unread: !t.unread } : x))
    await (t.unread ? api.put(`/tickets/${t.id}/read`, {}) : api.del(`/tickets/${t.id}/read`)).catch(() => {})
    qc.invalidateQueries({ queryKey: ['tickets'] })
  }
  return (
    <div
      id={`row-${t.id}`}
      className={`row ${t.unread ? 'unread' : ''} ${active ? 'active' : ''} ${focused ? 'focused' : ''} ${selected ? 'selected' : ''}`}
    >
      <input
        type="checkbox"
        className="row-check"
        aria-label={`Select #${t.number}`}
        checked={selected}
        onChange={() => {}}
        onClick={(e) => onCheck(t.id, e.shiftKey)}
      />
      <button className="dot" aria-label={t.unread ? `Mark #${t.number} as read` : `Mark #${t.number} as unread`} onClick={toggleRead}>
        {t.unread && <span aria-hidden>●</span>}
      </button>
      <Link to="/inbox/$view/$ticketId" params={{ view, ticketId: t.id }} search={listSearch(search)} className="row-link" onFocus={onFocus} onClick={onFocus}>
        <div className="row-top">
          <Avatar name={t.contact.name || t.contact.email} email={t.contact.email} small />
          <span className="contact">
            <Highlight text={t.contact.name || t.contact.email} q={q} />
          </span>
          <span className="number">#{t.number}</span>
          <RelTime iso={at}>{() => ago(at)}</RelTime>
        </div>
        <div className="subject">
          {t.seenBefore && (
            <span className="seen" title="Seen before: the brain has similar solved cases">
              ◆
            </span>
          )}
          <Highlight text={t.subject} q={q} />
        </div>
        <div className="snippet">
          {prefix}
          <Highlight text={m.snippet} q={t.searchMatch ? q : null} />
        </div>
        <div className="meta">
          <span className={`pill status-${t.status}`}>{STATUS_LABELS[t.status]}</span>
          {t.priority && <span className={`pill priority-${t.priority}`}>{PRIORITY_LABELS[t.priority]}</span>}
          {t.category && <span className="pill">{t.category.name}</span>}
          {t.hubspot && <span className="pill">HubSpot</span>}
          <Waiting t={t} />
          {t.snoozedUntil && <span className="pill snoozed">Until {formatSnooze(t.snoozedUntil)}</span>}
          {t.snoozeEnded && <span className="pill snoozed">Snooze ended</span>}
          <span className="spacer" />
          <Presence viewers={t.viewers} />
          {t.owner ? <AgentAvatar id={t.owner.id} name={t.owner.name} small /> : <span>Unassigned</span>}
        </div>
      </Link>
      <div className="row-actions">
        {!t.hubspot && t.owner?.id !== meId && meId && <button onClick={() => change([t], { ownerId: meId })}>Assign to me</button>}
        {!t.hubspot && t.status !== 'closed' && <button onClick={() => change([t], { status: 'closed' })}>Close</button>}
        {t.status !== 'closed' && (
          <Dropdown label="Snooze">{(close) => <ActionMenu kind="snooze" tickets={[t]} onClose={close} onDone={close} />}</Dropdown>
        )}
      </div>
    </div>
  )
}
