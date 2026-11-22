//! FolderSync, folder commands, Sync, GetItemEstimate and Ping.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ondura_wbxml::{Element, Node};
use tracing::debug;

use super::convert::{self, BodyPrefs};
use super::{Ctx, class_name, empty, folder, folder_type, item_id, server_id, wbxml};
use crate::model::{Contact, Event, FolderClass, ItemKind, Note, Task};
use crate::store::{Change, Folder, Item, now};

const HIERARCHY: &str = "hierarchy";

// ----------------------------------------------------------- folder sync

fn visible(f: &Folder) -> bool {
    f.class != FolderClass::Root
}

fn parent_id(ctx: &Ctx, f: &Folder) -> String {
    match f.parent_id.and_then(|p| ctx.app.store.folder(p).ok().flatten()) {
        Some(p) if p.class != FolderClass::Root => p.id.to_string(),
        _ => "0".into(),
    }
}

fn folder_element(ctx: &Ctx, tag: &str, f: &Folder) -> Element {
    Element::new(tag)
        .with_text("FolderHierarchy:ServerId", f.id.to_string())
        .with_text("FolderHierarchy:ParentId", parent_id(ctx, f))
        .with_text("FolderHierarchy:DisplayName", f.name.clone())
        .with_text("FolderHierarchy:Type", folder_type(f).to_string())
}

fn hierarchy_key(ctx: &Ctx, key: &str) -> Option<i64> {
    let key: i64 = key.trim().parse().ok()?;
    ctx.app.store.sync_key(ctx.user.id, &ctx.device_id, HIERARCHY, key).ok().flatten()
}

fn next_hierarchy_key(ctx: &Ctx, modseq: i64) -> Result<i64> {
    let next = ctx.app.store.latest_sync_key(ctx.user.id, &ctx.device_id, HIERARCHY)?.map(|(k, _)| k + 1).unwrap_or(1);
    ctx.app.store.save_sync_key(ctx.user.id, &ctx.device_id, HIERARCHY, next, modseq)?;
    Ok(next)
}

pub fn folder_sync(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let key = doc.and_then(|d| d.text_of("FolderHierarchy:SyncKey")).unwrap_or_else(|| "0".into());
    let base = if key.trim() == "0" {
        // A full resync invalidates all item sync states of the device.
        ctx.app.store.reset_sync(ctx.user.id, &ctx.device_id, None)?;
        0
    } else {
        match hierarchy_key(ctx, &key) {
            Some(m) => m,
            None => return Ok(wbxml(&Element::new("FolderHierarchy:FolderSync").with_text("FolderHierarchy:Status", "9"))),
        }
    };
    let changes = ctx.app.store.hierarchy_changes(ctx.user.id, base)?;
    let mut list = Element::new("FolderHierarchy:Changes");
    let mut count = 0;
    let mut items = Vec::new();
    for f in changes.updated.iter().filter(|f| visible(f)) {
        items.push(folder_element(ctx, "FolderHierarchy:Update", f));
    }
    for f in changes.deleted.iter().filter(|f| visible(f)) {
        items.push(Element::new("FolderHierarchy:Delete").with_text("FolderHierarchy:ServerId", f.id.to_string()));
    }
    for f in changes.added.iter().filter(|f| visible(f)) {
        items.push(folder_element(ctx, "FolderHierarchy:Add", f));
    }
    count += items.len();
    list.push_text("FolderHierarchy:Count", count.to_string());
    for i in items {
        list.push(i);
    }
    let new_key = next_hierarchy_key(ctx, changes.modseq)?;
    Ok(wbxml(
        &Element::new("FolderHierarchy:FolderSync")
            .with_text("FolderHierarchy:Status", "1")
            .with_text("FolderHierarchy:SyncKey", new_key.to_string())
            .with(list),
    ))
}

fn folder_class_for_type(t: &str) -> FolderClass {
    match t.trim() {
        "13" | "8" => FolderClass::Calendar,
        "14" | "9" => FolderClass::Contacts,
        "15" | "7" => FolderClass::Tasks,
        "17" | "10" => FolderClass::Notes,
        _ => FolderClass::Mail,
    }
}

