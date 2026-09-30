// Spec 001: login with a password and an authenticator (ADR 0011), the
// setup and reset link page, the beta note, onboarding.

import { useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link, useRouter, useSearch } from '@tanstack/react-router'
import { ApiError, BETA_CONTACT, api, errorCode, useHubspot, useMe } from './client'
import type { AuthLinkInfo } from './api/types/AuthLinkInfo'
import type { LoginResponse } from './api/types/LoginResponse'

function Page({ children }: { children: ReactNode }) {
  return (
    <div className="public">
      <div className="brand big">Muninn</div>
      <div className="card narrow">{children}</div>
    </div>
  )
}

// No public signup during the beta (spec 001): the operator creates workspaces.
export function BetaNote() {
  return (
    <Page>
      <h1>Muninn is in private beta</h1>
      <p>
        We set up each workspace ourselves. Write to <a href={`mailto:${BETA_CONTACT}`}>{BETA_CONTACT}</a> to get one.
      </p>
      <p className="muted">
        Already have an account? <Link to="/login">Log in</Link>
      </p>
    </Page>
  )
}

function CodeInput({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  return (
    <input
      required
      inputMode="numeric"
      autoComplete="one-time-code"
      pattern="[0-9]{6}"
      maxLength={6}
      placeholder="123456"
      value={value}
      onChange={(e) => onChange(e.target.value.replace(/\D/g, ''))}
    />
  )
}

const LOGIN_ERRORS: Record<string, string> = {
  invalidCredentials: 'Email, password or code is wrong.',
  tooManyAttempts: 'Too many attempts. Try again in 15 minutes.',
}

export function Login() {
  const router = useRouter()
  const qc = useQueryClient()
  const [form, setForm] = useState({ email: '', password: '', code: '' })
  const login = useMutation({
    mutationFn: () => api.post<LoginResponse>('/login', form),
    onSuccess: ({ redirect }) => {
      qc.clear()
      router.history.push(redirect)
    },
    // A wrong code is used up: clear it for the next one.
    onError: () => setForm((f) => ({ ...f, code: '' })),
  })
  const code = errorCode(login.error)
  const field = (k: keyof typeof form) => (v: string) => setForm((f) => ({ ...f, [k]: v }))

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
          <input type="email" required autoFocus autoComplete="username" value={form.email} onChange={(e) => field('email')(e.target.value)} />
        </label>
        <label>
          Password
          <input type="password" required autoComplete="current-password" value={form.password} onChange={(e) => field('password')(e.target.value)} />
        </label>
        <label>
          Code from your authenticator app
          <CodeInput value={form.code} onChange={field('code')} />
        </label>
        <button className="primary" disabled={login.isPending}>
          Log in
        </button>
        {code && <span className="error">{LOGIN_ERRORS[code] ?? 'Something went wrong. Try again.'}</span>}
      </form>
      <p className="muted">
        <Link to="/forgot">Forgot password?</Link> · Lost your authenticator? Ask your workspace owner to reset your login.
      </p>
    </Page>
  )
}

export function ForgotPassword() {
  const [email, setEmail] = useState('')
  const reset = useMutation({ mutationFn: () => api.post('/password-reset', { email }) })

  if (reset.isSuccess)
    return (
      <Page>
        <h1>Check your email</h1>
        <p>
          If <strong>{email}</strong> has an account, a link to choose a new password is on its way. It is valid for one
          hour, and you will need your authenticator app.
        </p>
      </Page>
    )

  return (
    <Page>
      <h1>Forgot password</h1>
      <form
        className="stack"
        onSubmit={(e) => {
          e.preventDefault()
          reset.mutate()
        }}
      >
        <label>
          Email
          <input type="email" required autoFocus value={email} onChange={(e) => setEmail(e.target.value)} />
        </label>
        {errorCode(reset.error) === 'invalidEmail' && <span className="error">Enter a valid email address.</span>}
        <button className="primary" disabled={reset.isPending}>
          Send me a link
        </button>
      </form>
      <p className="muted">
        <Link to="/login">Back to log in</Link>
      </p>
    </Page>
  )
}

const LINK_ERRORS: Record<string, string> = {
  invalidPassword: 'Choose a password of 10 to 256 characters.',
  tooManyAttempts: 'Too many attempts. Try again in 15 minutes.',
}

