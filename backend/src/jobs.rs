//! The job queue (ADR 0004): a `jobs` table, claimed with
//! `FOR UPDATE SKIP LOCKED`, worked by a loop inside the web binary.
//!
//! A claim pushes `run_at` forward by a five-minute lease, so a job whose process died is
//! picked up again after the lease — no transaction is held while a job calls
//! Jev or Postmark. A finished job is deleted; one out of attempts keeps its
//! row with `failed_at` and `last_error` for whoever investigates.

use std::sync::Arc;
use std::time::Duration;

use sqlx::{Postgres, Transaction};
use tokio::sync::{Semaphore, watch};
use uuid::Uuid;

use crate::AppState;

/// The first try and three retries (spec 002 §18, spec 003 §6).
const MAX_ATTEMPTS: i32 = 4;
const CONCURRENCY: usize = 8;

#[derive(Debug)]
pub enum JobError {
    /// Try again after this long, unless attempts are used up.
    Retry(Duration, String),
    /// Give up now.
    Fail(String),
}

impl From<sqlx::Error> for JobError {
    fn from(e: sqlx::Error) -> JobError {
        JobError::Retry(Duration::from_secs(30), format!("database: {e}"))
    }
}

pub type JobResult = Result<(), JobError>;

/// Enqueue inside the transaction that wrote the row the job is about.
pub async fn enqueue(
    tx: &mut Transaction<'static, Postgres>,
    workspace_id: Uuid,
    kind: &str,
    subject_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO jobs (workspace_id, kind, subject_id) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(kind)
        .bind(subject_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A job about a HubSpot object (spec 007), due in `delay_secs`. One for the
/// same object that has not started yet absorbs it (§18).
pub async fn enqueue_object(
    tx: &mut Transaction<'static, Postgres>,
    workspace_id: Uuid,
    kind: &str,
    subject_id: Option<Uuid>,
    object_ref: &str,
    delay_secs: f64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO jobs (workspace_id, kind, subject_id, object_ref, run_at)
         VALUES ($1, $2, $3, $4, now() + $5 * interval '1 second')
         ON CONFLICT (workspace_id, kind, object_ref)
             WHERE object_ref IS NOT NULL AND attempts = 0 AND failed_at IS NULL
         DO NOTHING",
    )
    .bind(workspace_id)
    .bind(kind)
    .bind(subject_id)
    .bind(object_ref)
    .bind(delay_secs)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub workspace_id: Uuid,
    pub kind: String,
    pub subject_id: Option<Uuid>,
    pub object_ref: Option<String>,
    pub attempts: i32,
}

async fn claim(st: &AppState) -> Result<Option<Job>, sqlx::Error> {
    // The lease: longer than any job takes (HTTP timeout is 30 s).
    sqlx::query_as(
        "UPDATE jobs SET run_at = now() + interval '5 minutes', attempts = attempts + 1
         WHERE id = (SELECT id FROM jobs WHERE failed_at IS NULL AND run_at <= now()
                     ORDER BY run_at LIMIT 1 FOR UPDATE SKIP LOCKED)
         RETURNING id, workspace_id, kind, subject_id, object_ref, attempts",
    )
    .fetch_optional(&st.db)
    .await
}

async fn dispatch(st: &AppState, job: &Job) -> JobResult {
    let ws = job.workspace_id;
    let id = job.subject_id;
    let subject = || id.ok_or_else(|| JobError::Fail("job without subject".into()));
    let object = || {
        job.object_ref
            .as_deref()
            .ok_or_else(|| JobError::Fail("job without object".into()))
    };
    match job.kind.as_str() {
        "suggest" => crate::copilot::suggest_job(st, ws, subject()?).await,
        "categorize" => crate::categories::categorize_job(st, ws, subject()?).await,
        "send" => crate::mail::send_job(st, ws, subject()?).await,
        "seats" => crate::billing::seats_job(st, ws).await,
        "hubspotImport" => crate::hubspot::import_job(st, ws, subject()?, object()?).await,
        "hubspotCheck" => crate::hubspot::check_job(st, ws, subject()?).await,
        "hubspotTicket" => crate::hubspot::ticket_job(st, ws, object()?).await,
        "hubspotThread" => crate::hubspot::thread_job(st, ws, object()?).await,
        other => Err(JobError::Fail(format!("unknown job kind {other}"))),
    }
}

/// Out of attempts: leave the subject in a state the UI can show
/// ("Suggestions unavailable", "Not sent — Retry").
async fn give_up(st: &AppState, job: &Job, error: &str) {
    let (ws, id) = (job.workspace_id, job.subject_id.unwrap_or_default());
    let result = match job.kind.as_str() {
        "suggest" => crate::copilot::suggest_gave_up(st, ws, id).await,
        "send" => crate::mail::send_gave_up(st, ws, id, error).await,
        _ => Ok(()), // categorize: stays uncategorised (spec 004 §10); seats: logged
    };
    if let Err(e) = result {
        tracing::error!(job = job.id, "give_up: {e}");
    }
}

/// Run one claimed job to completion and record the outcome.
pub async fn run(st: &AppState, job: Job) {
    let outcome = dispatch(st, &job).await;
    let result = match &outcome {
        Ok(()) => {
            sqlx::query("DELETE FROM jobs WHERE id = $1")
                .bind(job.id)
                .execute(&st.db)
                .await
        }
        Err(JobError::Retry(after, msg)) if job.attempts < MAX_ATTEMPTS => {
            tracing::warn!(
                job = job.id,
                kind = job.kind,
                attempt = job.attempts,
                "retrying: {msg}"
            );
            sqlx::query("UPDATE jobs SET run_at = now() + $2 * interval '1 second', last_error = $3 WHERE id = $1")
                .bind(job.id)
                .bind(after.as_secs_f64())
                .bind(msg)
                .execute(&st.db)
                .await
        }
        Err(JobError::Retry(_, msg) | JobError::Fail(msg)) => {
            tracing::error!(job = job.id, kind = job.kind, "failed: {msg}");
            give_up(st, &job, msg).await;
            sqlx::query("UPDATE jobs SET failed_at = now(), last_error = $2 WHERE id = $1")
                .bind(job.id)
                .bind(msg)
                .execute(&st.db)
                .await
        }
    };
    if let Err(e) = result {
        tracing::error!(job = job.id, "recording outcome: {e}");
    }
}

/// Run every job that is due now, one at a time. Tests call this directly.
pub async fn run_due(st: &AppState) -> usize {
    let mut n = 0;
    while let Ok(Some(job)) = claim(st).await {
        run(st, job).await;
        n += 1;
    }
    n
}

/// The loop in the web process. Polls once a second when idle; the sidebar
/// polls every 2 seconds (spec 003 §8), so LISTEN/NOTIFY would buy nothing.
///
/// When `stop` turns true it claims nothing more and returns once the jobs
/// already running have finished: a `send` cut off after Postmark accepted the
/// reply would run again after its lease and send the reply twice.
pub async fn worker(st: AppState, mut stop: watch::Receiver<bool>) {
    let slots = Arc::new(Semaphore::new(CONCURRENCY));
    loop {
        let permit = tokio::select! {
            biased;
            _ = stop.wait_for(|s| *s) => break,
            p = slots.clone().acquire_owned() => p.expect("semaphore"),
        };
        let idle = match claim(&st).await {
            Ok(Some(job)) => {
                let st = st.clone();
                tokio::spawn(async move {
                    run(&st, job).await;
                    drop(permit);
                });
                continue;
            }
            Ok(None) => Duration::from_secs(1),
            Err(e) => {
                tracing::error!("claiming job: {e}");
                Duration::from_secs(5)
            }
        };
        drop(permit);
        tokio::select! {
            biased;
            _ = stop.wait_for(|s| *s) => break,
            _ = tokio::time::sleep(idle) => {}
        }
    }
    let _ = slots.acquire_many(CONCURRENCY as u32).await;
}

/// Hourly: setup and reset links a day after they expire, sessions once they can no
/// longer log anyone in (spec 001 §30). They hold email addresses, and
/// nothing reads them after that.
pub async fn housekeeping(st: AppState) {
    let mut hourly = tokio::time::interval(Duration::from_secs(3600));
    loop {
        hourly.tick().await;
        if let Err(e) = clean(&st.db).await {
            tracing::error!("housekeeping: {e}");
        }
    }
}

pub async fn clean(db: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM auth_links WHERE expires_at < now() - interval '1 day'")
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE last_used_at < now() - interval '30 days'")
        .execute(db)
        .await?;
    Ok(())
}
