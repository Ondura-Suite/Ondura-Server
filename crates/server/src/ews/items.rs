//! Folder and item operations.

use std::cmp::Ordering;
use std::fmt::Write as _;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use super::read::{self, parse_time};
use super::write::{event_xml, folder_xml, item_xml, utc};
use super::{Ctx, EwsError, EwsResult, ItemRef, Shape, class_element, folder_in, item_id_xml, item_ref, load_item, message, resolve_folder, wrap};
use crate::calendar;
use crate::mime;
use crate::model::{Event, FolderClass, ItemKind, Occurrence, Person, Response, flags};
use crate::smtp::submit;
use crate::store::{Change, Folder, Item, NewMail, now};
use crate::xml::Node;

const WRITER: &str = "ews";

fn folder_refs(n: Option<&Node>) -> Vec<&Node> {
    n.map(|n| n.children.iter().filter(|c| c.name == "FolderId" || c.name == "DistinguishedFolderId").collect()).unwrap_or_default()
}

fn item_refs(n: Option<&Node>) -> Vec<&Node> {
    n.map(|n| n.children.iter().filter(|c| matches!(c.name.as_str(), "ItemId" | "OccurrenceItemId" | "RecurringMasterItemId")).collect()).unwrap_or_default()
}

fn visible(f: &Folder) -> bool {
    f.class != FolderClass::Root
}

fn subfolders(ctx: &Ctx, parent: &Folder, deep: bool) -> EwsResult<Vec<Folder>> {
    let all = ctx.app.store.folders(parent.user_id)?;
    let mut out = Vec::new();
    let mut queue = vec![parent.id];
    while let Some(p) = queue.pop() {
        let mut children: Vec<Folder> = all.iter().filter(|f| f.parent_id == Some(p)).cloned().collect();
        children.sort_by_key(|f| f.id);
        for c in children {
            if deep {
                queue.push(c.id);
            }
            out.push(c);
        }
    }
    out.sort_by_key(|f| f.id);
    Ok(out)
}

// ------------------------------------------------------------------ folders

pub fn get_folder(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let shape = Shape::parse(op.child("FolderShape"));
    let mut out = String::new();
    for r in folder_refs(op.child("FolderIds")) {
        let res = resolve_folder(ctx, r).map(|f| format!("<m:Folders>{}</m:Folders>", folder_xml(ctx, &f, &shape)));
        out.push_str(&message("GetFolder", res));
    }
    Ok(wrap("GetFolder", &out))
}

