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
import { ApiError, BETA_CONTACT, api, useMe } from './client'
import { Auth, BetaNote, ForgotPassword, Login, Onboarding } from './auth'
import { Inbox, NoTicket, TicketPane } from './inbox'
import { Settings } from './settings'
import './app.css'

const onError = (e: unknown) => {
  if (!(e instanceof ApiError)) return
  // Spec 001 §12: no valid session → login. (A wrong password is also a 401,
  // `invalidCredentials`, and stays on its form.) 402: the shell re-reads /me
  // and shows the paused screen.
  if (e.status === 401 && e.code === 'unauthenticated') router.navigate({ to: '/login' })
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

// Logged-in layout: top bar, and the paused screen when locked (spec 001 §21).
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
      <main className="main">{billing.locked ? <Paused /> : <Outlet />}</main>
    </div>
  )
}

function Paused() {
  return (
    <div className="card narrow">
      <h1>This workspace is paused</h1>
      <p>
        Contact us at <a href={`mailto:${BETA_CONTACT}`}>{BETA_CONTACT}</a> to resume it.
      </p>
      <p className="muted">Mail sent to your inbound address is still being received and stored. Nothing is lost.</p>
    </div>
  )
}

const str = (v: unknown) => (typeof v === 'string' ? v : undefined)

const rootRoute = createRootRoute({ component: Outlet })

const signupRoute = createRoute({ getParentRoute: () => rootRoute, path: '/signup', component: BetaNote })
const loginRoute = createRoute({ getParentRoute: () => rootRoute, path: '/login', component: Login })
const forgotRoute = createRoute({ getParentRoute: () => rootRoute, path: '/forgot', component: ForgotPassword })
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
  forgotRoute,
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
