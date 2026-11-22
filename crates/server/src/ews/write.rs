//! XML for folders and items in EWS responses (schema element order).

use std::fmt::Write as _;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::DateTime;
use chrono_tz::Tz;

use super::{Ctx, Shape, bool_str, class_element, folder_id, push};
use crate::mime;
use crate::model::{
    AttendeeKind, BusyStatus, Contact, Event, Frequency, ItemKind, Occurrence, Person, Recurrence, Response, Task, WEEKDAYS, flags,
};
use crate::store::{Folder, Item};
use crate::tzinfo;
use crate::xml::escape;

pub fn utc(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn ext_prop(tag: &str, kind: &str, value: &str) -> String {
    format!(r#"<t:ExtendedProperty><t:ExtendedFieldURI PropertyTag="{tag}" PropertyType="{kind}"/><t:Value>{}</t:Value></t:ExtendedProperty>"#, escape(value))
}

fn is_tag(tag: &str, num: u32) -> bool {
    let t = tag.trim().to_ascii_lowercase();
    let parsed = t.strip_prefix("0x").map(|h| u32::from_str_radix(h, 16).ok()).unwrap_or_else(|| t.parse().ok());
    parsed == Some(num)
}

// ----------------------------------------------------------------- folders

pub fn folder_xml(ctx: &Ctx, f: &Folder, shape: &Shape) -> String {
    let el = class_element(f.class);
    let mut s = format!("<t:{el}>");
    s.push_str(&folder_id(f));
    let all = shape.base != "IdOnly";
    if (all || shape.wants("folder:ParentFolderId", false))
        && let Some(p) = f.parent_id
    {
        let _ = write!(s, r#"<t:ParentFolderId Id="F.{p}"/>"#);
    }
    if all || shape.wants("folder:FolderClass", false) {
        push(&mut s, "FolderClass", f.class.ipf());
    }
    if all || shape.wants("folder:DisplayName", false) {
        push(&mut s, "DisplayName", &f.name);
    }
    let (total, unread) = ctx.app.store.unread_count(f.id).unwrap_or((0, 0));
    if all || shape.wants("folder:TotalCount", false) {
        push(&mut s, "TotalCount", &total.to_string());
    }
    if all || shape.wants("folder:ChildFolderCount", false) {
        let children = ctx.app.store.folders(f.user_id).map(|l| l.iter().filter(|c| c.parent_id == Some(f.id)).count()).unwrap_or(0);
        push(&mut s, "ChildFolderCount", &children.to_string());
    }
    for (tag, kind) in &shape.extended {
        if is_tag(tag, 0x0E08) {
            s.push_str(&ext_prop(tag, kind, &ctx.app.store.folder_size(f.id).unwrap_or(0).to_string()));
        }
    }
    if shape.base == "AllProperties" || shape.wants("folder:EffectiveRights", false) || shape.base == "Default" {
        let own = f.user_id == ctx.user.id || ctx.app.store.can_access(ctx.user.id, f.user_id);
        let _ = write!(
            s,
            "<t:EffectiveRights><t:CreateAssociated>{0}</t:CreateAssociated><t:CreateContents>{0}</t:CreateContents><t:CreateHierarchy>{0}</t:CreateHierarchy><t:Delete>{0}</t:Delete><t:Modify>{0}</t:Modify><t:Read>true</t:Read><t:ViewPrivateItems>{0}</t:ViewPrivateItems></t:EffectiveRights>",
            bool_str(own)
        );
    }
    if el == "Folder" && (all || shape.wants("folder:UnreadCount", false)) {
        push(&mut s, "UnreadCount", &unread.to_string());
    }
    let _ = write!(s, "</t:{el}>");
    s
}

// ------------------------------------------------------------------- items

pub fn mailbox(p: &Person, kind: &str) -> String {
    let mut s = String::from("<t:Mailbox>");
    if !p.name.is_empty() {
        push(&mut s, "Name", &p.name);
    }
    push(&mut s, "EmailAddress", &p.email);
    push(&mut s, "RoutingType", "SMTP");
    push(&mut s, "MailboxType", kind);
    s.push_str("</t:Mailbox>");
    s
}

fn recipients(name: &str, list: &[Person]) -> String {
    if list.is_empty() {
        return String::new();
    }
    let inner: String = list.iter().map(|p| mailbox(p, "OneOff")).collect();
    format!("<t:{name}>{inner}</t:{name}>")
}

fn single(name: &str, p: &Person) -> String {
    format!("<t:{name}>{}</t:{name}>", mailbox(p, "OneOff"))
}

/// `<t:Body>` in the requested body type.
pub fn body_xml(shape: &Shape, text: &str, html: Option<&str>) -> String {
    let (t, content) = match (shape.body_type.as_str(), html) {
        ("Text", Some(h)) if text.is_empty() => ("Text", mime::html_to_text(h)),
        ("Text", _) => ("Text", text.to_string()),
        ("HTML", Some(h)) | ("Best", Some(h)) => ("HTML", h.to_string()),
        ("HTML", None) => ("HTML", mime::text_to_html(text)),
        _ => ("Text", text.to_string()),
    };
    format!(r#"<t:Body BodyType="{t}" IsTruncated="false">{}</t:Body>"#, escape(&content))
}

fn categories(list: &[String]) -> String {
    if list.is_empty() {
        return String::new();
    }
    let inner: String = list.iter().map(|c| format!("<t:String>{}</t:String>", escape(c))).collect();
    format!("<t:Categories>{inner}</t:Categories>")
}

fn display_list(list: &[Person]) -> String {
    list.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("; ")
}

pub fn attachment_xml(item_id: i64, a: &mime::AttachmentInfo, content: Option<&[u8]>) -> String {
    let mut s = String::from("<t:FileAttachment>");
    let _ = write!(s, r#"<t:AttachmentId Id="A.{item_id}.{}"/>"#, a.index);
    push(&mut s, "Name", &a.name);
    push(&mut s, "ContentType", &a.content_type);
    if let Some(cid) = &a.content_id {
        push(&mut s, "ContentId", cid);
    }
    push(&mut s, "Size", &a.size.to_string());
    push(&mut s, "IsInline", bool_str(a.inline));
    if let Some(c) = content {
        push(&mut s, "Content", &B64.encode(c));
    }
    s.push_str("</t:FileAttachment>");
    s
}

fn mail_element(item: &Item) -> &'static str {
    let c = item.message_class.as_str();
    if c.starts_with("IPM.Schedule.Meeting.Request") {
        "MeetingRequest"
    } else if c.starts_with("IPM.Schedule.Meeting.Canceled") {
        "MeetingCancellation"
    } else if c.starts_with("IPM.Schedule.Meeting.Resp") {
        "MeetingResponse"
    } else {
        "Message"
    }
}

/// Item element for responses. `find` = list views (no body by default).
pub fn item_xml(ctx: &Ctx, item: &Item, shape: &Shape, find: bool) -> String {
    match item.kind {
        ItemKind::Mail | ItemKind::Note => mail_xml(ctx, item, shape, find),
        ItemKind::Event => {
            let e = item.event().unwrap_or_default();
            event_xml(ctx, item, &e, None, shape, find)
        }
        ItemKind::Contact => contact_xml(item, &item.contact().unwrap_or_default(), shape, find),
        ItemKind::Task => task_xml(item, &item.task().unwrap_or_default(), shape, find),
    }
}

fn common_head(s: &mut String, item: &Item, shape: &Shape, item_class: &str) {
    s.push_str(&super::item_id_xml(item.id, item.modseq));
    if shape.wants("item:ParentFolderId", false) {
        let _ = write!(s, r#"<t:ParentFolderId Id="F.{}"/>"#, item.folder_id);
    }
    if shape.wants("item:ItemClass", true) {
        push(s, "ItemClass", item_class);
    }
}

fn mail_xml(ctx: &Ctx, item: &Item, shape: &Shape, find: bool) -> String {
    let el = if item.kind == ItemKind::Note { "Message" } else { mail_element(item) };
    let raw = if (shape.include_mime || shape.wants("item:Body", !find) || shape.wants("item:Attachments", !find) || shape.wants("item:MimeContent", false)) && item.kind == ItemKind::Mail {
        ctx.app.store.mime(item.id).ok().flatten()
    } else {
        None
    };
    let mut s = format!("<t:{el}>");
    if let Some(raw) = raw.as_deref().filter(|_| shape.include_mime || shape.fields.iter().any(|f| f == "item:MimeContent")) {
        let _ = write!(s, r#"<t:MimeContent CharacterSet="UTF-8">{}</t:MimeContent>"#, B64.encode(raw));
    }
    let class = if item.kind == ItemKind::Note { "IPM.StickyNote".to_string() } else if item.message_class.is_empty() { "IPM.Note".into() } else { item.message_class.clone() };
    common_head(&mut s, item, shape, &class);
    if shape.wants("item:Subject", true) {
        push(&mut s, "Subject", &item.subject);
    }
    if shape.wants("item:Sensitivity", false) {
        push(&mut s, "Sensitivity", "Normal");
    }
    if shape.wants("item:Body", !find) {
        if item.kind == ItemKind::Note {
            s.push_str(&body_xml(shape, &item.note().unwrap_or_default().body, None));
        } else if let Some(raw) = &raw {
            let (text, html) = mime::bodies(raw);
            s.push_str(&body_xml(shape, text.as_deref().unwrap_or_default(), html.as_deref()));
        }
    }
    if shape.wants("item:Attachments", !find)
        && let Some(raw) = &raw
    {
        let atts = mime::attachments(raw);
        if !atts.is_empty() {
            s.push_str("<t:Attachments>");
            for a in &atts {
                s.push_str(&attachment_xml(item.id, a, None));
            }
            s.push_str("</t:Attachments>");
        }
    }
    if shape.wants("item:DateTimeReceived", true) {
        push(&mut s, "DateTimeReceived", &utc(item.received));
    }
    if shape.wants("item:Size", true) {
        push(&mut s, "Size", &item.size.to_string());
    }
    if shape.wants("item:Categories", false) {
        s.push_str(&categories(&item.categories));
    }
    if shape.wants("item:Importance", true) {
        push(&mut s, "Importance", item.importance.ews());
    }
    let draft = item.flags & flags::DRAFT != 0;
    if shape.wants("item:IsSubmitted", false) {
        push(&mut s, "IsSubmitted", "false");
    }
    if shape.wants("item:IsDraft", true) {
        push(&mut s, "IsDraft", bool_str(draft));
    }
    if shape.wants("item:IsFromMe", false) {
        let me = item.sender.as_ref().is_some_and(|p| p.email.eq_ignore_ascii_case(&ctx.user.email));
        push(&mut s, "IsFromMe", bool_str(me));
    }
    if shape.wants("item:DateTimeSent", true) {
        push(&mut s, "DateTimeSent", &utc(item.received));
    }
    if shape.wants("item:DateTimeCreated", true) {
        push(&mut s, "DateTimeCreated", &utc(item.received));
    }
    if shape.wants("item:DisplayCc", true) {
        push(&mut s, "DisplayCc", &display_list(&item.recipients.cc));
    }
    if shape.wants("item:DisplayTo", true) {
        push(&mut s, "DisplayTo", &display_list(&item.recipients.to));
    }
    if shape.wants("item:HasAttachments", true) {
        push(&mut s, "HasAttachments", bool_str(item.has_attachments));
    }
    for (tag, kind) in &shape.extended {
        if is_tag(tag, 0x0E08) {
            s.push_str(&ext_prop(tag, kind, &item.size.to_string()));
        } else if is_tag(tag, 0x0E07) {
            let v = (if item.flags & flags::READ != 0 { 1 } else { 0 }) | (if draft { 8 } else { 0 });
            s.push_str(&ext_prop(tag, kind, &v.to_string()));
        }
    }
    if shape.wants("item:LastModifiedTime", false) {
        push(&mut s, "LastModifiedTime", &utc(item.received));
    }
    if shape.wants("item:IsAssociated", false) {
        push(&mut s, "IsAssociated", "false");
    }
    if shape.wants("item:Flag", false) {
        let status = if item.flags & flags::FLAGGED != 0 {
            "Flagged"
        } else if item.flags & flags::COMPLETE != 0 {
            "Complete"
        } else {
            "NotFlagged"
        };
        let _ = write!(s, "<t:Flag><t:FlagStatus>{status}</t:FlagStatus></t:Flag>");
    }
    if shape.wants("item:Preview", false) {
        push(&mut s, "Preview", &item.preview);
    }
    // MessageType
    if shape.wants("message:Sender", false)
        && let Some(p) = &item.sender
    {
        s.push_str(&single("Sender", p));
    }
    if shape.wants("message:ToRecipients", !find) {
        s.push_str(&recipients("ToRecipients", &item.recipients.to));
    }
    if shape.wants("message:CcRecipients", !find) {
        s.push_str(&recipients("CcRecipients", &item.recipients.cc));
    }
    if shape.wants("message:BccRecipients", !find) {
        s.push_str(&recipients("BccRecipients", &item.recipients.bcc));
    }
    if shape.wants("message:IsReadReceiptRequested", false) {
        push(&mut s, "IsReadReceiptRequested", "false");
    }
    if shape.wants("message:ConversationTopic", false) {
        push(&mut s, "ConversationTopic", &item.subject);
    }
    if shape.wants("message:From", true)
        && let Some(p) = &item.sender
    {
        s.push_str(&single("From", p));
    }
    if shape.wants("message:InternetMessageId", false)
        && let Some(m) = &item.message_id
    {
        push(&mut s, "InternetMessageId", m);
    }
    if shape.wants("message:IsRead", true) {
        push(&mut s, "IsRead", bool_str(item.flags & flags::READ != 0));
    }
    if shape.wants("message:ReplyTo", false) && !item.recipients.reply_to.is_empty() {
        s.push_str(&recipients("ReplyTo", &item.recipients.reply_to));
    }
    if (el == "MeetingRequest" || el == "MeetingCancellation")
        && let Some(e) = item.data.as_deref().map(crate::ical::parse).and_then(|c| c.events.into_iter().next()) {
            let own = crate::calendar::find_by_uid(&ctx.app, item.user_id, &e.uid).ok().flatten();
            if let Some(o) = &own {
                let _ = write!(s, r#"<t:AssociatedCalendarItemId Id="I.{}" ChangeKey="C{}"/>"#, o.id, o.modseq);
            }
            push(&mut s, "HasBeenProcessed", "true");
            let response = own.and_then(|o| o.event()).map(|x| x.response).unwrap_or(Response::NotResponded);
            push(&mut s, "ResponseType", response_name(response));
            push(&mut s, "UID", &e.uid);
            if el == "MeetingRequest" {
                push(&mut s, "MeetingRequestType", "NewMeetingRequest");
                push(&mut s, "IntendedFreeBusyStatus", e.busy.ews());
            }
            push(&mut s, "Start", &utc(e.start));
            push(&mut s, "End", &utc(e.end));
            push(&mut s, "IsAllDayEvent", bool_str(e.all_day));
            push(&mut s, "Location", &e.location);
            push(&mut s, "IsMeeting", "true");
            push(&mut s, "IsCancelled", bool_str(el == "MeetingCancellation"));
            push(&mut s, "IsRecurring", bool_str(e.recurrence.is_some()));
            push(&mut s, "CalendarItemType", if e.recurrence.is_some() { "RecurringMaster" } else { "Single" });
            if let Some(o) = &e.organizer {
                s.push_str(&single("Organizer", o));
            }
        }
    let _ = write!(s, "</t:{el}>");
    s
}

pub fn response_name(r: Response) -> &'static str {
    match r {
        Response::None => "Unknown",
        Response::Organizer => "Organizer",
        Response::Tentative => "Tentative",
        Response::Accepted => "Accept",
        Response::Declined => "Decline",
        Response::NotResponded => "NoResponseReceived",
    }
}

fn attendee_list(name: &str, list: &[&crate::model::Attendee]) -> String {
    if list.is_empty() {
        return String::new();
    }
    let mut s = format!("<t:{name}>");
    for a in list {
        let _ = write!(s, "<t:Attendee>{}<t:ResponseType>{}</t:ResponseType></t:Attendee>", mailbox(&a.person, "OneOff"), response_name(a.status));
    }
    let _ = write!(s, "</t:{name}>");
    s
}

const DAY_NAMES: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTH_NAMES: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const INDEX_NAMES: [&str; 5] = ["First", "Second", "Third", "Fourth", "Last"];

pub fn days_of_week(mask: u8) -> String {
    match mask {
        127 => "Day".into(),
        62 => "Weekday".into(),
        65 => "WeekendDay".into(),
        _ => (0..7).filter(|i| mask & (1 << i) != 0).map(|i| DAY_NAMES[i]).collect::<Vec<_>>().join(" "),
    }
}

pub fn parse_days_of_week(s: &str) -> u8 {
    s.split_whitespace()
        .map(|d| match d {
            "Day" => 127,
            "Weekday" => 62,
            "WeekendDay" => 65,
            other => DAY_NAMES.iter().position(|n| n.eq_ignore_ascii_case(other)).map(|i| 1u8 << i).unwrap_or(0),
        })
        .fold(0, |a, b| a | b)
}

pub fn month_name(m: u32) -> &'static str {
    MONTH_NAMES[(m.clamp(1, 12) - 1) as usize]
}

pub fn parse_month(s: &str) -> u32 {
    MONTH_NAMES.iter().position(|m| m.eq_ignore_ascii_case(s.trim())).map(|i| i as u32 + 1).unwrap_or(1)
}

pub fn index_name(week: u32) -> &'static str {
    INDEX_NAMES[(week.clamp(1, 5) - 1) as usize]
}

pub fn parse_index(s: &str) -> u32 {
    INDEX_NAMES.iter().position(|m| m.eq_ignore_ascii_case(s.trim())).map(|i| i as u32 + 1).unwrap_or(1)
}

fn local_date(ts: i64, tz: Tz) -> String {
    DateTime::from_timestamp(ts, 0).unwrap_or_default().with_timezone(&tz).format("%Y-%m-%d").to_string()
}

pub fn recurrence_xml(r: &Recurrence, start: i64, tz: Tz) -> String {
    let pattern = match r.freq {
        Frequency::Daily if r.days != 0 => format!("<t:WeeklyRecurrence><t:Interval>1</t:Interval><t:DaysOfWeek>{}</t:DaysOfWeek></t:WeeklyRecurrence>", days_of_week(r.days)),
        Frequency::Daily => format!("<t:DailyRecurrence><t:Interval>{}</t:Interval></t:DailyRecurrence>", r.interval),
        Frequency::Weekly => format!(
            "<t:WeeklyRecurrence><t:Interval>{}</t:Interval><t:DaysOfWeek>{}</t:DaysOfWeek><t:FirstDayOfWeek>{}</t:FirstDayOfWeek></t:WeeklyRecurrence>",
            r.interval,
            days_of_week(r.days),
            DAY_NAMES[WEEKDAYS[(r.first_day_of_week % 7) as usize].num_days_from_sunday() as usize]
        ),
        Frequency::Monthly => format!("<t:AbsoluteMonthlyRecurrence><t:Interval>{}</t:Interval><t:DayOfMonth>{}</t:DayOfMonth></t:AbsoluteMonthlyRecurrence>", r.interval, r.day_of_month),
        Frequency::MonthlyNth => format!(
            "<t:RelativeMonthlyRecurrence><t:Interval>{}</t:Interval><t:DaysOfWeek>{}</t:DaysOfWeek><t:DayOfWeekIndex>{}</t:DayOfWeekIndex></t:RelativeMonthlyRecurrence>",
            r.interval,
            days_of_week(r.days),
            index_name(r.week_of_month)
        ),
        Frequency::Yearly => format!("<t:AbsoluteYearlyRecurrence><t:DayOfMonth>{}</t:DayOfMonth><t:Month>{}</t:Month></t:AbsoluteYearlyRecurrence>", r.day_of_month, month_name(r.month)),
        Frequency::YearlyNth => format!(
            "<t:RelativeYearlyRecurrence><t:DaysOfWeek>{}</t:DaysOfWeek><t:DayOfWeekIndex>{}</t:DayOfWeekIndex><t:Month>{}</t:Month></t:RelativeYearlyRecurrence>",
            days_of_week(r.days),
            index_name(r.week_of_month),
            month_name(r.month)
        ),
    };
    let sd = local_date(start, tz);
    let range = match (r.count, r.until) {
        (Some(n), _) => format!("<t:NumberedRecurrence><t:StartDate>{sd}</t:StartDate><t:NumberOfOccurrences>{n}</t:NumberOfOccurrences></t:NumberedRecurrence>"),
        (None, Some(u)) => format!("<t:EndDateRecurrence><t:StartDate>{sd}</t:StartDate><t:EndDate>{}</t:EndDate></t:EndDateRecurrence>", local_date(u, tz)),
        _ => format!("<t:NoEndRecurrence><t:StartDate>{sd}</t:StartDate></t:NoEndRecurrence>"),
    };
    format!("<t:Recurrence>{pattern}{range}</t:Recurrence>")
}

fn iso_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    let mut s = String::from("P");
    if d > 0 {
        let _ = write!(s, "{d}D");
    }
    s.push('T');
    let _ = write!(s, "{h}H{m}M");
    s
}

fn occurrence_info(e: &Event, item: &Item, o: &Occurrence) -> String {
    format!(
        r#"<t:ItemId Id="O.{}.{}"/><t:Start>{}</t:Start><t:End>{}</t:End><t:OriginalStart>{}</t:OriginalStart>"#,
        item.id,
        o.original_start,
        utc(o.start),
        utc(o.end),
        utc(o.original_start)
    )
    .replace("{e}", &e.uid)
}

/// Calendar item; `occ` renders one occurrence of a series.
pub fn event_xml(ctx: &Ctx, item: &Item, e: &Event, occ: Option<&Occurrence>, shape: &Shape, find: bool) -> String {
    let tz = e.zone();
    let ex = occ.and_then(|o| o.exception.as_ref());
    let subject = ex.and_then(|x| x.subject.clone()).unwrap_or_else(|| e.subject.clone());
    let location = ex.and_then(|x| x.location.clone()).unwrap_or_else(|| e.location.clone());
    let body = ex.and_then(|x| x.body.clone()).unwrap_or_else(|| e.body.clone());
    let busy = ex.and_then(|x| x.busy).unwrap_or(e.busy);
    let reminder = ex.and_then(|x| x.reminder).unwrap_or(e.reminder);
    let (start, end) = occ.map(|o| (o.start, o.end)).unwrap_or((e.start, e.end));
    let me: Vec<String> = ctx.app.store.send_as_addresses(item.user_id).unwrap_or_default();
    let organizer_is_me = e.organizer.as_ref().is_none_or(|o| o.email.is_empty() || me.iter().any(|m| m.eq_ignore_ascii_case(&o.email)));
    let meeting = !e.attendees.is_empty() || !organizer_is_me;

    let mut s = String::from("<t:CalendarItem>");
    match occ {
        Some(o) => {
            let _ = write!(s, r#"<t:ItemId Id="O.{}.{}" ChangeKey="C{}"/>"#, item.id, o.original_start, item.modseq);
        }
        None => s.push_str(&super::item_id_xml(item.id, item.modseq)),
    }
    if shape.wants("item:ParentFolderId", false) {
        let _ = write!(s, r#"<t:ParentFolderId Id="F.{}"/>"#, item.folder_id);
    }
    if shape.wants("item:ItemClass", true) {
        push(&mut s, "ItemClass", "IPM.Appointment");
    }
    if shape.wants("item:Subject", true) {
        push(&mut s, "Subject", &subject);
    }
    if shape.wants("item:Sensitivity", true) {
        push(&mut s, "Sensitivity", e.sensitivity.ews());
    }
    if shape.wants("item:Body", !find) {
        s.push_str(&body_xml(shape, &body, if ex.is_none() { e.body_html.as_deref() } else { None }));
    }
    if shape.wants("item:DateTimeReceived", false) {
        push(&mut s, "DateTimeReceived", &utc(item.received));
    }
    if shape.wants("item:Size", false) {
        push(&mut s, "Size", &item.size.to_string());
    }
    if shape.wants("item:Categories", false) {
        s.push_str(&categories(&e.categories));
    }
    if shape.wants("item:Importance", false) {
        push(&mut s, "Importance", "Normal");
    }
    if shape.wants("item:IsDraft", false) {
        push(&mut s, "IsDraft", "false");
    }
    if shape.wants("item:ReminderDueBy", false) {
        push(&mut s, "ReminderDueBy", &utc(start));
    }
    if shape.wants("item:ReminderIsSet", true) {
        push(&mut s, "ReminderIsSet", bool_str(reminder.is_some()));
    }
    if shape.wants("item:ReminderMinutesBeforeStart", true) {
        push(&mut s, "ReminderMinutesBeforeStart", &reminder.unwrap_or(15).to_string());
    }
    if shape.wants("item:DisplayTo", false) {
        let required: Vec<Person> = e.attendees.iter().filter(|a| a.kind == AttendeeKind::Required).map(|a| a.person.clone()).collect();
        push(&mut s, "DisplayTo", &display_list(&required));
    }
    if shape.wants("item:HasAttachments", true) {
        push(&mut s, "HasAttachments", "false");
    }
    if shape.wants("item:LastModifiedTime", false) {
        push(&mut s, "LastModifiedTime", &utc(item.received));
    }
    if shape.wants("item:Preview", false) {
        push(&mut s, "Preview", &body.chars().take(255).collect::<String>());
    }
    // CalendarItemType
    if shape.wants("calendar:UID", true) {
        push(&mut s, "UID", &e.uid);
    }
    if let Some(o) = occ {
        push(&mut s, "RecurrenceId", &utc(o.original_start));
    }
    if shape.wants("calendar:DateTimeStamp", false) && e.dtstamp > 0 {
        push(&mut s, "DateTimeStamp", &utc(e.dtstamp));
    }
    if shape.wants("calendar:Start", true) {
        push(&mut s, "Start", &utc(start));
    }
    if shape.wants("calendar:End", true) {
        push(&mut s, "End", &utc(end));
    }
    if let Some(o) = occ
        && shape.wants("calendar:OriginalStart", true)
    {
        push(&mut s, "OriginalStart", &utc(o.original_start));
    }
    if shape.wants("calendar:IsAllDayEvent", true) {
        push(&mut s, "IsAllDayEvent", bool_str(e.all_day));
    }
    if shape.wants("calendar:LegacyFreeBusyStatus", true) {
        push(&mut s, "LegacyFreeBusyStatus", busy.ews());
    }
    if shape.wants("calendar:Location", true) {
        push(&mut s, "Location", &location);
    }
    if shape.wants("calendar:IsMeeting", true) {
        push(&mut s, "IsMeeting", bool_str(meeting));
    }
    if shape.wants("calendar:IsCancelled", true) {
        push(&mut s, "IsCancelled", bool_str(e.cancelled || ex.and_then(|x| x.cancelled).unwrap_or(false)));
    }
    if shape.wants("calendar:IsRecurring", true) {
        push(&mut s, "IsRecurring", bool_str(e.recurrence.is_some()));
    }
    if shape.wants("calendar:MeetingRequestWasSent", false) {
        push(&mut s, "MeetingRequestWasSent", bool_str(meeting && organizer_is_me));
    }
    if shape.wants("calendar:IsResponseRequested", false) {
        push(&mut s, "IsResponseRequested", bool_str(meeting));
    }
    if shape.wants("calendar:CalendarItemType", true) {
        let kind = match (occ, &e.recurrence) {
            (Some(o), _) if o.exception.is_some() => "Exception",
            (Some(_), _) => "Occurrence",
            (None, Some(_)) => "RecurringMaster",
            (None, None) => "Single",
        };
        push(&mut s, "CalendarItemType", kind);
    }
    if shape.wants("calendar:MyResponseType", true) {
        let r = if organizer_is_me { if meeting { Response::Organizer } else { Response::None } } else { e.response };
        push(&mut s, "MyResponseType", if r == Response::None { "Organizer" } else { response_name(r) });
    }
    if shape.wants("calendar:Organizer", true) {
        let org = e.organizer.clone().unwrap_or_else(|| ctx.app.store.user(item.user_id).ok().flatten().map(|u| u.person()).unwrap_or_default());
        s.push_str(&single("Organizer", &org));
    }
    if shape.wants("calendar:RequiredAttendees", !find) {
        s.push_str(&attendee_list("RequiredAttendees", &e.attendees.iter().filter(|a| a.kind == AttendeeKind::Required).collect::<Vec<_>>()));
    }
    if shape.wants("calendar:OptionalAttendees", !find) {
        s.push_str(&attendee_list("OptionalAttendees", &e.attendees.iter().filter(|a| a.kind == AttendeeKind::Optional).collect::<Vec<_>>()));
    }
    if shape.wants("calendar:Resources", !find) {
        s.push_str(&attendee_list("Resources", &e.attendees.iter().filter(|a| a.kind == AttendeeKind::Resource).collect::<Vec<_>>()));
    }
    if shape.wants("calendar:Duration", false) {
        push(&mut s, "Duration", &iso_duration(end - start));
    }
    if shape.wants("calendar:TimeZone", false) {
        push(&mut s, "TimeZone", &tzinfo::windows_name(tz));
    }
    if shape.wants("calendar:AppointmentSequenceNumber", false) {
        push(&mut s, "AppointmentSequenceNumber", &e.sequence.to_string());
    }
    if shape.wants("calendar:AppointmentState", false) {
        let state = (if meeting { 1 } else { 0 }) | (if !organizer_is_me { 2 } else { 0 }) | (if e.cancelled { 4 } else { 0 });
        push(&mut s, "AppointmentState", &state.to_string());
    }
    if occ.is_none()
        && let Some(r) = &e.recurrence
    {
        if shape.wants("calendar:Recurrence", !find) {
            s.push_str(&recurrence_xml(r, e.start, tz));
        }
        let all = e.occurrences(e.start, e.start + 400 * 366 * 86_400);
        if shape.wants("calendar:FirstOccurrence", !find)
            && let Some(f) = all.first()
        {
            let _ = write!(s, "<t:FirstOccurrence>{}</t:FirstOccurrence>", occurrence_info(e, item, f));
        }
        if shape.wants("calendar:LastOccurrence", !find)
            && let Some(l) = all.last().filter(|_| r.count.is_some() || r.until.is_some())
        {
            let _ = write!(s, "<t:LastOccurrence>{}</t:LastOccurrence>", occurrence_info(e, item, l));
        }
        let modified: Vec<&Occurrence> = all.iter().filter(|o| o.exception.is_some()).collect();
        if shape.wants("calendar:ModifiedOccurrences", !find) && !modified.is_empty() {
            s.push_str("<t:ModifiedOccurrences>");
            for o in modified {
                let _ = write!(s, "<t:Occurrence>{}</t:Occurrence>", occurrence_info(e, item, o));
            }
            s.push_str("</t:ModifiedOccurrences>");
        }
        let deleted: Vec<i64> = e.exceptions.iter().filter(|x| x.deleted).map(|x| x.original_start).collect();
        if shape.wants("calendar:DeletedOccurrences", !find) && !deleted.is_empty() {
            s.push_str("<t:DeletedOccurrences>");
            for d in deleted {
                let _ = write!(s, "<t:DeletedOccurrence><t:Start>{}</t:Start></t:DeletedOccurrence>", utc(d));
            }
            s.push_str("</t:DeletedOccurrences>");
        }
    }
    if ctx.version.as_str() >= "Exchange2010" && shape.wants("calendar:StartTimeZone", false) {
        let _ = write!(s, r#"<t:StartTimeZone Id="{}" Name="{}"/>"#, escape(&tzinfo::windows_name(tz)), escape(tz.name()));
        let _ = write!(s, r#"<t:EndTimeZone Id="{}" Name="{}"/>"#, escape(&tzinfo::windows_name(tz)), escape(tz.name()));
    }
    if shape.wants("calendar:IsOnlineMeeting", false) {
        push(&mut s, "IsOnlineMeeting", bool_str(e.online_meeting_url.is_some()));
    }
    s.push_str("</t:CalendarItem>");
    s
}

fn contact_xml(item: &Item, c: &Contact, shape: &Shape, find: bool) -> String {
    let mut s = String::from("<t:Contact>");
    common_head(&mut s, item, shape, "IPM.Contact");
    if shape.wants("item:Subject", true) {
        push(&mut s, "Subject", &c.effective_name());
    }
    if shape.wants("item:Body", !find) && !c.notes.is_empty() {
        s.push_str(&body_xml(shape, &c.notes, None));
    }
    if shape.wants("item:Categories", false) {
        s.push_str(&categories(&c.categories));
    }
    if shape.wants("item:HasAttachments", false) {
        push(&mut s, "HasAttachments", "false");
    }
    if shape.wants("contacts:FileAs", true) {
        let file_as = if c.file_as.is_empty() { c.effective_name() } else { c.file_as.clone() };
        push(&mut s, "FileAs", &file_as);
    }
    if shape.wants("contacts:DisplayName", true) {
        push(&mut s, "DisplayName", &c.effective_name());
    }
    let opt = |s: &mut String, uri: &str, name: &str, v: &str| {
        if !v.is_empty() && shape.wants(uri, true) {
            push(s, name, v);
        }
    };
    opt(&mut s, "contacts:GivenName", "GivenName", &c.first_name);
    opt(&mut s, "contacts:MiddleName", "MiddleName", &c.middle_name);
    opt(&mut s, "contacts:Nickname", "Nickname", &c.nickname);
    opt(&mut s, "contacts:CompanyName", "CompanyName", &c.company);
    if shape.wants("contacts:EmailAddresses", true) && !c.emails.is_empty() {
        s.push_str("<t:EmailAddresses>");
        for (i, e) in c.emails.iter().take(3).enumerate() {
            let _ = write!(s, r#"<t:Entry Key="EmailAddress{}">{}</t:Entry>"#, i + 1, escape(e));
        }
        s.push_str("</t:EmailAddresses>");
    }
    if shape.wants("contacts:PhysicalAddresses", true) {
        let mut entries = String::new();
        for (key, a) in [("Business", &c.business_address), ("Home", &c.home_address), ("Other", &c.other_address)] {
            if a.is_empty() {
                continue;
            }
            let _ = write!(entries, r#"<t:Entry Key="{key}">"#);
            for (n, v) in [("Street", &a.street), ("City", &a.city), ("State", &a.state), ("CountryOrRegion", &a.country), ("PostalCode", &a.postal_code)] {
                if !v.is_empty() {
                    push(&mut entries, n, v);
                }
            }
            entries.push_str("</t:Entry>");
        }
        if !entries.is_empty() {
            let _ = write!(s, "<t:PhysicalAddresses>{entries}</t:PhysicalAddresses>");
        }
    }
    if shape.wants("contacts:PhoneNumbers", true) {
        let mut entries = String::new();
        for (key, v) in [
            ("AssistantPhone", &c.assistant_phone),
            ("BusinessFax", &c.business_fax),
            ("BusinessPhone", &c.business_phone),
            ("BusinessPhone2", &c.business_phone2),
            ("CarPhone", &c.car_phone),
            ("CompanyMainPhone", &c.company_main_phone),
            ("HomeFax", &c.home_fax),
            ("HomePhone", &c.home_phone),
            ("HomePhone2", &c.home_phone2),
            ("MobilePhone", &c.mobile_phone),
            ("Pager", &c.pager),
            ("RadioPhone", &c.radio_phone),
        ] {
            if !v.is_empty() {
                let _ = write!(entries, r#"<t:Entry Key="{key}">{}</t:Entry>"#, escape(v));
            }
        }
        if !entries.is_empty() {
            let _ = write!(s, "<t:PhoneNumbers>{entries}</t:PhoneNumbers>");
        }
    }
    opt(&mut s, "contacts:AssistantName", "AssistantName", &c.assistant);
    if let Some(b) = &c.birthday
        && shape.wants("contacts:Birthday", true)
    {
        push(&mut s, "Birthday", &format!("{}T11:59:00Z", &b[..10.min(b.len())]));
    }
    opt(&mut s, "contacts:BusinessHomePage", "BusinessHomePage", &c.web_page);
    if !c.children.is_empty() && shape.wants("contacts:Children", true) {
        let inner: String = c.children.iter().map(|x| format!("<t:String>{}</t:String>", escape(x))).collect();
        let _ = write!(s, "<t:Children>{inner}</t:Children>");
    }
    opt(&mut s, "contacts:Department", "Department", &c.department);
    opt(&mut s, "contacts:Generation", "Generation", &c.suffix);
    if !c.im.is_empty() && shape.wants("contacts:ImAddresses", true) {
        s.push_str("<t:ImAddresses>");
        for (i, x) in c.im.iter().take(3).enumerate() {
            let _ = write!(s, r#"<t:Entry Key="ImAddress{}">{}</t:Entry>"#, i + 1, escape(x));
        }
        s.push_str("</t:ImAddresses>");
    }
    opt(&mut s, "contacts:JobTitle", "JobTitle", &c.job_title);
    opt(&mut s, "contacts:Manager", "Manager", &c.manager);
    opt(&mut s, "contacts:OfficeLocation", "OfficeLocation", &c.office);
    opt(&mut s, "contacts:SpouseName", "SpouseName", &c.spouse);
    opt(&mut s, "contacts:Surname", "Surname", &c.last_name);
    if let Some(b) = &c.anniversary
        && shape.wants("contacts:WeddingAnniversary", true)
    {
        push(&mut s, "WeddingAnniversary", &format!("{}T11:59:00Z", &b[..10.min(b.len())]));
    }
    s.push_str("</t:Contact>");
    s
}

fn task_xml(item: &Item, t: &Task, shape: &Shape, find: bool) -> String {
    let mut s = String::from("<t:Task>");
    common_head(&mut s, item, shape, "IPM.Task");
    if shape.wants("item:Subject", true) {
        push(&mut s, "Subject", &t.subject);
    }
    if shape.wants("item:Sensitivity", true) {
        push(&mut s, "Sensitivity", t.sensitivity.ews());
    }
    if shape.wants("item:Body", !find) {
        s.push_str(&body_xml(shape, &t.body, None));
    }
    if shape.wants("item:Categories", true) {
        s.push_str(&categories(&t.categories));
    }
    if shape.wants("item:Importance", true) {
        push(&mut s, "Importance", t.importance.ews());
    }
    if let Some(r) = t.reminder
        && shape.wants("item:ReminderDueBy", true) {
            push(&mut s, "ReminderDueBy", &utc(r));
        }
    if shape.wants("item:ReminderIsSet", true) {
        push(&mut s, "ReminderIsSet", bool_str(t.reminder.is_some()));
    }
    if let Some(c) = t.completed_at.filter(|_| t.complete)
        && shape.wants("task:CompleteDate", true)
    {
        push(&mut s, "CompleteDate", &utc(c));
    }
    if let Some(d) = t.due
        && shape.wants("task:DueDate", true)
    {
        push(&mut s, "DueDate", &utc(d));
    }
    if shape.wants("task:IsComplete", true) {
        push(&mut s, "IsComplete", bool_str(t.complete));
    }
    if shape.wants("task:IsRecurring", true) {
        push(&mut s, "IsRecurring", "false");
    }
    if shape.wants("task:PercentComplete", true) {
        push(&mut s, "PercentComplete", &(if t.complete { 100 } else { t.percent_complete }).to_string());
    }
    if let Some(st) = t.start
        && shape.wants("task:StartDate", true)
    {
        push(&mut s, "StartDate", &utc(st));
    }
    if shape.wants("task:Status", true) {
        let status = if t.complete {
            "Completed"
        } else if t.percent_complete > 0 {
            "InProgress"
        } else {
            "NotStarted"
        };
        push(&mut s, "Status", status);
    }
    s.push_str("</t:Task>");
    s
}

pub fn busy_type(b: BusyStatus) -> &'static str {
    match b {
        BusyStatus::Free => "Free",
        BusyStatus::Tentative => "Tentative",
        BusyStatus::Oof => "OOF",
        BusyStatus::WorkingElsewhere => "WorkingElsewhere",
        BusyStatus::Busy => "Busy",
    }
}