pub fn find_folder(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let shape = Shape::parse(op.child("FolderShape"));
    let deep = op.attr("Traversal").is_some_and(|t| t == "Deep");
    let mut out = String::new();
    for r in folder_refs(op.child("ParentFolderIds")) {
        let res = resolve_folder(ctx, r).and_then(|parent| {
            let list: Vec<Folder> = subfolders(ctx, &parent, deep)?.into_iter().filter(visible).collect();
            let inner: String = list.iter().map(|f| folder_xml(ctx, f, &shape)).collect();
            Ok(format!(r#"<m:RootFolder TotalItemsInView="{}" IncludesLastItemInRange="true"><t:Folders>{inner}</t:Folders></m:RootFolder>"#, list.len()))
        });
        out.push_str(&message("FindFolder", res));
    }
    Ok(wrap("FindFolder", &out))
}

pub fn create_folder(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let parent = folder_in(ctx, op.child("ParentFolderId")).unwrap_or_else(|| Err(EwsError::folder_not_found()));
    let mut out = String::new();
    for f in op.child("Folders").map(|f| f.children.as_slice()).unwrap_or_default() {
        let res = parent.as_ref().map_err(|e| EwsError::new(e.code, e.message.clone())).and_then(|p| {
            let class = match f.name.as_str() {
                "CalendarFolder" => FolderClass::Calendar,
                "ContactsFolder" => FolderClass::Contacts,
                "TasksFolder" => FolderClass::Tasks,
                _ => f.text_of("FolderClass").map(FolderClass::from_ipf).unwrap_or(FolderClass::Mail),
            };
            let name = f.text_of("DisplayName").unwrap_or_default();
            let created = ctx.app.store.create_folder(p.user_id, Some(p.id), name, class, WRITER).map_err(|e| {
                if e.to_string().contains("already exists") {
                    EwsError::new("ErrorFolderExists", "A folder with the specified name already exists.")
                } else {
                    EwsError::from(e)
                }
            })?;
            Ok(format!("<m:Folders><t:{0}>{1}</t:{0}></m:Folders>", class_element(created.class), super::folder_id(&created)))
        });
        out.push_str(&message("CreateFolder", res));
    }
    Ok(wrap("CreateFolder", &out))
}

pub fn delete_folder(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let mut out = String::new();
    for r in folder_refs(op.child("FolderIds")) {
        let res = resolve_folder(ctx, r).and_then(|f| {
            if f.role.is_some() {
                return Err(EwsError::new("ErrorDeleteDistinguishedFolder", "Distinguished folders cannot be deleted."));
            }
            ctx.app.store.delete_folder(f.id, WRITER)?;
            Ok(String::new())
        });
        out.push_str(&message("DeleteFolder", res));
    }
    Ok(wrap("DeleteFolder", &out))
}

pub fn update_folder(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let mut out = String::new();
    for change in op.child("FolderChanges").map(|c| c.children.as_slice()).unwrap_or_default() {
        let target = change.children.iter().find(|c| c.name == "FolderId" || c.name == "DistinguishedFolderId");
        let res = target.ok_or_else(EwsError::folder_not_found).and_then(|t| resolve_folder(ctx, t)).and_then(|f| {
            let mut name = None;
            if let Some(updates) = change.child("Updates") {
                let mut names = Vec::new();
                updates.find_all("DisplayName", &mut names);
                name = names.first().map(|n| n.text.trim().to_string());
            }
            let f = ctx.app.store.update_folder(f.id, name.as_deref(), None).map_err(|e| EwsError::new("ErrorInvalidOperation", e.to_string()))?;
            Ok(format!("<m:Folders><t:{0}>{1}</t:{0}></m:Folders>", class_element(f.class), super::folder_id(&f)))
        });
        out.push_str(&message("UpdateFolder", res));
    }
    Ok(wrap("UpdateFolder", &out))
}

pub fn move_folder(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let target = folder_in(ctx, op.child("ToFolderId")).unwrap_or_else(|| Err(EwsError::folder_not_found()));
    let mut out = String::new();
    for r in folder_refs(op.child("FolderIds")) {
        let res = target.as_ref().map_err(|e| EwsError::new(e.code, e.message.clone())).and_then(|to| {
            let f = resolve_folder(ctx, r)?;
            if f.user_id != to.user_id {
                return Err(EwsError::new("ErrorMoveCopyFailed", "Folders cannot be moved between mailboxes."));
            }
            let f = ctx.app.store.update_folder(f.id, None, Some(to.id)).map_err(|e| EwsError::new("ErrorMoveCopyFailed", e.to_string()))?;
            Ok(format!("<m:Folders><t:{0}>{1}</t:{0}></m:Folders>", class_element(f.class), super::folder_id(&f)))
        });
        out.push_str(&message("MoveFolder", res));
    }
    Ok(wrap("MoveFolder", &out))
}

pub fn empty_folder(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let sub = op.attr("DeleteSubFolders").is_some_and(|v| v == "true");
    let mut out = String::new();
    for r in folder_refs(op.child("FolderIds")) {
        let res = resolve_folder(ctx, r).and_then(|f| {
            ctx.app.store.empty_folder(f.id, sub, WRITER)?;
            Ok(String::new())
        });
        out.push_str(&message("EmptyFolder", res));
    }
    Ok(wrap("EmptyFolder", &out))
}

pub fn sync_folder_hierarchy(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let shape = Shape::parse(op.child("FolderShape"));
    let owner = match folder_in(ctx, op.child("SyncFolderId")) {
        Some(f) => f?.user_id,
        None => ctx.user.id,
    };
    let since = match op.text_of("SyncState").filter(|s| !s.is_empty()) {
        Some(s) => s.strip_prefix('H').and_then(|m| m.parse().ok()).ok_or_else(|| EwsError::new("ErrorInvalidSyncStateData", "Synchronization state data is corrupt or otherwise invalid."))?,
        None => 0,
    };
    let ch = ctx.app.store.hierarchy_changes(owner, since)?;
    let mut changes = String::new();
    for f in ch.added.iter().filter(|f| visible(f)) {
        let _ = write!(changes, "<t:Create>{}</t:Create>", folder_xml(ctx, f, &shape));
    }
    for f in ch.updated.iter().filter(|f| visible(f)) {
        let _ = write!(changes, "<t:Update>{}</t:Update>", folder_xml(ctx, f, &shape));
    }
    for f in ch.deleted.iter().filter(|f| visible(f)) {
        let _ = write!(changes, r#"<t:Delete><t:FolderId Id="F.{}"/></t:Delete>"#, f.id);
    }
    let inner = format!("<m:SyncState>H{}</m:SyncState><m:IncludesLastFolderInRange>true</m:IncludesLastFolderInRange><m:Changes>{changes}</m:Changes>", ch.modseq);
    Ok(wrap("SyncFolderHierarchy", &message("SyncFolderHierarchy", Ok(inner))))
}

pub fn sync_folder_items(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let shape = Shape::parse(op.child("ItemShape"));
    let res = (|| {
        let folder = folder_in(ctx, op.child("SyncFolderId")).unwrap_or_else(|| Err(EwsError::folder_not_found()))?;
        let max: usize = op.text_of("MaxChangesReturned").and_then(|m| m.parse().ok()).unwrap_or(512).clamp(1, 512);
        let base: i64 = match op.text_of("SyncState").filter(|s| !s.is_empty()) {
            Some(s) => s
                .strip_prefix('S')
                .and_then(|x| x.split_once(':'))
                .filter(|(f, _)| f.parse::<i64>().ok() == Some(folder.id))
                .and_then(|(_, m)| m.parse().ok())
                .ok_or_else(|| EwsError::new("ErrorInvalidSyncStateData", "Synchronization state data is corrupt or otherwise invalid."))?,
            None => 0,
        };
        let current = ctx.app.store.current_modseq(folder.user_id)?;
        let mut changes: Vec<Change> = ctx.app.store.changes(folder.id, base, None)?.into_iter().filter(|c| c.modseq() <= current).collect();
        if base == 0 {
            changes.retain(|c| !matches!(c, Change::Delete { .. }));
        }
        let mut out = String::new();
        let mut count = 0;
        let mut last = base;
        let mut more = false;
        for c in changes {
            if count >= max && c.modseq() != last {
                more = true;
                break;
            }
            last = c.modseq();
            count += 1;
            match c {
                Change::Add(i) => {
                    let _ = write!(out, "<t:Create>{}</t:Create>", item_xml(ctx, &i, &shape, true));
                }
                Change::Update(i) => {
                    let _ = write!(out, "<t:Update>{}</t:Update>", item_xml(ctx, &i, &shape, true));
                }
                Change::Delete { item_id, .. } => {
                    let _ = write!(out, r#"<t:Delete><t:ItemId Id="I.{item_id}"/></t:Delete>"#);
                }
            }
        }
        let state = if more { last } else { current };
        Ok(format!(
            "<m:SyncState>S{}:{state}</m:SyncState><m:IncludesLastItemInRange>{}</m:IncludesLastItemInRange><m:Changes>{out}</m:Changes>",
            folder.id,
            !more
        ))
    })();
    Ok(wrap("SyncFolderItems", &message("SyncFolderItems", res)))
}

// ------------------------------------------------------------ restrictions

fn field_value(ctx: &Ctx, item: &Item, uri: &str) -> Option<String> {
    Some(match uri {
        "item:DateTimeReceived" | "item:DateTimeSent" | "item:DateTimeCreated" | "item:LastModifiedTime" => utc(item.received),
        "item:Subject" => item.subject.clone(),
        "item:ItemClass" => item.message_class.clone(),
        "item:Importance" => item.importance.ews().to_string(),
        "item:HasAttachments" => item.has_attachments.to_string(),
        "item:Categories" => item.categories.join(" "),
        "message:IsRead" => (item.flags & flags::READ != 0).to_string(),
        "message:From" | "message:Sender" => item.sender.as_ref().map(|p| format!("{} {}", p.name, p.email)).unwrap_or_default(),
        "message:InternetMessageId" => item.message_id.clone().unwrap_or_default(),
        "item:Body" | "item:Preview" => item.preview.clone(),
        "calendar:Start" => utc(item.event()?.start),
        "calendar:End" => utc(item.event()?.end),
        "contacts:DisplayName" | "contacts:FileAs" => item.subject.clone(),
        "item:ParentFolderId" => format!("F.{}", item.folder_id),
        _ => {
            let _ = ctx;
            return None;
        }
    })
}

fn constant(n: &Node) -> Option<String> {
    n.find("Constant").and_then(|c| c.attr("Value")).map(str::to_string)
}

fn compare(a: &str, b: &str) -> Ordering {
    // Dates as timestamps, numbers as numbers, everything else as text.
    let ta = parse_time(a, chrono_tz::UTC);
    let tb = parse_time(b, chrono_tz::UTC);
    if let (Some(x), Some(y)) = (ta, tb) {
        return x.cmp(&y);
    }
    if let (Ok(x), Ok(y)) = (a.parse::<f64>(), b.parse::<f64>()) {
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    a.to_lowercase().cmp(&b.to_lowercase())
}

fn matches(ctx: &Ctx, item: &Item, r: &Node) -> bool {
    let uri = || r.children.iter().find(|c| c.name == "FieldURI" || c.name == "IndexedFieldURI").and_then(|f| f.attr("FieldURI")).unwrap_or_default();
    match r.name.as_str() {
        "And" => r.children.iter().all(|c| matches(ctx, item, c)),
        "Or" => r.children.iter().any(|c| matches(ctx, item, c)),
        "Not" => !r.children.iter().any(|c| matches(ctx, item, c)),
        "Exists" => field_value(ctx, item, uri()).is_some_and(|v| !v.is_empty()),
        "Contains" => {
            let (Some(v), Some(c)) = (field_value(ctx, item, uri()), constant(r)) else { return false };
            let ignore = r.attr("ContainmentComparison").is_none_or(|m| m.contains("Ignore") || m == "Loose");
            let (v, c) = if ignore { (v.to_lowercase(), c.to_lowercase()) } else { (v, c) };
            match r.attr("ContainmentMode").unwrap_or("Substring") {
                "FullString" => v == c,
                "Prefixed" => v.starts_with(&c),
                _ => v.contains(&c),
            }
        }
        "IsEqualTo" | "IsNotEqualTo" | "IsGreaterThan" | "IsGreaterThanOrEqualTo" | "IsLessThan" | "IsLessThanOrEqualTo" => {
            let (Some(v), Some(c)) = (field_value(ctx, item, uri()), constant(r)) else { return false };
            let o = compare(&v, &c);
            match r.name.as_str() {
                "IsEqualTo" => o == Ordering::Equal,
                "IsNotEqualTo" => o != Ordering::Equal,
                "IsGreaterThan" => o == Ordering::Greater,
                "IsGreaterThanOrEqualTo" => o != Ordering::Less,
                "IsLessThan" => o == Ordering::Less,
                _ => o != Ordering::Greater,
            }
        }
        _ => true,
    }
}

fn sort_items(ctx: &Ctx, items: &mut [Item], order: Option<&Node>) {
    let Some(fo) = order.and_then(|o| o.child("FieldOrder")) else { return };
    let uri = fo.children.first().and_then(|f| f.attr("FieldURI")).unwrap_or("item:DateTimeReceived").to_string();
    let desc = fo.attr("Order").is_none_or(|o| o == "Descending");
    items.sort_by(|a, b| {
        let o = compare(&field_value(ctx, a, &uri).unwrap_or_default(), &field_value(ctx, b, &uri).unwrap_or_default());
        if desc { o.reverse() } else { o }
    });
}

// -------------------------------------------------------------- FindItem

pub fn find_item(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let shape = Shape::parse(op.child("ItemShape"));
    let mut out = String::new();
    let associated = op.attr("Traversal").is_some_and(|t| t == "Associated");
    for r in folder_refs(op.child("ParentFolderIds")) {
        let res = resolve_folder(ctx, r).and_then(|folder| {
            if associated {
                return Ok(r#"<m:RootFolder TotalItemsInView="0" IncludesLastItemInRange="true"><t:Items/></m:RootFolder>"#.to_string());
            }
            if let Some(view) = op.child("CalendarView") {
                return calendar_view(ctx, &folder, view, &shape);
            }
            let mut items: Vec<Item> = match op.text_of("QueryString").filter(|q| !q.is_empty()) {
                Some(q) => ctx.app.store.search(folder.user_id, &[folder.id], q, 5000)?,
                None => ctx.app.store.items(folder.id, 0, 100_000)?,
            };
            if let Some(restriction) = op.child("Restriction").and_then(|r| r.children.first()) {
                items.retain(|i| matches(ctx, i, restriction));
            }
            if folder.class == FolderClass::Contacts && op.child("SortOrder").is_none() {
                items.sort_by_key(|i| i.subject.to_lowercase());
            }
            sort_items(ctx, &mut items, op.child("SortOrder"));
            let total = items.len();
            let (offset, max) = match op.child("IndexedPageItemView").or(op.child("FractionalPageItemView")).or(op.child("ContactsView")) {
                Some(v) => {
                    let max: usize = v.attr("MaxEntriesReturned").and_then(|m| m.parse().ok()).unwrap_or(total);
                    let off: usize = v.attr("Offset").and_then(|m| m.parse().ok()).unwrap_or(0);
                    let off = if v.attr("BasePoint") == Some("End") { total.saturating_sub(off + max) } else { off };
                    (off, max)
                }
                None => (0, total),
            };
            let page: Vec<&Item> = items.iter().skip(offset).take(max).collect();
            let inner: String = page.iter().map(|i| item_xml(ctx, i, &shape, true)).collect();
            let next = offset + page.len();
            Ok(format!(
                r#"<m:RootFolder IndexedPagingOffset="{next}" TotalItemsInView="{total}" IncludesLastItemInRange="{}"><t:Items>{inner}</t:Items></m:RootFolder>"#,
                next >= total
            ))
        });
        out.push_str(&message("FindItem", res));
    }
    Ok(wrap("FindItem", &out))
}

fn calendar_view(ctx: &Ctx, folder: &Folder, view: &Node, shape: &Shape) -> EwsResult<String> {
    let from = view.attr("StartDate").and_then(|s| parse_time(s, ctx.tz)).ok_or_else(|| EwsError::new("ErrorInvalidRequest", "CalendarView needs StartDate"))?;
    let to = view.attr("EndDate").and_then(|s| parse_time(s, ctx.tz)).ok_or_else(|| EwsError::new("ErrorInvalidRequest", "CalendarView needs EndDate"))?;
    if to - from > 2 * 366 * 86_400 {
        return Err(EwsError::new("ErrorCalendarViewRangeTooBig", "The time range of a calendar view may be at most two years."));
    }
    let max: usize = view.attr("MaxEntriesReturned").and_then(|m| m.parse().ok()).unwrap_or(1000);
    let mut entries: Vec<(i64, String)> = Vec::new();
    for item in ctx.app.store.events_in_range(folder.id, from, to)? {
        let Some(e) = item.event() else { continue };
        if e.recurrence.is_some() {
            for o in e.occurrences(from, to) {
                entries.push((o.start, event_xml(ctx, &item, &e, Some(&o), shape, true)));
            }
        } else if e.start < to && (e.end > from || e.start >= from) {
            entries.push((e.start, event_xml(ctx, &item, &e, None, shape, true)));
        }
    }
    entries.sort_by_key(|(s, _)| *s);
    let total = entries.len();
    let inner: String = entries.into_iter().take(max).map(|(_, x)| x).collect();
    Ok(format!(r#"<m:RootFolder TotalItemsInView="{total}" IncludesLastItemInRange="{}"><t:Items>{inner}</t:Items></m:RootFolder>"#, total <= max))
}

// --------------------------------------------------------------- GetItem

fn occurrence_of(e: &Event, original: i64) -> Option<Occurrence> {
    let span = (e.end - e.start).max(0);
    e.occurrences(original - 400 * 86_400, original + span + 400 * 86_400).into_iter().find(|o| o.original_start == original)
}

pub fn render_ref(ctx: &Ctx, r: ItemRef, shape: &Shape) -> EwsResult<String> {
    let item = load_item(ctx, r)?;
    match r {
        ItemRef::Occurrence(_, original) => {
            let e = item.event().ok_or_else(EwsError::not_found)?;
            let o = occurrence_of(&e, original).ok_or_else(|| EwsError::new("ErrorItemNotFound", "The occurrence does not exist."))?;
            Ok(event_xml(ctx, &item, &e, Some(&o), shape, false))
        }
        ItemRef::Item(_) => Ok(item_xml(ctx, &item, shape, false)),
    }
}

pub fn get_item(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let shape = Shape::parse(op.child("ItemShape"));
    let mut out = String::new();
    for r in item_refs(op.child("ItemIds")) {
        let res = item_ref(ctx, r).and_then(|r| render_ref(ctx, r, &shape)).map(|x| format!("<m:Items>{x}</m:Items>"));
        out.push_str(&message("GetItem", res));
    }
    Ok(wrap("GetItem", &out))
}

// ------------------------------------------------------------ CreateItem

fn id_only(kind: &str, id: i64, modseq: i64) -> String {
    format!("<t:{kind}>{}</t:{kind}>", item_id_xml(id, modseq))
}

fn saved_folder(ctx: &Ctx, op: &Node, role: &str) -> EwsResult<Folder> {
    match folder_in(ctx, op.child("SavedItemFolderId")) {
        Some(f) => f,
        None => Ok(ctx.app.store.ensure_folder(ctx.user.id, role)?),
    }
}

/// PR_MESSAGE_FLAGS (0x0E07) in a message element: (read, draft).
fn message_flag_prop(n: &Node) -> Option<(bool, bool)> {
    n.children_named("ExtendedProperty").find_map(|p| {
        let tag = p.child("ExtendedFieldURI")?.attr("PropertyTag")?;
        let t = tag.trim().to_ascii_lowercase();
        let num = t.strip_prefix("0x").map(|h| u32::from_str_radix(h, 16).ok()).unwrap_or_else(|| t.parse().ok())?;
        if num != 0x0E07 {
            return None;
        }
        let v: i64 = p.text_of("Value")?.parse().ok()?;
        Some((v & 1 != 0, v & 8 != 0))
    })
}

fn sends(disposition: &str) -> bool {
    matches!(disposition, "SendOnly" | "SendAndSaveCopy")
}

fn invitations_requested(v: Option<&str>) -> bool {
    matches!(v, Some("SendOnlyToAll" | "SendToAllAndSaveCopy" | "SendOnlyToChanged" | "SendToChangedAndSaveCopy"))
}

fn raw_message(ctx: &Ctx, n: &Node) -> EwsResult<Vec<u8>> {
    match n.text_of("MimeContent") {
        Some(b64) => B64.decode(b64.split_whitespace().collect::<String>()).map_err(|_| EwsError::new("ErrorMimeContentInvalid", "The MIME content is not valid base64.")),
        None => Ok(read::outgoing_from_fields(n, ctx.user.person()).build(ctx.app.hostname())),
    }
}

/// Recipients of a Message element that are not part of the MIME (Bcc).
fn extra_bcc(n: &Node) -> Vec<String> {
    read::persons(n.child("BccRecipients")).into_iter().map(|p| p.email).collect()
}

async fn create_one(ctx: &Ctx, op: &Node, n: &Node) -> EwsResult<String> {
    let disposition = op.attr("MessageDisposition").unwrap_or("SaveOnly");
    let store = &ctx.app.store;
    match n.name.as_str() {
        "Message" => {
            let raw = raw_message(ctx, n)?;
            let (read_flag, draft) = message_flag_prop(n).unwrap_or((false, !sends(disposition)));
            if sends(disposition) {
                let mut recipients = mime::recipients(&raw);
                if n.text_of("MimeContent").is_some() {
                    for b in extra_bcc(n) {
                        if !recipients.contains(&b) {
                            recipients.push(b);
                        }
                    }
                }
                submit::send(&ctx.app, &ctx.user, raw, submit::Options { recipients: Some(recipients), save_copy: disposition == "SendAndSaveCopy", ..Default::default() })
                    .await
                    .map_err(|e| EwsError::new("ErrorMessageSubmissionBlocked", e.to_string()))?;
                return Ok(String::new());
            }
            let folder = saved_folder(ctx, op, "drafts")?;
            let mut f = 0;
            if read_flag || draft {
                f |= flags::READ;
            }
            if draft {
                f |= flags::DRAFT;
            }
            let raw = mime::complete_headers(&raw, ctx.app.hostname());
            let id = store.insert_mail(NewMail { user_id: folder.user_id, folder_id: folder.id, mime: &raw, flags: f, received: now(), writer: "ews" })?;
            let item = store.item(id)?.ok_or_else(EwsError::not_found)?;
            Ok(id_only("Message", id, item.modseq))
        }
        "CalendarItem" => {
            let folder = saved_folder(ctx, op, "calendar")?;
            let mut e = read::apply_event(n, Event { tz: ctx.tz.name().to_string(), dtstamp: now(), ..Default::default() }, ctx.tz);
            if !e.attendees.is_empty() && e.organizer.is_none() {
                e.organizer = Some(ctx.user.person());
                e.response = Response::Organizer;
            }
            let id = store.insert_pim(folder.user_id, folder.id, ItemKind::Event, &serde_json::to_string(&e).map_err(anyhow::Error::from)?, Some(&e.uid), WRITER)?;
            if !e.attendees.is_empty() && invitations_requested(op.attr("SendMeetingInvitations")) {
                calendar::send_invitations(&ctx.app, &ctx.user, &e, "REQUEST", None, None).await?;
            }
            let item = store.item(id)?.ok_or_else(EwsError::not_found)?;
            Ok(id_only("CalendarItem", id, item.modseq))
        }
        "Contact" => {
            let folder = saved_folder(ctx, op, "contacts")?;
            let c = read::apply_contact(n, Default::default());
            let id = store.insert_pim(folder.user_id, folder.id, ItemKind::Contact, &serde_json::to_string(&c).map_err(anyhow::Error::from)?, None, WRITER)?;
            let item = store.item(id)?.ok_or_else(EwsError::not_found)?;
            Ok(id_only("Contact", id, item.modseq))
        }
        "Task" => {
            let folder = saved_folder(ctx, op, "tasks")?;
            let t = read::apply_task(n, Default::default(), ctx.tz);
            let id = store.insert_pim(folder.user_id, folder.id, ItemKind::Task, &serde_json::to_string(&t).map_err(anyhow::Error::from)?, None, WRITER)?;
            let item = store.item(id)?.ok_or_else(EwsError::not_found)?;
            Ok(id_only("Task", id, item.modseq))
        }
        "AcceptItem" | "TentativelyAcceptItem" | "DeclineItem" => {
            let r = n.child("ReferenceItemId").ok_or_else(EwsError::not_found).and_then(|x| item_ref(ctx, x))?;
            let item = load_item(ctx, r)?;
            let event = match item.kind {
                ItemKind::Event => item,
                ItemKind::Mail => calendar::event_for_request(&ctx.app, &ctx.user, &item).map_err(|e| EwsError::new("ErrorItemNotFound", e.to_string()))?,
                _ => return Err(EwsError::not_found()),
            };
            let answer = match n.name.as_str() {
                "AcceptItem" => Response::Accepted,
                "TentativelyAcceptItem" => Response::Tentative,
                _ => Response::Declined,
            };
            let occurrence = match r {
                ItemRef::Occurrence(_, o) => Some(o),
                _ => None,
            };
            let comment = n.child("Body").map(|b| mime::html_to_text(&b.text)).unwrap_or_default();
            calendar::respond(&ctx.app, &ctx.user, event.id, answer, occurrence, disposition != "SaveOnly", &comment).await?;
            Ok(String::new())
        }
        "CancelCalendarItem" => {
            let r = n.child("ReferenceItemId").ok_or_else(EwsError::not_found).and_then(|x| item_ref(ctx, x))?;
            let item = load_item(ctx, r)?;
            let e = item.event().ok_or_else(EwsError::not_found)?;
            calendar::send_invitations(&ctx.app, &ctx.user, &e, "CANCEL", None, None).await?;
            store.delete_item(item.id, WRITER)?;
            Ok(String::new())
        }
        "ReplyToItem" | "ReplyAllToItem" | "ForwardItem" => {
            let r = n.child("ReferenceItemId").ok_or_else(EwsError::not_found).and_then(|x| item_ref(ctx, x))?;
            let original = load_item(ctx, r)?;
            let orig_raw = store.mime(original.id)?.unwrap_or_default();
            let forward = n.name == "ForwardItem";
            let me = store.send_as_addresses(ctx.user.id)?;
            let mut to: Vec<Person> = read::persons(n.child("ToRecipients"));
            let mut cc: Vec<Person> = read::persons(n.child("CcRecipients"));
            if !forward && to.is_empty() {
                let reply_to = if original.recipients.reply_to.is_empty() { original.sender.clone().into_iter().collect() } else { original.recipients.reply_to.clone() };
                to = reply_to;
                if n.name == "ReplyAllToItem" {
                    to.extend(original.recipients.to.iter().filter(|p| !me.iter().any(|m| m.eq_ignore_ascii_case(&p.email))).cloned());
                    cc.extend(original.recipients.cc.iter().filter(|p| !me.iter().any(|m| m.eq_ignore_ascii_case(&p.email))).cloned());
                }
            }
            let prefix = if forward { "FW: " } else { "RE: " };
            let subject = n.raw_text_of("Subject").map(str::to_string).unwrap_or_else(|| format!("{prefix}{}", original.subject));
            let body = n.child("NewBodyContent").or_else(|| n.child("Body"));
            let (text, html) = match body {
                Some(b) if b.attr("BodyType") == Some("HTML") => (mime::html_to_text(&b.text), Some(b.text.clone())),
                Some(b) => (b.text.clone(), None),
                None => (String::new(), None),
            };
            let new = mime::Outgoing {
                from: ctx.user.person(),
                to,
                cc,
                subject,
                text,
                html,
                in_reply_to: original.message_id.clone(),
                ..Default::default()
            }
            .build(ctx.app.hostname());
            let combined = mime::combine_with_original(&new, &orig_raw, forward);
            if sends(disposition) {
                submit::send(&ctx.app, &ctx.user, combined, submit::Options { save_copy: disposition == "SendAndSaveCopy", ..Default::default() })
                    .await
                    .map_err(|e| EwsError::new("ErrorMessageSubmissionBlocked", e.to_string()))?;
                store.update_flags(original.id, if forward { flags::FORWARDED } else { flags::ANSWERED }, 0, None, WRITER)?;
                Ok(String::new())
            } else {
                let drafts = saved_folder(ctx, op, "drafts")?;
                let id = store.insert_mail(NewMail { user_id: drafts.user_id, folder_id: drafts.id, mime: &combined, flags: flags::READ | flags::DRAFT, received: now(), writer: WRITER })?;
                let item = store.item(id)?.ok_or_else(EwsError::not_found)?;
                Ok(id_only("Message", id, item.modseq))
            }
        }
        other => Err(EwsError::new("ErrorInvalidRequest", format!("Creating {other} items is not supported."))),
    }
}

pub async fn create_item(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let mut out = String::new();
    for n in op.child("Items").map(|i| i.children.as_slice()).unwrap_or_default() {
        let res = create_one(ctx, op, n).await.map(|x| if x.is_empty() { "<m:Items/>".to_string() } else { format!("<m:Items>{x}</m:Items>") });
        out.push_str(&message("CreateItem", res));
    }
    Ok(wrap("CreateItem", &out))
}

// ------------------------------------------------------------ UpdateItem

/// All item-field values of the Set/Append updates merged into one element.
fn merged_updates(updates: &Node) -> (Node, Vec<(String, Option<String>)>) {
    let mut merged = Node { name: "Item".into(), ..Default::default() };
    let mut deletes = Vec::new();
    for u in &updates.children {
        let uri_node = u.children.iter().find(|c| matches!(c.name.as_str(), "FieldURI" | "IndexedFieldURI" | "ExtendedFieldURI"));
        match u.name.as_str() {
            "SetItemField" | "AppendToItemField" => {
                if let Some(item) = u.children.iter().find(|c| !matches!(c.name.as_str(), "FieldURI" | "IndexedFieldURI" | "ExtendedFieldURI")) {
                    for c in &item.children {
                        // Indexed entries (e-mail addresses, phones) accumulate.
                        match merged.children.iter_mut().find(|m| m.name == c.name && matches!(c.name.as_str(), "EmailAddresses" | "PhoneNumbers" | "PhysicalAddresses" | "ImAddresses")) {
                            Some(m) => m.children.extend(c.children.clone()),
                            None => {
                                merged.children.retain(|m| m.name != c.name || c.name == "ExtendedProperty");
                                merged.children.push(c.clone());
                            }
                        }
                    }
                }
            }
            "DeleteItemField" => {
                if let Some(f) = uri_node {
                    deletes.push((f.attr("FieldURI").unwrap_or_default().to_string(), f.attr("FieldIndex").map(str::to_string)));
                }
            }
            _ => {}
        }
    }
    (merged, deletes)
}

async fn update_one(ctx: &Ctx, op: &Node, change: &Node) -> EwsResult<String> {
    let store = &ctx.app.store;
    let target = change.children.iter().find(|c| matches!(c.name.as_str(), "ItemId" | "OccurrenceItemId" | "RecurringMasterItemId")).ok_or_else(EwsError::not_found)?;
    let r = item_ref(ctx, target)?;
    let item = load_item(ctx, r)?;
    let (merged, deletes) = merged_updates(change.child("Updates").ok_or_else(|| EwsError::new("ErrorInvalidRequest", "ItemChange without Updates"))?);
    let disposition = op.attr("MessageDisposition").unwrap_or("SaveOnly");
    match item.kind {
        ItemKind::Mail | ItemKind::Note => {
            let (mut set, mut clear, mut cats) = read::message_flags(&merged);
            if let Some((r, _)) = message_flag_prop(&merged) {
                if r {
                    set |= flags::READ;
                } else {
                    clear |= flags::READ;
                }
            }
            if deletes.iter().any(|(u, _)| u == "item:Categories") {
                cats = Some(vec![]);
            }
            if deletes.iter().any(|(u, _)| u == "item:Flag") {
                clear |= flags::FLAGGED | flags::COMPLETE;
            }
            store.update_flags(item.id, set, clear, cats.as_deref(), WRITER)?;
            let content_change = ["Subject", "Body", "ToRecipients", "CcRecipients", "BccRecipients", "Importance", "MimeContent"].iter().any(|f| merged.child(f).is_some());
            if content_change && item.flags & flags::DRAFT != 0 {
                let raw = match merged.text_of("MimeContent") {
                    Some(b64) => B64.decode(b64.split_whitespace().collect::<String>()).map_err(|_| EwsError::new("ErrorMimeContentInvalid", "Invalid MIME content"))?,
                    None => rebuild_draft(ctx, &item, &merged)?,
                };
                store.replace_mime(item.id, &raw, WRITER)?;
            }
            if sends(disposition) {
                let raw = store.mime(item.id)?.unwrap_or_default();
                submit::send(&ctx.app, &ctx.user, raw, submit::Options { save_copy: disposition == "SendAndSaveCopy", ..Default::default() })
                    .await
                    .map_err(|e| EwsError::new("ErrorMessageSubmissionBlocked", e.to_string()))?;
                store.delete_item(item.id, WRITER)?;
                return Ok(String::new());
            }
            let fresh = store.item(item.id)?.ok_or_else(EwsError::not_found)?;
            Ok(id_only("Message", fresh.id, fresh.modseq))
        }
        ItemKind::Event => {
            let mut e = item.event().ok_or_else(EwsError::not_found)?;
            let occurrence = match r {
                ItemRef::Occurrence(_, original) => {
                    let o = occurrence_of(&e, original).ok_or_else(EwsError::not_found)?;
                    // Apply onto a single event shaped like the occurrence, then store the differences.
                    let mut one = e.clone();
                    one.recurrence = None;
                    one.exceptions.clear();
                    one.start = o.start;
                    one.end = o.end;
                    if let Some(x) = &o.exception {
                        one.subject = x.subject.clone().unwrap_or(one.subject);
                        one.location = x.location.clone().unwrap_or(one.location);
                        one.body = x.body.clone().unwrap_or(one.body);
                        one.busy = x.busy.unwrap_or(one.busy);
                        one.reminder = x.reminder.unwrap_or(one.reminder);
                    }
                    let changed = read::apply_event(&merged, one, e.zone());
                    let ex = e.exception_mut(original);
                    ex.start = Some(changed.start);
                    ex.end = Some(changed.end);
                    ex.subject = Some(changed.subject.clone());
                    ex.location = Some(changed.location.clone());
                    ex.body = Some(changed.body.clone());
                    ex.busy = Some(changed.busy);
                    ex.reminder = Some(changed.reminder);
                    Some(original)
                }
                ItemRef::Item(_) => {
                    let old_rule = e.recurrence.clone();
                    e = read::apply_event(&merged, e, ctx.tz);
                    for (uri, _) in &deletes {
                        read::delete_event_field(&mut e, uri);
                    }
                    if e.recurrence != old_rule {
                        e.exceptions.retain(|x| x.deleted);
                    }
                    None
                }
            };
            let me = store.send_as_addresses(ctx.user.id)?;
            let organizer = e.organizer.as_ref().is_none_or(|o| me.iter().any(|m| m.eq_ignore_ascii_case(&o.email)));
            let notify = organizer && !e.attendees.is_empty() && invitations_requested(op.attr("SendMeetingInvitationsOrCancellations"));
            if notify {
                e.sequence += 1;
                if e.organizer.is_none() {
                    e.organizer = Some(ctx.user.person());
                }
            }
            store.update_pim(item.id, &serde_json::to_string(&e).map_err(anyhow::Error::from)?, WRITER)?;
            if notify {
                calendar::send_invitations(&ctx.app, &ctx.user, &e, "REQUEST", None, occurrence).await?;
            }
            let fresh = store.item(item.id)?.ok_or_else(EwsError::not_found)?;
            Ok(match occurrence {
                Some(o) => format!(r#"<t:CalendarItem><t:ItemId Id="O.{}.{o}" ChangeKey="C{}"/></t:CalendarItem>"#, fresh.id, fresh.modseq),
                None => id_only("CalendarItem", fresh.id, fresh.modseq),
            })
        }
        ItemKind::Contact => {
            let mut c = read::apply_contact(&merged, item.contact().unwrap_or_default());
            for (uri, index) in &deletes {
                read::delete_contact_field(&mut c, uri, index.as_deref());
            }
            store.update_pim(item.id, &serde_json::to_string(&c).map_err(anyhow::Error::from)?, WRITER)?;
            let fresh = store.item(item.id)?.ok_or_else(EwsError::not_found)?;
            Ok(id_only("Contact", fresh.id, fresh.modseq))
        }
        ItemKind::Task => {
            let mut t = read::apply_task(&merged, item.task().unwrap_or_default(), ctx.tz);
            for (uri, _) in &deletes {
                match uri.as_str() {
                    "task:DueDate" => t.due = None,
                    "task:StartDate" => t.start = None,
                    "item:Body" => t.body.clear(),
                    "item:Categories" => t.categories.clear(),
                    "item:ReminderIsSet" | "item:ReminderDueBy" => t.reminder = None,
                    _ => {}
                }
            }
            store.update_pim(item.id, &serde_json::to_string(&t).map_err(anyhow::Error::from)?, WRITER)?;
            let fresh = store.item(item.id)?.ok_or_else(EwsError::not_found)?;
            Ok(id_only("Task", fresh.id, fresh.modseq))
        }
    }
}

/// New MIME for a draft whose fields changed (attachments are kept).
fn rebuild_draft(ctx: &Ctx, item: &Item, merged: &Node) -> EwsResult<Vec<u8>> {
    let raw = ctx.app.store.mime(item.id)?.unwrap_or_default();
    let meta = mime::parse_meta(&raw);
    let (text, html) = mime::bodies(&raw);
    let fields = read::outgoing_from_fields(merged, meta.from.clone().unwrap_or_else(|| ctx.user.person()));
    let files = mime::attachments(&raw)
        .into_iter()
        .filter_map(|a| mime::attachment_data(&raw, a.index))
        .map(|(info, data)| mime::File { name: info.name, content_type: info.content_type, data, content_id: info.content_id, inline: info.inline })
        .collect();
    let mut headers = fields.headers.clone();
    if merged.child("BccRecipients").is_none() && !meta.bcc.is_empty() {
        headers.push(("Bcc".into(), mime::format_list(&meta.bcc)));
    }
    let out = mime::Outgoing {
        from: fields.from,
        to: if merged.child("ToRecipients").is_some() { fields.to } else { meta.to },
        cc: if merged.child("CcRecipients").is_some() { fields.cc } else { meta.cc },
        subject: if merged.child("Subject").is_some() { fields.subject } else { meta.subject },
        text: if merged.child("Body").is_some() { fields.text } else { text.unwrap_or_default() },
        html: if merged.child("Body").is_some() { fields.html } else { html },
        headers,
        files,
        ..Default::default()
    };
    Ok(out.build(ctx.app.hostname()))
}

pub async fn update_item(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let mut out = String::new();
    for change in op.child("ItemChanges").map(|c| c.children.as_slice()).unwrap_or_default() {
        let res = update_one(ctx, op, change)
            .await
            .map(|x| format!("<m:Items>{x}</m:Items><m:ConflictResults><t:Count>0</t:Count></m:ConflictResults>"));
        out.push_str(&message("UpdateItem", res));
    }
    Ok(wrap("UpdateItem", &out))
}

// ------------------------------------------------------------ DeleteItem

pub async fn delete_item(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let delete_type = op.attr("DeleteType").unwrap_or("MoveToDeletedItems");
    let cancel = invitations_requested(op.attr("SendMeetingCancellations"));
    let store = &ctx.app.store;
    let mut out = String::new();
    for r in item_refs(op.child("ItemIds")) {
        let res: EwsResult<String> = async {
            let r = item_ref(ctx, r)?;
            let item = load_item(ctx, r)?;
            if item.kind == ItemKind::Event {
                let mut e = item.event().ok_or_else(EwsError::not_found)?;
                let me = store.send_as_addresses(ctx.user.id)?;
                let organizer = e.organizer.as_ref().is_none_or(|o| me.iter().any(|m| m.eq_ignore_ascii_case(&o.email)));
                if let ItemRef::Occurrence(_, original) = r {
                    e.exception_mut(original).deleted = true;
                    store.update_pim(item.id, &serde_json::to_string(&e).map_err(anyhow::Error::from)?, WRITER)?;
                    if cancel && organizer && !e.attendees.is_empty() {
                        calendar::send_invitations(&ctx.app, &ctx.user, &e, "CANCEL", None, Some(original)).await?;
                    }
                    return Ok(String::new());
                }
                if cancel && organizer && !e.attendees.is_empty() && !e.cancelled {
                    calendar::send_invitations(&ctx.app, &ctx.user, &e, "CANCEL", None, None).await?;
                }
                store.delete_item(item.id, WRITER)?;
                return Ok(String::new());
            }
            let folder = store.folder(item.folder_id)?;
            if delete_type == "MoveToDeletedItems" && item.kind == ItemKind::Mail && folder.as_ref().and_then(|f| f.role.as_deref()) != Some("deleteditems") {
                let trash = store.ensure_folder(item.user_id, "deleteditems")?;
                store.move_item(item.id, trash.id, WRITER)?;
            } else {
                store.delete_item(item.id, WRITER)?;
            }
            Ok(String::new())
        }
        .await;
        out.push_str(&message("DeleteItem", res));
    }
    Ok(wrap("DeleteItem", &out))
}

pub fn move_or_copy(ctx: &Ctx, op: &Node, copy: bool) -> EwsResult<String> {
    let name = if copy { "CopyItem" } else { "MoveItem" };
    let target = folder_in(ctx, op.child("ToFolderId")).unwrap_or_else(|| Err(EwsError::folder_not_found()));
    let store = &ctx.app.store;
    let mut out = String::new();
    for r in item_refs(op.child("ItemIds")) {
        let res = target.as_ref().map_err(|e| EwsError::new(e.code, e.message.clone())).and_then(|to| {
            let item = load_item(ctx, item_ref(ctx, r)?)?;
            let new_id = if copy || item.user_id != to.user_id {
                let id = store.copy_item(item.id, to.id, WRITER)?;
                if !copy {
                    store.delete_item(item.id, WRITER)?;
                }
                id
            } else {
                store.move_item(item.id, to.id, WRITER)?;
                item.id
            };
            let fresh = store.item(new_id)?.ok_or_else(EwsError::not_found)?;
            let el = match fresh.kind {
                ItemKind::Event => "CalendarItem",
                ItemKind::Contact => "Contact",
                ItemKind::Task => "Task",
                _ => "Message",
            };
            Ok(format!("<m:Items>{}</m:Items>", id_only(el, fresh.id, fresh.modseq)))
        });
        out.push_str(&message(name, res));
    }
    Ok(wrap(name, &out))
}

pub async fn send_item(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let save = op.attr("SaveItemToFolder").is_none_or(|v| v == "true");
    let mut out = String::new();
    for r in item_refs(op.child("ItemIds")) {
        let res: EwsResult<String> = async {
            let item = load_item(ctx, item_ref(ctx, r)?)?;
            let raw = ctx.app.store.mime(item.id)?.ok_or_else(EwsError::not_found)?;
            submit::send(&ctx.app, &ctx.user, raw, submit::Options { save_copy: save, ..Default::default() })
                .await
                .map_err(|e| EwsError::new("ErrorMessageSubmissionBlocked", e.to_string()))?;
            ctx.app.store.delete_item(item.id, WRITER)?;
            Ok(String::new())
        }
        .await;
        out.push_str(&message("SendItem", res));
    }
    Ok(wrap("SendItem", &out))
}

pub fn mark_all_read(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let read = op.text_of("ReadFlag").is_none_or(|v| v == "true");
    let mut out = String::new();
    for r in folder_refs(op.child("FolderIds")) {
        let res = resolve_folder(ctx, r).and_then(|f| {
            for item in ctx.app.store.items(f.id, 0, 1_000_000)? {
                let (set, clear) = if read { (flags::READ, 0) } else { (0, flags::READ) };
                ctx.app.store.update_flags(item.id, set, clear, None, WRITER)?;
            }
            Ok(String::new())
        });
        out.push_str(&message("MarkAllItemsAsRead", res));
    }
    Ok(wrap("MarkAllItemsAsRead", &out))
}

