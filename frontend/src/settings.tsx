// Settings: profile, team and billing (spec 001), sending domain (spec 002),
// categories (spec 004). Owner-only controls are hidden from agents (spec 001 §17).

import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link, useParams } from '@tanstack/react-router'
import { api, errorCode, formatDate, useAgents, useCategories, useMe } from './client'
import { CopyButton } from './auth'
import type { Category } from './api/types/Category'
import type { NewCategory } from './api/types/NewCategory'
import type { PatchCategory } from './api/types/PatchCategory'
import type { Me } from './api/types/Me'
import type { SendingDomain } from './api/types/SendingDomain'

const SECTIONS = [
  { section: 'profile', label: 'Profile' },
  { section: 'team', label: 'Team' },
  { section: 'billing', label: 'Billing' },
  { section: 'sending', label: 'Sending domain' },
  { section: 'categories', label: 'Categories' },
] as const

export function Settings() {
  const { section = 'profile' } = useParams({ strict: false })
  const me = useMe()
  if (!me.data) return null
  const isOwner = me.data.agent.role === 'owner'
  return (
    <div className="settings">
      <nav className="views">
        <h2>Settings</h2>
        {SECTIONS.map((s) => (
          <Link key={s.section} to="/settings/$section" params={{ section: s.section }} className={s.section === section ? 'active' : ''}>
            {s.label}
          </Link>
        ))}
      </nav>
      <div className="settings-body">
        {section === 'profile' && <Profile me={me.data} />}
        {section === 'team' && <Team isOwner={isOwner} />}
        {section === 'billing' && <BillingSection isOwner={isOwner} />}
        {section === 'sending' && <Sending isOwner={isOwner} />}
        {section === 'categories' && <Categories isOwner={isOwner} />}
      </div>
    </div>
  )
}

function Profile({ me }: { me: Me }) {
  const qc = useQueryClient()
  const [name, setName] = useState(me.agent.name)
  const save = useMutation({
    mutationFn: () => api.patch<Me>('/me', { name }),
    onSuccess: (data) => qc.setQueryData(['me'], data),
  })
  return (
    <section className="card">
      <h1>Profile</h1>
      <form
        className="stack"
        onSubmit={(e) => {
          e.preventDefault()
          save.mutate()
        }}
      >
        <label>
          Display name
          <input required maxLength={64} value={name} onChange={(e) => setName(e.target.value)} />
        </label>
        <div className="muted">{me.agent.email}</div>
        <div>
          <button className="primary" disabled={save.isPending}>
            Save
          </button>{' '}
          {save.isSuccess && <span className="muted">Saved.</span>}
          {save.isError && <span className="error">Enter a name of at most 64 characters.</span>}
        </div>
      </form>
      <h3>Inbound address</h3>
      <div className="address">
        <code>{me.workspace.inboundAddress}</code> <CopyButton text={me.workspace.inboundAddress} />
      </div>
      <ChangePassword />
    </section>
  )
}

// Spec 001 §11: this session stays, the others end.
function ChangePassword() {
  const [form, setForm] = useState({ currentPassword: '', newPassword: '' })
  const change = useMutation({
    mutationFn: () => api.post('/me/password', form),
    onSuccess: () => setForm({ currentPassword: '', newPassword: '' }),
  })
  const error = { wrongPassword: 'Your current password is not right.', invalidPassword: 'Choose a password of 10 to 256 characters.' }[
    errorCode(change.error) ?? ''
  ]
  return (
    <form
      className="stack"
      onSubmit={(e) => {
        e.preventDefault()
        change.mutate()
      }}
    >
      <h3>Password</h3>
      <label>
        Current password
        <input
          type="password"
          required
          autoComplete="current-password"
          value={form.currentPassword}
          onChange={(e) => setForm((f) => ({ ...f, currentPassword: e.target.value }))}
        />
      </label>
      <label>
        New password
        <input
          type="password"
          required
          minLength={10}
          maxLength={256}
          autoComplete="new-password"
          value={form.newPassword}
          onChange={(e) => setForm((f) => ({ ...f, newPassword: e.target.value }))}
        />
        <span className="hint">At least 10 characters. You stay logged in here; other devices are logged out.</span>
      </label>
      <div>
        <button className="primary" disabled={change.isPending}>
          Change password
        </button>{' '}
        {change.isSuccess && <span className="muted">Changed.</span>}
        {error && <span className="error">{error}</span>}
        {change.isError && !error && <span className="error">Could not change the password.</span>}
      </div>
    </form>
  )
}

