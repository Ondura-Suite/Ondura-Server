//! Attachments, name resolution, out of office, availability, rooms,
//! notifications (streaming and pull) and smaller operations.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, NaiveDateTime, TimeZone};
use chrono_tz::Tz;

use super::read::parse_time;
use super::write::{self, attachment_xml, busy_type, utc};
use super::{Ctx, EwsError, EwsResult, NS_M, NS_T, envelope, item_ref, load_item, message, push, resolve_folder, wrap};
use crate::calendar;
use crate::directory;
use crate::mime;
use crate::model::{FolderClass, ItemKind};
use crate::store::{OofSettings, now};
use crate::tzinfo::{self, Transition, TzRule};
use crate::xml::{Node, escape};

// ------------------------------------------------------------ attachments

fn parse_attachment_id(id: &str) -> Option<(i64, usize)> {
    let mut p = id.split('.');
    if p.next()? != "A" {
        return None;
    }
    Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?))
}

pub fn get_attachment(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let mut out = String::new();
    for a in op.child("AttachmentIds").map(|x| x.children.as_slice()).unwrap_or_default() {
        let res = (|| {
            let (item_id, index) = a.attr("Id").and_then(parse_attachment_id).ok_or_else(|| EwsError::new("ErrorInvalidIdMalformed", "Invalid attachment id."))?;
            let item = load_item(ctx, super::ItemRef::Item(item_id))?;
            let raw = ctx.app.store.mime(item.id)?.ok_or_else(EwsError::not_found)?;
            let (info, data) = mime::attachment_data(&raw, index).ok_or_else(|| EwsError::new("ErrorAttachmentNotFound", "The attachment was not found."))?;
            Ok(format!("<m:Attachments>{}</m:Attachments>", attachment_xml(item.id, &info, Some(&data))))
        })();
        out.push_str(&message("GetAttachment", res));
    }
    Ok(wrap("GetAttachment", &out))
}

