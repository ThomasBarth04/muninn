//! Spec 005: an mbox export of the old support mailbox becomes closed tickets
//! in the brain. Pass one streams the files and keeps each message's text;
//! pass two threads them and writes one closed ticket per solved thread.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;

use chrono::{DateTime, Utc};
use mail_parser::mailbox::mbox::MessageIterator;
use mail_parser::{HeaderValue, MessageParser, PartType};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::AppState;
use crate::db::{tenant_tx, unique_violation};
use crate::inbound::html_to_text;
use crate::session::normalize_email;

/// One message as the import keeps it: text only, quoted history cut.
#[derive(Debug, Clone)]
pub struct Mail {
    pub message_id: String,
    /// The Message-IDs it answers, nearest first.
    pub parents: Vec<String>,
    pub from_email: String,
    pub from_name: Option<String>,
    pub date: DateTime<Utc>,
    pub subject: String,
    pub text: String,
}

#[derive(Debug)]
enum Read {
    Mail(Mail),
    Automatic,
    Unreadable,
}

/// Who the support team is: exact addresses and `@domain`s (§2).
pub struct Team(Vec<String>);

impl Team {
    pub fn new(specs: &[String]) -> Team {
        Team(
            specs
                .iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        )
    }

    fn has(&self, email: &str) -> bool {
        self.0.iter().any(|t| {
            if t.starts_with('@') {
                email.ends_with(t.as_str())
            } else {
                email == t
            }
        })
    }
}

fn ids(value: &HeaderValue) -> Vec<String> {
    let clean = |id: &str| id.trim().trim_matches(|c| c == '<' || c == '>').to_string();
    match value {
        HeaderValue::Text(id) => vec![clean(id)],
        HeaderValue::TextList(list) => list.iter().map(|id| clean(id)).collect(),
        _ => vec![],
    }
    .into_iter()
    .filter(|id| !id.is_empty())
    .collect()
}

/// §3, §7, §8: one raw message → what the import keeps of it.
fn read(raw: &[u8]) -> Read {
    let Some(m) = MessageParser::default().parse(raw) else {
        return Read::Unreadable;
    };
    let header = |name: &str| m.header_raw(name).map(|v| v.trim().to_ascii_lowercase());
    let from = m.from().and_then(|a| a.first());
    let from_email = from.and_then(|a| a.address()).and_then(normalize_email);
    let report = m
        .header("Content-Type")
        .and_then(|h| h.as_content_type())
        .is_some_and(|c| {
            c.ctype().eq_ignore_ascii_case("multipart")
                && c.subtype()
                    .is_some_and(|s| s.eq_ignore_ascii_case("report"))
        });
    let automatic = header("Auto-Submitted").is_some_and(|v| v != "no")
        || header("Precedence")
            .is_some_and(|v| matches!(v.as_str(), "bulk" | "junk" | "list" | "auto_reply"))
        || header("X-Autoreply").is_some()
        || header("X-Autorespond").is_some()
        || report
        || from_email
            .as_deref()
            .is_some_and(|e| e.starts_with("mailer-daemon@") || e.starts_with("postmaster@"));
    if automatic {
        return Read::Automatic;
    }
    let date = m
        .date()
        .filter(|d| d.is_valid())
        .and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0));
    let (Some(from_email), Some(date)) = (from_email, date) else {
        return Read::Unreadable;
    };

    let body = match m.text_part(0).map(|p| &p.body) {
        Some(PartType::Text(text)) => text.to_string(),
        Some(PartType::Html(html)) => html_to_text(html),
        _ => String::new(),
    };
    let text = strip_quotes(&body);
    let message_id = m
        .message_id()
        .map(|id| id.trim_matches(|c| c == '<' || c == '>').to_string())
        .filter(|id| !id.is_empty())
        // §11: without one, the same mail must get the same id on every run.
        .unwrap_or_else(|| {
            let hash = Sha256::digest(format!("{from_email}\n{}\n{text}", date.timestamp()));
            format!("{}@import.muninn", hex::encode(&hash[..16]))
        });
    let mut parents = ids(m.in_reply_to());
    parents.extend(ids(m.references()).into_iter().rev());
    Read::Mail(Mail {
        message_id,
        parents,
        from_email,
        from_name: from
            .and_then(|a| a.name())
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
        date,
        subject: m.subject().unwrap_or("").trim().to_string(),
        text,
    })
}

