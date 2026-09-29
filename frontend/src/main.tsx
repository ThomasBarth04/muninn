import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { MutationCache, QueryCache, QueryClient, QueryClientProvider, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  Link,
  Outlet,
  RouterProvider,
  createRootRoute,
  createRoute,
  createRouter,
  redirect,
  useNavigate,
} from '@tanstack/react-router'
import { ApiError, api, useMe } from './client'
import { Auth, Login, Onboarding, Signup } from './auth'
import { Inbox, NoTicket, TicketPane } from './inbox'
import { Settings } from './settings'
import type { UrlResponse } from './api/types/UrlResponse'
import './app.css'

const onError = (e: unknown) => {
  if (!(e instanceof ApiError)) return
  // Spec 001 §11: no valid session → login. 402: the shell re-reads /me and shows the paywall.
  if (e.status === 401) router.navigate({ to: '/login' })
  if (e.status === 402) queryClient.invalidateQueries({ queryKey: ['me'] })
}

const queryClient = new QueryClient({
  queryCache: new QueryCache({ onError }),
  mutationCache: new MutationCache({ onError }),
  defaultOptions: {
    queries: {
      retry: (n, e) => !(e instanceof ApiError && e.status < 500) && n < 2,
      refetchOnWindowFocus: true,
    },
  },
})

// Logged-in layout: top bar, trial banner, paywall when locked (spec 001 §18, §24).
function Shell() {
  const me = useMe()
  const qc = useQueryClient()
  const navigate = useNavigate()
  const logout = useMutation({
    mutationFn: () => api.post('/logout'),
    onSettled: () => {
      qc.clear()
      navigate({ to: '/login' })
    },
  })
  if (!me.data) return <div className="center muted">{me.isError ? 'Could not load Muninn.' : 'Loading…'}</div>
  const { agent, workspace } = me.data
  const billing = workspace.billing
  const isOwner = agent.role === 'owner'
  const daysLeft = Math.max(0, Math.ceil((new Date(billing.trialEndsAt).getTime() - Date.now()) / 86400000))

  return (
    <div className="shell">
      <header className="topbar">
        <span className="brand">Muninn</span>
        {!billing.locked && (
          <nav>
            <Link to="/inbox/$view" params={{ view: 'unassigned' }} activeProps={{ className: 'active' }}>
              Inbox
            </Link>
            <Link to="/settings/$section" params={{ section: 'profile' }} activeProps={{ className: 'active' }}>
              Settings
            </Link>
          </nav>
        )}
        <span className="spacer" />
        <span className="muted">
          {agent.name} · {workspace.name}
        </span>
        <button className="link" onClick={() => logout.mutate()}>
          Log out
        </button>
      </header>
      {billing.status === 'trialing' && (
        <div className="banner">
          {daysLeft} {daysLeft === 1 ? 'day' : 'days'} left in your trial.{' '}
          {isOwner && (
            <Link to="/settings/$section" params={{ section: 'billing' }}>
              Subscribe
            </Link>
          )}
        </div>
      )}
      {billing.status === 'pastDue' && (
        <div className="banner warn">
          Your last payment failed; Stripe is retrying.{' '}
          {isOwner && (
            <Link to="/settings/$section" params={{ section: 'billing' }}>
              Update your card
            </Link>
          )}
        </div>
      )}
      <main className="main">{billing.locked ? <Paywall isOwner={isOwner} expired={billing.status === 'trialExpired'} /> : <Outlet />}</main>
    </div>
  )
}

function Paywall({ isOwner, expired }: { isOwner: boolean; expired: boolean }) {
  const checkout = useMutation({
    mutationFn: () => api.post<UrlResponse>('/billing/checkout'),
    onSuccess: ({ url }) => (location.href = url),
  })
  return (
    <div className="card narrow">
      <h1>{expired ? 'Your trial has ended' : 'Your subscription has ended'}</h1>
      <p>Mail sent to your inbound address is still being received and stored. Nothing is lost.</p>
      {isOwner ? (
        <button className="primary" disabled={checkout.isPending} onClick={() => checkout.mutate()}>
          Subscribe
        </button>
      ) : (
        <p>
          <strong>Ask your workspace owner to subscribe.</strong>
        </p>
      )}
      {checkout.isError && <p className="error">Could not start checkout. Try again.</p>}
    </div>
  )
}

const str = (v: unknown) => (typeof v === 'string' ? v : undefined)

const rootRoute = createRootRoute({ component: Outlet })

const signupRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/signup',
  component: Signup,
  validateSearch: (s): { email?: string; workspaceName?: string; slug?: string; language?: string } => ({
    email: str(s.email),
    workspaceName: str(s.workspaceName),
    slug: str(s.slug),
    language: str(s.language),
  }),
})
const loginRoute = createRoute({ getParentRoute: () => rootRoute, path: '/login', component: Login })
const authRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/auth',
  component: Auth,
  validateSearch: (s): { token?: string } => ({ token: str(s.token) }),
})

const appRoute = createRoute({ getParentRoute: () => rootRoute, id: 'app', component: Shell })
const indexRoute = createRoute({
  getParentRoute: () => appRoute,
  path: '/',
  beforeLoad: () => {
    throw redirect({ to: '/inbox/$view', params: { view: 'unassigned' } })
  },
})
const onboardingRoute = createRoute({ getParentRoute: () => appRoute, path: '/onboarding', component: Onboarding })
const inboxRoute = createRoute({ getParentRoute: () => appRoute, path: '/inbox/$view', component: Inbox })
const noTicketRoute = createRoute({ getParentRoute: () => inboxRoute, path: '/', component: NoTicket })
const ticketRoute = createRoute({
  getParentRoute: () => inboxRoute,
  path: '$ticketId',
  component: TicketPane,
  // `from`: the ticket a copilot case was opened from, for "Back".
  validateSearch: (s): { from?: string } => ({ from: str(s.from) }),
})
const settingsIndexRoute = createRoute({
  getParentRoute: () => appRoute,
  path: '/settings',
  beforeLoad: () => {
    throw redirect({ to: '/settings/$section', params: { section: 'profile' } })
  },
})
const settingsRoute = createRoute({ getParentRoute: () => appRoute, path: '/settings/$section', component: Settings })

const routeTree = rootRoute.addChildren([
  signupRoute,
  loginRoute,
  authRoute,
  appRoute.addChildren([
    indexRoute,
    onboardingRoute,
    inboxRoute.addChildren([noTicketRoute, ticketRoute]),
    settingsIndexRoute,
    settingsRoute,
  ]),
])

const router = createRouter({ routeTree, defaultNotFoundComponent: () => <div className="center muted">Not found.</div> })

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router
  }
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
)
