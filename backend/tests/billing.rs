//! Spec 001 §23–27: Checkout, the Stripe webhook, seat sync (switched off in
//! production during the beta, kept working).

mod common;

use common::{Client, STRIPE_WEBHOOK_SECRET, TestApp};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

/// A workspace on a trial, the state Checkout starts from once self-serve
/// signup returns (the beta creates workspaces active).
async fn trial_owner(app: &TestApp, email: &str, slug: &str) -> Client {
    let (c, _) = app.owner(slug, email).await;
    sqlx::query(
        "UPDATE workspaces SET billing_status = 'trialing', trial_ends_at = now() + interval '14 days'
         WHERE slug = $1",
    )
    .bind(slug)
    .execute(&app.owner)
    .await
    .unwrap();
    c
}

/// POST an event to the webhook, signed like Stripe does unless `sig` is given.
async fn deliver(app: &TestApp, event: &Value, sig: Option<&str>) -> u16 {
    let body = event.to_string();
    let t = chrono::Utc::now().timestamp();
    let mut mac = Hmac::<Sha256>::new_from_slice(STRIPE_WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(format!("{t}.{body}").as_bytes());
    let good = format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()));
    reqwest::Client::new()
        .post(format!("{}/hooks/stripe", app.url))
        .header("content-type", "application/json")
        .header("stripe-signature", sig.unwrap_or(&good))
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn stripe_mock(app: &TestApp) {
    app.mock.respond(|_, path, _| match path {
        "/v1/checkout/sessions" => Some((
            200,
            json!({ "url": "https://checkout.stripe.test/c/pay/cs_1" }),
        )),
        "/v1/billing_portal/sessions" => Some((
            200,
            json!({ "url": "https://billing.stripe.test/p/session/1" }),
        )),
        "/v1/subscriptions/sub_1" => Some((
            200,
            json!({ "id": "sub_1", "items": { "data": [{ "id": "si_1" }] } }),
        )),
        _ => None,
    });
}

