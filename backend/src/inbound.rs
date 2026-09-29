//! Spec 002: the Postmark inbound webhook. The only code that knows Postmark's
//! inbound JSON (ADR 0005).
//!
//! Crosses tenants once — finding the workspace by slug (ADR 0003's named
//! exception) — and then does everything inside that workspace's tenant
//! transaction. Inbound mail is never dropped: only a failure to store
//! answers non-2xx, so Postmark retries (spec 002 §9).

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::post;
use axum::{Router, body::Bytes};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::db::{tenant_tx, unique_violation};
use crate::session::normalize_email;
use crate::{AppState, jobs};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/postmark/inbound", post(inbound))
        // Postmark accepts messages up to 35 MB, base64 included.
        .layer(DefaultBodyLimit::max(40 * 1024 * 1024))
}

/// The fields of Postmark's inbound JSON that spec 002's contract reads.
/// Every one optional: Postmark sends `null` for absent parts.
#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Inbound {
    original_recipient: Option<String>,
    to_full: Option<Vec<Addr>>,
    cc_full: Option<Vec<Addr>>,
    from_full: Option<Addr>,
    subject: Option<String>,
    mailbox_hash: Option<String>,
    text_body: Option<String>,
    html_body: Option<String>,
    stripped_text_reply: Option<String>,
    headers: Option<Vec<Header>>,
    attachments: Option<Vec<Attachment>>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Addr {
    email: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Header {
    name: String,
    value: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Attachment {
    name: Option<String>,
    content: Option<String>,
    content_type: Option<String>,
    #[serde(rename = "ContentID")]
    content_id: Option<String>,
}

fn text(s: &Option<String>) -> &str {
    s.as_deref().map(str::trim).unwrap_or("")
}

impl Inbound {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .flatten()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    /// §2: `OriginalRecipient`, else the first To/Cc at the inbound domain.
    /// Returns (slug, token) — the local part split at `+`.
    fn recipient(&self, domain: &str) -> Option<(String, String)> {
        let original = self.original_recipient.iter().map(String::as_str);
        let listed = self
            .to_full
            .iter()
            .chain(self.cc_full.iter())
            .flatten()
            .filter_map(|a| a.email.as_deref());
        original.chain(listed).find_map(|address| {
            let (local, at) = address.trim().rsplit_once('@')?;
            if !at.eq_ignore_ascii_case(domain) {
                return None;
            }
            let local = local.to_lowercase();
            let (slug, token) = local.split_once('+').unwrap_or((&local, ""));
            Some((slug.to_string(), token.to_string()))
        })
    }
}

/// `<a@b>` → `a@b`; also splits `References`' whitespace-separated list.
fn message_ids(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .split_whitespace()
        .map(|id| id.trim_matches(|c| c == '<' || c == '>').to_string())
        .filter(|id| !id.is_empty())
        .collect()
}

/// Postmark's webhook URL carries basic-auth credentials (spec 002 §1).
fn authorized(headers: &HeaderMap, st: &AppState) -> bool {
    let expected = format!(
        "{}:{}",
        st.cfg.postmark_inbound_user, st.cfg.postmark_inbound_password
    );
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| STANDARD.decode(b.trim()).ok())
        .unwrap_or_default();
    // Compare digests, not strings, so the comparison time says nothing.
    Sha256::digest(&given) == Sha256::digest(expected.as_bytes())
}

async fn inbound(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    if !authorized(&headers, &st) {
        return StatusCode::UNAUTHORIZED;
    }
    let mail: Inbound = match serde_json::from_slice(&body) {
        Ok(mail) => mail,
        Err(e) => {
            tracing::error!("postmark inbound: unreadable payload: {e}");
            return StatusCode::BAD_REQUEST;
        }
    };
    let Some((slug, token)) = mail.recipient(&st.cfg.inbound_domain) else {
        return StatusCode::FORBIDDEN;
    };
    let workspace_id: Option<Uuid> = match sqlx::query_scalar("SELECT workspace_id_by_slug($1)")
        .bind(&slug)
        .fetch_one(&st.db)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("postmark inbound: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };
    // 403 tells Postmark to stop retrying: there is no one to store it for.
    let Some(ws) = workspace_id else {
        return StatusCode::FORBIDDEN;
    };
    let token = if token.is_empty() {
        text(&mail.mailbox_hash).to_lowercase()
    } else {
        token
    };
    match store(&st, ws, &mail, &token).await {
        Ok(()) => StatusCode::OK,
        Err(e) if unique_violation(&e) == Some("messages_message_id") => StatusCode::OK,
        Err(e) => {
            tracing::error!(workspace = %ws, "postmark inbound: storing failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn store(st: &AppState, ws: Uuid, mail: &Inbound, token: &str) -> Result<(), sqlx::Error> {
    let mut tx = tenant_tx(&st.db, ws).await?;
    let message_id = message_ids(mail.header("Message-ID")).into_iter().next();

    // §3: Postmark retries on failure, so duplicates are normal.
    if let Some(id) = &message_id {
        let seen: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE workspace_id = $1 AND message_id = $2)",
        )
        .bind(ws)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if seen {
            return Ok(());
        }
    }

    // §4: the token first, then the Message-IDs this mail answers.
    let mut ticket: Option<Uuid> = None;
    if !token.is_empty() {
        ticket =
            sqlx::query_scalar("SELECT id FROM tickets WHERE workspace_id = $1 AND token = $2")
                .bind(ws)
                .bind(token)
                .fetch_optional(&mut *tx)
                .await?;
    }
    if ticket.is_none() {
        let mut refs = message_ids(mail.header("In-Reply-To"));
        refs.extend(message_ids(mail.header("References")));
        if !refs.is_empty() {
            ticket = sqlx::query_scalar(
                "SELECT ticket_id FROM messages WHERE workspace_id = $1 AND message_id = ANY($2)
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(ws)
            .bind(&refs)
            .fetch_optional(&mut *tx)
            .await?;
        }
    }

    let from = mail.from_full.as_ref();
    let from_email = from.map(|a| text(&a.email)).unwrap_or("");
    let from_email = normalize_email(from_email).unwrap_or_else(|| from_email.to_lowercase());
    let from_name = from
        .map(|a| text(&a.name))
        .filter(|n| !n.is_empty())
        .map(str::to_string);

    // §7: when appending, the reply without its quoted history.
    let stripped = text(&mail.stripped_text_reply);
    let plain = text(&mail.text_body);
    let body = if ticket.is_some() && !stripped.is_empty() {
        stripped.to_string()
    } else if !plain.is_empty() {
        plain.to_string()
    } else {
        html_to_text(text(&mail.html_body))
    };

    let ticket_id = match ticket {
        // §6: an answer moves the ticket back to us — a closed one reopens
        // and leaves the brain (ADR 0010). The owner is kept.
        Some(id) => {
            sqlx::query(
                "UPDATE tickets SET status = 'waitingOnUs', last_activity_at = now(),
                        search = NULL, closed_at = NULL
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(ws)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            id
        }
        // §5: a new ticket, and the copilot's jobs in the same transaction.
        None => {
            let contact: Uuid = sqlx::query_scalar(
                "INSERT INTO contacts (workspace_id, email, name) VALUES ($1, $2, $3)
                 ON CONFLICT (workspace_id, email) DO UPDATE SET email = EXCLUDED.email
                 RETURNING id",
            )
            .bind(ws)
            .bind(&from_email)
            .bind(&from_name)
            .fetch_one(&mut *tx)
            .await?;
            let subject = text(&mail.subject);
            let subject = if subject.is_empty() {
                "(no subject)"
            } else {
                subject
            };
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO tickets (workspace_id, token, subject, contact_id)
                 VALUES ($1, $2, $3, $4) RETURNING id",
            )
            .bind(ws)
            .bind(hex::encode(rand::random::<[u8; 16]>()))
            .bind(subject)
            .bind(contact)
            .fetch_one(&mut *tx)
            .await?;
            jobs::enqueue(&mut tx, ws, "suggest", Some(id)).await?;
            jobs::enqueue(&mut tx, ws, "categorize", Some(id)).await?;
            id
        }
    };

    let html = text(&mail.html_body);
    let message: Uuid = sqlx::query_scalar(
        "INSERT INTO messages (workspace_id, ticket_id, kind, from_name, from_email, text, html_body, message_id)
         VALUES ($1, $2, 'customer', $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(ws)
    .bind(ticket_id)
    .bind(&from_name)
    .bind(&from_email)
    .bind(&body)
    .bind((!html.is_empty()).then_some(html))
    .bind(&message_id)
    .fetch_one(&mut *tx)
    .await?;

    // §8: every attachment, inline images included.
    for a in mail.attachments.iter().flatten() {
        let content: String = text(&a.content).split_whitespace().collect();
        let Ok(bytes) = STANDARD.decode(content) else {
            // ponytail: an undecodable attachment is logged and skipped; the mail itself is kept.
            tracing::warn!(workspace = %ws, %message, "inbound attachment is not base64, skipped");
            continue;
        };
        let content_type = text(&a.content_type);
        sqlx::query(
            "INSERT INTO attachments (workspace_id, message_id, name, content_type, size, content_id, content)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(ws)
        .bind(message)
        .bind(text(&a.name))
        .bind(if content_type.is_empty() { "application/octet-stream" } else { content_type })
        .bind(bytes.len() as i32)
        .bind(a.content_id.as_deref().filter(|c| !c.is_empty()))
        .bind(bytes)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await
}

/// The text of an HTML-only mail (spec 002 §7). Not a renderer — enough to
/// read the message: tags and script/style contents dropped, block tags
/// become line breaks, entities decoded, whitespace collapsed.
pub fn html_to_text(html: &str) -> String {
    // ASCII lowercasing keeps byte offsets, so `lower` indexes like `html`.
    let lower = html.to_ascii_lowercase();
    let mut out = String::new();
    let mut i = 0;
    while i < html.len() {
        let rest = &html[i..];
        if rest.starts_with('<') {
            let skip_to = ["script", "style"]
                .iter()
                .find(|t| lower[i + 1..].starts_with(*t))
                .map(|t| {
                    lower[i..]
                        .find(&format!("</{t}"))
                        .map_or(html.len(), |e| i + e)
                });
            if let Some(end) = skip_to.filter(|&e| e > i) {
                i = end;
                continue;
            }
            let Some(close) = rest.find('>') else { break };
            let name: String = lower[i + 1..i + close]
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if matches!(
                name.as_str(),
                "br" | "p"
                    | "div"
                    | "li"
                    | "tr"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "blockquote"
                    | "table"
                    | "hr"
            ) {
                out.push('\n');
            }
            i += close + 1;
        } else if rest.starts_with('&') {
            let (decoded, len) = entity(rest);
            out.push_str(&decoded);
            i += len;
        } else {
            let c = rest.chars().next().unwrap();
            // Source whitespace is not layout in HTML.
            out.push(if c.is_whitespace() { ' ' } else { c });
            i += c.len_utf8();
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for line in out.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !(line.is_empty() && lines.last().is_none_or(|l| l.is_empty())) {
            lines.push(line);
        }
    }
    lines.join("\n").trim().to_string()
}

/// One entity at the start of `s` → (text, bytes consumed). Unknown → `&`.
fn entity(s: &str) -> (String, usize) {
    // `;` is ASCII, so its byte position is a char boundary.
    let Some(end) = s.bytes().take(12).position(|b| b == b';') else {
        return ("&".into(), 1);
    };
    let name = &s[1..end];
    let decoded = match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => name
            .strip_prefix("#x")
            .or_else(|| name.strip_prefix("#X"))
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
            .and_then(char::from_u32),
    };
    match decoded {
        Some(c) => (c.to_string(), end + 1),
        None => ("&".into(), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_readable_text() {
        let html = "<html><head><style>p { color: red }</style><script>alert(1)</script></head>\
                    <body><p>Hi&nbsp;there,</p>\n\n<p>Tom &amp; Jerry &lt;3 caf&#233; &#x2713;</p>\
                    <br><br><br><div>Bye<br/>Ola</div> &bogus; & done</body></html>";
        assert_eq!(
            html_to_text(html),
            "Hi there,\n\nTom & Jerry <3 café ✓\n\nBye\nOla\n&bogus; & done"
        );
        assert_eq!(html_to_text(""), "");
        assert_eq!(html_to_text("<p>unclosed <b"), "unclosed");
        // Double-encoded stays single-decoded.
        assert_eq!(html_to_text("&amp;lt;"), "&lt;");
    }

    #[test]
    fn recipients_and_ids() {
        let mail: Inbound = serde_json::from_value(serde_json::json!({
            "OriginalRecipient": "support@acme.com",
            "ToFull": [{"Email": "support@acme.com"}, {"Email": "Acme+AB12@In.Muninn.io"}],
            "CcFull": null,
        }))
        .unwrap();
        assert_eq!(
            mail.recipient("in.muninn.io"),
            Some(("acme".into(), "ab12".into()))
        );
        assert_eq!(
            message_ids(Some(" <a@x> \n <b@y>")),
            vec!["a@x".to_string(), "b@y".to_string()]
        );
    }
}