/// Does quoted history start at line `i`? (§8)
fn quote_starts(lines: &[&str], i: usize) -> bool {
    let line = lines[i].trim().to_lowercase();
    let next = lines
        .get(i + 1)
        .map(|l| l.trim().to_lowercase())
        .unwrap_or_default();
    // "On Mon, 3 Mar 2025 at 10:12, Ola <ola@kunde.no> wrote:", on one line or wrapped.
    line.ends_with("wrote:")
        || (line.starts_with("on ") && next.ends_with("wrote:"))
        // "tir. 3. mar. 2025 kl. 10:12 skrev Ola Nordmann <ola@kunde.no>:", likewise.
        || (line.contains("skrev") && (line.ends_with(">:") || next.ends_with(">:")))
        || matches!(
            line.trim_matches('-').trim(),
            "original message" | "opprinnelig melding"
        )
        // Outlook's header block: From/Fra, then Sent/Sendt a line or two later.
        || ((line.starts_with("from:") || line.starts_with("fra:"))
            && lines[i + 1..].iter().take(4).any(|l| {
                let l = l.trim().to_lowercase();
                l.starts_with("sent:") || l.starts_with("sendt:")
            }))
}

/// §8: the message without the thread it quotes. Falls back to the whole
/// text when nothing is left, so a reply is never empty for a heuristic.
pub fn strip_quotes(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let cut = (0..lines.len())
        .find(|&i| quote_starts(&lines, i))
        .unwrap_or(lines.len());
    let mut kept: Vec<&str> = lines[..cut]
        .iter()
        .filter(|l| !l.trim_start().starts_with('>'))
        .copied()
        .collect();
    // Outlook's "______" rule above its header block.
    while kept
        .last()
        .is_some_and(|l| l.trim().chars().all(|c| c == '_' || c == '-'))
    {
        kept.pop();
    }
    let out = kept.join("\n").trim().to_string();
    if out.is_empty() {
        text.trim().to_string()
    } else {
        out
    }
}

/// §4: a message hangs under the nearest message it answers that is in the
/// export; without one it starts a thread. Threads come oldest first, each
/// in date order.
pub fn threads(mails: Vec<Mail>) -> Vec<Vec<Mail>> {
    let index: HashMap<&str, usize> = mails
        .iter()
        .enumerate()
        .map(|(i, m)| (m.message_id.as_str(), i))
        .collect();
    let parent: Vec<Option<usize>> = mails
        .iter()
        .enumerate()
        .map(|(i, m)| {
            m.parents
                .iter()
                .find_map(|p| index.get(p.as_str()).copied().filter(|&p| p != i))
        })
        .collect();
    // Up the tree to the root. References can loop; a loop's root is its
    // smallest index, the same from wherever it is entered.
    let root = |start: usize| {
        let mut path = vec![start];
        let mut at = start;
        while let Some(p) = parent[at] {
            if let Some(k) = path.iter().position(|&x| x == p) {
                return *path[k..].iter().min().expect("a loop has members");
            }
            path.push(p);
            at = p;
        }
        at
    };
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..mails.len() {
        groups.entry(root(i)).or_default().push(i);
    }
    let mut slots: Vec<Option<Mail>> = mails.into_iter().map(Some).collect();
    let mut threads: Vec<Vec<Mail>> = groups
        .into_values()
        .map(|mut members| {
            members.sort_by_key(|&i| (slots[i].as_ref().expect("once").date, i));
            members
                .iter()
                .map(|&i| slots[i].take().expect("once"))
                .collect()
        })
        .collect();
    threads.sort_by_key(|t| (t[0].date, t[0].message_id.clone()));
    threads
}

#[derive(Default)]
struct Reading {
    read: usize,
    automatic: usize,
    unreadable: usize,
    mails: Vec<Mail>,
}

/// Pass one (§14): the files are streamed a message at a time and only the
/// text is kept, so attachments cost nothing.
/// ponytail: every message's text is held in memory until threading; spool to
/// a temporary table if an export's text alone outgrows the box.
fn read_files(paths: &[String]) -> Result<Reading, String> {
    let mut r = Reading::default();
    let mut seen = HashSet::new();
    for path in paths {
        let file = File::open(path).map_err(|e| format!("cannot read {path} ({e})"))?;
        for raw in MessageIterator::new(BufReader::new(file)) {
            let raw = raw.map_err(|e| format!("cannot read {path} ({e})"))?;
            r.read += 1;
            if r.read % 5000 == 0 {
                eprintln!("read {} messages…", r.read);
            }
            match read(raw.contents()) {
                Read::Automatic => r.automatic += 1,
                Read::Unreadable => r.unreadable += 1,
                Read::Mail(mail) => {
                    if seen.insert(mail.message_id.clone()) {
                        r.mails.push(mail);
                    }
                }
            }
        }
    }
    Ok(r)
}

