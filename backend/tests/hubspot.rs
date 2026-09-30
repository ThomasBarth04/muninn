//! Spec 007: a HubSpot account mirrored into Muninn — connected, imported,
//! kept in step by webhooks and the hourly check, and read-only here.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use common::{Client, HUBSPOT_CLIENT_SECRET, TestApp};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use uuid::Uuid;

const PORTAL: i64 = 139574231;

fn iso(days_ago: i64) -> String {
    (Utc::now() - Duration::days(days_ago)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn ms_of(v: &Value) -> i64 {
    DateTime::parse_from_rfc3339(v.as_str().unwrap())
        .unwrap()
        .timestamp_millis()
}

/// HubSpot as far as Muninn can see it: one account's tickets, threads,
/// notes, contacts, owners and pipelines.
struct Fake {
    portal: i64,
    /// Uninstalled or revoked: every call is a 401, every refresh refused.
    revoked: bool,
    /// Every API call answers this status: an outage, a rate limit.
    down: Option<u16>,
    tickets: Vec<Value>,
    /// Merged-away ticket id → the ticket it went into.
    merged: HashMap<String, String>,
    /// Ticket id → its thread's id and messages.
    threads: HashMap<String, (String, Vec<Value>)>,
    notes: Vec<Value>,
    contacts: HashMap<String, Value>,
    /// Message id → the full text of a truncated message.
    originals: HashMap<String, String>,
    pipelines: Value,
    owners: Value,
}

impl Fake {
    fn new() -> Fake {
        let stages = |open: &[&str], closed: &str| {
            let mut s: Vec<Value> = open
                .iter()
                .map(|id| json!({ "id": id, "metadata": { "ticketState": "OPEN" } }))
                .collect();
            s.push(json!({ "id": closed, "metadata": { "ticketState": "CLOSED" } }));
            s
        };
        Fake {
            portal: PORTAL,
            revoked: false,
            down: None,
            tickets: vec![],
            merged: HashMap::new(),
            threads: HashMap::new(),
            notes: vec![],
            contacts: HashMap::new(),
            originals: HashMap::new(),
            pipelines: json!({ "results": [
                { "id": "0", "label": "Support Pipeline", "stages": stages(&["1", "2", "3"], "4") },
                { "id": "7", "label": "Onboarding", "stages": stages(&["70"], "71") },
            ]}),
            owners: json!({ "results": [
                { "id": "11", "userId": 400, "email": "Frank@acme.com", "firstName": "Frank", "lastName": "" },
                { "id": "12", "userId": 500, "email": "kari@acme.com", "firstName": "Kari", "lastName": "Holm" },
            ]}),
        }
    }

    /// A ticket in the default pipeline, last modified `days_ago`.
    fn ticket(&mut self, id: &str, subject: &str, stage: &str, days_ago: i64) -> &mut Value {
        self.tickets.push(json!({
            "id": id,
            "properties": {
                "subject": subject, "content": "", "hs_pipeline": "0", "hs_pipeline_stage": stage,
                "hs_ticket_priority": null, "hubspot_owner_id": null,
                "createdate": iso(days_ago + 1),
                "closed_date": if stage == "4" { json!(iso(days_ago)) } else { json!(null) },
                "hs_lastmodifieddate": iso(days_ago),
            },
            "associations": {},
        }));
        self.tickets.last_mut().unwrap()
    }

    fn get(&mut self, id: &str) -> &mut Value {
        self.tickets.iter_mut().find(|t| t["id"] == id).unwrap()
    }

    /// Change a ticket the way someone in HubSpot would, which touches it.
    fn edit(&mut self, id: &str, property: &str, value: Value) {
        let t = self.get(id);
        t["properties"][property] = value;
        t["properties"]["hs_lastmodifieddate"] = json!(iso(0));
    }

    fn thread(&mut self, ticket: &str, thread: &str, messages: Vec<Value>) {
        self.threads
            .insert(ticket.into(), (thread.into(), messages));
    }

    fn search(&self, body: &Value) -> Value {
        let filters = body["filterGroups"][0]["filters"].as_array().unwrap();
        let mut hits: Vec<&Value> = self
            .tickets
            .iter()
            .filter(|t| {
                filters.iter().all(|f| {
                    let prop = &t["properties"][f["propertyName"].as_str().unwrap()];
                    let value = || f["value"].as_str().unwrap().parse::<i64>().unwrap();
                    match f["operator"].as_str().unwrap() {
                        "IN" => f["values"].as_array().unwrap().contains(prop),
                        "GTE" => ms_of(prop) >= value(),
                        "LTE" => ms_of(prop) <= value(),
                        op => panic!("fake search has no {op}"),
                    }
                })
            })
            .collect();
        hits.sort_by_key(|t| std::cmp::Reverse(ms_of(&t["properties"]["hs_lastmodifieddate"])));
        let after: usize = body["after"].as_str().map_or(0, |a| a.parse().unwrap());
        let limit = body["limit"].as_u64().unwrap() as usize;
        let results: Vec<Value> = hits
            .iter()
            .skip(after)
            .take(limit)
            // The properties asked for, as HubSpot answers.
            .map(|t| {
                let wanted = body["properties"].as_array().unwrap();
                let props: serde_json::Map<String, Value> = wanted
                    .iter()
                    .map(|k| {
                        (
                            k.as_str().unwrap().to_string(),
                            t["properties"][k.as_str().unwrap()].clone(),
                        )
                    })
                    .collect();
                json!({ "id": t["id"], "properties": props })
            })
            .collect();
        let paging = (after + limit < hits.len())
            .then(|| json!({ "next": { "after": (after + limit).to_string() } }));
        json!({ "total": hits.len(), "results": results, "paging": paging })
    }

    fn answer(&self, method: &str, path: &str, body: &Value) -> Option<(u16, Value)> {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let param = |k: &str| {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
                .map(str::to_string)
        };
        match route {
            "/oauth/2026-03/token" => {
                let form = body.as_str().unwrap_or("");
                return Some(
                    if form.contains("grant_type=authorization_code") || !self.revoked {
                        (
                            200,
                            json!({ "access_token": "at", "refresh_token": "rt", "expires_in": 1800 }),
                        )
                    } else {
                        (
                            400,
                            json!({ "status": "BAD_REFRESH_TOKEN", "message": "missing or invalid refresh token" }),
                        )
                    },
                );
            }
            "/oauth/2026-03/token/introspect" => {
                return Some((
                    200,
                    json!({ "hub_id": self.portal, "hub_domain": "acme.com" }),
                ));
            }
            _ => {}
        }
        if !["/crm/", "/conversations/", "/appinstalls/"]
            .iter()
            .any(|p| route.starts_with(p))
        {
            return None; // Postmark, Jev
        }
        if self.revoked {
            return Some((401, json!({ "message": "expired token" })));
        }
        if let Some(status) = self.down {
            return Some((status, json!({ "message": "down" })));
        }
        let not_found = Some((404, json!({ "message": "not found" })));
        let segments: Vec<&str> = route.trim_start_matches('/').split('/').collect();
        match (method, segments.as_slice()) {
            ("GET", ["crm", "pipelines", _, "tickets"]) => Some((200, self.pipelines.clone())),
            ("GET", ["crm", "owners", _]) => Some((200, self.owners.clone())),
            ("POST", ["crm", "objects", _, "tickets", "search"]) => Some((200, self.search(body))),
            ("GET", ["crm", "objects", _, "tickets", id]) => {
                let id = self.merged.get(*id).map_or(*id, String::as_str);
                let t = self.tickets.iter().find(|t| t["id"] == id);
                t.map(|t| (200, t.clone())).or(not_found)
            }
            ("GET", ["crm", "objects", _, "contacts", id]) => {
                self.contacts.get(*id).map(|c| (200, c.clone())).or(not_found)
            }
            ("POST", ["crm", "objects", _, "notes", "batch", "read"]) => {
                let wanted: Vec<&Value> = body["inputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| &i["id"])
                    .collect();
                let results: Vec<&Value> = self
                    .notes
                    .iter()
                    .filter(|n| wanted.contains(&&n["id"]))
                    .collect();
                Some((200, json!({ "results": results })))
            }
            ("GET", ["conversations", "conversations", _, "threads"]) => {
                let ticket = param("associatedTicketId").unwrap();
                let results: Vec<Value> = self
                    .threads
                    .get(&ticket)
                    .map(|(t, _)| json!({ "id": t }))
                    .into_iter()
                    .collect();
                Some((200, json!({ "results": results })))
            }
            ("GET", ["conversations", "conversations", _, "threads", thread]) => self
                .threads
                .iter()
                .find(|(_, (t, _))| t == thread)
                .map(|(ticket, _)| {
                    (200, json!({ "id": thread, "threadAssociations": { "associatedTicketId": ticket } }))
                })
                .or(not_found),
            ("GET", ["conversations", "conversations", _, "threads", thread, "messages"]) => self
                .threads
                .values()
                .find(|(t, _)| t == thread)
                .map(|(_, m)| (200, json!({ "results": m })))
                .or(not_found),
            ("GET", [.., "messages", id, "original-content"]) => self
                .originals
                .get(*id)
                .map(|t| (200, json!({ "text": t })))
                .or(not_found),
            ("DELETE", ["appinstalls", "v3", "external-install"]) => Some((204, json!({}))),
            _ => Some((500, json!({ "message": format!("the fake has no {method} {route}") }))),
        }
    }
}

fn message(id: &str, kind: &str, sender: Value, text: &str, days_ago: i64) -> Value {
    let (kind, direction) = match kind {
        "customer" => ("MESSAGE", "INCOMING"),
        "agent" => ("MESSAGE", "OUTGOING"),
        other => (other, "OUTGOING"),
    };
    json!({
        "id": id, "type": kind, "direction": direction, "createdAt": iso(days_ago),
        "senders": [sender], "text": text, "richText": format!("<p>{text}</p>"),
        "truncationStatus": "NOT_TRUNCATED",
    })
}

fn customer(name: &str, email: &str) -> Value {
    json!({ "actorId": "V-1", "name": name, "deliveryIdentifier": { "type": "HS_EMAIL_ADDRESS", "value": email } })
}

/// A HubSpot user; the mail itself went out from the shared inbox.
fn agent(user_id: u32, name: &str) -> Value {
    json!({ "actorId": format!("A-{user_id}"), "name": name,
            "deliveryIdentifier": { "type": "HS_EMAIL_ADDRESS", "value": "support@acme.com" } })
}

/// Acme's owner logged in, with a HubSpot account at the fake.
struct Hub {
    app: TestApp,
    fake: Arc<Mutex<Fake>>,
    frank: Client,
    ws: Uuid,
}

async fn hub() -> Option<Hub> {
    let app = common::spawn().await?;
    let fake = Arc::new(Mutex::new(Fake::new()));
    let f = fake.clone();
    app.mock
        .respond(move |method, path, body| f.lock().unwrap().answer(method, path, body));
    let (frank, _) = app.owner("acme", "frank@acme.com").await;
    let ws = workspace(&app, "acme").await;
    Some(Hub {
        app,
        fake,
        frank,
        ws,
    })
}

async fn workspace(app: &TestApp, slug: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM workspaces WHERE slug = $1")
        .bind(slug)
        .fetch_one(&app.owner)
        .await
        .unwrap()
}

/// Connect HubSpot the way the browser does: consent URL, HubSpot's
/// redirect back, and where that lands.
async fn connect(c: &mut Client, query: &str) -> String {
    let r = c.post("/api/integrations/hubspot/connect", json!({})).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let url = r.body["url"].as_str().unwrap();
    let state = url.split("state=").nth(1).unwrap();
    let back = c
        .get(&format!(
            "/api/integrations/hubspot/callback?{query}&state={state}"
        ))
        .await;
    assert_eq!(back.status, 303);
    back.headers["location"].to_str().unwrap().to_string()
}

impl Hub {
    fn fake(&self) -> MutexGuard<'_, Fake> {
        self.fake.lock().unwrap()
    }

    /// Connected, pipelines saved, the import run.
    async fn imported(&mut self, pipelines: &[&str]) -> Value {
        assert_eq!(
            connect(&mut self.frank, "code=abc").await,
            "/settings/hubspot"
        );
        let r = self
            .frank
            .put(
                "/api/integrations/hubspot/pipelines",
                json!({ "pipelineIds": pipelines }),
            )
            .await;
        assert_eq!(r.status, 200, "{:?}", r.body);
        self.app.run_jobs().await;
        self.status().await
    }

    async fn status(&mut self) -> Value {
        let r = self.frank.get("/api/integrations/hubspot").await;
        assert_eq!(r.status, 200, "{:?}", r.body);
        r.body["connection"].clone()
    }

    /// The Muninn ticket for HubSpot ticket `id`.
    async fn ticket(&self, id: &str) -> Option<Uuid> {
        sqlx::query_scalar("SELECT id FROM tickets WHERE workspace_id = $1 AND hubspot_id = $2")
            .bind(self.ws)
            .bind(format!("{PORTAL}/{id}"))
            .fetch_optional(&self.app.owner)
            .await
            .unwrap()
    }

    async fn detail(&mut self, id: &str) -> Value {
        let t = self.ticket(id).await.expect("in Muninn");
        let r = self.frank.get(&format!("/api/tickets/{t}")).await;
        assert_eq!(r.status, 200, "{:?}", r.body);
        r.body
    }

    async fn brain(&mut self, id: &str) -> i64 {
        let t = self.ticket(id).await.expect("in Muninn");
        let r = self
            .frank
            .get(&format!("/api/tickets/{t}/suggestions"))
            .await;
        r.body["brainSize"].as_i64().unwrap()
    }

    /// HubSpot calling the webhook, signed as HubSpot signs it.
    async fn webhook(&self, events: Value) -> u16 {
        let body = events.to_string();
        let ts = Utc::now().timestamp_millis().to_string();
        post_webhook(&self.app, &body, &ts, &sign(&self.app, &body, &ts)).await
    }

    async fn jobs(&self, kind: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE workspace_id = $1 AND kind = $2")
            .bind(self.ws)
            .bind(kind)
            .fetch_one(&self.app.owner)
            .await
            .unwrap()
    }

    /// The subjects Jev was asked about.
    fn jev_subjects(&self) -> Vec<String> {
        let mut s: Vec<String> = self
            .app
            .mock
            .calls("/v1/systemone")
            .iter()
            .filter_map(|c| c.body["state"]["subject"].as_str().map(str::to_string))
            .collect();
        s.sort();
        s.dedup();
        s
    }
}

fn sign(app: &TestApp, body: &str, ts: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(HUBSPOT_CLIENT_SECRET.as_bytes()).unwrap();
    mac.update(format!("POST{}/hooks/hubspot{body}{ts}", app.url).as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

async fn post_webhook(app: &TestApp, body: &str, ts: &str, signature: &str) -> u16 {
    reqwest::Client::new()
        .post(format!("{}/hooks/hubspot", app.url))
        .header("content-type", "application/json")
        .header("x-hubspot-request-timestamp", ts)
        .header("x-hubspot-signature-v3", signature)
        .body(body.to_string())
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn event(kind: &str, object: &str) -> Value {
    json!({ "portalId": PORTAL, "subscriptionType": kind, "objectId": object.parse::<i64>().unwrap(),
            "eventId": 1, "occurredAt": Utc::now().timestamp_millis(), "attemptNumber": 0 })
}

/// Two tickets that share a problem: one open today, one solved last week.
fn two_tickets(f: &mut Fake) {
    f.ticket("1", "SSO login fails", "3", 1);
    f.thread(
        "1",
        "901",
        vec![message(
            "m1",
            "customer",
            customer("Ola Nordmann", "ola@kunde.no"),
            "SSO login fails since this morning.",
            1,
        )],
    );
    f.ticket("2", "SSO login fails after cert rotation", "4", 7);
    f.thread(
        "2",
        "902",
        vec![
            message(
                "m2",
                "customer",
                customer("Per", "per@kunde.no"),
                "SSO login fails for everyone.",
                8,
            ),
            message(
                "m3",
                "agent",
                agent(500, "Kari Holm"),
                "Re-upload the IdP certificate.",
                7,
            ),
        ],
    );
}

#[tokio::test]
async fn connecting_goes_through_hubspots_consent_page_and_back() {
    let Some(mut h) = hub().await else { return };

    // §1: without the app's credentials there is no HubSpot at all.
    let mut cfg = (*h.app.st.cfg).clone();
    cfg.hubspot_client_id = None;
    let st = muninn::AppState::new(h.app.st.db.clone(), cfg);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut off = h.app.client();
    off.base = format!("http://{}", listener.local_addr().unwrap());
    off.cookie = h.frank.cookie.clone();
    tokio::spawn(async move { axum::serve(listener, muninn::router(st)).await.unwrap() });
    assert_eq!(off.get("/api/integrations/hubspot").await.status, 404);
    assert_eq!(
        off.post("/api/integrations/hubspot/connect", json!({}))
            .await
            .status,
        404
    );

    assert_eq!(h.status().await, Value::Null);

    // §2: only the owner connects; others see the status.
    let r = h
        .frank
        .post("/api/invites", json!({ "email": "vetle@acme.com" }))
        .await;
    assert_eq!(r.status, 201, "{:?}", r.body);
    let (mut vetle, _) = h.app.set_up_account("vetle@acme.com").await;
    let r = vetle
        .post("/api/integrations/hubspot/connect", json!({}))
        .await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (403, Some("ownerOnly"))
    );

    let r = h
        .frank
        .post("/api/integrations/hubspot/connect", json!({}))
        .await;
    let url = r.body["url"].as_str().unwrap();
    assert!(
        url.starts_with(
            "https://app.hubspot.test/oauth/authorize?client_id=hs-client&redirect_uri="
        ),
        "{url}"
    );
    assert!(url.contains(&format!(
        "redirect_uri={}%2Fapi%2Fintegrations%2Fhubspot%2Fcallback",
        h.app.url.replace(':', "%3A").replace('/', "%2F")
    )));
    assert!(
        url.contains("scope=tickets%20crm.objects.tickets.read%20conversations.read"),
        "{url}"
    );
    assert!(!url.contains("write"), "read-only scopes: {url}");

    // §3: the state is this owner's, for 10 minutes, once.
    let back = |status: u16, location: &str| (status, location.to_string());
    let r = h
        .frank
        .get("/api/integrations/hubspot/callback?code=abc&state=forged")
        .await;
    assert_eq!(
        back(r.status, r.headers["location"].to_str().unwrap()),
        back(303, "/settings/hubspot?error=expired")
    );
    let mut stranger = h.app.client();
    let r = stranger
        .get("/api/integrations/hubspot/callback?code=abc&state=x")
        .await;
    assert_eq!(
        back(r.status, r.headers["location"].to_str().unwrap()),
        back(303, "/login")
    );
    assert_eq!(
        connect(&mut h.frank, "error=access_denied").await,
        "/settings/hubspot?error=denied"
    );
    let r = h
        .frank
        .post("/api/integrations/hubspot/connect", json!({}))
        .await;
    let state = r.body["url"]
        .as_str()
        .unwrap()
        .split("state=")
        .nth(1)
        .unwrap()
        .to_string();
    sqlx::query("UPDATE hubspot_states SET expires_at = now() - interval '1 second'")
        .execute(&h.app.owner)
        .await
        .unwrap();
    let r = h
        .frank
        .get(&format!(
            "/api/integrations/hubspot/callback?code=abc&state={state}"
        ))
        .await;
    assert_eq!(
        r.headers["location"].to_str().unwrap(),
        "/settings/hubspot?error=expired"
    );

    assert_eq!(connect(&mut h.frank, "code=abc").await, "/settings/hubspot");
    let c = h.status().await;
    assert_eq!(c["accountId"], PORTAL);
    assert_eq!(c["accountName"], "acme.com");
    // §6: the default pipeline is ticked, and nothing is imported until saved.
    assert_eq!(c["status"], "pickPipelines");
    assert_eq!(
        c["pipelines"],
        json!([
            { "id": "0", "label": "Support Pipeline", "selected": true },
            { "id": "7", "label": "Onboarding", "selected": false },
        ])
    );
    assert_eq!(
        (c["import"].clone(), c["tickets"].clone()),
        (Value::Null, json!(0))
    );
    assert!(h.app.mock.calls("/crm/objects/2026-09/tickets").is_empty());
    let r = vetle.get("/api/integrations/hubspot").await;
    assert_eq!(r.body["connection"]["status"], "pickPipelines");

    let r = h
        .frank
        .post("/api/integrations/hubspot/connect", json!({}))
        .await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (409, Some("alreadyConnected"))
    );

    // §4: the same HubSpot account cannot join a second workspace, and its
    // tokens are dropped without revoking the first workspace's install.
    let (mut ola, _) = h.app.owner("globex", "ola@globex.no").await;
    assert_eq!(
        connect(&mut ola, "code=abc").await,
        "/settings/hubspot?error=portalTaken"
    );
    assert_eq!(
        ola.get("/api/integrations/hubspot").await.body["connection"],
        Value::Null
    );
    assert!(h.app.mock.calls("/oauth/2026-03/token/revoke").is_empty());
    assert!(h.app.mock.calls("/appinstalls").is_empty());
}

#[tokio::test]
async fn the_import_brings_in_a_year_of_tickets_and_fills_the_brain() {
    let Some(mut h) = hub().await else { return };
    {
        let mut f = h.fake();
        two_tickets(&mut f);
        let t = f.get("1");
        t["properties"]["hs_ticket_priority"] = json!("HIGH");
        t["properties"]["hubspot_owner_id"] = json!("11");
        // The customer's reply quotes the thread; an assignment is not a message.
        let (_, messages) = f.threads.get_mut("1").unwrap();
        messages[0]["text"] =
            json!("SSO login fails since this morning.\n\nOn Mon, 3 Mar 2025, Kari wrote:\n> Hi");
        messages
            .push(json!({ "id": "a1", "type": "ASSIGNMENT", "createdAt": iso(1), "senders": [] }));
        // A comment, a truncated message and a note on the solved one.
        let (_, messages) = f.threads.get_mut("2").unwrap();
        messages.push(message(
            "m4",
            "COMMENT",
            agent(500, "Kari Holm"),
            "Their IdP cert expired.",
            6,
        ));
        let mut long = message(
            "m5",
            "customer",
            customer("Per", "per@kunde.no"),
            "Thanks, that",
            5,
        );
        long["truncationStatus"] = json!("TRUNCATED_TO_MOST_RECENT_REPLY");
        messages.push(long);
        f.originals
            .insert("m5".into(), "Thanks, that fixed it for all of us.".into());
        f.get("2")["associations"] = json!({ "notes": { "results": [{ "id": "n1" }] } });
        f.notes.push(json!({ "id": "n1", "properties": {
            "hs_note_body": "<p>Fixed by <b>re-uploading</b> the cert.</p>",
            "hs_timestamp": iso(4), "hubspot_owner_id": "12" } }));
        // Created by hand: no thread, its description is the message; the
        // customer comes from its contact.
        let t = f.ticket("3", "Printer on fire", "4", 30);
        t["properties"]["content"] = json!("The office printer is on fire.");
        t["associations"] = json!({ "contacts": { "results": [{ "id": "c1" }] } });
        f.contacts.insert(
            "c1".into(),
            json!({ "id": "c1", "properties": {
            "email": "Lise@Kunde.no", "firstname": "Lise", "lastname": "Berg" } }),
        );
        // §16: an anonymous chat.
        f.ticket("4", "Hello?", "4", 2);
        f.thread("4", "904", vec![message("m6", "customer",
            json!({ "actorId": "V-9", "name": "Visitor", "deliveryIdentifier": { "type": "HS_VISITOR_ID", "value": "v9" } }),
            "hello", 2)]);
        // Not ticked, and modified more than 12 months ago.
        f.ticket("5", "Onboarding call", "70", 3)["properties"]["hs_pipeline"] = json!("7");
        f.ticket("6", "Ancient history", "4", 400);
    }

    let r = h
        .frank
        .put(
            "/api/integrations/hubspot/pipelines",
            json!({ "pipelineIds": ["0"] }),
        )
        .await;
    assert_eq!(r.status, 404, "not connected yet");
    assert_eq!(connect(&mut h.frank, "code=abc").await, "/settings/hubspot");
    let r = h
        .frank
        .put(
            "/api/integrations/hubspot/pipelines",
            json!({ "pipelineIds": ["0"] }),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    // §9: the import is under way.
    assert_eq!(r.body["status"], "importing");
    assert_eq!(r.body["import"], json!({ "done": 0, "total": 0 }));

    h.app.run_jobs().await;
    let c = h.status().await;
    assert_eq!(c["status"], "synced");
    assert_eq!(
        (c["tickets"].clone(), c["skipped"].clone()),
        (json!(3), json!(1))
    );
    assert_eq!(c["import"], Value::Null);
    assert!(c["lastSyncedAt"].is_string() && c["lastError"].is_null());

    // §7: the ticked pipelines, the last 12 months; §8: newest first.
    let search = &h.app.mock.calls("/crm/objects/2026-09/tickets/search")[0].body;
    let filters = &search["filterGroups"][0]["filters"];
    assert_eq!(
        filters[0],
        json!({ "propertyName": "hs_pipeline", "operator": "IN", "values": ["0"] })
    );
    let since: i64 = filters[1]["value"].as_str().unwrap().parse().unwrap();
    let year = (Utc::now() - Duration::days(365)).timestamp_millis();
    assert!((since - year).abs() < 60_000, "{since} vs {year}");
    let read: Vec<String> = h
        .app
        .mock
        .calls("/crm/objects/2026-09/tickets/")
        .iter()
        .filter(|c| c.method == "GET")
        .map(|c| {
            c.path
                .split('/')
                .nth(5)
                .unwrap()
                .split('?')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(read, vec!["1", "4", "2", "3"]);
    assert!(
        h.ticket("4").await.is_none()
            && h.ticket("5").await.is_none()
            && h.ticket("6").await.is_none()
    );

    // §12, §24: the open one, with its link, mapped fields and owner.
    let open = h.frank.get("/api/tickets?view=open").await.body["tickets"].clone();
    assert_eq!(open.as_array().unwrap().len(), 1);
    let t = &open[0];
    assert_eq!(t["subject"], "SSO login fails");
    assert_eq!(
        t["hubspot"]["url"],
        format!("https://app.hubspot.test/contacts/{PORTAL}/record/0-5/1")
    );
    assert_eq!(
        (t["status"].as_str(), t["priority"].as_str()),
        (Some("waitingOnUs"), Some("high"))
    );
    assert_eq!(t["owner"]["name"], "frank");
    assert_eq!(
        t["contact"],
        json!({ "id": t["contact"]["id"], "email": "ola@kunde.no", "name": "Ola Nordmann" })
    );

    // §13–15: the thread, text only, quoted history cut.
    let d = h.detail("1").await;
    let texts: Vec<&str> = d["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, vec!["SSO login fails since this morning."]);
    let d = h.detail("2").await;
    let thread: Vec<(String, String, String)> = d["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["kind"].as_str().unwrap().into(),
                m["author"]["email"].as_str().unwrap().into(),
                m["text"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        thread,
        vec![
            (
                "customer".into(),
                "per@kunde.no".into(),
                "SSO login fails for everyone.".into()
            ),
            (
                "agent".into(),
                "kari@acme.com".into(),
                "Re-upload the IdP certificate.".into()
            ),
            (
                "comment".into(),
                "kari@acme.com".into(),
                "Their IdP cert expired.".into()
            ),
            (
                "customer".into(),
                "per@kunde.no".into(),
                "Thanks, that fixed it for all of us.".into()
            ),
            (
                "comment".into(),
                "kari@acme.com".into(),
                "Fixed by re-uploading the cert.".into()
            ),
        ]
    );
    assert_eq!(d["messages"][1]["author"]["name"], "Kari Holm");
    assert_eq!(
        d["messages"][1]["delivery"],
        json!({ "status": "sent", "error": null })
    );
    assert_eq!(d["status"], "closed");
    let d = h.detail("3").await;
    assert_eq!(d["contact"]["email"], "lise@kunde.no");
    assert_eq!(d["contact"]["name"], "Lise Berg");
    assert_eq!(d["messages"][0]["text"], "The office printer is on fire.");
    let html: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE html_body IS NOT NULL")
        .fetch_one(&h.app.owner)
        .await
        .unwrap();
    assert_eq!(html, 0, "HubSpot's richText is never stored");
    let sent: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE sent_at IS NOT NULL")
        .fetch_one(&h.app.owner)
        .await
        .unwrap();
    assert_eq!(
        sent, 0,
        "HubSpot's replies are not ours: the trial send cap never counts them"
    );
    assert!(
        h.app
            .mock
            .emails()
            .iter()
            .all(|e| e["To"] == "frank@acme.com"),
        "nothing goes to a customer, only Frank's setup link"
    );

    // §17: the closed ones are the brain.
    assert_eq!(h.brain("1").await, 2);

    // §11: Jev only for the open one, after the import — so it found the case
    // the import brought in after it.
    assert_eq!(h.jev_subjects(), vec!["SSO login fails"]);
    let cases: Vec<Value> = h
        .app
        .mock
        .calls("/v1/systemone")
        .into_iter()
        .filter(|c| !c.body["questions"]["case_0"].is_null())
        .map(|c| c.body)
        .collect();
    assert_eq!(cases.len(), 1);
    let past = cases[0]["questions"]["case_0"]["instructions"]["past_case"]
        .as_str()
        .unwrap();
    assert!(
        past.starts_with("SSO login fails after cert rotation"),
        "{past}"
    );
}

#[tokio::test]
async fn webhooks_keep_the_copy_in_step() {
    let Some(mut h) = hub().await else { return };
    {
        let mut f = h.fake();
        two_tickets(&mut f);
        f.ticket("3", "Printer on fire", "4", 30)["properties"]["content"] = json!("On fire.");
        f.get("3")["associations"] = json!({ "contacts": { "results": [{ "id": "c1" }] } });
        f.contacts.insert(
            "c1".into(),
            json!({ "properties": { "email": "lise@kunde.no" } }),
        );
    }
    h.imported(&["0"]).await;
    assert_eq!(h.brain("1").await, 2);

    // Only HubSpot's signature, and only a fresh one, gets in.
    let body = json!([event("ticket.propertyChange", "1")]).to_string();
    let ts = Utc::now().timestamp_millis().to_string();
    assert_eq!(post_webhook(&h.app, &body, &ts, "bm9wZQ==").await, 401);
    let old = (Utc::now().timestamp_millis() - 600_000).to_string();
    assert_eq!(
        post_webhook(&h.app, &body, &old, &sign(&h.app, &body, &old)).await,
        401
    );
    // An account no workspace has is acknowledged and ignored.
    let mut stray = event("ticket.creation", "1");
    stray["portalId"] = json!(42);
    assert_eq!(h.webhook(json!([stray])).await, 200);
    assert_eq!(h.jobs("hubspotTicket").await, 0);

    // §18, §20: closed in HubSpot → closed here and in the brain. Duplicates
    // queue one re-read.
    h.fake().edit("1", "hs_pipeline_stage", json!("4"));
    let e = event("ticket.propertyChange", "1");
    assert_eq!(h.webhook(json!([e, e])).await, 200);
    assert_eq!(h.webhook(json!([e])).await, 200);
    assert_eq!(h.jobs("hubspotTicket").await, 1);
    h.app.run_jobs().await;
    assert_eq!(h.detail("1").await["status"], "closed");
    assert_eq!(h.brain("1").await, 3);

    // A reply on the thread reopens the solved one: out of the brain.
    {
        let mut f = h.fake();
        f.edit("2", "hs_pipeline_stage", json!("1"));
        let (_, messages) = f.threads.get_mut("2").unwrap();
        messages.push(message(
            "m9",
            "customer",
            customer("Per", "per@kunde.no"),
            "It broke again.",
            0,
        ));
    }
    assert_eq!(
        h.webhook(json!([event("conversation.newMessage", "902")]))
            .await,
        200
    );
    h.app.run_jobs().await;
    let d = h.detail("2").await;
    assert_eq!(d["status"], "new");
    assert_eq!(
        d["messages"].as_array().unwrap().last().unwrap()["text"],
        "It broke again."
    );
    assert_eq!(h.brain("1").await, 2);

    // §21: merged away and deleted in HubSpot → gone here, with the
    // suggestions that pointed at them.
    let (one, three) = (h.ticket("1").await.unwrap(), h.ticket("3").await.unwrap());
    sqlx::query("INSERT INTO suggestions (workspace_id, ticket_id, case_ticket_id, score, rank) VALUES ($1, $2, $3, 0.9, 1)")
        .bind(h.ws)
        .bind(h.ticket("2").await.unwrap())
        .bind(three)
        .execute(&h.app.owner)
        .await
        .unwrap();
    {
        let mut f = h.fake();
        f.tickets.retain(|t| t["id"] != "3");
        f.merged.insert("3".into(), "1".into());
    }
    let mut merge = event("ticket.merge", "1");
    merge["primaryObjectId"] = json!(1);
    merge["mergedObjectIds"] = json!([3]);
    assert_eq!(h.webhook(json!([merge])).await, 200);
    h.app.run_jobs().await;
    assert_eq!(
        (h.ticket("1").await, h.ticket("3").await),
        (Some(one), None)
    );
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM suggestions")
        .fetch_one(&h.app.owner)
        .await
        .unwrap();
    assert_eq!(left, 0);

    h.fake().tickets.retain(|t| t["id"] != "2");
    assert_eq!(h.webhook(json!([event("ticket.deletion", "2")])).await, 200);
    h.app.run_jobs().await;
    assert!(h.ticket("2").await.is_none());

    // Moved to an unticked pipeline → deleted; created → imported, and
    // since the import is over, Jev is asked at once.
    h.fake().edit("1", "hs_pipeline", json!("7"));
    h.fake().ticket("8", "VPN keeps dropping", "1", 0);
    h.fake().thread(
        "8",
        "908",
        vec![message(
            "m8",
            "customer",
            customer("Ola", "ola@kunde.no"),
            "VPN drops.",
            0,
        )],
    );
    let events = json!([
        event("ticket.propertyChange", "1"),
        event("ticket.creation", "8")
    ]);
    assert_eq!(h.webhook(events).await, 200);
    h.app.run_jobs().await;
    assert!(h.ticket("1").await.is_none());
    assert_eq!(h.detail("8").await["status"], "new");
    assert!(h.jev_subjects().contains(&"VPN keeps dropping".to_string()));
}

#[tokio::test]
async fn the_hourly_check_catches_what_a_webhook_missed() {
    let Some(mut h) = hub().await else { return };
    two_tickets(&mut h.fake());
    h.imported(&["0"]).await;
    assert_eq!(h.jobs("hubspotCheck").await, 1);

    // Changed in HubSpot, and the webhooks never came: one edited, one moved
    // out of the ticked pipelines.
    h.fake()
        .edit("1", "subject", json!("SSO login fails on mobile"));
    h.fake().edit("2", "hs_pipeline", json!("7"));
    let checked: DateTime<Utc> = sqlx::query_scalar("SELECT checked_at FROM hubspot_connections")
        .fetch_one(&h.app.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'hubspotCheck'")
        .execute(&h.app.owner)
        .await
        .unwrap();
    let before = h
        .app
        .mock
        .calls("/crm/objects/2026-09/tickets/search")
        .len();
    h.app.run_jobs().await;
    assert_eq!(h.detail("1").await["subject"], "SSO login fails on mobile");
    assert!(h.ticket("2").await.is_none());

    // It looked back to an hour before the last check, in every pipeline,
    // and the next one is queued.
    let searches = h.app.mock.calls("/crm/objects/2026-09/tickets/search");
    let filters = &searches[before].body["filterGroups"][0]["filters"];
    assert_eq!(filters.as_array().unwrap().len(), 1, "{filters}");
    let since: i64 = filters[0]["value"].as_str().unwrap().parse().unwrap();
    assert_eq!(since, (checked - Duration::hours(1)).timestamp_millis());
    let (next, due): (i64, bool) = sqlx::query_as(
        "SELECT count(*), bool_and(run_at > now() + interval '59 minutes') FROM jobs WHERE kind = 'hubspotCheck'",
    )
    .fetch_one(&h.app.owner)
    .await
    .unwrap();
    assert_eq!((next, due), (1, true));
    let later: DateTime<Utc> = sqlx::query_scalar("SELECT checked_at FROM hubspot_connections")
        .fetch_one(&h.app.owner)
        .await
        .unwrap();
    assert!(later > checked);
}

#[tokio::test]
async fn pipelines_ticked_later_are_imported_and_unticked_ones_leave() {
    let Some(mut h) = hub().await else { return };
    {
        let mut f = h.fake();
        two_tickets(&mut f);
        let t = f.ticket("5", "Onboarding call", "71", 3);
        t["properties"]["hs_pipeline"] = json!("7");
        t["properties"]["content"] = json!("Book the kickoff.");
        t["associations"] = json!({ "contacts": { "results": [{ "id": "c1" }] } });
        f.contacts.insert(
            "c1".into(),
            json!({ "properties": { "email": "lise@kunde.no" } }),
        );
    }
    h.imported(&["0"]).await;
    assert!(h.ticket("5").await.is_none());

    // §22: ticked later — its last 12 months come in.
    let r = h
        .frank
        .put(
            "/api/integrations/hubspot/pipelines",
            json!({ "pipelineIds": ["0", "7"] }),
        )
        .await;
    assert_eq!(r.body["status"], "importing");
    h.app.run_jobs().await;
    assert_eq!(h.status().await["status"], "synced");
    assert!(h.ticket("5").await.is_some());
    let searches = h.app.mock.calls("/crm/objects/2026-09/tickets/search");
    assert_eq!(
        searches.last().unwrap().body["filterGroups"][0]["filters"][0]["values"],
        json!(["7"]),
        "only the new pipeline is imported"
    );

    // Unticked: its tickets leave, closed ones too.
    let r = h
        .frank
        .put(
            "/api/integrations/hubspot/pipelines",
            json!({ "pipelineIds": ["0"] }),
        )
        .await;
    assert_eq!(r.status, 200);
    assert!(h.ticket("5").await.is_none());
    assert!(h.ticket("2").await.is_some());

    let r = h
        .frank
        .put(
            "/api/integrations/hubspot/pipelines",
            json!({ "pipelineIds": [] }),
        )
        .await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (400, Some("noPipelines"))
    );
    let r = h
        .frank
        .put(
            "/api/integrations/hubspot/pipelines",
            json!({ "pipelineIds": ["99"] }),
        )
        .await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (400, Some("unknownPipeline"))
    );
    h.frank
        .post("/api/invites", json!({ "email": "vetle@acme.com" }))
        .await;
    let (mut vetle, _) = h.app.set_up_account("vetle@acme.com").await;
    let r = vetle
        .put(
            "/api/integrations/hubspot/pipelines",
            json!({ "pipelineIds": ["0"] }),
        )
        .await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (403, Some("ownerOnly"))
    );
}

#[tokio::test]
async fn hubspot_tickets_are_read_only_here_but_category_and_feedback_work() {
    let Some(mut h) = hub().await else { return };
    two_tickets(&mut h.fake());
    h.imported(&["0"]).await;
    let id = h.ticket("1").await.unwrap();

    // §25: answered and changed in HubSpot, not here.
    for (path, body) in [
        (
            format!("/api/tickets/{id}/replies"),
            json!({ "text": "Hi" }),
        ),
        (
            format!("/api/tickets/{id}/comments"),
            json!({ "text": "Note" }),
        ),
    ] {
        let r = h.frank.post(&path, body).await;
        assert_eq!(
            (r.status, r.body["error"].as_str()),
            (409, Some("syncedFromHubSpot")),
            "{path}"
        );
    }
    let category: Uuid =
        sqlx::query_scalar("SELECT id FROM categories WHERE workspace_id = $1 AND name = 'Bug'")
            .bind(h.ws)
            .fetch_one(&h.app.owner)
            .await
            .unwrap();
    for body in [
        json!({ "status": "closed" }),
        json!({ "ownerId": null }),
        json!({ "priority": "low" }),
        json!({ "categoryId": category, "status": "closed" }),
    ] {
        let r = h
            .frank
            .patch(&format!("/api/tickets/{id}"), body.clone())
            .await;
        assert_eq!(
            (r.status, r.body["error"].as_str()),
            (409, Some("syncedFromHubSpot")),
            "{body}"
        );
    }
    let d = h.detail("1").await;
    assert_eq!(
        (d["status"].as_str(), d["category"].clone()),
        (Some("waitingOnUs"), Value::Null)
    );

    // §26: the category and the copilot's feedback are Muninn's own.
    let r = h
        .frank
        .patch(
            &format!("/api/tickets/{id}"),
            json!({ "categoryId": category }),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["category"]["name"], "Bug");
    let suggestion: Uuid = sqlx::query_scalar(
        "INSERT INTO suggestions (workspace_id, ticket_id, case_ticket_id, score, rank) VALUES ($1, $2, $3, 0.9, 1) RETURNING id",
    )
    .bind(h.ws)
    .bind(id)
    .bind(h.ticket("2").await.unwrap())
    .fetch_one(&h.app.owner)
    .await
    .unwrap();
    let r = h
        .frank
        .post(
            &format!("/api/suggestions/{suggestion}/feedback"),
            json!({ "verdict": "helped" }),
        )
        .await;
    assert_eq!(r.status, 204);
    assert!(
        h.app
            .mock
            .calls("/crm/objects/2026-09/tickets/1")
            .iter()
            .all(|c| c.method == "GET")
    );

    // §23: a paused workspace keeps syncing.
    muninn::admin::run(&h.app.st, None, &["pause".into(), "acme".into()])
        .await
        .unwrap();
    h.fake()
        .edit("1", "subject", json!("SSO login fails on mobile"));
    assert_eq!(
        h.webhook(json!([event("ticket.propertyChange", "1")]))
            .await,
        200
    );
    h.app.run_jobs().await;
    let subject: String = sqlx::query_scalar("SELECT subject FROM tickets WHERE id = $1")
        .bind(id)
        .fetch_one(&h.app.owner)
        .await
        .unwrap();
    assert_eq!(subject, "SSO login fails on mobile");
}

#[tokio::test]
async fn hubspot_trouble_is_retried_and_a_revoked_app_says_so() {
    let Some(mut h) = hub().await else { return };
    two_tickets(&mut h.fake());
    h.imported(&["0"]).await;

    // §27–28: a rate limit or an outage is retried, and Settings says so.
    h.fake().down = Some(429);
    h.fake()
        .edit("1", "subject", json!("SSO login fails on mobile"));
    h.webhook(json!([event("ticket.propertyChange", "1")]))
        .await;
    h.app.run_jobs().await;
    let (attempts, waiting): (i32, bool) = sqlx::query_as(
        "SELECT attempts, run_at > now() FROM jobs WHERE kind = 'hubspotTicket' AND failed_at IS NULL",
    )
    .fetch_one(&h.app.owner)
    .await
    .unwrap();
    assert_eq!((attempts, waiting), (1, true));
    assert_eq!(h.status().await["lastError"], "HubSpot is not answering");
    h.fake().down = None;
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'hubspotTicket'")
        .execute(&h.app.owner)
        .await
        .unwrap();
    h.app.run_jobs().await;
    assert_eq!(h.detail("1").await["subject"], "SSO login fails on mobile");
    assert_eq!(h.status().await["lastError"], Value::Null);

    // §29: uninstalled in HubSpot — the refresh is refused, the tickets stay.
    h.fake().revoked = true;
    h.webhook(json!([event("ticket.propertyChange", "1")]))
        .await;
    h.app.run_jobs().await;
    let c = h.status().await;
    assert_eq!(c["status"], "revoked");
    assert_eq!(c["lastError"], "HubSpot disconnected Muninn");
    assert!(h.ticket("1").await.is_some() && h.ticket("2").await.is_some());
    let failed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE failed_at IS NOT NULL AND kind LIKE 'hubspot%'",
    )
    .fetch_one(&h.app.owner)
    .await
    .unwrap();
    assert_eq!(failed, 0, "revoked is a state, not a failing job");
    // Webhooks for a revoked connection queue nothing.
    h.webhook(json!([event("ticket.propertyChange", "2")]))
        .await;
    assert_eq!(h.jobs("hubspotTicket").await, 0);

    // A different account is refused until the owner disconnects.
    h.fake().revoked = false;
    h.fake().portal = 777;
    assert_eq!(
        connect(&mut h.frank, "code=abc").await,
        "/settings/hubspot?error=alreadyConnected"
    );
    // The same account reconnects and the hourly check catches up.
    h.fake().portal = PORTAL;
    assert_eq!(connect(&mut h.frank, "code=abc").await, "/settings/hubspot");
    assert_eq!(h.status().await["status"], "synced");
    h.fake()
        .edit("2", "subject", json!("Changed while revoked"));
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'hubspotCheck'")
        .execute(&h.app.owner)
        .await
        .unwrap();
    h.app.run_jobs().await;
    assert_eq!(h.detail("2").await["subject"], "Changed while revoked");
}

#[tokio::test]
async fn disconnecting_keeps_the_brain_and_drops_the_open_tickets() {
    let Some(mut h) = hub().await else { return };
    two_tickets(&mut h.fake());
    h.imported(&["0"]).await;
    let solved = h.ticket("2").await.unwrap();

    h.frank
        .post("/api/invites", json!({ "email": "vetle@acme.com" }))
        .await;
    let (mut vetle, _) = h.app.set_up_account("vetle@acme.com").await;
    let r = vetle.delete("/api/integrations/hubspot").await;
    assert_eq!(
        (r.status, r.body["error"].as_str()),
        (403, Some("ownerOnly"))
    );

    // §30: uninstalled from HubSpot; the closed ticket stays, the open one goes.
    assert_eq!(
        h.frank.delete("/api/integrations/hubspot").await.status,
        204
    );
    let uninstall = h.app.mock.calls("/appinstalls/v3/external-install");
    assert_eq!(uninstall.len(), 1);
    assert_eq!(uninstall[0].method, "DELETE");
    assert_eq!(h.status().await, Value::Null);
    assert!(h.ticket("1").await.is_none());
    assert_eq!(h.ticket("2").await, Some(solved));
    let d = h.detail("2").await;
    assert!(d["hubspot"]["url"].is_string(), "still marked");
    let r = h
        .frank
        .post(
            &format!("/api/tickets/{solved}/comments"),
            json!({ "text": "x" }),
        )
        .await;
    assert_eq!(r.status, 409, "still read-only");
    let brain = h
        .frank
        .get(&format!("/api/tickets/{solved}/suggestions"))
        .await;
    assert_eq!(brain.body["brainSize"], 1);
    assert_eq!(
        h.frank.delete("/api/integrations/hubspot").await.status,
        404
    );

    // Another account's first save unticks its default pipeline "0" — which
    // is not the kept ticket's pipeline, whatever its id says.
    h.fake().portal = 777;
    assert_eq!(connect(&mut h.frank, "code=abc").await, "/settings/hubspot");
    let r = h
        .frank
        .put(
            "/api/integrations/hubspot/pipelines",
            json!({ "pipelineIds": ["7"] }),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(h.ticket("2").await, Some(solved));
    assert_eq!(
        h.frank.delete("/api/integrations/hubspot").await.status,
        204
    );
    h.fake().portal = PORTAL;

    // Nothing of the old connections runs on: their checks end.
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'hubspotCheck'")
        .execute(&h.app.owner)
        .await
        .unwrap();
    h.app.run_jobs().await;
    assert_eq!(h.jobs("hubspotCheck").await, 0);

    // §31: connecting again imports again, without doubling what was kept.
    let c = h.imported(&["0"]).await;
    assert_eq!(c["tickets"], 2);
    assert_eq!(h.ticket("2").await, Some(solved));
}

#[tokio::test]
async fn an_agent_who_joins_later_owns_their_hubspot_tickets() {
    let Some(mut h) = hub().await else { return };
    two_tickets(&mut h.fake());
    h.fake().get("1")["properties"]["hubspot_owner_id"] = json!("12");
    h.imported(&["0"]).await;
    assert_eq!(h.detail("1").await["owner"], Value::Null);

    // §12: invited with the email Kari uses in HubSpot.
    h.frank
        .post("/api/invites", json!({ "email": "kari@acme.com" }))
        .await;
    h.app.set_up_account("kari@acme.com").await;
    assert_eq!(h.detail("1").await["owner"]["name"], "kari");
    // Her replies in the thread are hers now too, from the next change on.
    h.fake().edit(
        "2",
        "subject",
        json!("SSO login fails after cert rotation!"),
    );
    h.webhook(json!([event("ticket.propertyChange", "2")]))
        .await;
    h.app.run_jobs().await;
    let reply = &h.detail("2").await["messages"][1];
    assert_eq!(
        (
            reply["author"]["name"].as_str(),
            reply["author"]["email"].as_str()
        ),
        (Some("kari"), Some("kari@acme.com"))
    );
}

#[tokio::test]
async fn mail_never_threads_into_a_hubspot_ticket() {
    let Some(mut h) = hub().await else { return };
    two_tickets(&mut h.fake());
    h.imported(&["0"]).await;
    let solved = h.ticket("2").await.unwrap();

    // A mail that names the HubSpot thread's messages, however it got them.
    let hubspot_id = format!("hubspot:{PORTAL}/m3");
    let mail = json!({
        "OriginalRecipient": "acme@in.muninn.test",
        "ToFull": [{ "Email": "acme@in.muninn.test", "Name": "", "MailboxHash": "" }],
        "CcFull": [],
        "FromFull": { "Email": "per@kunde.no", "Name": "Per" },
        "Subject": "Re: SSO login fails after cert rotation",
        "MailboxHash": "",
        "TextBody": "Still broken.",
        "HtmlBody": "",
        "StrippedTextReply": "Still broken.",
        "Headers": [
            { "Name": "Message-ID", "Value": format!("<hubspot:{PORTAL}/m2>") },
            { "Name": "In-Reply-To", "Value": format!("<{hubspot_id}>") },
            { "Name": "References", "Value": format!("<{hubspot_id}>") },
        ],
        "Attachments": [],
    });
    let status = reqwest::Client::new()
        .post(format!("{}/hooks/postmark/inbound", h.app.url))
        .basic_auth("postmark", Some(common::INBOUND_PASSWORD))
        .json(&mail)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 200);

    // It is a ticket of its own, not a reopened read-only one.
    let d = h.detail("2").await;
    assert_eq!(d["status"], "closed");
    assert_eq!(d["messages"].as_array().unwrap().len(), 2);
    let own: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tickets t JOIN messages m ON m.ticket_id = t.id
         WHERE t.hubspot_id IS NULL AND m.text = 'Still broken.'",
    )
    .fetch_one(&h.app.owner)
    .await
    .unwrap();
    assert_eq!(own, 1);
    // And a re-read of the HubSpot ticket leaves it where it is.
    h.webhook(json!([event("ticket.propertyChange", "2")]))
        .await;
    h.app.run_jobs().await;
    assert_eq!(h.ticket("2").await, Some(solved));
    let still: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE text = 'Still broken.'")
            .fetch_one(&h.app.owner)
            .await
            .unwrap();
    assert_eq!(still, 1);
}

#[tokio::test]
async fn an_import_that_gave_up_is_picked_up_by_the_hourly_check() {
    let Some(mut h) = hub().await else { return };
    two_tickets(&mut h.fake());
    assert_eq!(connect(&mut h.frank, "code=abc").await, "/settings/hubspot");
    h.fake().down = Some(503);
    let r = h
        .frank
        .put(
            "/api/integrations/hubspot/pipelines",
            json!({ "pipelineIds": ["0"] }),
        )
        .await;
    assert_eq!(r.status, 200);
    h.app.run_jobs().await;
    // HubSpot stayed down past every retry.
    sqlx::query("UPDATE jobs SET failed_at = now() WHERE kind = 'hubspotImport'")
        .execute(&h.app.owner)
        .await
        .unwrap();
    let c = h.status().await;
    assert_eq!(c["status"], "importing");
    assert_eq!(c["lastError"], "HubSpot is not answering");

    // §10, §19: HubSpot is back, and the next check starts the import again.
    h.fake().down = None;
    sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'hubspotCheck'")
        .execute(&h.app.owner)
        .await
        .unwrap();
    h.app.run_jobs().await;
    let c = h.status().await;
    assert_eq!(c["status"], "synced");
    assert_eq!(c["tickets"], 2);
    assert_eq!(c["lastError"], Value::Null);
    // §11: the open ticket got Jev once the import was over.
    assert_eq!(h.jev_subjects(), vec!["SSO login fails"]);
}
