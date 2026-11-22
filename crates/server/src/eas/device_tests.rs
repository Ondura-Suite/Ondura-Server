//! End-to-end ActiveSync tests through the HTTP router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use ondura_wbxml::Element;
use tower::ServiceExt;

use crate::app::{App, Shared};
use crate::store::{NewMail, now};

struct Device {
    app: Shared,
    id: &'static str,
    policy: String,
}

impl Device {
    async fn call(&self, cmd: &str, body: Option<Element>) -> (StatusCode, Option<Element>) {
        let auth = base64::engine::general_purpose::STANDARD.encode("niklas@example.com:secret");
        let req = Request::post(format!("/Microsoft-Server-ActiveSync?Cmd={cmd}&User=niklas%40example.com&DeviceId={}&DeviceType=iPhone", self.id))
            .header("Authorization", format!("Basic {auth}"))
            .header("MS-ASProtocolVersion", "14.1")
            .header("X-MS-PolicyKey", self.policy.clone())
            .header("Content-Type", "application/vnd.ms-sync.wbxml")
            .body(body.map(|b| Body::from(b.encode())).unwrap_or_else(Body::empty))
            .unwrap();
        let resp = crate::http::router(self.app.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, if bytes.is_empty() { None } else { Element::decode(&bytes).ok() })
    }

    async fn provision(&mut self) {
        let req = || Element::new("Provision:Provision").with(Element::new("Provision:Policies").with(Element::new("Provision:Policy").with_text("Provision:PolicyType", "MS-EAS-Provisioning-WBXML")));
        let (_, r) = self.call("Provision", Some(req())).await;
        let temp = r.unwrap().path("Policies/Policy").unwrap().text_of("PolicyKey").unwrap();
        let ack = Element::new("Provision:Provision").with(
            Element::new("Provision:Policies").with(
                Element::new("Provision:Policy")
                    .with_text("Provision:PolicyType", "MS-EAS-Provisioning-WBXML")
                    .with_text("Provision:PolicyKey", temp)
                    .with_text("Provision:Status", "1"),
            ),
        );
        let (_, r) = self.call("Provision", Some(ack)).await;
        self.policy = r.unwrap().path("Policies/Policy").unwrap().text_of("PolicyKey").unwrap();
    }
}

fn sync_req(key: &str, collection: &str, commands: Option<Element>) -> Element {
    let mut c = Element::new("AirSync:Collection").with_text("AirSync:SyncKey", key).with_text("AirSync:CollectionId", collection);
    if key != "0" {
        c.push(Element::new("AirSync:GetChanges"));
        c.push_text("AirSync:WindowSize", "2");
        c.push(Element::new("AirSync:Options").with(Element::new("AirSyncBase:BodyPreference").with_text("AirSyncBase:Type", "2").with_text("AirSyncBase:TruncationSize", "20000")));
    }
    if let Some(cmds) = commands {
        c.push(cmds);
    }
    Element::new("AirSync:Sync").with(Element::new("AirSync:Collections").with(c))
}