enum Outcome {
    Imported(usize),
    NoTeamReply,
    StartedByTheTeam,
    AlreadyIn,
}

/// §5–10: one thread, one transaction.
async fn import_thread(
    st: &AppState,
    ws: Uuid,
    team: &Team,
    thread: &[Mail],
) -> Result<Outcome, sqlx::Error> {
    if team.has(&thread[0].from_email) {
        return Ok(Outcome::StartedByTheTeam);
    }
    if !thread[1..].iter().any(|m| team.has(&m.from_email)) {
        return Ok(Outcome::NoTeamReply);
    }
    let mut tx = tenant_tx(&st.db, ws).await?;
    let ids: Vec<&str> = thread.iter().map(|m| m.message_id.as_str()).collect();
    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM messages WHERE workspace_id = $1 AND message_id = ANY($2))",
    )
    .bind(ws)
    .bind(&ids)
    .fetch_one(&mut *tx)
    .await?;
    if already {
        return Ok(Outcome::AlreadyIn);
    }

    let (first, last) = (&thread[0], &thread[thread.len() - 1]);
    let contact: Uuid = sqlx::query_scalar(
        "INSERT INTO contacts (workspace_id, email, name) VALUES ($1, $2, $3)
         ON CONFLICT (workspace_id, email) DO UPDATE SET email = EXCLUDED.email
         RETURNING id",
    )
    .bind(ws)
    .bind(&first.from_email)
    .bind(&first.from_name)
    .fetch_one(&mut *tx)
    .await?;
    let subject = if first.subject.is_empty() {
        "(no subject)"
    } else {
        &first.subject
    };
    // Closed, no owner, priority or category, and not for Jev (§6).
    let ticket: Uuid = sqlx::query_scalar(
        "INSERT INTO tickets (workspace_id, token, subject, contact_id, status, suggest_status,
                              created_at, last_activity_at, closed_at, imported_at)
         VALUES ($1, $2, $3, $4, 'closed', 'ready', $5, $6, $6, now()) RETURNING id",
    )
    .bind(ws)
    .bind(hex::encode(rand::random::<[u8; 16]>()))
    .bind(subject)
    .bind(contact)
    .bind(first.date)
    .bind(last.date)
    .fetch_one(&mut *tx)
    .await?;
    let kinds: Vec<&str> = thread
        .iter()
        .map(|m| {
            if team.has(&m.from_email) {
                "agent"
            } else {
                "customer"
            }
        })
        .collect();
    let names: Vec<Option<&str>> = thread.iter().map(|m| m.from_name.as_deref()).collect();
    let emails: Vec<&str> = thread.iter().map(|m| m.from_email.as_str()).collect();
    let texts: Vec<&str> = thread.iter().map(|m| m.text.as_str()).collect();
    let dates: Vec<DateTime<Utc>> = thread.iter().map(|m| m.date).collect();
    // Team replies count as sent at their date, and are never sent (§9).
    sqlx::query(
        "INSERT INTO messages (workspace_id, ticket_id, kind, from_name, from_email, text, message_id,
                               created_at, delivery_status, sent_at)
         SELECT $1, $2, kind, name, email, text, id, at,
                CASE WHEN kind = 'agent' THEN 'sent' END, CASE WHEN kind = 'agent' THEN at END
         FROM unnest($3::text[], $4::text[], $5::text[], $6::text[], $7::text[], $8::timestamptz[])
              AS x(kind, name, email, text, id, at)",
    )
    .bind(ws)
    .bind(ticket)
    .bind(&kinds)
    .bind(&names)
    .bind(&emails)
    .bind(&texts)
    .bind(&ids)
    .bind(&dates)
    .execute(&mut *tx)
    .await?;
    // Into the brain, exactly as closing does (§10).
    sqlx::query(
        "UPDATE tickets SET search = brain_tsvector($1, $2) WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(ticket)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Outcome::Imported(thread.len()))
}