// A setup, invite or reset link opens this page. Reading it does not use the
// link, because corporate link scanners fetch every URL in an email; only
// submitting the form does (spec 001 §5).
export function Auth() {
  const { token = '' } = useSearch({ from: '/auth' })
  const router = useRouter()
  const qc = useQueryClient()
  const [password, setPassword] = useState('')
  const [code, setCode] = useState('')
  const info = useQuery({
    queryKey: ['authLink', token],
    queryFn: () => api.get<AuthLinkInfo>(`/auth/link?token=${encodeURIComponent(token)}`),
    retry: false,
  })
  const submit = useMutation({
    mutationFn: () => api.post<LoginResponse>('/auth/link', { token, password, code }),
    onSuccess: ({ redirect }) => {
      qc.clear()
      router.history.push(redirect)
    },
    onError: () => setCode(''),
  })
  const error = errorCode(submit.error) ?? (info.error instanceof ApiError ? info.error.code : null)

  if (error === 'linkExpired' || (info.isError && !error))
    return (
      <Page>
        <h1>This link has expired</h1>
        <p>Links work once: for 7 days to set up a login, for one hour to reset a password.</p>
        <p>
          Ask your workspace owner for a new invite, or <Link to="/forgot">request a new password link</Link>.
        </p>
      </Page>
    )
  if (!info.data) return <Page>Loading…</Page>

  const { purpose, workspaceName, email, totp } = info.data
  if (error === 'emailTaken')
    return (
      <Page>
        <h1>You already have an account</h1>
        <p>
          <strong>{email}</strong> already belongs to an agent. One email is one agent in one workspace.
        </p>
        <Link to="/login">Log in instead</Link>
      </Page>
    )

  const title = { invite: `Join ${workspaceName}`, setup: `Set up your login to ${workspaceName}`, reset: 'Choose a new password' }[purpose]
  return (
    <Page>
      <h1>{title}</h1>
      <p className="muted">{email}</p>
      <form
        className="stack"
        onSubmit={(e) => {
          e.preventDefault()
          submit.mutate()
        }}
      >
        <label>
          {purpose === 'reset' ? 'New password' : 'Choose a password'}
          <input
            type="password"
            required
            minLength={10}
            maxLength={256}
            autoComplete="new-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
          <span className="hint">At least 10 characters.</span>
        </label>
        {totp ? (
          <div className="stack">
            <div>
              <strong>Set up your authenticator app</strong>
              <p className="muted">
                Scan this with Google Authenticator, Microsoft Authenticator, 1Password or any authenticator app. You will
                enter a code from it every time you log in.
              </p>
            </div>
            <img className="qr" src={`data:image/svg+xml;utf8,${encodeURIComponent(totp.qrSvg)}`} alt="QR code for your authenticator app" width={200} height={200} />
            <p className="muted small">
              Can't scan? Enter this key: <code className="break">{totp.secret}</code> <CopyButton text={totp.secret} />
            </p>
            <label>
              The code your app shows now
              <CodeInput value={code} onChange={setCode} />
            </label>
          </div>
        ) : (
          <label>
            Code from your authenticator app
            <CodeInput value={code} onChange={setCode} />
          </label>
        )}
        <button className="primary" disabled={submit.isPending}>
          {purpose === 'invite' ? `Join ${workspaceName}` : purpose === 'reset' ? 'Save and log in' : 'Set up and log in'}
        </button>
        {error === 'invalidCode' && (
          <span className="error">
            {totp ? 'That code does not match. Try the newest code your app shows.' : 'That is not the current code from your authenticator app.'}
          </span>
        )}
        {error && error !== 'invalidCode' && <span className="error">{LINK_ERRORS[error] ?? 'Something went wrong. Try again.'}</span>}
      </form>
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
  const hubspot = useHubspot()
  if (!me.data) return null
  return (
    <div className="card narrow">
      <h1>Welcome to Muninn</h1>
      <p>Your workspace receives mail at:</p>
      <ForwardingInstructions address={me.data.workspace.inboundAddress} />
      {hubspot.data && (
        <p>
          Answering in HubSpot Help Desk? <Link to="/settings/$section" params={{ section: 'hubspot' }}>Connect HubSpot</Link>{' '}
          instead, and Muninn reads your tickets from there.
        </p>
      )}
      <Link className="button primary" to="/inbox/$view" params={{ view: 'unassigned' }}>
        Go to inbox
      </Link>
    </div>
  )
}