fn folder_reply(tag: &str, status: &str, key: Option<i64>, server_id: Option<i64>) -> Response {
    let mut e = Element::new(tag).with_text("FolderHierarchy:Status", status);
    if let Some(k) = key {
        e.push_text("FolderHierarchy:SyncKey", k.to_string());
    }
    if let Some(id) = server_id {
        e.push_text("FolderHierarchy:ServerId", id.to_string());
    }
    wbxml(&e)
}

pub fn folder_create(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    const TAG: &str = "FolderHierarchy:FolderCreate";
    let Some(d) = doc else { return Ok(folder_reply(TAG, "10", None, None)) };
    if hierarchy_key(ctx, &d.text_of("FolderHierarchy:SyncKey").unwrap_or_default()).is_none() {
        return Ok(folder_reply(TAG, "9", None, None));
    }
    let name = d.text_of("FolderHierarchy:DisplayName").unwrap_or_default();
    let parent = d.text_of("FolderHierarchy:ParentId").unwrap_or_else(|| "0".into());
    let parent_folder = if parent.trim() == "0" { None } else { folder(ctx, &parent) };
    if parent.trim() != "0" && parent_folder.is_none() {
        return Ok(folder_reply(TAG, "5", None, None));
    }
    let class = folder_class_for_type(&d.text_of("FolderHierarchy:Type").unwrap_or_default());
    match ctx.app.store.create_folder(ctx.user.id, parent_folder.map(|p| p.id), &name, class, &ctx.writer) {
        Ok(f) => {
            let key = next_hierarchy_key(ctx, ctx.app.store.current_modseq(ctx.user.id)?)?;
            Ok(folder_reply(TAG, "1", Some(key), Some(f.id)))
        }
        Err(e) if e.to_string().contains("already exists") => Ok(folder_reply(TAG, "2", None, None)),
        Err(_) => Ok(folder_reply(TAG, "6", None, None)),
    }
}

pub fn folder_update(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    const TAG: &str = "FolderHierarchy:FolderUpdate";
    let Some(d) = doc else { return Ok(folder_reply(TAG, "10", None, None)) };
    if hierarchy_key(ctx, &d.text_of("FolderHierarchy:SyncKey").unwrap_or_default()).is_none() {
        return Ok(folder_reply(TAG, "9", None, None));
    }
    let Some(f) = d.text_of("FolderHierarchy:ServerId").and_then(|id| folder(ctx, &id)) else {
        return Ok(folder_reply(TAG, "4", None, None));
    };
    let parent = match d.text_of("FolderHierarchy:ParentId").as_deref().map(str::trim) {
        Some("0") | None => ctx.app.store.folder_by_role(ctx.user.id, "msgfolderroot")?.map(|r| r.id),
        Some(p) => match folder(ctx, p) {
            Some(pf) => Some(pf.id),
            None => return Ok(folder_reply(TAG, "5", None, None)),
        },
    };
    let name = d.text_of("FolderHierarchy:DisplayName");
    match ctx.app.store.update_folder(f.id, name.as_deref(), parent) {
        Ok(_) => {
            let key = next_hierarchy_key(ctx, ctx.app.store.current_modseq(ctx.user.id)?)?;
            Ok(folder_reply(TAG, "1", Some(key), None))
        }
        Err(e) if e.to_string().contains("default folders") => Ok(folder_reply(TAG, "2", None, None)),
        Err(_) => Ok(folder_reply(TAG, "6", None, None)),
    }
}

pub fn folder_delete(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    const TAG: &str = "FolderHierarchy:FolderDelete";
    let Some(d) = doc else { return Ok(folder_reply(TAG, "10", None, None)) };
    if hierarchy_key(ctx, &d.text_of("FolderHierarchy:SyncKey").unwrap_or_default()).is_none() {
        return Ok(folder_reply(TAG, "9", None, None));
    }
    let Some(f) = d.text_of("FolderHierarchy:ServerId").and_then(|id| folder(ctx, &id)) else {
        return Ok(folder_reply(TAG, "4", None, None));
    };
    if f.role.is_some() {
        return Ok(folder_reply(TAG, "3", None, None));
    }
    ctx.app.store.delete_folder(f.id, &ctx.writer)?;
    let key = next_hierarchy_key(ctx, ctx.app.store.current_modseq(ctx.user.id)?)?;
    Ok(folder_reply(TAG, "1", Some(key), None))
}

