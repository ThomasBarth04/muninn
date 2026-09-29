// Spec 001: signup, login, the magic-link page, onboarding.

import { useState, type FormEvent, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link, useRouter, useSearch } from '@tanstack/react-router'
import { ApiError, api, errorCode, useMe } from './client'
import type { AuthLinkInfo } from './api/types/AuthLinkInfo'
import type { ConsumeLinkResponse } from './api/types/ConsumeLinkResponse'
import type { SignupRequest } from './api/types/SignupRequest'

// Postgres' built-in stemmers (ADR 0010); `simple` matches exact words only.
const LANGUAGES = [
  'english', 'danish', 'dutch', 'finnish', 'french', 'german', 'hungarian', 'italian', 'norwegian',
  'portuguese', 'romanian', 'russian', 'spanish', 'swedish', 'turkish',
]

// ponytail: display-only; after signup the real address comes from /api/me.
const INBOUND_DOMAIN = 'in.muninn.io'

export function slugify(name: string) {
  return name
    .toLowerCase()
    .replace(/æ/g, 'ae')
    .replace(/ø/g, 'o')
    .replace(/ß/g, 'ss')
    .normalize('NFKD')
    .replace(/[̀-ͯ]/g, '')
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 32)
}

function Page({ children }: { children: ReactNode }) {
  return (
    <div className="public">
      <div className="brand big">Muninn</div>
      <div className="card narrow">{children}</div>
    </div>
  )
}

const SIGNUP_ERRORS: Record<string, string> = {
  invalidEmail: 'Enter a valid email address.',
  invalidWorkspaceName: 'Enter a workspace name of at most 64 characters.',
  invalidSlug: '3–32 characters: lowercase letters, digits and dashes.',
  unsupportedLanguage: 'Pick a language from the list.',
  slugTaken: 'That address is taken. Try another.',
}

export function Signup() {
  const search = useSearch({ from: '/signup' })
  const [form, setForm] = useState<SignupRequest>({
    email: search.email ?? '',
    workspaceName: search.workspaceName ?? '',
    slug: search.slug ?? '',
    language: search.language ?? 'english',
  })
  const [slugEdited, setSlugEdited] = useState(!!search.slug)
  const signup = useMutation({ mutationFn: (body: SignupRequest) => api.post('/signup', body) })
  const code = errorCode(signup.error)

  if (signup.isSuccess)
    return (
      <Page>
        <h1>Check your email</h1>
        <p>
          We sent a link to <strong>{form.email}</strong>. It is valid for 15 minutes.
        </p>
      </Page>
    )

  const submit = (e: FormEvent) => {
    e.preventDefault()
    signup.mutate(form)
  }
  const field = (k: keyof SignupRequest) => (v: string) => setForm((f) => ({ ...f, [k]: v }))

  return (
    <Page>
      <h1>Start your free trial</h1>
      <p className="muted">14 days, no card.</p>
      <form onSubmit={submit} className="stack">
        <label>
          Your work email
          <input type="email" required autoFocus value={form.email} onChange={(e) => field('email')(e.target.value)} />
          {code === 'invalidEmail' && <span className="error">{SIGNUP_ERRORS[code]}</span>}
        </label>
        <label>
          Workspace name
          <input
            required
            maxLength={64}
            value={form.workspaceName}
            onChange={(e) =>
              setForm((f) => ({ ...f, workspaceName: e.target.value, slug: slugEdited ? f.slug : slugify(e.target.value) }))
            }
          />
          {code === 'invalidWorkspaceName' && <span className="error">{SIGNUP_ERRORS[code]}</span>}
        </label>
        <label>
          Inbound address
          <span className="input-suffix">
            <input
              required
              value={form.slug}
              pattern="[a-z0-9\-]{3,32}"
              onChange={(e) => {
                setSlugEdited(true)
                field('slug')(e.target.value.toLowerCase())
              }}
            />
            <span className="muted">@{INBOUND_DOMAIN}</span>
          </span>
          {(code === 'invalidSlug' || code === 'slugTaken') && <span className="error">{SIGNUP_ERRORS[code]}</span>}
        </label>
        <label>
          Language of your support mail
          <select value={form.language} onChange={(e) => field('language')(e.target.value)}>
            {LANGUAGES.map((l) => (
              <option key={l} value={l}>
                {l[0].toUpperCase() + l.slice(1)}
              </option>
            ))}
            <option value="simple">Other language (no stemming)</option>
          </select>
          <span className="hint">This decides how past cases are matched and cannot be changed later.</span>
          {code === 'unsupportedLanguage' && <span className="error">{SIGNUP_ERRORS[code]}</span>}
        </label>
        <button className="primary" disabled={signup.isPending}>
          Send me a link
        </button>
        {code && !SIGNUP_ERRORS[code] && <p className="error">Something went wrong. Try again.</p>}
      </form>
      <p className="muted">
        Already have an account? <Link to="/login">Log in</Link>
      </p>
    </Page>
  )
}