#[tokio::test]
async fn checkout_webhook_and_replays() {
    let Some(app) = common::spawn().await else {
        return;
    };
    stripe_mock(&app);
    let mut frank = trial_owner(&app, "frank@acme.com", "acme").await;
    let ws = frank.get("/api/me").await.body["workspace"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(
        frank.post("/api/billing/portal", json!({})).await.body["error"],
        "noSubscription"
    );

    let r = frank.post("/api/billing/checkout", json!({})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["url"], "https://checkout.stripe.test/c/pay/cs_1");
    let calls = app.mock.calls("/v1/checkout/sessions");
    let form = calls[0].body.as_str().unwrap();
    for field in [
        "mode=subscription".to_string(),
        "line_items%5B0%5D%5Bprice%5D=price_seat".into(),
        "line_items%5B0%5D%5Bquantity%5D=1".into(),
        format!("client_reference_id={ws}"),
        format!("subscription_data%5Bmetadata%5D%5Bworkspace_id%5D={ws}"),
        "customer_email=frank%40acme.com".into(),
    ] {
        assert!(form.contains(&field), "{field} missing from {form}");
    }

    // Unsigned or wrongly signed: refused, nothing applied.
    let completed = json!({
        "id": "evt_1", "type": "checkout.session.completed",
        "data": { "object": { "client_reference_id": ws, "customer": "cus_1", "subscription": "sub_1", "payment_status": "paid" } }
    });
    assert_eq!(deliver(&app, &completed, Some("t=1,v1=00")).await, 400);
    assert_eq!(deliver(&app, &completed, Some("")).await, 400);
    assert_eq!(
        frank.get("/api/me").await.body["workspace"]["billing"]["status"],
        "trialing"
    );

    // Paid — the webhook, not the browser, activates.
    assert_eq!(deliver(&app, &completed, None).await, 200);
    assert_eq!(
        frank.get("/api/me").await.body["workspace"]["billing"]["status"],
        "active"
    );
    assert_eq!(
        frank.post("/api/billing/checkout", json!({})).await.body["error"],
        "alreadySubscribed"
    );
    let r = frank.post("/api/billing/portal", json!({})).await;
    assert_eq!(r.body["url"], "https://billing.stripe.test/p/session/1");
    assert!(
        app.mock.calls("/v1/billing_portal/sessions")[0]
            .body
            .as_str()
            .unwrap()
            .contains("customer=cus_1")
    );

    // past_due keeps it unlocked.
    let sub = |id: &str, kind: &str, status: &str| {
        json!({ "id": id, "type": kind, "data": { "object": {
            "id": "sub_1", "customer": "cus_1", "status": status, "metadata": { "workspace_id": ws } } } })
    };
    assert_eq!(
        deliver(
            &app,
            &sub("evt_2", "customer.subscription.updated", "past_due"),
            None
        )
        .await,
        200
    );
    let billing = frank.get("/api/me").await.body["workspace"]["billing"].clone();
    assert_eq!(
        (billing["status"].as_str(), billing["locked"].as_bool()),
        (Some("pastDue"), Some(false))
    );

    // Canceled locks; a replay of the old completion is not applied again.
    assert_eq!(
        deliver(
            &app,
            &sub("evt_3", "customer.subscription.deleted", "canceled"),
            None
        )
        .await,
        200
    );
    assert_eq!(deliver(&app, &completed, None).await, 200);
    let billing = frank.get("/api/me").await.body["workspace"]["billing"].clone();
    assert_eq!(
        (billing["status"].as_str(), billing["locked"].as_bool()),
        (Some("canceled"), Some(true))
    );
    assert_eq!(frank.get("/api/agents").await.status, 402);

    // Unknown types and workspaces are acknowledged and ignored.
    assert_eq!(
        deliver(
            &app,
            &json!({ "id": "evt_4", "type": "invoice.paid", "data": { "object": {} } }),
            None
        )
        .await,
        200
    );
    let stray = json!({ "id": "evt_5", "type": "checkout.session.completed",
        "data": { "object": { "client_reference_id": uuid::Uuid::new_v4(), "payment_status": "paid" } } });
    assert_eq!(deliver(&app, &stray, None).await, 200);

    // Resubscribing reuses the Stripe customer.
    assert_eq!(
        frank.post("/api/billing/checkout", json!({})).await.status,
        200
    );
    assert!(
        app.mock.calls("/v1/checkout/sessions")[1]
            .body
            .as_str()
            .unwrap()
            .contains("customer=cus_1")
    );
}

#[tokio::test]
async fn seats_follow_the_team() {
    let Some(app) = common::spawn().await else {
        return;
    };
    stripe_mock(&app);
    let mut frank = trial_owner(&app, "frank@acme.com", "acme").await;
    let ws = frank.get("/api/me").await.body["workspace"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Trial: no seat sync.
    assert_eq!(
        frank.post("/api/billing/portal", json!({})).await.status,
        409
    );
    let completed = json!({
        "id": "evt_1", "type": "checkout.session.completed",
        "data": { "object": { "client_reference_id": ws, "customer": "cus_1", "subscription": "sub_1", "payment_status": "paid" } }
    });
    assert_eq!(deliver(&app, &completed, None).await, 200);
    app.run_jobs().await;

    assert_eq!(
        frank
            .post("/api/invites", json!({ "email": "vetle@acme.com" }))
            .await
            .status,
        201
    );
    // A pending invite is not a seat.
    app.run_jobs().await;
    let quantities = || -> Vec<String> {
        app.mock
            .calls("/v1/subscription_items/si_1")
            .iter()
            .map(|c| c.body.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        quantities(),
        vec!["quantity=1&proration_behavior=create_prorations"]
    );

    let (mut agent, _) = app.set_up_account("vetle@acme.com").await;
    assert_eq!(app.run_jobs().await, 1);
    assert_eq!(
        quantities().last().unwrap(),
        "quantity=2&proration_behavior=create_prorations"
    );

    let vetle = agent.get("/api/me").await.body["agent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        frank.delete(&format!("/api/agents/{vetle}")).await.status,
        204
    );
    assert_eq!(app.run_jobs().await, 1);
    assert_eq!(
        quantities().last().unwrap(),
        "quantity=1&proration_behavior=create_prorations"
    );
}
