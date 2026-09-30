//! The HTTP contract of specs 001–006, as Rust structs. `cargo test` exports
//! each one to `frontend/src/api/types/` with ts-rs (ADR 0007) — the frontend
//! never hand-writes these. If this file and a spec disagree, the spec is right.
//!
//! Enum-like fields are `String` with the allowed values spelled out for
//! TypeScript; the database CHECK constraints hold the same lists.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use crate::session::double_option;

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ErrorBody {
    pub error: String,
}

// ---------------------------------------------------------------------------
// Spec 001 — workspaces, login and billing
// ---------------------------------------------------------------------------

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
    /// The six digits the authenticator app shows.
    pub code: String,
}

/// `POST /api/login` and `POST /api/auth/link`: where the SPA goes next.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct LoginResponse {
    pub redirect: String,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct PasswordResetRequest {
    pub email: String,
}

/// `GET /api/auth/link` — what the `/auth` page shows before it is submitted.
#[derive(Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AuthLinkInfo {
    #[ts(type = "'invite' | 'setup' | 'reset'")]
    pub purpose: String,
    pub workspace_name: String,
    pub email: String,
    /// The authenticator to set up; null on `reset`, which uses the existing one.
    pub totp: Option<Totp>,
}

#[derive(Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Totp {
    /// Base32, for typing into the app by hand.
    pub secret: String,
    /// `otpauth://totp/…`, what the QR code encodes.
    pub uri: String,
    /// An SVG image of `uri`.
    pub qr_svg: String,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct UseLinkRequest {
    pub token: String,
    pub password: String,
    pub code: String,
}

#[derive(Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Serialize, Deserialize, TS, Clone)]
#[ts(export)]
pub struct Agent {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    #[ts(type = "'owner' | 'agent'")]
    pub role: String,
}

#[derive(Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Billing {
    #[ts(type = "'trialing' | 'trialExpired' | 'active' | 'pastDue' | 'canceled'")]
    pub status: String,
    pub trial_ends_at: DateTime<Utc>,
    pub locked: bool,
}

#[derive(Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub inbound_address: String,
    pub language: String,
    pub billing: Billing,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct Me {
    pub agent: Agent,
    pub workspace: Workspace,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct UpdateMeRequest {
    pub name: String,
}

#[derive(Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Invite {
    pub email: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct AgentsResponse {
    pub agents: Vec<Agent>,
    pub invites: Vec<Invite>,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct InviteRequest {
    pub email: String,
}

/// Stripe Checkout and customer portal.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct UrlResponse {
    pub url: String,
}

// ---------------------------------------------------------------------------
// Spec 002 — tickets and email
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentRef {
    pub id: Uuid,
    pub name: String,
}

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Contact {
    pub id: Uuid,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TicketCategory {
    pub id: Uuid,
    pub name: String,
    #[ts(type = "'jev' | 'agent'")]
    pub source: String,
    /// Jev's when `source` is `jev`, otherwise null.
    pub probability: Option<f64>,
}

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CategorySuggestion {
    pub id: Uuid,
    pub name: String,
    pub probability: f64,
}

/// `lastMessage`, and `searchMatch` (spec 006 §13).
#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LastMessage {
    #[ts(type = "'customer' | 'agent' | 'comment'")]
    pub kind: String,
    /// The display name of whoever wrote it.
    pub author: String,
    pub snippet: String,
    pub at: DateTime<Utc>,
}

/// Another agent with the ticket open in the last 30 seconds (spec 006 §29).
#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Viewer {
    pub id: Uuid,
    pub name: String,
    /// Their draft on it changed in the last 60 seconds.
    pub replying: bool,
}

#[derive(Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Ticket {
    pub id: Uuid,
    /// Per workspace, from 1 in order of arrival (spec 006 §1).
    pub number: i32,
    pub subject: String,
    #[ts(type = "'new' | 'waitingOnContact' | 'waitingOnUs' | 'closed'")]
    pub status: String,
    #[ts(type = "'low' | 'medium' | 'high' | 'urgent' | null")]
    pub priority: Option<String>,
    pub owner: Option<AgentRef>,
    pub contact: Contact,
    pub category: Option<TicketCategory>,
    pub category_suggestions: Vec<CategorySuggestion>,
    pub seen_before: bool,
    pub last_message: LastMessage,
    /// The newest message matching `q`, its snippet around the match; null
    /// without `q` or when only the subject, number or contact matched.
    pub search_match: Option<LastMessage>,
    pub created_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    /// `unread`, `snoozeEnded` and `viewers` are relative to the agent asking.
    pub unread: bool,
    /// In `new` and `waitingOnUs`: the first customer message after the last
    /// agent reply, or when the status was set if earlier. Else null (spec 006 §16).
    pub waiting_since: Option<DateTime<Utc>>,
    pub snoozed_until: Option<DateTime<Utc>>,
    pub snooze_ended: bool,
    pub viewers: Vec<Viewer>,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct Counts {
    pub unassigned: i32,
    pub mine: i32,
    pub open: i32,
    pub drafts: i32,
    /// Every saved view the agent can see, by id.
    pub views: HashMap<Uuid, i32>,
}