// ------------------------------------------------------------------ sync

/// Earliest date for a FilterType (`None` = everything).
fn filter_cutoff(filter: u8) -> Option<i64> {
    let days = match filter {
        1 => 1,
        2 => 3,
        3 => 7,
        4 => 14,
        5 => 31,
        6 => 92,
        7 => 183,
        _ => return None,
    };
    Some(now() - days * 86_400)
}

fn passes_filter(item: &Item, filter: u8) -> bool {
    match item.kind {
        ItemKind::Mail => filter_cutoff(filter).is_none_or(|c| item.received >= c),
        ItemKind::Event => match filter_cutoff(filter) {
            None => true,
            Some(c) => item.event().is_none_or(|e| e.last_end().is_none_or(|end| end >= c)),
        },
        ItemKind::Task if filter == 8 => item.task().is_none_or(|t| !t.complete),
        _ => true,
    }
}

/// Render the ApplicationData of an item.
pub fn render(ctx: &Ctx, item: &Item, prefs: &BodyPrefs, full: bool) -> Element {
    match item.kind {
        ItemKind::Mail => {
            if full {
                let raw = ctx.app.store.mime(item.id).ok().flatten();
                let me = ctx.user.email.clone();
                convert::email(item, raw.as_deref(), prefs, ctx.version, &me)
            } else {
                convert::email_flags(item, ctx.version)
            }
        }
        ItemKind::Event => convert::event(&item.event().unwrap_or_default(), prefs, ctx.version, &ctx.me, &ctx.user.person()),
        ItemKind::Contact => convert::contact(&item.contact().unwrap_or_default(), prefs, ctx.version),
        ItemKind::Task => convert::task(&item.task().unwrap_or_default(), prefs),
        ItemKind::Note => convert::note(&item.note().unwrap_or_default(), prefs, item.received),
    }
}

#[derive(Clone)]
struct CollectionRequest {
    key: String,
    id: String,
    get_changes: Option<bool>,
    window: usize,
    filter: u8,
    prefs: BodyPrefs,
    deletes_as_moves: bool,
    commands: Option<Element>,
    conflict: u8,
}

fn parse_collection(c: &Element, default_window: usize) -> CollectionRequest {
    let options = c.children_named("AirSync:Options").find(|o| o.text_of("AirSync:Class").is_none_or(|cl| cl != "SMS")).or_else(|| c.child("AirSync:Options"));
    CollectionRequest {
        key: c.text_of("AirSync:SyncKey").unwrap_or_else(|| "0".into()).trim().to_string(),
        id: c.text_of("AirSync:CollectionId").unwrap_or_default().trim().to_string(),
        get_changes: c.child("AirSync:GetChanges").map(|g| g.content().trim() != "0"),
        window: c.text_of("AirSync:WindowSize").and_then(|w| w.trim().parse().ok()).unwrap_or(default_window).clamp(1, 512),
        filter: options.and_then(|o| o.text_of("AirSync:FilterType")).and_then(|f| f.trim().parse().ok()).unwrap_or(0),
        prefs: BodyPrefs::parse(options),
        deletes_as_moves: c.text_of("AirSync:DeletesAsMoves").is_none_or(|v| v.trim() != "0"),
        commands: c.child("AirSync:Commands").cloned(),
        conflict: options.and_then(|o| o.text_of("AirSync:Conflict")).and_then(|f| f.trim().parse().ok()).unwrap_or(1),
    }
}

struct Pending {
    req: CollectionRequest,
    folder: Option<Folder>,
    status: &'static str,
    responses: Vec<Element>,
    /// Server changes to send and the modseq the new key stands for.
    commands: Vec<Element>,
    more: bool,
    new_modseq: i64,
    /// Respond with this collection even without changes.
    force: bool,
}