#[tokio::test]
async fn device_flow() {
    let (app, user) = App::test();
    let mut dev = Device { app: app.clone(), id: "dev1", policy: "0".into() };

    // Unprovisioned devices get status 142.
    let (_, r) = dev.call("FolderSync", Some(Element::new("FolderHierarchy:FolderSync").with_text("FolderHierarchy:SyncKey", "0"))).await;
    assert_eq!(r.unwrap().text_of("Status").as_deref(), Some("142"));
    dev.provision().await;

    let (status, r) = dev.call("FolderSync", Some(Element::new("FolderHierarchy:FolderSync").with_text("FolderHierarchy:SyncKey", "0"))).await;
    assert_eq!(status, StatusCode::OK);
    let r = r.unwrap();
    assert_eq!(r.text_of("Status").as_deref(), Some("1"));
    let changes = r.child("FolderHierarchy:Changes").unwrap();
    let inbox = changes
        .children_named("FolderHierarchy:Add")
        .find(|a| a.text_of("Type").as_deref() == Some("2"))
        .and_then(|a| a.text_of("ServerId"))
        .unwrap();
    let calendar = changes.children_named("FolderHierarchy:Add").find(|a| a.text_of("Type").as_deref() == Some("8")).and_then(|a| a.text_of("ServerId")).unwrap();
    let hierarchy_key = r.text_of("SyncKey").unwrap();

    // Three mails in the inbox.
    for i in 0..3 {
        let raw = format!("From: Max <max@other.org>\r\nTo: niklas@example.com\r\nSubject: Mail {i}\r\nContent-Type: text/html\r\n\r\n<p>Hallo {i}</p>");
        app.store.insert_mail(NewMail { user_id: user.id, folder_id: inbox.parse().unwrap(), mime: raw.as_bytes(), flags: 0, received: now(), writer: "smtp" }).unwrap();
    }
    let (_, r) = dev.call("Sync", Some(sync_req("0", &inbox, None))).await;
    let key = r.unwrap().path("Collections/Collection").unwrap().text_of("SyncKey").unwrap();
    assert_eq!(key, "1");

    // Window of 2: two adds and MoreAvailable, then the rest.
    let (_, r) = dev.call("Sync", Some(sync_req(&key, &inbox, None))).await;
    let col = r.unwrap().path("Collections/Collection").unwrap().clone();
    assert!(col.has("MoreAvailable"));
    assert_eq!(col.child("AirSync:Commands").unwrap().children_named("AirSync:Add").count(), 2);
    let first = col.path("Commands/Add").unwrap();
    assert!(first.find("Data").unwrap().content().contains("<p>Hallo"));
    let key = col.text_of("SyncKey").unwrap();
    let (_, r) = dev.call("Sync", Some(sync_req(&key, &inbox, None))).await;
    let col = r.unwrap().path("Collections/Collection").unwrap().clone();
    assert!(!col.has("MoreAvailable"));
    let adds: Vec<_> = col.child("AirSync:Commands").unwrap().children_named("AirSync:Add").collect();
    assert_eq!(adds.len(), 1);
    let sid = adds[0].text_of("ServerId").unwrap();
    let key = col.text_of("SyncKey").unwrap();

    // Mark read from the device: not echoed, stored.
    let change = Element::new("AirSync:Commands").with(
        Element::new("AirSync:Change").with_text("AirSync:ServerId", sid.clone()).with(Element::new("AirSync:ApplicationData").with_text("Email:Read", "1")),
    );
    let (_, r) = dev.call("Sync", Some(sync_req(&key, &inbox, Some(change)))).await;
    let col = r.unwrap().path("Collections/Collection").unwrap().clone();
    assert!(col.child("AirSync:Commands").is_none());
    let key = col.text_of("SyncKey").unwrap();
    assert!(app.store.item(super::item_id(&sid).unwrap()).unwrap().unwrap().is_read());

    // Nothing new: empty response.
    let (status, r) = dev.call("Sync", Some(sync_req(&key, &inbox, None))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(r.is_none());

    // Ping sees a new mail.
    let app2 = app.clone();
    let inbox_id: i64 = inbox.parse().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        app2.store.insert_mail(NewMail { user_id: user.id, folder_id: inbox_id, mime: b"Subject: neu\r\n\r\nx", flags: 0, received: now(), writer: "smtp" }).unwrap();
    });
    let ping = Element::new("Ping:Ping")
        .with_text("Ping:HeartbeatInterval", "60")
        .with(Element::new("Ping:Folders").with(Element::new("Ping:Folder").with_text("Ping:Id", inbox.clone()).with_text("Ping:Class", "Email")));
    let (_, r) = dev.call("Ping", Some(ping)).await;
    let r = r.unwrap();
    assert_eq!(r.text_of("Status").as_deref(), Some("2"));

    // Calendar: create an event from the device.
    let (_, r) = dev.call("Sync", Some(sync_req("0", &calendar, None))).await;
    let ckey = r.unwrap().path("Collections/Collection").unwrap().text_of("SyncKey").unwrap();
    let add = Element::new("AirSync:Commands").with(
        Element::new("AirSync:Add").with_text("AirSync:ClientId", "c1").with(
            Element::new("AirSync:ApplicationData")
                .with_text("Calendar:TimeZone", crate::tzinfo::eas_blob(chrono_tz::Europe::Berlin))
                .with_text("Calendar:AllDayEvent", "0")
                .with_text("Calendar:StartTime", "20261130T080000Z")
                .with_text("Calendar:EndTime", "20261130T090000Z")
                .with_text("Calendar:Subject", "Zahnarzt")
                .with_text("Calendar:BusyStatus", "2")
                .with_text("Calendar:Reminder", "30"),
        ),
    );
    let (_, r) = dev.call("Sync", Some(sync_req(&ckey, &calendar, Some(add)))).await;
    let col = r.unwrap().path("Collections/Collection").unwrap().clone();
    let resp = col.path("Responses/Add").unwrap();
    assert_eq!(resp.text_of("Status").as_deref(), Some("1"));
    let event_id = super::item_id(&resp.text_of("ServerId").unwrap()).unwrap();
    let e = app.store.item(event_id).unwrap().unwrap().event().unwrap();
    assert_eq!(e.tz, "Europe/Berlin");
    assert_eq!(e.reminder, Some(30));

    // A second device sees the event.
    let mut dev2 = Device { app: app.clone(), id: "dev2", policy: "0".into() };
    dev2.provision().await;
    let (_, _) = dev2.call("FolderSync", Some(Element::new("FolderHierarchy:FolderSync").with_text("FolderHierarchy:SyncKey", "0"))).await;
    let (_, r) = dev2.call("Sync", Some(sync_req("0", &calendar, None))).await;
    let k = r.unwrap().path("Collections/Collection").unwrap().text_of("SyncKey").unwrap();
    let (_, r) = dev2.call("Sync", Some(sync_req(&k, &calendar, None))).await;
    let r = r.unwrap();
    let add = r.path("Collections/Collection/Commands/Add").unwrap();
    assert_eq!(add.find("Calendar:Subject").unwrap().content(), "Zahnarzt");

    // Folder created on the device does not come back with the next FolderSync.
    let create = Element::new("FolderHierarchy:FolderCreate")
        .with_text("FolderHierarchy:SyncKey", hierarchy_key)
        .with_text("FolderHierarchy:ParentId", "0")
        .with_text("FolderHierarchy:DisplayName", "Projekte")
        .with_text("FolderHierarchy:Type", "12");
    let (_, r) = dev.call("FolderCreate", Some(create)).await;
    let r = r.unwrap();
    assert_eq!(r.text_of("Status").as_deref(), Some("1"));
    let (_, r) = dev.call("FolderSync", Some(Element::new("FolderHierarchy:FolderSync").with_text("FolderHierarchy:SyncKey", r.text_of("SyncKey").unwrap()))).await;
    assert_eq!(r.unwrap().path("Changes/Count").unwrap().content(), "0");

    // SendMail with a copy in Sent Items.
    let send = Element::new("ComposeMail:SendMail")
        .with_text("ComposeMail:ClientId", "m1")
        .with_empty("ComposeMail:SaveInSentItems")
        .with(Element::new("ComposeMail:MIME").opaque(b"From: niklas@example.com\r\nTo: someone@other.org\r\nSubject: Hi\r\n\r\nHallo".to_vec()));
    let (status, r) = dev.call("SendMail", Some(send)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(r.is_none());
    assert_eq!(app.store.queue().unwrap().len(), 1);

    // Settings: user information.
    let (_, r) = dev.call("Settings", Some(Element::new("Settings:Settings").with(Element::new("Settings:UserInformation").with(Element::new("Settings:Get"))))).await;
    assert_eq!(r.unwrap().find("PrimarySmtpAddress").unwrap().content(), "niklas@example.com");

    // OPTIONS advertises the versions.
    let auth = base64::engine::general_purpose::STANDARD.encode("niklas@example.com:secret");
    let req = Request::options("/Microsoft-Server-ActiveSync").header("Authorization", format!("Basic {auth}")).body(Body::empty()).unwrap();
    let resp = crate::http::router(app.clone()).oneshot(req).await.unwrap();
    assert!(resp.headers().get("MS-ASProtocolVersions").unwrap().to_str().unwrap().contains("14.1"));
}