/// `GET /api/tickets?view=…&q=…&status=…&sort=…`
#[derive(Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TicketList {
    pub tickets: Vec<Ticket>,
    pub next_cursor: Option<String>,
    pub counts: Counts,
    /// False until the workspace's first mail: the SPA shows forwarding
    /// instructions instead of an empty view (spec 002 §13).
    pub has_tickets: bool,
}

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Author {
    pub name: String,
    pub email: String,
}

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Delivery {
    #[ts(type = "'queued' | 'sent' | 'failed' | 'held'")]
    pub status: String,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AttachmentMeta {
    pub id: Uuid,
    pub name: String,
    pub content_type: String,
    pub size: i32,
}

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Message {
    pub id: Uuid,
    #[ts(type = "'customer' | 'agent' | 'comment'")]
    pub kind: String,
    pub author: Author,
    pub text: String,
    pub at: DateTime<Utc>,
    /// Null except on `agent`.
    pub delivery: Option<Delivery>,
    pub attachments: Vec<AttachmentMeta>,
}

#[derive(Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ContactTicket {
    pub id: Uuid,
    pub subject: String,
    #[ts(type = "'new' | 'waitingOnContact' | 'waitingOnUs' | 'closed'")]
    pub status: String,
    pub created_at: DateTime<Utc>,
}

/// `GET /api/tickets/{id}` — the Ticket plus its thread.
#[derive(Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TicketDetail {
    #[serde(flatten)]
    pub ticket: Ticket,
    pub messages: Vec<Message>,
    pub contact_tickets: Vec<ContactTicket>,
    /// The requesting agent's, or null (spec 006 §32).
    pub draft: Option<Draft>,
}

#[derive(Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Draft {
    #[ts(type = "'reply' | 'comment'")]
    pub mode: String,
    pub text: String,
    pub updated_at: DateTime<Utc>,
}

/// `PUT /api/tickets/{id}/draft` — empty text deletes it.
#[derive(Deserialize, TS)]
#[ts(export)]
pub struct DraftRequest {
    #[ts(type = "'reply' | 'comment'")]
    pub mode: String,
    pub text: String,
}

/// `PATCH /api/tickets/{id}` — any subset; `null` clears.
#[derive(Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PatchTicket {
    #[ts(
        optional,
        type = "'new' | 'waitingOnContact' | 'waitingOnUs' | 'closed'"
    )]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    #[ts(optional, type = "string | null")]
    pub owner_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "double_option")]
    #[ts(optional, type = "'low' | 'medium' | 'high' | 'urgent' | null")]
    pub priority: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    #[ts(optional, type = "string | null")]
    pub category_id: Option<Option<Uuid>>,
    /// `null` unsnoozes (spec 006 §25–27).
    #[serde(default, deserialize_with = "double_option")]
    #[ts(optional, type = "string | null")]
    pub snoozed_until: Option<Option<DateTime<Utc>>>,
}

/// `PATCH /api/tickets` — several at once, all or nothing (spec 006 §23).
#[derive(Deserialize, TS)]
#[ts(export)]
pub struct PatchTickets {
    pub tickets: Vec<PatchTicketsItem>,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct PatchTicketsItem {
    pub id: Uuid,
    #[serde(flatten)]
    pub patch: PatchTicket,
}

/// `PATCH /api/tickets`, in request order.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct TicketsResponse {
    pub tickets: Vec<Ticket>,
}

/// Internal comment bodies.
#[derive(Deserialize, TS)]
#[ts(export)]
pub struct TextRequest {
    pub text: String,
}