/// `muninn admin import <slug> <export.mbox>… --team <address or @domain>…`
pub async fn run(
    st: &AppState,
    slug: &str,
    paths: &[String],
    team: &[String],
) -> Result<String, String> {
    let team = Team::new(team);
    if team.0.is_empty() {
        return Err("--team is required".into());
    }
    let db = |e: sqlx::Error| format!("database: {e} — run the import again to continue");
    let ws: Uuid = sqlx::query_scalar::<_, Option<Uuid>>("SELECT workspace_id_by_slug($1)")
        .bind(slug)
        .fetch_one(&st.db)
        .await
        .map_err(db)?
        .ok_or(format!("no workspace {slug}"))?;
    for path in paths {
        File::open(path).map_err(|e| format!("cannot read {path} ({e})"))?;
    }

    let paths = paths.to_vec();
    let reading = tokio::task::spawn_blocking(move || read_files(&paths))
        .await
        .map_err(|e| format!("reading failed: {e}"))??;
    let threads = threads(reading.mails);

    let (mut tickets, mut messages, mut no_reply, mut by_team, mut already) = (0, 0, 0, 0, 0);
    for (n, thread) in threads.iter().enumerate() {
        match import_thread(st, ws, &team, thread).await {
            Ok(Outcome::Imported(count)) => {
                tickets += 1;
                messages += count;
            }
            Ok(Outcome::NoTeamReply) => no_reply += 1,
            Ok(Outcome::StartedByTheTeam) => by_team += 1,
            Ok(Outcome::AlreadyIn) => already += 1,
            // Mail with one of these ids arrived while the import ran.
            Err(e) if unique_violation(&e) == Some("messages_message_id") => already += 1,
            Err(e) => return Err(db(e)),
        }
        if (n + 1) % 1000 == 0 {
            eprintln!("{} of {} threads, {tickets} tickets…", n + 1, threads.len());
        }
    }

    let mut tx = tenant_tx(&st.db, ws).await.map_err(db)?;
    let brain: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tickets WHERE workspace_id = $1 AND status = 'closed'",
    )
    .bind(ws)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(format!(
        "{slug}: read {} messages in {} threads\n\
         imported {tickets} tickets ({messages} messages)\n\
         skipped {} threads: {no_reply} no team reply, {by_team} started by the team, {already} already in Muninn\n\
         dropped {} automatic and {} unreadable messages\n\
         brain: {brain} cases",
        reading.read,
        threads.len(),
        no_reply + by_team + already,
        reading.automatic,
        reading.unreadable,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_history_is_cut() {
        for (mail, reply) in [
            (
                "Re-upload the certificate under Settings → SSO.\n\nOn Mon, 3 Mar 2025 at 10:12, Ola <ola@kunde.no> wrote:\n> SSO fails\n> since today",
                "Re-upload the certificate under Settings → SSO.",
            ),
            (
                "Thanks, that fixed it!\n\nOn Mon, 3 Mar 2025 at 10:12, Ola Nordmann <\nola@kunde.no> wrote:\n> old",
                "Thanks, that fixed it!",
            ),
            (
                "Last opp sertifikatet på nytt.\n\ntir. 3. mar. 2025 kl. 10:12 skrev Ola Nordmann <ola@kunde.no>:\n> Innlogging feiler",
                "Last opp sertifikatet på nytt.",
            ),
            (
                "Prøv igjen nå.\n\n________________________________\nFra: Ola Nordmann <ola@kunde.no>\nSendt: 3. mars 2025 10:12\nTil: support@acme.no\nEmne: SSO",
                "Prøv igjen nå.",
            ),
            (
                "Try now.\n\n-----Original Message-----\nFrom: Ola\nSent: Monday\n\nSSO fails",
                "Try now.",
            ),
            (
                "Inline:\n> is it SSO?\nYes, it is SSO.",
                "Inline:\nYes, it is SSO.",
            ),
            // A sentence about writing is not a quote header.
            (
                "Jeg skrev til dere i går.\nHer er feilmeldingen:\nError 42",
                "Jeg skrev til dere i går.\nHer er feilmeldingen:\nError 42",
            ),
            // Nothing but a quote: keep it rather than an empty reply.
            ("> only a quote", "> only a quote"),
        ] {
            assert_eq!(strip_quotes(mail), reply, "{mail}");
        }
    }

    fn mail(id: &str, parents: &[&str], minute: i64) -> Mail {
        Mail {
            message_id: id.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            from_email: "ola@kunde.no".into(),
            from_name: None,
            date: DateTime::from_timestamp(1_700_000_000 + minute * 60, 0).unwrap(),
            subject: String::new(),
            text: String::new(),
        }
    }

    fn shape(threads: &[Vec<Mail>]) -> Vec<Vec<&str>> {
        threads
            .iter()
            .map(|t| t.iter().map(|m| m.message_id.as_str()).collect())
            .collect()
    }

    #[test]
    fn threads_follow_the_nearest_answered_message_in_the_export() {
        let threads = threads(vec![
            // A reply read before the mail it answers.
            mail("b", &["a"], 2),
            mail("a", &[], 1),
            // Its direct parent is missing; its grandparent is here.
            mail("c", &["missing", "a"], 3),
            // Answers only a mail outside the export: a thread of its own.
            mail("d", &["gone"], 4),
            mail("e", &["gone"], 5),
            // A loop of references still ends.
            mail("x", &["y"], 6),
            mail("y", &["x"], 7),
        ]);
        assert_eq!(
            shape(&threads),
            vec![vec!["a", "b", "c"], vec!["d"], vec!["e"], vec!["x", "y"]]
        );
    }

    #[test]
    fn reading_one_message() {
        let raw = "From: =?ISO-8859-1?Q?Ola_Nordmann?= <Ola@Kunde.no>\r\n\
                   Date: Mon, 3 Mar 2025 10:12:00 +0100\r\n\
                   Subject: =?UTF-8?B?SW5ubG9nZ2luZyBmZWlsZXI=?=\r\n\
                   Message-ID: <m1@kunde.no>\r\n\
                   References: <m0@acme.no>\r\n\
                   In-Reply-To: <m0@acme.no>\r\n\
                   Content-Type: text/plain; charset=iso-8859-1\r\n\
                   Content-Transfer-Encoding: quoted-printable\r\n\r\n\
                   P=E5 nytt: det feiler.\r\n\r\n> gammelt\r\n";
        let Read::Mail(m) = read(raw.as_bytes()) else {
            panic!("readable")
        };
        assert_eq!(m.from_email, "ola@kunde.no");
        assert_eq!(m.from_name.as_deref(), Some("Ola Nordmann"));
        assert_eq!(m.subject, "Innlogging feiler");
        assert_eq!(m.message_id, "m1@kunde.no");
        assert_eq!(m.parents, vec!["m0@acme.no", "m0@acme.no"]);
        assert_eq!(m.text, "På nytt: det feiler.");
        assert_eq!(m.date.to_rfc3339(), "2025-03-03T09:12:00+00:00");

        let html = "From: support@acme.no\r\nDate: Mon, 3 Mar 2025 11:00:00 +0000\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\r\n<p>Hei <b>Ola</b>,</p><p>pr&oslash;v n&aring;</p>";
        let Read::Mail(m) = read(html.as_bytes()) else {
            panic!("readable")
        };
        assert_eq!(
            m.text,
            html_to_text("<p>Hei <b>Ola</b>,</p><p>pr&oslash;v n&aring;</p>")
        );
        assert!(m.message_id.ends_with("@import.muninn"), "{}", m.message_id);
        let Read::Mail(again) = read(html.as_bytes()) else {
            panic!("readable")
        };
        assert_eq!(
            again.message_id, m.message_id,
            "the same mail gets the same id"
        );

        for automatic in [
            "Auto-Submitted: auto-replied\r\n",
            "Precedence: bulk\r\n",
            "X-Autoreply: yes\r\n",
            "Content-Type: multipart/report; report-type=delivery-status; boundary=x\r\n",
        ] {
            let raw = format!(
                "From: ola@kunde.no\r\nDate: Mon, 3 Mar 2025 10:12:00 +0000\r\n{automatic}\r\nOut of office"
            );
            assert!(
                matches!(read(raw.as_bytes()), Read::Automatic),
                "{automatic}"
            );
        }
        let bounce = "From: MAILER-DAEMON@kunde.no\r\nDate: Mon, 3 Mar 2025 10:12:00 +0000\r\n\r\nUndeliverable";
        assert!(matches!(read(bounce.as_bytes()), Read::Automatic));
        let human = "From: ola@kunde.no\r\nDate: Mon, 3 Mar 2025 10:12:00 +0000\r\nAuto-Submitted: no\r\n\r\nHi";
        assert!(matches!(read(human.as_bytes()), Read::Mail(_)));
        let undated = "From: ola@kunde.no\r\n\r\nHi";
        assert!(matches!(read(undated.as_bytes()), Read::Unreadable));
        let anonymous = "Date: Mon, 3 Mar 2025 10:12:00 +0000\r\n\r\nHi";
        assert!(matches!(read(anonymous.as_bytes()), Read::Unreadable));
    }
}
