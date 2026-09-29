// Spec 003 §7–13: the copilot shows how the team solved this before. Every
// word here was written by a human on a real ticket (ADR 0002).

import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useNavigate } from '@tanstack/react-router'
import { api, formatDate, pct } from './client'
import type { Suggestion } from './api/types/Suggestion'
import type { Suggestions } from './api/types/Suggestions'
import type { FeedbackRequest } from './api/types/FeedbackRequest'

const PREVIEW = 600

export function Copilot({ ticketId, view }: { ticketId: string; view: string }) {
  const q = useQuery({
    queryKey: ['suggestions', ticketId],
    queryFn: () => api.get<Suggestions>(`/tickets/${ticketId}/suggestions`),
    // Jev answers in well under a second: re-ask every 2 s while pending (§8).
    refetchInterval: (query) => (query.state.data?.status === 'pending' ? 2000 : false),
  })
  const d = q.data
  const cases = d ? `${d.brainSize} ${d.brainSize === 1 ? 'case' : 'cases'}` : ''

  return (
    <section className="copilot">
      <h2>Copilot {d && <span className="muted">· Brain: {cases}</span>}</h2>
      {q.isError && <p className="muted">Suggestions unavailable right now.</p>}
      {d?.status === 'pending' && <p className="muted">Looking through {d.brainSize} past cases…</p>}
      {d?.status === 'failed' && <p className="muted">Suggestions unavailable right now.</p>}
      {d?.status === 'ready' && d.suggestions.length === 0 && (
        <p className="muted">Nothing similar yet — every closed ticket teaches the brain.</p>
      )}
      {d?.status === 'ready' &&
        d.suggestions.map((s) => <Card key={s.id} s={s} ticketId={ticketId} view={view} />)}
    </section>
  )
}

function Card({ s, ticketId, view }: { s: Suggestion; ticketId: string; view: string }) {
  const [all, setAll] = useState(false)
  const qc = useQueryClient()
  const navigate = useNavigate()
  const feedback = useMutation({
    mutationFn: (verdict: FeedbackRequest['verdict']) => api.post(`/suggestions/${s.id}/feedback`, { verdict }),
    onSettled: () => qc.invalidateQueries({ queryKey: ['suggestions', ticketId] }),
  })
  const open = () => {
    api.post(`/suggestions/${s.id}/opened`).catch(() => {}) // measurement only (§12)
    navigate({ to: '/inbox/$view/$ticketId', params: { view, ticketId: s.case.ticketId }, search: { from: ticketId } })
  }
  const text = s.solution?.text ?? ''
  const verdict = feedback.isPending ? feedback.variables : s.myFeedback

  return (
    <article className="case">
      <header>
        <button className="link case-subject" onClick={open}>
          {s.case.subject}
        </button>
        <span className="match">{pct(s.score)} match</span>
      </header>
      <div className="muted small">Closed {formatDate(s.case.closedAt)}</div>
      {s.solution ? (
        <div className="solution">
          <div className="muted small">
            Solution by {s.solution.author.name} · {formatDate(s.solution.at)}
          </div>
          {/* Written by an agent, but quoted mail can be in it: text node only. */}
          <div className="msg-text">{all || text.length <= PREVIEW ? text : text.slice(0, PREVIEW) + '…'}</div>
          {text.length > PREVIEW && (
            <button className="link" onClick={() => setAll(!all)}>
              {all ? 'Show less' : 'Show all'}
            </button>
          )}
        </div>
      ) : (
        <p className="muted">Closed without a reply</p>
      )}
      <div className="verdict">
        <button aria-pressed={verdict === 'helped'} onClick={() => feedback.mutate('helped')}>
          Helped
        </button>
        <button aria-pressed={verdict === 'notRelevant'} onClick={() => feedback.mutate('notRelevant')}>
          Not relevant
        </button>
      </div>
    </article>
  )
}