/// Apply the client's Add/Change/Delete/Fetch commands.
fn apply_commands(ctx: &Ctx, folder: &Folder, req: &CollectionRequest, responses: &mut Vec<Element>) -> Result<()> {
    let Some(cmds) = &req.commands else { return Ok(()) };
    let store = &ctx.app.store;
    for cmd in cmds.elements() {
        match cmd.tag {
            "Add" => {
                let client_id = cmd.text_of("AirSync:ClientId").unwrap_or_default();
                let Some(data) = cmd.child("AirSync:ApplicationData") else { continue };
                let created = match folder.class {
                    FolderClass::Calendar => {
                        let e = convert::apply_event(data, Event::default(), &ctx.user.person());
                        // Avoid duplicates when a client retries an Add.
                        match crate::calendar::find_by_uid(&ctx.app, ctx.user.id, &e.uid)? {
                            Some(existing) if existing.folder_id == folder.id => Some(existing.id),
                            _ => Some(store.insert_pim(ctx.user.id, folder.id, ItemKind::Event, &serde_json::to_string(&e)?, Some(&e.uid), &ctx.writer)?),
                        }
                    }
                    FolderClass::Contacts => {
                        let c = convert::apply_contact(data, Contact::default());
                        Some(store.insert_pim(ctx.user.id, folder.id, ItemKind::Contact, &serde_json::to_string(&c)?, None, &ctx.writer)?)
                    }
                    FolderClass::Tasks => {
                        let t = convert::apply_task(data, Task::default());
                        Some(store.insert_pim(ctx.user.id, folder.id, ItemKind::Task, &serde_json::to_string(&t)?, None, &ctx.writer)?)
                    }
                    FolderClass::Notes => {
                        let n = convert::apply_note(data, Note::default());
                        Some(store.insert_pim(ctx.user.id, folder.id, ItemKind::Note, &serde_json::to_string(&n)?, None, &ctx.writer)?)
                    }
                    _ => None,
                };
                let mut r = Element::new("AirSync:Add").with_text("AirSync:ClientId", client_id);
                match created {
                    Some(id) => {
                        r.push_text("AirSync:ServerId", server_id(folder.id, id));
                        r.push_text("AirSync:Status", "1");
                    }
                    None => r.push_text("AirSync:Status", "6"),
                }
                responses.push(r);
            }
            "Change" => {
                let sid = cmd.text_of("AirSync:ServerId").unwrap_or_default();
                let item = item_id(&sid).and_then(|id| store.item(id).ok().flatten()).filter(|i| i.user_id == ctx.user.id);
                let Some(item) = item else {
                    responses.push(Element::new("AirSync:Change").with_text("AirSync:ServerId", sid).with_text("AirSync:Status", "8"));
                    continue;
                };
                let Some(data) = cmd.child("AirSync:ApplicationData") else { continue };
                let result: Result<()> = match item.kind {
                    ItemKind::Mail => {
                        let (set, clear, cats) = convert::email_changes(data);
                        store.update_flags(item.id, set, clear, cats.as_deref(), &ctx.writer)
                    }
                    ItemKind::Event => {
                        let e = convert::apply_event(data, item.event().unwrap_or_default(), &ctx.user.person());
                        store.update_pim(item.id, &serde_json::to_string(&e)?, &ctx.writer)
                    }
                    ItemKind::Contact => {
                        let c = convert::apply_contact(data, item.contact().unwrap_or_default());
                        store.update_pim(item.id, &serde_json::to_string(&c)?, &ctx.writer)
                    }
                    ItemKind::Task => {
                        let t = convert::apply_task(data, item.task().unwrap_or_default());
                        store.update_pim(item.id, &serde_json::to_string(&t)?, &ctx.writer)
                    }
                    ItemKind::Note => {
                        let n = convert::apply_note(data, item.note().unwrap_or_default());
                        store.update_pim(item.id, &serde_json::to_string(&n)?, &ctx.writer)
                    }
                };
                if result.is_err() {
                    responses.push(Element::new("AirSync:Change").with_text("AirSync:ServerId", sid).with_text("AirSync:Status", "6"));
                }
            }
            "Delete" => {
                let sid = cmd.text_of("AirSync:ServerId").unwrap_or_default();
                let Some(item) = item_id(&sid).and_then(|id| store.item(id).ok().flatten()).filter(|i| i.user_id == ctx.user.id) else {
                    continue;
                };
                if item.kind == ItemKind::Mail && req.deletes_as_moves && folder.role.as_deref() != Some("deleteditems") {
                    let trash = store.ensure_folder(ctx.user.id, "deleteditems")?;
                    store.move_item(item.id, trash.id, &ctx.writer)?;
                } else {
                    store.delete_item(item.id, &ctx.writer)?;
                }
            }
            "Fetch" => {
                let sid = cmd.text_of("AirSync:ServerId").unwrap_or_default();
                let mut r = Element::new("AirSync:Fetch").with_text("AirSync:ServerId", sid.clone());
                match item_id(&sid).and_then(|id| store.item(id).ok().flatten()).filter(|i| i.user_id == ctx.user.id) {
                    Some(item) => {
                        r.push_text("AirSync:Status", "1");
                        r.push(render(ctx, &item, &req.prefs, true));
                    }
                    None => r.push_text("AirSync:Status", "8"),
                }
                responses.push(r);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Server changes for a collection (no side effects).
fn collect(ctx: &Ctx, folder: &Folder, req: &CollectionRequest, base: i64) -> Result<(Vec<Element>, bool, i64)> {
    let store = &ctx.app.store;
    let current = store.current_modseq(ctx.user.id)?;
    let mut changes: Vec<Change> = store.changes(folder.id, base, Some(&ctx.writer))?.into_iter().filter(|c| c.modseq() <= current).collect();
    if base == 0 {
        changes.retain(|c| !matches!(c, Change::Delete { .. }));
    }
    // Initial sync: newest mail first is what clients show first.
    let mut out = Vec::new();
    let mut last_modseq = base;
    let mut more = false;
    for c in changes {
        // Changes with the same modseq (bulk operations) stay together.
        if out.len() >= req.window && c.modseq() != last_modseq {
            more = true;
            break;
        }
        last_modseq = c.modseq();
        match c {
            Change::Add(item) => {
                if !passes_filter(&item, req.filter) {
                    continue;
                }
                out.push(
                    Element::new("AirSync:Add")
                        .with_text("AirSync:ServerId", server_id(folder.id, item.id))
                        .with(render(ctx, &item, &req.prefs, true)),
                );
            }
            Change::Update(item) => {
                let full = item.kind != ItemKind::Mail || folder.role.as_deref() == Some("drafts");
                out.push(
                    Element::new("AirSync:Change")
                        .with_text("AirSync:ServerId", server_id(folder.id, item.id))
                        .with(render(ctx, &item, &req.prefs, full)),
                );
            }
            Change::Delete { item_id, .. } => {
                out.push(Element::new("AirSync:Delete").with_text("AirSync:ServerId", server_id(folder.id, item_id)));
            }
        }
    }
    let new_modseq = if more { last_modseq } else { current };
    Ok((out, more, new_modseq))
}

fn sync_status(code: &str) -> Response {
    wbxml(&Element::new("AirSync:Sync").with_text("AirSync:Status", code))
}

pub async fn sync(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let store = &ctx.app.store;
    // Empty request: repeat the last full request of the device.
    let cached = store.device_state(ctx.user.id, &ctx.device_id, "sync_request")?.and_then(|s| B64.decode(s).ok()).and_then(|b| Element::decode(&b).ok());
    let partial = doc.is_some_and(|d| d.has("AirSync:Partial"));
    let request = match (doc, &cached) {
        (None, Some(c)) => c.clone(),
        (None, None) => return Ok(sync_status("13")),
        (Some(d), Some(c)) if partial => {
            // Merge: collections of this request replace the cached ones.
            let mut merged = c.clone();
            if let Some(Node::Element(cols)) = merged.children.iter_mut().find(|n| matches!(n, Node::Element(e) if e.tag == "Collections")) {
                let new_ids: HashSet<String> = d.path("Collections").map(|cs| cs.children_named("Collection").filter_map(|x| x.text_of("CollectionId")).collect()).unwrap_or_default();
                cols.children.retain(|n| !matches!(n, Node::Element(e) if e.text_of("CollectionId").is_some_and(|id| new_ids.contains(&id))));
                if let Some(cs) = d.path("Collections") {
                    for x in cs.children_named("Collection") {
                        cols.push(x.clone());
                    }
                }
            }
            for name in ["AirSync:Wait", "AirSync:HeartbeatInterval", "AirSync:WindowSize"] {
                merged.children.retain(|n| !matches!(n, Node::Element(e) if e.is(name)));
                if let Some(v) = d.child(name) {
                    merged.push(v.clone());
                }
            }
            merged
        }
        (Some(d), _) => d.clone(),
    };
    let default_window: usize = request.text_of("AirSync:WindowSize").and_then(|w| w.trim().parse().ok()).unwrap_or(100);
    let wait_secs: Option<u64> = request
        .text_of("AirSync:HeartbeatInterval")
        .and_then(|h| h.trim().parse().ok())
        .or_else(|| request.text_of("AirSync:Wait").and_then(|w| w.trim().parse::<u64>().ok()).map(|m| m * 60));
    if let Some(w) = wait_secs
        && (w < 60 || w > ctx.app.cfg.activesync.max_heartbeat.max(60))
    {
        let limit = if w < 60 { 60 } else { ctx.app.cfg.activesync.max_heartbeat };
        let tag = "AirSync:Limit";
        let value = if request.has("AirSync:Wait") { (limit / 60).to_string() } else { limit.to_string() };
        return Ok(wbxml(&Element::new("AirSync:Sync").with_text("AirSync:Status", "14").with_text(tag, value)));
    }
    // Remember the request (without commands) for empty and partial requests.
    let mut to_cache = request.clone();
    if let Some(Node::Element(cols)) = to_cache.children.iter_mut().find(|n| matches!(n, Node::Element(e) if e.tag == "Collections")) {
        for n in cols.children.iter_mut() {
            if let Node::Element(c) = n {
                c.children.retain(|x| !matches!(x, Node::Element(e) if e.tag == "Commands" || e.tag == "SyncKey"));
            }
        }
    }

    let Some(cols) = request.path("Collections") else { return Ok(sync_status("4")) };
    let mut pending: Vec<Pending> = Vec::new();
    let mut had_commands = false;
    for c in cols.children_named("AirSync:Collection") {
        let mut req = parse_collection(c, default_window);
        // Cached requests carry no sync key: use the latest one of the device.
        if !c.has("AirSync:SyncKey")
            && let Some((k, _)) = store.latest_sync_key(ctx.user.id, &ctx.device_id, &req.id)?
        {
            req.key = k.to_string();
        }
        had_commands |= req.commands.is_some();
        let folder = folder(ctx, &req.id);
        let mut p = Pending { req, folder, status: "1", responses: vec![], commands: vec![], more: false, new_modseq: 0, force: false };
        let Some(f) = p.folder.clone() else {
            p.status = "12";
            p.force = true;
            pending.push(p);
            continue;
        };
        if p.req.key == "0" {
            store.reset_sync(ctx.user.id, &ctx.device_id, Some(&p.req.id))?;
            p.new_modseq = 0;
            p.force = true;
            pending.push(p);
            continue;
        }
        let key: i64 = p.req.key.parse().unwrap_or(-1);
        let Some(base) = store.sync_key(ctx.user.id, &ctx.device_id, &p.req.id, key)? else {
            p.status = "3";
            p.force = true;
            pending.push(p);
            continue;
        };
        let mut responses = Vec::new();
        apply_commands(ctx, &f, &p.req, &mut responses)?;
        p.force = !responses.is_empty() || p.req.commands.is_some();
        p.responses = responses;
        if p.req.get_changes.unwrap_or(true) {
            let (commands, more, new_modseq) = collect(ctx, &f, &p.req, base)?;
            p.commands = commands;
            p.more = more;
            p.new_modseq = new_modseq;
        } else {
            p.new_modseq = base.max(store.current_modseq(ctx.user.id)?);
        }
        pending.push(p);
    }

    let nothing = |pending: &[Pending]| pending.iter().all(|p| !p.force && p.commands.is_empty() && !p.more);
    if nothing(&pending) && !had_commands {
        if let Some(secs) = wait_secs {
            // Long poll until something changes in one of the folders.
            let folders: HashSet<i64> = pending.iter().filter_map(|p| p.folder.as_ref().map(|f| f.id)).collect();
            let mut rx = store.subscribe();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
            loop {
                let notice = tokio::time::timeout_at(deadline, rx.recv()).await;
                match notice {
                    Err(_) => break,
                    Ok(Ok(n)) if n.user_id == ctx.user.id && (folders.contains(&n.folder_id) || n.folder_id == 0) => {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let mut changed = false;
                        for p in pending.iter_mut() {
                            let Some(f) = p.folder.clone() else { continue };
                            let key: i64 = p.req.key.parse().unwrap_or(-1);
                            let Some(base) = store.sync_key(ctx.user.id, &ctx.device_id, &p.req.id, key)? else { continue };
                            let (commands, more, new_modseq) = collect(ctx, &f, &p.req, base)?;
                            changed |= !commands.is_empty();
                            p.commands = commands;
                            p.more = more;
                            p.new_modseq = new_modseq;
                        }
                        if changed {
                            break;
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                    Ok(Err(_)) => break,
                    Ok(Ok(_)) => {}
                }
            }
        }
        if nothing(&pending) {
            store.set_device_state(ctx.user.id, &ctx.device_id, "sync_request", &B64.encode(to_cache.encode()))?;
            return Ok(empty());
        }
    }

    let mut collections = Element::new("AirSync:Collections");
    for p in pending {
        let mut c = Element::new("AirSync:Collection");
        if ctx.version < 14.0
            && let Some(f) = &p.folder
        {
            c.push_text("AirSync:Class", class_name(f.class));
        }
        if p.status == "1" {
            let key: i64 = if p.req.key == "0" { 0 } else { p.req.key.parse().unwrap_or(0) };
            let new_key = key + 1;
            store.save_sync_key(ctx.user.id, &ctx.device_id, &p.req.id, new_key, p.new_modseq)?;
            c.push_text("AirSync:SyncKey", new_key.to_string());
        } else {
            c.push_text("AirSync:SyncKey", if p.status == "3" { "0".to_string() } else { p.req.key.clone() });
        }
        c.push_text("AirSync:CollectionId", p.req.id.clone());
        c.push_text("AirSync:Status", p.status);
        if p.more {
            c.push(Element::new("AirSync:MoreAvailable"));
        }
        if !p.commands.is_empty() {
            let mut cmds = Element::new("AirSync:Commands");
            for x in p.commands {
                cmds.push(x);
            }
            c.push(cmds);
        }
        if !p.responses.is_empty() {
            let mut rs = Element::new("AirSync:Responses");
            for x in p.responses {
                rs.push(x);
            }
            c.push(rs);
        }
        let _ = p.req.conflict;
        collections.push(c);
    }
    store.set_device_state(ctx.user.id, &ctx.device_id, "sync_request", &B64.encode(to_cache.encode()))?;
    Ok(wbxml(&Element::new("AirSync:Sync").with(collections)))
}

// ------------------------------------------------------- GetItemEstimate

pub fn get_item_estimate(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let mut root = Element::new("GetItemEstimate:GetItemEstimate");
    let Some(cols) = doc.and_then(|d| d.child("GetItemEstimate:Collections")) else {
        return Ok(wbxml(&root.with(Element::new("GetItemEstimate:Response").with_text("GetItemEstimate:Status", "2"))));
    };
    for c in cols.children_named("GetItemEstimate:Collection") {
        let id = c.text_of("GetItemEstimate:CollectionId").unwrap_or_default();
        let key = c.text_of("AirSync:SyncKey").unwrap_or_else(|| "0".into());
        let filter: u8 = c.child("AirSync:Options").and_then(|o| o.text_of("AirSync:FilterType")).and_then(|f| f.trim().parse().ok()).unwrap_or(0);
        let mut resp = Element::new("GetItemEstimate:Response");
        let Some(f) = folder(ctx, &id) else {
            resp.push_text("GetItemEstimate:Status", "2");
            root.push(resp);
            continue;
        };
        let base = if key.trim() == "0" { Some(0) } else { ctx.app.store.sync_key(ctx.user.id, &ctx.device_id, &id, key.trim().parse().unwrap_or(-1))? };
        let Some(base) = base else {
            resp.push_text("GetItemEstimate:Status", "4");
            root.push(resp);
            continue;
        };
        let estimate = ctx
            .app
            .store
            .changes(f.id, base, Some(&ctx.writer))?
            .iter()
            .filter(|c| match c {
                Change::Add(i) => passes_filter(i, filter),
                Change::Delete { .. } => base > 0,
                Change::Update(_) => true,
            })
            .count();
        resp.push_text("GetItemEstimate:Status", "1");
        resp.push(Element::new("GetItemEstimate:Collection").with_text("GetItemEstimate:CollectionId", id).with_text("GetItemEstimate:Estimate", estimate.to_string()));
        root.push(resp);
    }
    Ok(wbxml(&root))
}

// ------------------------------------------------------------------ ping

pub async fn ping(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let store = &ctx.app.store;
    let max = ctx.app.cfg.activesync.max_heartbeat.max(60);
    // Request or the cached one.
    let (heartbeat, folders): (u64, Vec<String>) = match doc {
        Some(d) => {
            let hb = d.text_of("Ping:HeartbeatInterval").and_then(|h| h.trim().parse().ok());
            let ids: Vec<String> = d.path("Folders").map(|f| f.children_named("Ping:Folder").filter_map(|x| x.text_of("Ping:Id")).collect()).unwrap_or_default();
            let cached: Option<(u64, Vec<String>)> = store.device_state(ctx.user.id, &ctx.device_id, "ping")?.and_then(|s| serde_json::from_str(&s).ok());
            let hb = hb.or(cached.as_ref().map(|c| c.0)).unwrap_or(480);
            let ids = if ids.is_empty() { cached.map(|c| c.1).unwrap_or_default() } else { ids };
            (hb, ids)
        }
        None => match store.device_state(ctx.user.id, &ctx.device_id, "ping")?.and_then(|s| serde_json::from_str::<(u64, Vec<String>)>(&s).ok()) {
            Some(c) => c,
            None => return Ok(wbxml(&Element::new("Ping:Ping").with_text("Ping:Status", "3"))),
        },
    };
    if heartbeat < 60 || heartbeat > max {
        return Ok(wbxml(&Element::new("Ping:Ping").with_text("Ping:Status", "5").with_text("Ping:HeartbeatInterval", if heartbeat < 60 { "60".to_string() } else { max.to_string() })));
    }
    if folders.is_empty() {
        return Ok(wbxml(&Element::new("Ping:Ping").with_text("Ping:Status", "3")));
    }
    store.set_device_state(ctx.user.id, &ctx.device_id, "ping", &serde_json::to_string(&(heartbeat, &folders))?)?;

    let mut watched: HashMap<i64, (String, i64)> = HashMap::new();
    for id in &folders {
        let Some(f) = folder(ctx, id) else {
            return Ok(wbxml(&Element::new("Ping:Ping").with_text("Ping:Status", "7")));
        };
        let base = store.latest_sync_key(ctx.user.id, &ctx.device_id, id)?.map(|(_, m)| m);
        watched.insert(f.id, (id.clone(), base.unwrap_or(-1)));
    }
    let changed = |watched: &HashMap<i64, (String, i64)>| -> Result<Vec<String>> {
        let mut out = Vec::new();
        for (fid, (id, base)) in watched {
            if *base < 0 || store.has_changes(*fid, *base, Some(&ctx.writer))? {
                out.push(id.clone());
            }
        }
        Ok(out)
    };
    let mut rx = store.subscribe();
    let mut hit = changed(&watched)?;
    if hit.is_empty() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(heartbeat);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => break,
                Ok(Ok(n)) if n.user_id == ctx.user.id && (watched.contains_key(&n.folder_id) || n.folder_id == 0) => {
                    if n.folder_id == 0 && store.hierarchy_changes(ctx.user.id, store.latest_sync_key(ctx.user.id, &ctx.device_id, HIERARCHY)?.map(|k| k.1).unwrap_or(0))?.deleted.iter().any(|f| watched.contains_key(&f.id)) {
                        return Ok(wbxml(&Element::new("Ping:Ping").with_text("Ping:Status", "7")));
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    hit = changed(&watched)?;
                    if !hit.is_empty() {
                        break;
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Ok(_) => {}
            }
        }
    }
    if hit.is_empty() {
        debug!(device = %ctx.device_id, "ping expired");
        return Ok(wbxml(&Element::new("Ping:Ping").with_text("Ping:Status", "1")));
    }
    let mut list = Element::new("Ping:Folders");
    for id in hit {
        list.push_text("Ping:Folder", id);
    }
    Ok(wbxml(&Element::new("Ping:Ping").with_text("Ping:Status", "2").with(list)))
}
