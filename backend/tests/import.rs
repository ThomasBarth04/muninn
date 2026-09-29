//! Spec 005: a mailbox export fills the brain, and the copilot finds what the
//! team solved before Muninn.

mod common;

use serde_json::{Value, json};

/// An mbox as Google Takeout writes one: a `From ` line before each message,
/// and a body line starting with "From " escaped as ">From ".
const EXPORT: &str = "\
From ola@kunde.no Mon Mar  3 09:12:00 2025
From: Ola Nordmann <ola@kunde.no>
To: support@acme.no
Date: Mon, 3 Mar 2025 10:12:00 +0100
Subject: SSO login fails
Message-ID: <m1@kunde.no>

Since this morning none of us can log in with SSO.

From support@acme.no Mon Mar  3 10:00:00 2025
From: Acme Support <support@acme.no>
To: ola@kunde.no
Date: Mon, 3 Mar 2025 11:00:00 +0100
Subject: Re: SSO login fails
Message-ID: <m2@acme.no>
In-Reply-To: <m1@kunde.no>
References: <m1@kunde.no>

Your IdP rotated its signing certificate. Re-upload it under Settings > SSO.
>From now on it renews itself.

On Mon, 3 Mar 2025 at 10:12, Ola Nordmann <ola@kunde.no> wrote:
> Since this morning none of us can log in with SSO.

From ola@kunde.no Mon Mar  3 11:30:00 2025
From: Ola Nordmann <ola@kunde.no>
Date: Mon, 3 Mar 2025 12:30:00 +0100
Subject: Re: SSO login fails
Message-ID: <m3@kunde.no>
In-Reply-To: <m2@acme.no>
References: <m1@kunde.no> <m2@acme.no>

That fixed it, thanks!

From kari@annen.no Tue Mar  4 08:00:00 2025
From: kari@annen.no
Date: Tue, 4 Mar 2025 09:00:00 +0100
Subject: Invoice question
Message-ID: <u1@annen.no>

Nobody ever answered this one.

From support@acme.no Wed Mar  5 08:00:00 2025
From: Acme Support <support@acme.no>
Date: Wed, 5 Mar 2025 09:00:00 +0100
Subject: Our spring newsletter
Message-ID: <n1@acme.no>

News from Acme.

From ola@kunde.no Wed Mar  5 09:00:00 2025
From: ola@kunde.no
Date: Wed, 5 Mar 2025 10:00:00 +0100
Subject: Out of office
Auto-Submitted: auto-replied
Message-ID: <auto1@kunde.no>

I am away until Monday.

From nobody Thu Mar  6 08:00:00 2025
From: per@kunde.no
Subject: No date on this one

Unreadable.

From per@kunde.no Fri Mar  7 08:00:00 2025
From: =?ISO-8859-1?Q?Per_H=E5konsen?= <per@kunde.no>
Date: Fri, 7 Mar 2025 09:00:00 +0100
Subject: =?UTF-8?Q?Eksport_feiler_p=C3=A5_store_filer?=
Message-ID: <p1@kunde.no>
Content-Type: text/html; charset=utf-8

<p>Eksporten stopper p&aring; <b>store filer</b>.</p>

From support@acme.no Fri Mar  7 09:00:00 2025
From: Kari Support <kari@acme.no>
Date: Fri, 7 Mar 2025 10:00:00 +0100
Subject: Re: Eksport feiler
Message-ID: <p2@acme.no>
In-Reply-To: <p1@kunde.no>
Content-Type: text/plain; charset=iso-8859-1
Content-Transfer-Encoding: quoted-printable

Sl=E5 av komprimering under Innstillinger, s=E5 g=E5r det.

";

async fn import(app: &common::TestApp, args: &[&str]) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    muninn::admin::run(&app.st, None, &args).await
}