/// `POST /api/tickets/{id}/replies`.
#[derive(Deserialize, TS)]
#[ts(export)]
pub struct ReplyRequest {
    pub text: String,
    /// The ticket's status after the reply (spec 006 §33).
    #[ts(optional, type = "'waitingOnContact' | 'closed'")]
    pub status: Option<String>,
    /// The newest message the agent has seen; omitted, nothing is checked (§36).
    #[ts(optional)]
    pub after: Option<Uuid>,
}

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DnsRecord {
    #[serde(rename = "type")]
    #[ts(rename = "type", type = "'TXT' | 'CNAME'")]
    pub kind: String,
    pub host: String,
    pub value: String,
}

#[derive(Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SendingDomain {
    pub from_address: Option<String>,
    #[ts(type = "'pending' | 'verified' | null")]
    pub status: Option<String>,
    pub dns_records: Vec<DnsRecord>,
}

#[derive(Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetSendingDomain {
    pub from_address: String,
}

// ---------------------------------------------------------------------------
// Spec 003 — similar cases
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Case {
    pub ticket_id: Uuid,
    pub subject: String,
    pub closed_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NameOnly {
    pub name: String,
}

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Solution {
    pub text: String,
    pub author: NameOnly,
    pub at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Suggestion {
    pub id: Uuid,
    pub case: Case,
    pub score: f64,
    /// Null for a case closed without an agent reply.
    pub solution: Option<Solution>,
    #[ts(type = "'helped' | 'notRelevant' | null")]
    pub my_feedback: Option<String>,
}

/// `GET /api/tickets/{id}/suggestions`
#[derive(Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Suggestions {
    #[ts(type = "'pending' | 'ready' | 'failed'")]
    pub status: String,
    pub brain_size: i32,
    pub suggestions: Vec<Suggestion>,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct FeedbackRequest {
    #[ts(type = "'helped' | 'notRelevant'")]
    pub verdict: String,
}

// ---------------------------------------------------------------------------
// Spec 004 — categories
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Category {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub archived: bool,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct CategoriesResponse {
    pub categories: Vec<Category>,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct NewCategory {
    pub name: String,
    #[ts(optional)]
    pub description: Option<String>,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct PatchCategory {
    #[ts(optional)]
    pub name: Option<String>,
    #[ts(optional)]
    pub description: Option<String>,
    #[ts(optional)]
    pub archived: Option<bool>,
}

// ---------------------------------------------------------------------------
// Spec 006 — triage: saved views and snippets
// ---------------------------------------------------------------------------

/// The same values as `GET /api/tickets`'s parameters, as JSON.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ViewFilters {
    #[ts(type = "'unassigned' | 'mine' | 'open' | 'snoozed' | 'drafts' | 'closed' | 'all'")]
    pub view: String,
    pub q: Option<String>,
    #[ts(type = "Array<'new' | 'waitingOnContact' | 'waitingOnUs' | 'closed'>")]
    pub status: Vec<String>,
    /// `me`, `none`, or agent ids.
    pub owner: Vec<String>,
    #[ts(type = "Array<'none' | 'low' | 'medium' | 'high' | 'urgent'>")]
    pub priority: Vec<String>,
    /// `none`, or category ids.
    pub category: Vec<String>,
    #[ts(type = "'24h' | '7d' | '30d' | null")]
    pub created: Option<String>,
    pub unread: bool,
    pub seen_before: bool,
    #[ts(type = "'recent' | 'oldest' | 'waiting' | 'priority' | 'created'")]
    pub sort: String,
}

#[derive(Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct View {
    pub id: Uuid,
    pub name: String,
    pub shared: bool,
    pub created_by: AgentRef,
    pub filters: ViewFilters,
}

/// `GET /api/views` — the agent's own, then those others shared, each by name.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct ViewsResponse {
    pub views: Vec<View>,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct NewView {
    pub name: String,
    pub shared: bool,
    pub filters: ViewFilters,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct PatchView {
    #[ts(optional)]
    pub name: Option<String>,
    #[ts(optional)]
    pub shared: Option<bool>,
    #[ts(optional)]
    pub filters: Option<ViewFilters>,
}

#[derive(Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Snippet {
    pub id: Uuid,
    pub name: String,
    pub text: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct SnippetsResponse {
    pub snippets: Vec<Snippet>,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct NewSnippet {
    pub name: String,
    pub text: String,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct PatchSnippet {
    #[ts(optional)]
    pub name: Option<String>,
    #[ts(optional)]
    pub text: Option<String>,
}