pub fn create_attachment(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let res = (|| {
        let parent = op.child("ParentItemId").ok_or_else(EwsError::not_found).and_then(|p| item_ref(ctx, p))?;
        let item = load_item(ctx, parent)?;
        if item.kind != ItemKind::Mail {
            return Err(EwsError::new("ErrorInvalidRequest", "Attachments are only supported on messages."));
        }
        let raw = ctx.app.store.mime(item.id)?.unwrap_or_default();
        let mut files = mime::files_of(&raw);
        let first_new = files.len();
        for a in op.child("Attachments").map(|x| x.children.as_slice()).unwrap_or_default() {
            if a.name != "FileAttachment" {
                return Err(EwsError::new("ErrorInvalidRequest", "Only file attachments are supported."));
            }
            files.push(mime::File {
                name: a.text_of("Name").unwrap_or("attachment").to_string(),
                content_type: a.text_of("ContentType").unwrap_or("application/octet-stream").to_string(),
                data: B64.decode(a.text_of("Content").unwrap_or_default().split_whitespace().collect::<String>()).map_err(|_| EwsError::new("ErrorInvalidRequest", "Invalid attachment content."))?,
                content_id: a.text_of("ContentId").map(str::to_string),
                inline: a.text_of("IsInline") == Some("true"),
            });
        }
        let count = files.len();
        let new_raw = mime::with_files(&raw, files, ctx.app.hostname());
        ctx.app.store.replace_mime(item.id, &new_raw, "ews")?;
        let fresh = ctx.app.store.item(item.id)?.ok_or_else(EwsError::not_found)?;
        let mut s = String::from("<m:Attachments>");
        for i in first_new..count {
            let _ = write!(s, r#"<t:FileAttachment><t:AttachmentId Id="A.{0}.{i}" RootItemId="I.{0}" RootItemChangeKey="C{1}"/></t:FileAttachment>"#, fresh.id, fresh.modseq);
        }
        s.push_str("</m:Attachments>");
        Ok(s)
    })();
    Ok(wrap("CreateAttachment", &message("CreateAttachment", res)))
}

pub fn delete_attachment(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let mut out = String::new();
    for a in op.child("AttachmentIds").map(|x| x.children.as_slice()).unwrap_or_default() {
        let res = (|| {
            let (item_id, index) = a.attr("Id").and_then(parse_attachment_id).ok_or_else(|| EwsError::new("ErrorInvalidIdMalformed", "Invalid attachment id."))?;
            let item = load_item(ctx, super::ItemRef::Item(item_id))?;
            let raw = ctx.app.store.mime(item.id)?.unwrap_or_default();
            let mut files = mime::files_of(&raw);
            if index >= files.len() {
                return Err(EwsError::new("ErrorAttachmentNotFound", "The attachment was not found."));
            }
            files.remove(index);
            let new_raw = mime::with_files(&raw, files, ctx.app.hostname());
            ctx.app.store.replace_mime(item.id, &new_raw, "ews")?;
            let fresh = ctx.app.store.item(item.id)?.ok_or_else(EwsError::not_found)?;
            Ok(format!(r#"<m:RootItemId RootItemId="I.{}" RootItemChangeKey="C{}"/>"#, fresh.id, fresh.modseq))
        })();
        out.push_str(&message("DeleteAttachment", res));
    }
    Ok(wrap("DeleteAttachment", &out))
}

// ---------------------------------------------------------- ResolveNames

pub fn resolve_names(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let query = op.text_of("UnresolvedEntry").unwrap_or_default().trim_start_matches("SMTP:").trim_start_matches("smtp:");
    let full = op.attr("ReturnFullContactData").is_some_and(|v| v == "true");
    let mut resolutions: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let users = match directory::resolve(&ctx.app, query) {
        Some(u) => vec![u],
        None => directory::search(&ctx.app, query, 100),
    };
    for u in users {
        if !seen.insert(u.email.clone()) {
            continue;
        }
        let mut r = String::from("<t:Resolution>");
        r.push_str(&write::mailbox(&u.person(), "Mailbox"));
        if full {
            let (first, last) = directory::split_name(&u.name);
            r.push_str("<t:Contact>");
            push(&mut r, "DisplayName", if u.name.is_empty() { &u.email } else { &u.name });
            push(&mut r, "GivenName", &first);
            let _ = write!(r, "<t:EmailAddresses><t:Entry Key=\"EmailAddress1\">SMTP:{}</t:Entry></t:EmailAddresses>", escape(&u.email));
            push(&mut r, "Surname", &last);
            push(&mut r, "ContactSource", "ActiveDirectory");
            r.push_str("</t:Contact>");
        }
        r.push_str("</t:Resolution>");
        resolutions.push(r);
    }
    // Own contacts.
    let contact_folders: Vec<i64> = ctx.app.store.folders(ctx.user.id)?.into_iter().filter(|f| f.class == FolderClass::Contacts).map(|f| f.id).collect();
    if !query.is_empty() && !contact_folders.is_empty() {
        for item in ctx.app.store.search(ctx.user.id, &contact_folders, query, 50)? {
            let Some(c) = item.contact() else { continue };
            for email in c.emails.iter().take(1) {
                if !seen.insert(email.to_ascii_lowercase()) {
                    continue;
                }
                let mut r = String::from("<t:Resolution>");
                let p = crate::model::Person::new(c.effective_name(), email.clone());
                r.push_str(&write::mailbox(&p, "Contact").replace("</t:Mailbox>", &format!(r#"<t:ItemId Id="I.{}"/></t:Mailbox>"#, item.id)));
                if full {
                    r.push_str("<t:Contact>");
                    push(&mut r, "DisplayName", &c.effective_name());
                    push(&mut r, "GivenName", &c.first_name);
                    let _ = write!(r, "<t:EmailAddresses><t:Entry Key=\"EmailAddress1\">SMTP:{}</t:Entry></t:EmailAddresses>", escape(email));
                    push(&mut r, "JobTitle", &c.job_title);
                    push(&mut r, "Department", &c.department);
                    push(&mut r, "Surname", &c.last_name);
                    push(&mut r, "ContactSource", "Store");
                    r.push_str("</t:Contact>");
                }
                r.push_str("</t:Resolution>");
                resolutions.push(r);
            }
        }
    }
    let msg = match resolutions.len() {
        0 => message("ResolveNames", Err(EwsError::new("ErrorNameResolutionNoResults", "No results were found."))),
        n => {
            let set = format!(r#"<m:ResolutionSet TotalItemsInView="{n}" IncludesLastItemInRange="true">{}</m:ResolutionSet>"#, resolutions.join(""));
            if n == 1 {
                message("ResolveNames", Ok(set))
            } else {
                format!(
                    r#"<m:ResolveNamesResponseMessage ResponseClass="Warning"><m:MessageText>Multiple results were found.</m:MessageText><m:ResponseCode>ErrorNameResolutionMultipleResults</m:ResponseCode><m:DescriptiveLinkKey>0</m:DescriptiveLinkKey>{set}</m:ResolveNamesResponseMessage>"#
                )
            }
        }
    };
    Ok(wrap("ResolveNames", &msg))
}

// ---------------------------------------------------------- out of office

fn oof_owner(ctx: &Ctx, op: &Node) -> EwsResult<i64> {
    let addr = op.find("Address").map(|a| a.text.trim().to_string()).unwrap_or_else(|| ctx.user.email.clone());
    let (id, _) = ctx.app.store.resolve_address(&addr)?.ok_or_else(|| EwsError::new("ErrorNonExistentMailbox", "The mailbox does not exist."))?;
    if !ctx.app.store.can_access(ctx.user.id, id) {
        return Err(EwsError::denied());
    }
    Ok(id)
}

pub fn get_oof(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let owner = oof_owner(ctx, op)?;
    let o = ctx.app.store.settings(owner)?.oof;
    let state = if o.state.is_empty() { "Disabled" } else { o.state.as_str() };
    let audience = if o.external_audience.is_empty() { "All" } else { o.external_audience.as_str() };
    let (start, end) = (o.start.unwrap_or_else(now), o.end.unwrap_or_else(|| now() + 86_400));
    Ok(format!(
        r#"<m:GetUserOofSettingsResponse xmlns:m="{NS_M}" xmlns:t="{NS_T}"><m:ResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode></m:ResponseMessage><t:OofSettings><t:OofState>{state}</t:OofState><t:ExternalAudience>{audience}</t:ExternalAudience><t:Duration><t:StartTime>{}</t:StartTime><t:EndTime>{}</t:EndTime></t:Duration><t:InternalReply><t:Message>{}</t:Message></t:InternalReply><t:ExternalReply><t:Message>{}</t:Message></t:ExternalReply></t:OofSettings><m:AllowExternalOof>All</m:AllowExternalOof></m:GetUserOofSettingsResponse>"#,
        utc(start),
        utc(end),
        escape(&o.internal_reply),
        escape(&o.external_reply)
    ))
}

pub fn set_oof(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let owner = oof_owner(ctx, op)?;
    let s = op.find("UserOofSettings").ok_or_else(|| EwsError::new("ErrorInvalidRequest", "UserOofSettings missing"))?;
    let mut settings = ctx.app.store.settings(owner)?;
    let duration = s.child("Duration");
    let oof = OofSettings {
        state: s.text_of("OofState").unwrap_or("Disabled").to_string(),
        start: duration.and_then(|d| d.text_of("StartTime")).and_then(|t| parse_time(t, chrono_tz::UTC)),
        end: duration.and_then(|d| d.text_of("EndTime")).and_then(|t| parse_time(t, chrono_tz::UTC)),
        internal_reply: s.child("InternalReply").and_then(|r| r.raw_text_of("Message")).unwrap_or_default().to_string(),
        external_reply: s.child("ExternalReply").and_then(|r| r.raw_text_of("Message")).unwrap_or_default().to_string(),
        external_audience: s.text_of("ExternalAudience").unwrap_or("All").to_string(),
    };
    if oof.state != settings.oof.state {
        settings.oof_sent.clear();
    }
    settings.oof = oof;
    ctx.app.store.set_settings(owner, &settings)?;
    Ok(format!(r#"<m:SetUserOofSettingsResponse xmlns:m="{NS_M}"><m:ResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode></m:ResponseMessage></m:SetUserOofSettingsResponse>"#))
}

// ---------------------------------------------------------- availability

fn transition(n: Option<&Node>) -> Option<Transition> {
    let n = n?;
    let month: u32 = n.text_of("Month")?.parse().ok()?;
    if month == 0 {
        return None;
    }
    let time = n.text_of("Time").unwrap_or("00:00:00");
    let mut hm = time.split(':');
    let hour = hm.next().and_then(|h| h.parse().ok()).unwrap_or(0);
    let minute = hm.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    let weekday = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"].iter().position(|d| Some(*d) == n.text_of("DayOfWeek")).unwrap_or(0) as u32;
    Some(Transition { month, week: n.text_of("DayOrder").and_then(|d| d.parse().ok()).unwrap_or(1), weekday, hour, minute })
}

/// Zone of a legacy `SerializableTimeZone` (Bias + Standard/Daylight).
fn request_zone(n: Option<&Node>) -> Tz {
    let Some(n) = n else { return chrono_tz::UTC };
    let bias: i32 = n.text_of("Bias").and_then(|b| b.parse().ok()).unwrap_or(0);
    let std = n.child("StandardTime");
    let dst = n.child("DaylightTime");
    let std_bias: i32 = std.and_then(|s| s.text_of("Bias")).and_then(|b| b.parse().ok()).unwrap_or(0);
    let dst_bias: i32 = dst.and_then(|s| s.text_of("Bias")).and_then(|b| b.parse().ok()).unwrap_or(0);
    let std_offset = -(bias + std_bias);
    let dst_offset = -(bias + dst_bias);
    let rule = if std_offset == dst_offset {
        TzRule { std_offset, dst_offset, ..Default::default() }
    } else {
        TzRule { std_offset, dst_offset, to_std: transition(std), to_dst: transition(dst), ..Default::default() }
    };
    if rule.to_std.is_none() && std_offset == 0 {
        return chrono_tz::UTC;
    }
    tzinfo::zone_for_rule(&rule)
}

fn local_str(ts: i64, tz: Tz) -> String {
    DateTime::from_timestamp(ts, 0).unwrap_or_default().with_timezone(&tz).format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn local_time(s: &str, tz: Tz) -> Option<i64> {
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp());
    }
    let d = NaiveDateTime::parse_from_str(s.split('.').next()?, "%Y-%m-%dT%H:%M:%S").ok()?;
    tz.from_local_datetime(&d).earliest().map(|x| x.timestamp())
}

pub fn availability(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let tz = request_zone(op.child("TimeZone"));
    let opts = op.child("FreeBusyViewOptions");
    let window = opts.and_then(|o| o.child("TimeWindow"));
    let from = window.and_then(|w| w.text_of("StartTime")).and_then(|t| local_time(t, tz)).ok_or_else(|| EwsError::new("ErrorInvalidTimeInterval", "TimeWindow missing"))?;
    let to = window.and_then(|w| w.text_of("EndTime")).and_then(|t| local_time(t, tz)).ok_or_else(|| EwsError::new("ErrorInvalidTimeInterval", "TimeWindow missing"))?;
    if to <= from || to - from > 62 * 86_400 {
        return Err(EwsError::new("ErrorInvalidTimeInterval", "The time window must be between one minute and 62 days."));
    }
    let interval: i64 = opts.and_then(|o| o.text_of("MergedFreeBusyIntervalInMinutes")).and_then(|m| m.parse().ok()).unwrap_or(30);
    let view = opts.and_then(|o| o.text_of("RequestedView")).unwrap_or("Detailed");
    let merged = view.contains("Merged");
    let detailed = view.starts_with("Detailed");
    let mut out = String::new();
    for md in op.child("MailboxDataArray").map(|a| a.children.as_slice()).unwrap_or_default() {
        let address = md.find("Address").map(|a| a.text.trim().to_string()).unwrap_or_default();
        let Some(user) = directory::resolve(&ctx.app, &address) else {
            let _ = write!(
                out,
                r#"<m:FreeBusyResponse><m:ResponseMessage ResponseClass="Error"><m:MessageText>Unable to resolve e-mail address {} to an Active Directory object.</m:MessageText><m:ResponseCode>ErrorMailRecipientNotFound</m:ResponseCode><m:DescriptiveLinkKey>0</m:DescriptiveLinkKey></m:ResponseMessage><m:FreeBusyView><t:FreeBusyViewType>None</t:FreeBusyViewType></m:FreeBusyView></m:FreeBusyResponse>"#,
                escape(&address)
            );
            continue;
        };
        let busy = calendar::busy_times(&ctx.app, user.id, from, to)?;
        let details_allowed = ctx.app.store.can_access(ctx.user.id, user.id) || user.kind == "room";
        let mut fb = format!("<m:FreeBusyView><t:FreeBusyViewType>{}</t:FreeBusyViewType>", if detailed { if merged { "DetailedMerged" } else { "Detailed" } } else if merged { "FreeBusyMerged" } else { "FreeBusy" });
        if merged {
            push(&mut fb, "MergedFreeBusy", &calendar::merged_free_busy(&busy, from, to, interval));
        }
        fb.push_str("<t:CalendarEventArray>");
        for b in &busy {
            let _ = write!(fb, "<t:CalendarEvent><t:StartTime>{}</t:StartTime><t:EndTime>{}</t:EndTime><t:BusyType>{}</t:BusyType>", local_str(b.start, tz), local_str(b.end, tz), busy_type(b.status));
            if detailed && details_allowed {
                fb.push_str("<t:CalendarEventDetails>");
                push(&mut fb, "ID", &format!("I.{}", b.item_id));
                push(&mut fb, "Subject", if b.private { "" } else { &b.subject });
                push(&mut fb, "Location", if b.private { "" } else { &b.location });
                push(&mut fb, "IsMeeting", "false");
                push(&mut fb, "IsRecurring", if b.recurring { "true" } else { "false" });
                push(&mut fb, "IsException", "false");
                push(&mut fb, "IsReminderSet", "false");
                push(&mut fb, "IsPrivate", if b.private { "true" } else { "false" });
                fb.push_str("</t:CalendarEventDetails>");
            }
            fb.push_str("</t:CalendarEvent>");
        }
        fb.push_str("</t:CalendarEventArray>");
        fb.push_str(r#"<t:WorkingHours><t:TimeZone><t:Bias>0</t:Bias><t:StandardTime><t:Bias>0</t:Bias><t:Time>00:00:00</t:Time><t:DayOrder>0</t:DayOrder><t:Month>0</t:Month><t:DayOfWeek>Sunday</t:DayOfWeek></t:StandardTime><t:DaylightTime><t:Bias>0</t:Bias><t:Time>00:00:00</t:Time><t:DayOrder>0</t:DayOrder><t:Month>0</t:Month><t:DayOfWeek>Sunday</t:DayOfWeek></t:DaylightTime></t:TimeZone><t:WorkingPeriodArray><t:WorkingPeriod><t:DayOfWeek>Monday Tuesday Wednesday Thursday Friday</t:DayOfWeek><t:StartTimeInMinutes>480</t:StartTimeInMinutes><t:EndTimeInMinutes>1020</t:EndTimeInMinutes></t:WorkingPeriod></t:WorkingPeriodArray></t:WorkingHours>"#);
        fb.push_str("</m:FreeBusyView>");
        let _ = write!(out, r#"<m:FreeBusyResponse><m:ResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode></m:ResponseMessage>{fb}</m:FreeBusyResponse>"#);
    }
    Ok(format!(r#"<m:GetUserAvailabilityResponse xmlns:m="{NS_M}" xmlns:t="{NS_T}"><m:FreeBusyResponseArray>{out}</m:FreeBusyResponseArray></m:GetUserAvailabilityResponse>"#))
}

// ------------------------------------------------------------------ rooms

fn address_xml(name: &str, email: &str, kind: &str) -> String {
    format!("<t:Name>{}</t:Name><t:EmailAddress>{}</t:EmailAddress><t:RoutingType>SMTP</t:RoutingType><t:MailboxType>{kind}</t:MailboxType>", escape(name), escape(email))
}

pub fn room_lists(ctx: &Ctx) -> EwsResult<String> {
    let rooms: Vec<_> = ctx.app.store.users()?.into_iter().filter(|u| u.kind == "room" && u.enabled).collect();
    let domains: Vec<String> = rooms.iter().map(|r| r.domain().to_string()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let lists: String = domains.iter().map(|d| format!("<t:Address>{}</t:Address>", address_xml(&format!("Rooms ({d})"), &format!("rooms@{d}"), "PublicDL"))).collect();
    Ok(format!(
        r#"<m:GetRoomListsResponse ResponseClass="Success" xmlns:m="{NS_M}" xmlns:t="{NS_T}"><m:ResponseCode>NoError</m:ResponseCode><m:RoomLists>{lists}</m:RoomLists></m:GetRoomListsResponse>"#
    ))
}

pub fn rooms(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let list = op.find("EmailAddress").map(|e| e.text.trim().to_ascii_lowercase()).unwrap_or_default();
    let domain = list.strip_prefix("rooms@").unwrap_or_default().to_string();
    let rooms: String = ctx
        .app
        .store
        .users()?
        .into_iter()
        .filter(|u| u.kind == "room" && u.enabled && u.domain() == domain)
        .map(|u| format!("<t:Room><t:Id>{}</t:Id></t:Room>", address_xml(if u.name.is_empty() { &u.email } else { &u.name }, &u.email, "Mailbox")))
        .collect();
    Ok(format!(r#"<m:GetRoomsResponse ResponseClass="Success" xmlns:m="{NS_M}" xmlns:t="{NS_T}"><m:ResponseCode>NoError</m:ResponseCode><m:Rooms>{rooms}</m:Rooms></m:GetRoomsResponse>"#))
}

// ---------------------------------------------------------- subscriptions

#[derive(Clone)]
struct Subscription {
    user_id: i64,
    /// Watched folders (empty = all folders of the mailbox).
    folders: HashSet<i64>,
    watermark: i64,
}

fn subscriptions() -> &'static Mutex<HashMap<String, Subscription>> {
    static SUBS: OnceLock<Mutex<HashMap<String, Subscription>>> = OnceLock::new();
    SUBS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn subscribe(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let req = op.children.first().ok_or_else(|| EwsError::new("ErrorInvalidRequest", "Subscription request missing"))?;
    let mut folders = HashSet::new();
    let mut owner = ctx.user.id;
    if req.attr("SubscribeToAllFolders") != Some("true") {
        for f in req.child("FolderIds").map(|x| x.children.as_slice()).unwrap_or_default() {
            let folder = resolve_folder(ctx, f)?;
            owner = folder.user_id;
            folders.insert(folder.id);
        }
    }
    let watermark = ctx.app.store.current_modseq(owner)?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    subscriptions().lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), Subscription { user_id: owner, folders, watermark });
    let inner = format!("<m:SubscriptionId>{id}</m:SubscriptionId><m:Watermark>W{watermark}</m:Watermark>");
    Ok(wrap("Subscribe", &message("Subscribe", Ok(inner))))
}

pub fn unsubscribe(_ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let id = op.text_of("SubscriptionId").unwrap_or_default();
    let removed = subscriptions().lock().unwrap_or_else(|e| e.into_inner()).remove(id).is_some();
    let res = if removed { Ok(String::new()) } else { Err(EwsError::new("ErrorSubscriptionNotFound", "The specified subscription was not found.")) };
    Ok(wrap("Unsubscribe", &message("Unsubscribe", res)))
}

fn changed_folders(ctx: &Ctx, sub: &Subscription) -> EwsResult<Vec<i64>> {
    let folders: Vec<i64> = if sub.folders.is_empty() { ctx.app.store.folders(sub.user_id)?.into_iter().map(|f| f.id).collect() } else { sub.folders.iter().copied().collect() };
    let mut out = Vec::new();
    for f in folders {
        if ctx.app.store.has_changes(f, sub.watermark, None)? {
            out.push(f);
        }
    }
    Ok(out)
}

fn events_xml(id: &str, folders: &[i64], watermark: i64) -> String {
    let mut s = format!("<m:Notification><t:SubscriptionId>{id}</t:SubscriptionId><t:PreviousWatermark>W{watermark}</t:PreviousWatermark><t:MoreEvents>false</t:MoreEvents>");
    for f in folders {
        let _ = write!(s, r#"<t:ModifiedEvent><t:Watermark>W{watermark}</t:Watermark><t:TimeStamp>{}</t:TimeStamp><t:FolderId Id="F.{f}"/><t:ParentFolderId Id="F.{f}"/></t:ModifiedEvent>"#, utc(now()));
    }
    if folders.is_empty() {
        let _ = write!(s, "<t:StatusEvent><t:Watermark>W{watermark}</t:Watermark></t:StatusEvent>");
    }
    s.push_str("</m:Notification>");
    s
}

pub fn get_events(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let id = op.text_of("SubscriptionId").unwrap_or_default().to_string();
    let sub = subscriptions().lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
    let res = match sub {
        None => Err(EwsError::new("ErrorSubscriptionNotFound", "The specified subscription was not found.")),
        Some(sub) => {
            let changed = changed_folders(ctx, &sub)?;
            let now_mark = ctx.app.store.current_modseq(sub.user_id)?;
            if let Some(s) = subscriptions().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
                s.watermark = now_mark;
            }
            Ok(format!("<m:Notification>{}</m:Notification>", events_xml(&id, &changed, sub.watermark).trim_start_matches("<m:Notification>").trim_end_matches("</m:Notification>")))
        }
    };
    Ok(wrap("GetEvents", &message("GetEvents", res)))
}

fn stream_chunk(inner: &str) -> axum::body::Bytes {
    axum::body::Bytes::from(envelope(&wrap("GetStreamingEvents", &format!(r#"<m:GetStreamingEventsResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode>{inner}</m:GetStreamingEventsResponseMessage>"#))))
}

/// Streaming notifications: keeps the response open and writes a SOAP
/// envelope for every change (and a keep-alive every minute).
pub async fn get_streaming_events(ctx: &Ctx, op: &Node) -> Response {
    let ids: Vec<String> = op.child("SubscriptionIds").map(|s| s.children.iter().map(|c| c.text.trim().to_string()).collect()).unwrap_or_default();
    let minutes: u64 = op.text_of("ConnectionTimeout").and_then(|m| m.parse().ok()).unwrap_or(30).clamp(1, 30);
    let subs: Vec<(String, Subscription)> = {
        let map = subscriptions().lock().unwrap_or_else(|e| e.into_inner());
        ids.iter().filter_map(|id| map.get(id).map(|s| (id.clone(), s.clone()))).collect()
    };
    if subs.is_empty() || subs.iter().any(|(_, s)| !ctx.app.store.can_access(ctx.user.id, s.user_id)) {
        let body = envelope(&wrap("GetStreamingEvents", &message("GetStreamingEvents", Err(EwsError::new("ErrorInvalidSubscription", "The subscription is invalid or expired.")))));
        let mut r = (StatusCode::OK, body).into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/xml; charset=utf-8"));
        return r;
    }
    let app = ctx.app.clone();
    let rx = app.store.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(minutes * 60);
    struct State {
        app: crate::app::Shared,
        rx: tokio::sync::broadcast::Receiver<crate::store::Notice>,
        subs: Vec<(String, Subscription)>,
        deadline: tokio::time::Instant,
        done: bool,
        started: bool,
    }
    let state = State { app, rx, subs, deadline, done: false, started: false };
    let stream = futures::stream::unfold(state, |mut st| async move {
        if st.done {
            return None;
        }
        if !st.started {
            st.started = true;
            return Some((Ok::<_, std::io::Error>(stream_chunk("<m:ConnectionStatus>OK</m:ConnectionStatus>")), st));
        }
        let keepalive = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let until = keepalive.min(st.deadline);
            match tokio::time::timeout_at(until, st.rx.recv()).await {
                Err(_) if tokio::time::Instant::now() >= st.deadline => {
                    st.done = true;
                    return Some((Ok(stream_chunk("<m:ConnectionStatus>Closed</m:ConnectionStatus>")), st));
                }
                Err(_) => return Some((Ok(stream_chunk("<m:ConnectionStatus>OK</m:ConnectionStatus>")), st)),
                Ok(Ok(n)) => {
                    let mut notes = String::new();
                    for (id, sub) in st.subs.iter_mut() {
                        if n.user_id != sub.user_id || !(sub.folders.is_empty() || sub.folders.contains(&n.folder_id) || n.folder_id == 0) {
                            continue;
                        }
                        let mark = st.app.store.current_modseq(sub.user_id).unwrap_or(sub.watermark);
                        let folders = if n.folder_id == 0 { sub.folders.iter().copied().collect() } else { vec![n.folder_id] };
                        notes.push_str(&events_xml(id, &folders, sub.watermark));
                        sub.watermark = mark;
                        if let Some(s) = subscriptions().lock().unwrap_or_else(|e| e.into_inner()).get_mut(id.as_str()) {
                            s.watermark = mark;
                        }
                    }
                    if !notes.is_empty() {
                        return Some((Ok(stream_chunk(&format!("<m:Notifications>{notes}</m:Notifications>"))), st));
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(_)) => {
                    st.done = true;
                    return Some((Ok(stream_chunk("<m:ConnectionStatus>Closed</m:ConnectionStatus>")), st));
                }
            }
        }
    });
    let mut r = (StatusCode::OK, Body::from_stream(stream)).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/xml; charset=utf-8"));
    r
}

// ---------------------------------------------------------------- others

pub fn server_time_zones(_ctx: &Ctx, _op: &Node) -> EwsResult<String> {
    let zones = ["Europe/Berlin", "Europe/London", "Europe/Paris", "Europe/Helsinki", "America/New_York", "America/Chicago", "America/Denver", "America/Los_Angeles", "Asia/Tokyo", "Asia/Shanghai", "Australia/Sydney", "UTC"];
    let defs: String = zones
        .iter()
        .filter_map(|z| z.parse::<Tz>().ok())
        .map(|tz| format!(r#"<t:TimeZoneDefinition Id="{}" Name="{}"/>"#, escape(&tzinfo::windows_name(tz)), escape(tz.name())))
        .collect();
    Ok(wrap("GetServerTimeZones", &message("GetServerTimeZones", Ok(format!("<m:TimeZoneDefinitions>{defs}</m:TimeZoneDefinitions>")))))
}

pub fn convert_id(_ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let dest = op.attr("DestinationFormat").unwrap_or("EwsId");
    let mut out = String::new();
    for a in op.child("SourceIds").map(|s| s.children.as_slice()).unwrap_or_default() {
        let inner = format!(
            r#"<m:AlternateId xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="t:AlternateIdType" Format="{dest}" Id="{}" Mailbox="{}"/>"#,
            escape(a.attr("Id").unwrap_or_default()),
            escape(a.attr("Mailbox").unwrap_or_default())
        );
        out.push_str(&message("ConvertId", Ok(inner)));
    }
    Ok(wrap("ConvertId", &out))
}

pub fn mail_tips(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let mut out = String::new();
    for m in op.child("Recipients").map(|r| r.children.as_slice()).unwrap_or_default() {
        let address = m.text_of("EmailAddress").unwrap_or_default().to_string();
        let mut tips = format!("<t:RecipientAddress><t:EmailAddress>{}</t:EmailAddress><t:RoutingType>SMTP</t:RoutingType></t:RecipientAddress><t:PendingMailTips/>", escape(&address));
        if let Some(u) = directory::resolve(&ctx.app, &address) {
            let o = ctx.app.store.settings(u.id)?.oof;
            let reply = if o.active(now()) { o.internal_reply } else { String::new() };
            let _ = write!(tips, "<t:OutOfOffice><t:ReplyBody><t:Message>{}</t:Message></t:ReplyBody></t:OutOfOffice>", escape(&reply));
            tips.push_str("<t:External>false</t:External>");
        } else {
            tips.push_str("<t:External>true</t:External>");
        }
        let _ = write!(out, r#"<m:MailTipsResponseMessageType ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:MailTips>{tips}</m:MailTips></m:MailTipsResponseMessageType>"#);
    }
    Ok(format!(r#"<m:GetMailTipsResponse ResponseClass="Success" xmlns:m="{NS_M}" xmlns:t="{NS_T}"><m:ResponseCode>NoError</m:ResponseCode><m:ResponseMessages>{out}</m:ResponseMessages></m:GetMailTipsResponse>"#))
}

// ------------------------------------------------------- master category list

/// Map an Ondura category colour name to an Outlook master-category colour index.
fn cat_color_index(name: &str) -> u8 {
    match name {
        "red" => 0,
        "orange" => 1,
        "yellow" => 3,
        "green" => 4,
        "teal" => 5,
        "blue" => 7,
        "purple" => 8,
        "pink" => 9,
        "gray" => 12,
        _ => 7,
    }
}

/// Inverse of `cat_color_index`, for categories created or recoloured in Outlook.
fn cat_color_name(idx: u8) -> &'static str {
    match idx {
        0 | 15 => "red",
        1 | 16 => "orange",
        3 | 18 => "yellow",
        4 | 19 => "green",
        5 | 20 => "teal",
        8 | 23 => "purple",
        9 | 24 => "pink",
        12 | 13 => "gray",
        _ => "blue",
    }
}

/// Deterministic GUID for a category id, stable across requests so Outlook keeps
/// one entry per category instead of duplicating it.
fn cat_guid(id: i64) -> String {
    let d = <sha2::Sha256 as sha2::Digest>::digest(format!("ondura-category-{id}").as_bytes());
    format!(
        "{{{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}}}",
        d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7], d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]
    )
}

/// GetUserConfiguration — serves the "CategoryList" master category list so
/// Outlook renders webmail categories with their colours. Other configuration
/// names are reported as not found.
pub fn get_user_configuration(ctx: &Ctx, op: &Node) -> EwsResult<String> {
    let name = op.child("UserConfigurationName").and_then(|n| n.attr("Name")).unwrap_or("");
    if name != "CategoryList" {
        return Err(EwsError::not_found());
    }
    let cats = ctx.app.store.categories(ctx.user.id)?;
    let mut inner = String::new();
    for (id, cname, color) in &cats {
        let _ = write!(inner, r#"<category name="{}" color="{}" keyboardShortcut="0" guid="{}" />"#, escape(cname), cat_color_index(color), cat_guid(*id));
    }
    let xml = format!(r#"<?xml version="1.0" encoding="utf-8"?><categories default="" lastSavedSession="0" lastSavedTime="{}" xmlns="CategoryList.xsd">{inner}</categories>"#, utc(now()));
    let data = B64.encode(xml.as_bytes());
    let ck = B64.encode(&<sha2::Sha256 as sha2::Digest>::digest(xml.as_bytes())[..8]);
    Ok(format!(
        r#"<m:GetUserConfigurationResponse xmlns:m="{NS_M}" xmlns:t="{NS_T}"><m:ResponseMessages><m:GetUserConfigurationResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode><m:UserConfiguration><t:UserConfigurationName Name="CategoryList"><t:DistinguishedFolderId Id="calendar"/></t:UserConfigurationName><t:ItemId Id="UC.CategoryList" ChangeKey="{ck}"/><t:XmlData>{data}</t:XmlData></m:UserConfiguration></m:GetUserConfigurationResponseMessage></m:ResponseMessages></m:GetUserConfigurationResponse>"#
    ))
}

/// Create/Update/DeleteUserConfiguration — accepted so Outlook can save category
/// edits. For the category list, new or recoloured categories are merged into the
/// registry additively (names never deleted here, to protect webmail categories).
pub fn write_user_configuration(ctx: &Ctx, op: &Node, verb: &str) -> EwsResult<String> {
    if verb != "Delete"
        && let Some(cfg) = op.child("UserConfiguration")
        && cfg.child("UserConfigurationName").and_then(|n| n.attr("Name")) == Some("CategoryList")
        && let Some(b64) = cfg.text_of("XmlData")
        && let Ok(bytes) = B64.decode(b64.trim())
        && let Ok(xml) = std::str::from_utf8(&bytes)
        && let Ok(doc) = Node::parse(xml)
    {
        let mut cats = Vec::new();
        doc.find_all("category", &mut cats);
        for c in cats {
            if let Some(nm) = c.attr("name").filter(|s| !s.trim().is_empty()) {
                let idx = c.attr("color").and_then(|s| s.parse::<i64>().ok()).unwrap_or(7).clamp(0, 24) as u8;
                let _ = ctx.app.store.create_category(ctx.user.id, nm, cat_color_name(idx));
            }
        }
    }
    Ok(format!(
        r#"<m:{verb}UserConfigurationResponse xmlns:m="{NS_M}" xmlns:t="{NS_T}"><m:ResponseMessages><m:{verb}UserConfigurationResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode></m:{verb}UserConfigurationResponseMessage></m:ResponseMessages></m:{verb}UserConfigurationResponse>"#
    ))
}

/// GetInboxRules — expose the user's server-side inbox rules so Outlook and
/// other EWS clients can display them. Read-only (rules are managed in webmail).
pub fn get_inbox_rules(ctx: &Ctx) -> EwsResult<String> {
    let rules = ctx.app.store.rules(ctx.user.id).map_err(|_| EwsError::new("ErrorInternalServerError", "could not load inbox rules"))?;
    let strings = |v: &[String]| v.iter().map(|s| format!("<t:String>{}</t:String>", escape(s))).collect::<String>();
    let mut out = String::new();
    for (i, r) in rules.iter().enumerate() {
        let (mut subj, mut sender, mut rcpt) = (Vec::new(), Vec::new(), Vec::new());
        for c in &r.conditions {
            match c.field.as_str() {
                "subject" => subj.push(c.value.clone()),
                "from" => sender.push(c.value.clone()),
                "to" => rcpt.push(c.value.clone()),
                _ => {}
            }
        }
        let mut conds = String::new();
        if !subj.is_empty() {
            let _ = write!(conds, "<t:ContainsSubjectStrings>{}</t:ContainsSubjectStrings>", strings(&subj));
        }
        if !sender.is_empty() {
            let _ = write!(conds, "<t:ContainsSenderStrings>{}</t:ContainsSenderStrings>", strings(&sender));
        }
        if !rcpt.is_empty() {
            let _ = write!(conds, "<t:ContainsRecipientStrings>{}</t:ContainsRecipientStrings>", strings(&rcpt));
        }
        let mut acts = String::new();
        let mut cats = Vec::new();
        for a in &r.actions {
            match a.kind.as_str() {
                "move" => {
                    if let Some(f) = a.folder {
                        let _ = write!(acts, r#"<t:MoveToFolder><t:FolderId Id="F.{f}"/></t:MoveToFolder>"#);
                    }
                }
                "markRead" => acts.push_str("<t:MarkAsRead>true</t:MarkAsRead>"),
                "categorize" => {
                    if let Some(c) = &a.category {
                        cats.push(c.clone());
                    }
                }
                "delete" => acts.push_str("<t:Delete>true</t:Delete>"),
                _ => {}
            }
        }
        if !cats.is_empty() {
            let _ = write!(acts, "<t:AssignCategories>{}</t:AssignCategories>", strings(&cats));
        }
        if r.stop {
            acts.push_str("<t:StopProcessingRules>true</t:StopProcessingRules>");
        }
        let _ = write!(
            out,
            r#"<t:Rule><t:RuleId>{}</t:RuleId><t:DisplayName>{}</t:DisplayName><t:Priority>{}</t:Priority><t:IsEnabled>{}</t:IsEnabled><t:IsNotSupported>false</t:IsNotSupported><t:IsInError>false</t:IsInError><t:Conditions>{}</t:Conditions><t:Actions>{}</t:Actions></t:Rule>"#,
            r.id,
            escape(&r.name),
            i + 1,
            r.enabled,
            conds,
            acts
        );
    }
    Ok(format!(
        r#"<m:GetInboxRulesResponse ResponseClass="Success" xmlns:m="{NS_M}" xmlns:t="{NS_T}"><m:ResponseCode>NoError</m:ResponseCode><m:OutlookRuleBlobExists>false</m:OutlookRuleBlobExists><m:InboxRules>{out}</m:InboxRules></m:GetInboxRulesResponse>"#
    ))
}