async fn post_inbound(app: &common::TestApp, mail: Value) -> u16 {
    reqwest::Client::new()
        .post(format!("{}/hooks/postmark/inbound", app.url))
        .basic_auth("postmark", Some(common::INBOUND_PASSWORD))
        .json(&mail)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn inbound(
    from: &str,
    subject: &str,
    text: &str,
    message_id: &str,
    in_reply_to: Option<&str>,
) -> Value {
    let mut headers = vec![json!({ "Name": "Message-ID", "Value": format!("<{message_id}>") })];
    if let Some(parent) = in_reply_to {
        headers.push(json!({ "Name": "In-Reply-To", "Value": format!("<{parent}>") }));
    }
    json!({
        "OriginalRecipient": "acme@in.muninn.test",
        "FromFull": { "Email": from, "Name": "" },
        "ToFull": [{ "Email": "acme@in.muninn.test", "Name": "" }],
        "Subject": subject, "TextBody": text, "Headers": headers, "Attachments": [],
    })
}

#[tokio::test]
async fn a_mailbox_export_fills_the_brain() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (mut frank, _) = app.owner("acme", "frank@acme.no").await;
    let path = std::env::temp_dir().join(format!(
        "muninn-export-{}.mbox",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&path, EXPORT).unwrap();
    let path = path.to_str().unwrap();

    assert_eq!(
        import(&app, &["import", "nope", path, "--team", "@acme.no"]).await,
        Err("no workspace nope".into())
    );
    assert_eq!(
        import(&app, &["import", "acme", path]).await,
        Err("--team is required".into())
    );
    assert!(
        import(
            &app,
            &["import", "acme", "/no/such.mbox", "--team", "@acme.no"]
        )
        .await
        .unwrap_err()
        .starts_with("cannot read /no/such.mbox")
    );

    let summary = import(&app, &["import", "acme", path, "--team", "@acme.no"])
        .await
        .unwrap();
    assert_eq!(
        summary,
        "acme: read 9 messages in 4 threads\n\
         imported 2 tickets (5 messages)\n\
         skipped 2 threads: 1 no team reply, 1 started by the team, 0 already in Muninn\n\
         dropped 1 automatic and 1 unreadable messages\n\
         brain: 2 cases"
    );

    // Closed, uncategorised, never sent to Jev, dated as the mail was.
    let closed = frank.get("/api/tickets?view=closed").await.body;
    let tickets = closed["tickets"].as_array().unwrap();
    let subjects: Vec<&str> = tickets
        .iter()
        .map(|t| t["subject"].as_str().unwrap())
        .collect();
    assert_eq!(
        subjects,
        ["Eksport feiler på store filer", "SSO login fails"]
    );
    let sso = &tickets[1];
    assert_eq!(sso["status"], "closed");
    assert_eq!(sso["owner"], Value::Null);
    assert_eq!(sso["category"], Value::Null);
    assert_eq!(sso["contact"]["email"], "ola@kunde.no");
    assert_eq!(sso["createdAt"], "2025-03-03T09:12:00Z");
    assert_eq!(sso["lastActivityAt"], "2025-03-03T11:30:00Z");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    assert_eq!(jobs, 0);

    let id = sso["id"].as_str().unwrap();
    let thread = frank.get(&format!("/api/tickets/{id}")).await.body;
    let messages = thread["messages"].as_array().unwrap();
    let kinds: Vec<&str> = messages
        .iter()
        .map(|m| m["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["customer", "agent", "customer"]);
    let reply = &messages[1];
    assert_eq!(
        reply["author"],
        json!({ "name": "Acme Support", "email": "support@acme.no" })
    );
    assert_eq!(
        reply["delivery"],
        json!({ "status": "sent", "error": null })
    );
    // The quoted question is cut; the escaped "From " line is the mail's own.
    assert_eq!(
        reply["text"],
        "Your IdP rotated its signing certificate. Re-upload it under Settings > SSO.\nFrom now on it renews itself."
    );
    let export = frank
        .get(&format!(
            "/api/tickets/{}",
            tickets[0]["id"].as_str().unwrap()
        ))
        .await
        .body;
    assert_eq!(export["messages"][0]["author"]["name"], "Per Håkonsen");
    assert_eq!(
        export["messages"][0]["text"],
        "Eksporten stopper på store filer."
    );
    assert_eq!(
        export["messages"][1]["text"],
        "Slå av komprimering under Innstillinger, så går det."
    );

    // Again: nothing twice.
    let again = import(&app, &["import", "acme", path, "--team", "@acme.no"])
        .await
        .unwrap();
    assert!(again.contains("imported 0 tickets (0 messages)"), "{again}");
    assert!(again.contains("2 already in Muninn"), "{again}");
    assert!(again.ends_with("brain: 2 cases"), "{again}");

    // A new ticket about the same problem: the copilot shows what Acme Support wrote.
    app.mock.respond(|_, path, body| {
        if path != "/v1/systemone" {
            return None;
        }
        let mut answers = serde_json::Map::new();
        for (key, q) in body["questions"].as_object().unwrap() {
            let answer = if q["type"] == "choice" {
                json!({ "choice": "Login & access", "probabilities": { "Login & access": 0.9 } })
            } else {
                json!({ "noul": 0.92 })
            };
            answers.insert(key.clone(), answer);
        }
        Some((200, json!({ "answers": answers })))
    });
    let new = inbound(
        "siri@tredje.no",
        "SSO login fails for all of us",
        "SSO stopped working this morning.",
        "s1@tredje.no",
        None,
    );
    assert_eq!(post_inbound(&app, new).await, 200);
    app.run_jobs().await;
    let open = frank.get("/api/tickets?view=unassigned").await.body;
    let new_id = open["tickets"][0]["id"].as_str().unwrap().to_string();
    let s = frank
        .get(&format!("/api/tickets/{new_id}/suggestions"))
        .await
        .body;
    assert_eq!(s["brainSize"], 2);
    let top = &s["suggestions"][0];
    assert_eq!(top["case"]["ticketId"], id);
    assert_eq!(top["solution"]["author"]["name"], "Acme Support");
    assert!(
        top["solution"]["text"]
            .as_str()
            .unwrap()
            .starts_with("Your IdP rotated")
    );

    // Ola answers the old thread: the imported ticket reopens and leaves the brain.
    let late = inbound(
        "ola@kunde.no",
        "Re: SSO login fails",
        "It broke again.",
        "m4@kunde.no",
        Some("m2@acme.no"),
    );
    assert_eq!(post_inbound(&app, late).await, 200);
    let reopened = frank.get(&format!("/api/tickets/{id}")).await.body;
    assert_eq!(reopened["status"], "waitingOnUs");
    assert_eq!(reopened["messages"].as_array().unwrap().len(), 4);
    std::fs::remove_file(path).unwrap();
}