export function Login() {
  const [email, setEmail] = useState('')
  const login = useMutation({ mutationFn: () => api.post('/login', { email }) })

  if (login.isSuccess)
    return (
      <Page>
        <h1>Check your email</h1>
        <p>
          If <strong>{email}</strong> has an account, a login link is on its way. It is valid for 15 minutes.
        </p>
      </Page>
    )

  return (
    <Page>
      <h1>Log in</h1>
      <form
        className="stack"
        onSubmit={(e) => {
          e.preventDefault()
          login.mutate()
        }}
      >
        <label>
          Email
          <input type="email" required autoFocus value={email} onChange={(e) => setEmail(e.target.value)} />
        </label>
        {errorCode(login.error) === 'invalidEmail' && <span className="error">Enter a valid email address.</span>}
        <button className="primary" disabled={login.isPending}>
          Send me a link
        </button>
      </form>
      <p className="muted">
        New to Muninn? <Link to="/signup">Start a free trial</Link>
      </p>
    </Page>
  )
}

// The link opens this page; only the button consumes the token, because
// corporate link scanners fetch every URL in an email (ADR 0006).
export function Auth() {
  const { token = '' } = useSearch({ from: '/auth' })
  const router = useRouter()
  const qc = useQueryClient()
  const info = useQuery({
    queryKey: ['authLink', token],
    queryFn: () => api.get<AuthLinkInfo>(`/auth/link?token=${encodeURIComponent(token)}`),
    retry: false,
  })
  const consume = useMutation({
    mutationFn: () => api.post<ConsumeLinkResponse>('/auth/link', { token }),
    onSuccess: ({ redirect }) => {
      qc.clear()
      router.history.push(redirect)
    },
  })
  const code = errorCode(consume.error) ?? (info.error instanceof ApiError ? info.error.code : null)

  if (code === 'linkExpired' || (info.isError && !code))
    return (
      <Page>
        <h1>This link has expired</h1>
        <p>Links work once, for 15 minutes (invites for 7 days).</p>
        <p>
          <Link to="/login">Send a new login link</Link> · <Link to="/signup">Start a new signup</Link>
        </p>
      </Page>
    )
  if (!info.data) return <Page>Loading…</Page>

  const { purpose, workspaceName, email, slug, language } = info.data
  if (code === 'slugTaken')
    return (
      <Page>
        <h1>That address was just taken</h1>
        <p>Someone else created {slug}@{INBOUND_DOMAIN} a moment ago.</p>
        <Link
          to="/signup"
          search={{ email, workspaceName, slug: slug ?? undefined, language: language ?? undefined }}
        >
          Pick another address
        </Link>
      </Page>
    )
  if (code === 'emailTaken')
    return (
      <Page>
        <h1>You already have an account</h1>
        <p>
          <strong>{email}</strong> already belongs to an agent. One email is one agent in one workspace.
        </p>
        <Link to="/login">Log in instead</Link>
      </Page>
    )

  const label = { signup: 'Create', login: 'Log in to', invite: 'Join' }[purpose]
  return (
    <Page>
      <h1>{workspaceName}</h1>
      <p className="muted">{email}</p>
      <button className="primary" disabled={consume.isPending} onClick={() => consume.mutate()}>
        {label} {workspaceName}
      </button>
      {consume.isError && code !== 'linkExpired' && <p className="error">Something went wrong. Try again.</p>}
    </Page>
  )
}

export function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false)
  return (
    <button
      type="button"
      onClick={() =>
        navigator.clipboard.writeText(text).then(() => {
          setCopied(true)
          setTimeout(() => setCopied(false), 1500)
        })
      }
    >
      {copied ? 'Copied' : 'Copy'}
    </button>
  )
}

export function ForwardingInstructions({ address }: { address: string }) {
  return (
    <div className="stack">
      <div className="address">
        <code>{address}</code> <CopyButton text={address} />
      </div>
      <p>Forward your support inbox (e.g. support@yourcompany.com) to this address. Everything that arrives becomes a ticket.</p>
      <h3>Google Workspace</h3>
      <ol>
        <li>In Gmail for your support inbox, open Settings → See all settings → Forwarding and POP/IMAP.</li>
        <li>Choose "Add a forwarding address" and paste the address above.</li>
        <li>Google sends a confirmation email to it — it arrives here as a ticket. Open it and follow the link.</li>
        <li>Back in Gmail, choose "Forward a copy of incoming mail to" the address, keep Gmail's copy, and save.</li>
      </ol>
      <h3>Microsoft 365</h3>
      <ol>
        <li>In the Exchange admin center, open Recipients → Mailboxes and select your support mailbox.</li>
        <li>Under Mail flow settings, edit Email forwarding: forward to the address above and keep a copy.</li>
        <li>
          If mail does not arrive, your outbound spam policy may block automatic forwarding: allow it in Microsoft
          Defender → Policies → Anti-spam → outbound policy.
        </li>
      </ol>
    </div>
  )
}

export function Onboarding() {
  const me = useMe()
  if (!me.data) return null
  return (
    <div className="card narrow">
      <h1>Welcome to Muninn</h1>
      <p>Your workspace receives mail at:</p>
      <ForwardingInstructions address={me.data.workspace.inboundAddress} />
      <Link className="button primary" to="/inbox/$view" params={{ view: 'unassigned' }}>
        Go to inbox
      </Link>
    </div>
  )
}