function Team({ isOwner }: { isOwner: boolean }) {
  const qc = useQueryClient()
  const agents = useAgents()
  const [email, setEmail] = useState('')
  const [removing, setRemoving] = useState<string | null>(null)
  const [resetting, setResetting] = useState<string | null>(null)
  const invite = useMutation({
    mutationFn: () => api.post('/invites', { email }),
    onSuccess: () => {
      setEmail('')
      qc.invalidateQueries({ queryKey: ['agents'] })
    },
  })
  const remove = useMutation({
    mutationFn: (id: string) => api.del(`/agents/${id}`),
    onSettled: () => {
      setRemoving(null)
      qc.invalidateQueries({ queryKey: ['agents'] })
    },
  })
  // Spec 001 §10: a lost authenticator. Both factors cleared, a new setup link.
  const reset = useMutation({
    mutationFn: (id: string) => api.post(`/agents/${id}/reset-login`),
    onSettled: () => setResetting(null),
  })
  const inviteError = { invalidEmail: 'Enter a valid email address.', emailTaken: 'That address is already an agent in a Muninn workspace.' }[
    errorCode(invite.error) ?? ''
  ]

  return (
    <section className="card">
      <h1>Team</h1>
      <table>
        <tbody>
          {agents.data?.agents.map((a) => (
            <tr key={a.id}>
              <td>{a.name}</td>
              <td className="muted">{a.email}</td>
              <td>{a.role === 'owner' ? 'Owner' : 'Agent'}</td>
              <td className="right">
                {isOwner &&
                  a.role !== 'owner' &&
                  (resetting === a.id ? (
                    <>
                      Log {a.name} out and email a new setup link?{' '}
                      <button className="danger" disabled={reset.isPending} onClick={() => reset.mutate(a.id)}>
                        Reset login
                      </button>{' '}
                      <button onClick={() => setResetting(null)}>Cancel</button>
                    </>
                  ) : removing === a.id ? (
                    <>
                      Remove {a.name}?{' '}
                      <button className="danger" disabled={remove.isPending} onClick={() => remove.mutate(a.id)}>
                        Remove
                      </button>{' '}
                      <button onClick={() => setRemoving(null)}>Cancel</button>
                    </>
                  ) : (
                    <>
                      <button onClick={() => setResetting(a.id)}>Reset login</button>{' '}
                      <button onClick={() => setRemoving(a.id)}>Remove</button>
                    </>
                  ))}
              </td>
            </tr>
          ))}
          {agents.data?.invites.map((i) => (
            <tr key={i.email}>
              <td className="muted">Invited</td>
              <td className="muted">{i.email}</td>
              <td className="muted" colSpan={2}>
                Link valid until {formatDate(i.expiresAt)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {isOwner && (
        <form
          className="inline"
          onSubmit={(e) => {
            e.preventDefault()
            invite.mutate()
          }}
        >
          <label>
            Invite a teammate
            <input type="email" required placeholder="kari@acme.com" value={email} onChange={(e) => setEmail(e.target.value)} />
          </label>
          <button className="primary" disabled={invite.isPending}>
            Send invite
          </button>
          {inviteError && <span className="error">{inviteError}</span>}
          {reset.isSuccess && <span className="muted">Login reset. A new setup link is on its way.</span>}
          {reset.isError && <span className="error">Could not reset the login.</span>}
          {invite.isError && !inviteError && <span className="error">Could not send the invite.</span>}
        </form>
      )}
    </section>
  )
}

// Spec 001 §18–19: during the beta seats are invoiced by hand, not through Stripe.
function BillingSection({ isOwner }: { isOwner: boolean }) {
  const agents = useAgents()
  return (
    <section className="card">
      <h1>Billing</h1>
      {isOwner ? (
        <>
          <p>Invoiced monthly per seat.</p>
          {agents.data && (
            <p>
              <strong>{agents.data.agents.length}</strong> {agents.data.agents.length === 1 ? 'seat' : 'seats'} now. Pending
              invites are not seats.
            </p>
          )}
        </>
      ) : (
        <p className="muted">Only the workspace owner sees billing.</p>
      )}
    </section>
  )
}

function Sending({ isOwner }: { isOwner: boolean }) {
  const qc = useQueryClient()
  const domain = useQuery({ queryKey: ['sendingDomain'], queryFn: () => api.get<SendingDomain>('/sending-domain') })
  const [address, setAddress] = useState('')
  const set = (d: SendingDomain) => qc.setQueryData(['sendingDomain'], d)
  const put = useMutation({ mutationFn: () => api.put<SendingDomain>('/sending-domain', { fromAddress: address }), onSuccess: set })
  const verify = useMutation({ mutationFn: () => api.post<SendingDomain>('/sending-domain/verify'), onSuccess: set })
  const remove = useMutation({
    mutationFn: () => api.del('/sending-domain'),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['sendingDomain'] }),
  })
  const putError = { invalidAddress: 'Enter a valid email address.', domainTaken: 'Another workspace already sends from that domain.' }[
    errorCode(put.error) ?? ''
  ]
  const d = domain.data
  if (!d) return null

  return (
    <section className="card">
      <h1>Sending domain</h1>
      {!d.fromAddress ? (
        <>
          <p>
            Replies go out from your Muninn address until you verify your own. Sending as, say, support@acme.com needs two
            DNS records.
          </p>
          {isOwner && (
            <form
              className="inline"
              onSubmit={(e: FormEvent) => {
                e.preventDefault()
                put.mutate()
              }}
            >
              <label>
                Send replies from
                <input type="email" required placeholder="support@acme.com" value={address} onChange={(e) => setAddress(e.target.value)} />
              </label>
              <button className="primary" disabled={put.isPending}>
                Save
              </button>
              {putError && <span className="error">{putError}</span>}
              {put.isError && !putError && <span className="error">Could not register the domain. Try again.</span>}
            </form>
          )}
        </>
      ) : (
        <>
          <p>
            <strong>{d.fromAddress}</strong>{' '}
            <span className={`pill ${d.status === 'verified' ? 'status-closed' : 'status-new'}`}>
              {d.status === 'verified' ? 'Verified' : 'Pending'}
            </span>
          </p>
          {d.status === 'verified' ? (
            <p className="muted">Every reply is sent from this address.</p>
          ) : (
            <p className="muted">Add these records at your DNS provider, then check. Replies use your Muninn address until then.</p>
          )}
          <table>
            <thead>
              <tr>
                <th>Type</th>
                <th>Host</th>
                <th>Value</th>
              </tr>
            </thead>
            <tbody>
              {d.dnsRecords.map((r) => (
                <tr key={r.type}>
                  <td>{r.type}</td>
                  <td>
                    <code>{r.host}</code> <CopyButton text={r.host} />
                  </td>
                  <td className="break">
                    <code>{r.value}</code> <CopyButton text={r.value} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {isOwner && (
            <div className="row-buttons">
              {d.status !== 'verified' && (
                <button className="primary" disabled={verify.isPending} onClick={() => verify.mutate()}>
                  Check now
                </button>
              )}
              <button className="danger" disabled={remove.isPending} onClick={() => remove.mutate()}>
                Remove
              </button>
              {verify.isSuccess && verify.data.status !== 'verified' && <span className="muted">Not verified yet. DNS can take a while.</span>}
              {(verify.isError || remove.isError) && <span className="error">Could not reach Postmark. Try again.</span>}
            </div>
          )}
        </>
      )}
    </section>
  )
}

const CATEGORY_ERRORS: Record<string, string> = {
  invalidName: 'Enter a name of at most 40 characters.',
  invalidDescription: 'Keep the description to 200 characters.',
  nameTaken: 'An active category already has that name.',
  tooManyCategories: 'At most 255 active categories.',
}

function Categories({ isOwner }: { isOwner: boolean }) {
  const qc = useQueryClient()
  const categories = useCategories()
  const [name, setName] = useState('')
  const [description, setDescription] = useState('')
  const add = useMutation({
    mutationFn: (body: NewCategory) => api.post('/categories', body),
    onSuccess: () => {
      setName('')
      setDescription('')
      qc.invalidateQueries({ queryKey: ['categories'] })
    },
  })
  const code = errorCode(add.error)

  return (
    <section className="card">
      <h1>Categories</h1>
      <p className="muted">Jev reads each description when it categorises a new ticket. Existing tickets are not re-categorised.</p>
      <table>
        <tbody>
          {categories.data?.categories.map((c) => (
            <CategoryRow key={c.id} c={c} isOwner={isOwner} />
          ))}
        </tbody>
      </table>
      {isOwner && (
        <form
          className="stack"
          onSubmit={(e) => {
            e.preventDefault()
            add.mutate({ name, description })
          }}
        >
          <h3>Add a category</h3>
          <label>
            Name
            <input required maxLength={40} value={name} onChange={(e) => setName(e.target.value)} />
          </label>
          <label>
            Description (optional)
            <input maxLength={200} value={description} onChange={(e) => setDescription(e.target.value)} />
          </label>
          <div>
            <button className="primary" disabled={add.isPending}>
              Add
            </button>{' '}
            {code && <span className="error">{CATEGORY_ERRORS[code] ?? 'Could not save.'}</span>}
          </div>
        </form>
      )}
    </section>
  )
}

function CategoryRow({ c, isOwner }: { c: Category; isOwner: boolean }) {
  const qc = useQueryClient()
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(c.name)
  const [description, setDescription] = useState(c.description)
  const patch = useMutation({
    mutationFn: (body: PatchCategory) => api.patch<Category>(`/categories/${c.id}`, body),
    onSuccess: () => {
      setEditing(false)
      qc.invalidateQueries({ queryKey: ['categories'] })
    },
  })
  const code = errorCode(patch.error)

  if (editing)
    return (
      <tr>
        <td colSpan={3}>
          <form
            className="inline"
            onSubmit={(e) => {
              e.preventDefault()
              patch.mutate({ name, description })
            }}
          >
            <input aria-label="Name" required maxLength={40} value={name} onChange={(e) => setName(e.target.value)} />
            <input aria-label="Description" maxLength={200} value={description} onChange={(e) => setDescription(e.target.value)} />
            <button className="primary" disabled={patch.isPending}>
              Save
            </button>
            <button type="button" onClick={() => setEditing(false)}>
              Cancel
            </button>
            {code && <span className="error">{CATEGORY_ERRORS[code] ?? 'Could not save.'}</span>}
          </form>
        </td>
      </tr>
    )

  return (
    <tr className={c.archived ? 'archived' : ''}>
      <td>
        <strong>{c.name}</strong>
        {c.archived && <span className="muted"> (archived)</span>}
      </td>
      <td className="muted">{c.description}</td>
      <td className="right nowrap">
        {isOwner && (
          <>
            {!c.archived && <button onClick={() => setEditing(true)}>Edit</button>}{' '}
            <button disabled={patch.isPending} onClick={() => patch.mutate({ archived: !c.archived })}>
              {c.archived ? 'Unarchive' : 'Archive'}
            </button>
            {code && <span className="error"> {CATEGORY_ERRORS[code] ?? 'Could not save.'}</span>}
          </>
        )}
      </td>
    </tr>
  )
}
