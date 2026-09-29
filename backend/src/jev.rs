//! The Jev client (ADR 0002): one POST to api.typesafe.ai with reqwest.
//! API: https://docs.typesafe.ai/api.md — `answers` is keyed like `questions`;
//! a `noul` answer carries `noul` (0–1), a `choice` answer carries `choice`,
//! `probabilities` (every option) and `confidence`.

use std::time::Duration;

use serde_json::{Value, json};

use crate::AppState;
use crate::jobs::JobError;

/// Ask Jev `questions` about `state`; returns the `answers` object.
/// Rate limits and overload become retries (spec 003 §6); a bad key or a
/// request Jev rejects is our bug and fails at once.
pub async fn ask(st: &AppState, state: Value, questions: Value) -> Result<Value, JobError> {
    let Some(key) = &st.cfg.jev_api_key else {
        return Err(JobError::Fail("JEV_API_KEY unset".into()));
    };
    let res = st
        .http
        .post(format!("{}/v1/systemone", st.cfg.jev_api_url))
        .bearer_auth(key)
        .json(&json!({ "model": "jev-latest", "state": state, "questions": questions }))
        .send()
        .await
        .map_err(|e| JobError::Retry(Duration::from_secs(5), format!("Jev unreachable: {e}")))?;
    let status = res.status().as_u16();
    // Not documented by TypeSafe, but honoured if sent.
    let retry_after = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .map_or(Duration::from_secs(5), Duration::from_secs);
    let body = res.text().await.unwrap_or_default();
    match status {
        200 => {
            let v: Value = serde_json::from_str(&body)
                .map_err(|e| JobError::Fail(format!("Jev answered unparseable JSON: {e}")))?;
            Ok(v["answers"].clone())
        }
        // 429 rate limited, 529 overloaded, other 5xx: Jev's side, try again.
        429 | 500.. => Err(JobError::Retry(
            retry_after,
            format!("Jev {status}: {body}"),
        )),
        // 401 bad key, 422 bad request: ours, retrying cannot help.
        _ => {
            tracing::error!("Jev {status} — configuration bug (key or request shape): {body}");
            Err(JobError::Fail(format!("Jev {status}: {body}")))
        }
    }
}
