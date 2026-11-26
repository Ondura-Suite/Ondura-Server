//! EWS tests through the HTTP router, using the request shapes of Ondura Mail.

use axum::body::Body;
use axum::http::Request;
use base64::Engine;
use tower::ServiceExt;

use crate::app::{App, Shared};
use crate::xml::Node;

async fn call(app: &Shared, body: &str) -> Node {
    let auth = base64::engine::general_purpose::STANDARD.encode("niklas@example.com:secret");
    let soap = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/" xmlns:t="http://schemas.microsoft.com/exchange/services/2006/types" xmlns:m="http://schemas.microsoft.com/exchange/services/2006/messages"><soap:Header><t:RequestServerVersion Version="Exchange2013_SP1"/></soap:Header><soap:Body>{body}</soap:Body></soap:Envelope>"#
    );
    let req = Request::post("/EWS/Exchange.asmx").header("Authorization", format!("Basic {auth}")).header("Content-Type", "text/xml").body(Body::from(soap)).unwrap();
    let resp = crate::http::router(app.clone()).oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(!text.contains("ResponseClass=\"Error\"") || body.contains("expect-error"), "{text}");
    Node::parse(&text).unwrap()
}

fn all<'a>(n: &'a Node, name: &str) -> Vec<&'a Node> {
    let mut v = Vec::new();
    n.find_all(name, &mut v);
    v
}

#[tokio::test]
async fn ondura_mail_requests() {
    let (app, _) = App::test();
    // Account test and well-known folders.
    let doc = call(&app, r#"<m:GetFolder><m:FolderShape><t:BaseShape>IdOnly</t:BaseShape></m:FolderShape><m:FolderIds><t:DistinguishedFolderId Id="msgfolderroot"/><t:DistinguishedFolderId Id="inbox"/><t:DistinguishedFolderId Id="sentitems"/></m:FolderIds></m:GetFolder>"#).await;
    let ids: Vec<String> = all(&doc, "FolderId").iter().map(|f| f.attr("Id").unwrap().to_string()).collect();
    assert_eq!(ids.len(), 3);
    let inbox = ids[1].clone();

    let doc = call(&app, r#"<m:FindFolder Traversal="Deep"><m:FolderShape><t:BaseShape>Default</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI="folder:ParentFolderId"/><t:FieldURI FieldURI="folder:FolderClass"/></t:AdditionalProperties></m:FolderShape><m:ParentFolderIds><t:DistinguishedFolderId Id="msgfolderroot"/></m:ParentFolderIds></m:FindFolder>"#).await;
    assert!(all(&doc, "Folder").len() >= 6);
    assert_eq!(all(&doc, "CalendarFolder").len(), 1);
    assert_eq!(all(&doc, "ContactsFolder").len(), 1);

    // Import a message (like import_message), then sync it.
    let mime = base64::engine::general_purpose::STANDARD.encode("From: Max <max@other.org>\r\nTo: niklas@example.com\r\nSubject: Angebot\r\nContent-Type: text/plain\r\n\r\nHallo Niklas");
    call(&app, &format!(r#"<m:CreateItem MessageDisposition="SaveOnly"><m:SavedItemFolderId><t:FolderId Id="{inbox}"/></m:SavedItemFolderId><m:Items><t:Message><t:MimeContent CharacterSet="UTF-8">{mime}</t:MimeContent><t:ExtendedProperty><t:ExtendedFieldURI PropertyTag="3591" PropertyType="Integer"/><t:Value>0</t:Value></t:ExtendedProperty></t:Message></m:Items></m:CreateItem>"#)).await;
    let props = r#"<t:AdditionalProperties><t:FieldURI FieldURI="item:Subject"/><t:FieldURI FieldURI="message:From"/><t:FieldURI FieldURI="message:IsRead"/><t:FieldURI FieldURI="item:Preview"/><t:FieldURI FieldURI="item:Flag"/><t:FieldURI FieldURI="item:Categories"/></t:AdditionalProperties>"#;
    let doc = call(&app, &format!(r#"<m:SyncFolderItems><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape>{props}</m:ItemShape><m:SyncFolderId><t:FolderId Id="{inbox}"/></m:SyncFolderId><m:MaxChangesReturned>512</m:MaxChangesReturned></m:SyncFolderItems>"#)).await;
    let create = doc.find("Create").unwrap();
    assert_eq!(create.find("Subject").unwrap().text, "Angebot");
    assert_eq!(create.find("IsRead").unwrap().text, "false");
    assert_eq!(create.find("Preview").unwrap().text, "Hallo Niklas");
    let item_id = create.find("ItemId").unwrap().attr("Id").unwrap().to_string();
    let state = doc.find("SyncState").unwrap().text.clone();

    // Flags, then incremental sync sees an update.
    call(&app, &format!(r#"<m:UpdateItem MessageDisposition="SaveOnly" ConflictResolution="AlwaysOverwrite"><m:ItemChanges><t:ItemChange><t:ItemId Id="{item_id}"/><t:Updates><t:SetItemField><t:FieldURI FieldURI="message:IsRead"/><t:Message><t:IsRead>true</t:IsRead></t:Message></t:SetItemField><t:SetItemField><t:FieldURI FieldURI="item:Flag"/><t:Message><t:Flag><t:FlagStatus>Flagged</t:FlagStatus></t:Flag></t:Message></t:SetItemField></t:Updates></t:ItemChange></m:ItemChanges></m:UpdateItem>"#)).await;
    let doc = call(&app, &format!(r#"<m:SyncFolderItems><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape>{props}</m:ItemShape><m:SyncFolderId><t:FolderId Id="{inbox}"/></m:SyncFolderId><m:SyncState>{state}</m:SyncState><m:MaxChangesReturned>512</m:MaxChangesReturned></m:SyncFolderItems>"#)).await;
    let update = doc.find("Update").unwrap();
    assert_eq!(update.find("IsRead").unwrap().text, "true");
    assert_eq!(update.find("FlagStatus").unwrap().text, "Flagged");

    // MIME body.
    let doc = call(&app, &format!(r#"<m:GetItem><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:IncludeMimeContent>true</t:IncludeMimeContent></m:ItemShape><m:ItemIds><t:ItemId Id="{item_id}"/></m:ItemIds></m:GetItem>"#)).await;
    let mime_back = base64::engine::general_purpose::STANDARD.decode(doc.find("MimeContent").unwrap().text.trim()).unwrap();
    assert!(String::from_utf8_lossy(&mime_back).contains("Hallo Niklas"));

    // Search.
    let doc = call(&app, &format!(r#"<m:FindItem Traversal="Shallow"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape>{props}<t:FieldURI FieldURI="item:ParentFolderId"/></m:ItemShape><m:IndexedPageItemView MaxEntriesReturned="10" Offset="0" BasePoint="Beginning"/><m:SortOrder><t:FieldOrder Order="Descending"><t:FieldURI FieldURI="item:DateTimeReceived"/></t:FieldOrder></m:SortOrder><m:ParentFolderIds><t:DistinguishedFolderId Id="inbox"/><t:DistinguishedFolderId Id="sentitems"/></m:ParentFolderIds><m:QueryString>Angebot</m:QueryString></m:FindItem>"#)).await;
    assert_eq!(all(&doc, "Message").len(), 1);

    // Weekly series in the calendar, viewed via CalendarView, one occurrence moved.
    let doc = call(&app, r#"<m:CreateItem SendMeetingInvitations="SendToNone"><m:SavedItemFolderId><t:DistinguishedFolderId Id="calendar"/></m:SavedItemFolderId><m:Items><t:CalendarItem><t:Subject>Jour fixe</t:Subject><t:Body BodyType="Text">Agenda</t:Body><t:ReminderIsSet>true</t:ReminderIsSet><t:ReminderMinutesBeforeStart>10</t:ReminderMinutesBeforeStart><t:Start>2026-11-16T08:00:00Z</t:Start><t:End>2026-11-16T09:00:00Z</t:End><t:IsAllDayEvent>false</t:IsAllDayEvent><t:LegacyFreeBusyStatus>Busy</t:LegacyFreeBusyStatus><t:Location>Raum 2</t:Location><t:Recurrence><t:WeeklyRecurrence><t:Interval>1</t:Interval><t:DaysOfWeek>Monday</t:DaysOfWeek></t:WeeklyRecurrence><t:NumberedRecurrence><t:StartDate>2026-11-16</t:StartDate><t:NumberOfOccurrences>4</t:NumberOfOccurrences></t:NumberedRecurrence></t:Recurrence></t:CalendarItem></m:Items></m:CreateItem>"#).await;
    assert!(doc.find("ItemId").is_some());
    let cal = |doc: &Node| all(doc, "CalendarItem").iter().map(|c| (c.find("ItemId").unwrap().attr("Id").unwrap().to_string(), c.find("Start").unwrap().text.clone(), c.find("CalendarItemType").unwrap().text.clone())).collect::<Vec<_>>();
    let view = r#"<m:FindItem Traversal="Shallow"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI="item:Subject"/><t:FieldURI FieldURI="calendar:Start"/><t:FieldURI FieldURI="calendar:End"/><t:FieldURI FieldURI="calendar:CalendarItemType"/><t:FieldURI FieldURI="item:ReminderMinutesBeforeStart"/></t:AdditionalProperties></m:ItemShape><m:CalendarView MaxEntriesReturned="1000" StartDate="2026-11-01T00:00:00Z" EndDate="2026-12-31T00:00:00Z"/><m:ParentFolderIds><t:DistinguishedFolderId Id="calendar"/></m:ParentFolderIds></m:FindItem>"#;
    let doc = call(&app, view).await;
    let occ = cal(&doc);
    assert_eq!(occ.len(), 4);
    assert_eq!(occ[0].2, "Occurrence");
    let second = occ[1].0.clone();
    call(&app, &format!(r#"<m:UpdateItem ConflictResolution="AlwaysOverwrite" SendMeetingInvitationsOrCancellations="SendToNone"><m:ItemChanges><t:ItemChange><t:ItemId Id="{second}"/><t:Updates><t:SetItemField><t:FieldURI FieldURI="calendar:Start"/><t:CalendarItem><t:Start>2026-11-23T13:00:00Z</t:Start></t:CalendarItem></t:SetItemField><t:SetItemField><t:FieldURI FieldURI="calendar:End"/><t:CalendarItem><t:End>2026-11-23T14:00:00Z</t:End></t:CalendarItem></t:SetItemField></t:Updates></t:ItemChange></m:ItemChanges></m:UpdateItem>"#)).await;
    let occ = cal(&call(&app, view).await);
    assert_eq!(occ[1].1, "2026-11-23T13:00:00Z");
    assert_eq!(occ[1].2, "Exception");
    // Delete the third occurrence.
    call(&app, &format!(r#"<m:DeleteItem DeleteType="MoveToDeletedItems" SendMeetingCancellations="SendToNone"><m:ItemIds><t:ItemId Id="{}"/></m:ItemIds></m:DeleteItem>"#, occ[2].0)).await;
    assert_eq!(cal(&call(&app, view).await).len(), 3);
    // Master start via RecurringMasterItemId.
    let doc = call(&app, &format!(r#"<m:GetItem><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI="calendar:Start"/></t:AdditionalProperties></m:ItemShape><m:ItemIds><t:RecurringMasterItemId OccurrenceId="{}"/></m:ItemIds></m:GetItem>"#, occ[0].0)).await;
    assert_eq!(doc.find("Start").unwrap().text, "2026-11-16T08:00:00Z");

    // Contacts like Ondura creates them; read back with AllProperties.
    call(&app, r#"<m:CreateItem><m:SavedItemFolderId><t:DistinguishedFolderId Id="contacts"/></m:SavedItemFolderId><m:Items><t:Contact><t:Body BodyType="Text">Notiz</t:Body><t:FileAs>Max Becker</t:FileAs><t:DisplayName>Max Becker</t:DisplayName><t:GivenName>Max</t:GivenName><t:EmailAddresses><t:Entry Key="EmailAddress1">max@other.org</t:Entry></t:EmailAddresses><t:PhysicalAddresses><t:Entry Key="Business"><t:Street>Hauptstr. 1</t:Street><t:City>Homburg</t:City></t:Entry></t:PhysicalAddresses><t:PhoneNumbers><t:Entry Key="MobilePhone">0170 1</t:Entry></t:PhoneNumbers><t:Birthday>1990-05-01T11:59:00Z</t:Birthday><t:Surname>Becker</t:Surname></t:Contact></m:Items></m:CreateItem>"#).await;
    let doc = call(&app, r#"<m:FindItem Traversal="Shallow"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape></m:ItemShape><m:IndexedPageItemView MaxEntriesReturned="2000" Offset="0" BasePoint="Beginning"/><m:ParentFolderIds><t:DistinguishedFolderId Id="contacts"/></m:ParentFolderIds></m:FindItem>"#).await;
    let cid = doc.find("ItemId").unwrap().attr("Id").unwrap().to_string();
    let doc = call(&app, &format!(r#"<m:GetItem><m:ItemShape><t:BaseShape>AllProperties</t:BaseShape><t:BodyType>Text</t:BodyType></m:ItemShape><m:ItemIds><t:ItemId Id="{cid}"/></m:ItemIds></m:GetItem>"#)).await;
    let c = doc.find("Contact").unwrap();
    assert_eq!(c.find("Surname").unwrap().text, "Becker");
    assert_eq!(c.find("City").unwrap().text, "Homburg");
    assert_eq!(c.child("Body").unwrap().text, "Notiz");
    assert_eq!(c.find("Birthday").unwrap().text, "1990-05-01T11:59:00Z");

    // Out of office round trip.
    call(&app, r#"<m:SetUserOofSettingsRequest><t:Mailbox><t:Address>niklas@example.com</t:Address></t:Mailbox><t:UserOofSettings><t:OofState>Enabled</t:OofState><t:ExternalAudience>All</t:ExternalAudience><t:Duration><t:StartTime>2026-12-20T08:00:00Z</t:StartTime><t:EndTime>2027-01-06T17:00:00Z</t:EndTime></t:Duration><t:InternalReply><t:Message>Urlaub</t:Message></t:InternalReply><t:ExternalReply><t:Message>Away</t:Message></t:ExternalReply></t:UserOofSettings></m:SetUserOofSettingsRequest>"#).await;
    let doc = call(&app, r#"<m:GetUserOofSettingsRequest><t:Mailbox><t:Address>niklas@example.com</t:Address></t:Mailbox></m:GetUserOofSettingsRequest>"#).await;
    assert_eq!(doc.find("OofState").unwrap().text, "Enabled");

    // Free/busy in UTC like Ondura asks.
    let tz = r#"<t:TimeZone><t:Bias>0</t:Bias><t:StandardTime><t:Bias>0</t:Bias><t:Time>00:00:00</t:Time><t:DayOrder>1</t:DayOrder><t:Month>1</t:Month><t:DayOfWeek>Sunday</t:DayOfWeek></t:StandardTime><t:DaylightTime><t:Bias>0</t:Bias><t:Time>00:00:00</t:Time><t:DayOrder>1</t:DayOrder><t:Month>1</t:Month><t:DayOfWeek>Sunday</t:DayOfWeek></t:DaylightTime></t:TimeZone>"#;
    let doc = call(&app, &format!(r#"<m:GetUserAvailabilityRequest>{tz}<m:MailboxDataArray><t:MailboxData><t:Email><t:Address>niklas@example.com</t:Address></t:Email><t:AttendeeType>Required</t:AttendeeType><t:ExcludeConflicts>false</t:ExcludeConflicts></t:MailboxData></m:MailboxDataArray><t:FreeBusyViewOptions><t:TimeWindow><t:StartTime>2026-11-16T00:00:00</t:StartTime><t:EndTime>2026-11-17T00:00:00</t:EndTime></t:TimeWindow><t:MergedFreeBusyIntervalInMinutes>30</t:MergedFreeBusyIntervalInMinutes><t:RequestedView>Detailed</t:RequestedView></t:FreeBusyViewOptions></m:GetUserAvailabilityRequest>"#)).await;
    let ev = doc.find("CalendarEvent").unwrap();
    assert_eq!(ev.text_of("StartTime"), Some("2026-11-16T08:00:00"));

    // Directory.
    let doc = call(&app, r#"<m:ResolveNames ReturnFullContactData="true" SearchScope="ActiveDirectoryContacts"><m:UnresolvedEntry>nik</m:UnresolvedEntry></m:ResolveNames>"#).await;
    assert_eq!(doc.find("Resolution").unwrap().find("EmailAddress").unwrap().text, "niklas@example.com");

    // Streaming subscription exists.
    let doc = call(&app, &format!(r#"<m:Subscribe><m:StreamingSubscriptionRequest><t:FolderIds><t:FolderId Id="{inbox}"/></t:FolderIds><t:EventTypes><t:EventType>NewMailEvent</t:EventType></t:EventTypes></m:StreamingSubscriptionRequest></m:Subscribe>"#)).await;
    assert!(!doc.find("SubscriptionId").unwrap().text.is_empty());
}

#[tokio::test]
async fn get_inbox_rules_lists_stored_rules() {
    use crate::store::{RuleAction, RuleCondition};
    let (app, user) = App::test();
    let archive = app.store.ensure_folder(user.id, "archive").unwrap();
    app.store
        .save_rule(
            user.id,
            None,
            "boss to archive",
            true,
            true,
            true,
            &[RuleCondition { field: "from".into(), value: "boss@corp".into() }],
            &[RuleAction { kind: "move".into(), folder: Some(archive.id), category: None }, RuleAction { kind: "markRead".into(), folder: None, category: None }],
        )
        .unwrap();
    let doc = call(&app, r#"<m:GetInboxRules><m:MailboxSmtpAddress>niklas@example.com</m:MailboxSmtpAddress></m:GetInboxRules>"#).await;
    let rules = all(&doc, "Rule");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].text_of("DisplayName"), Some("boss to archive"));
    assert_eq!(rules[0].text_of("IsEnabled"), Some("true"));
    assert_eq!(all(&doc, "ContainsSenderStrings").len(), 1);
    assert_eq!(all(&doc, "MoveToFolder").len(), 1);
    assert_eq!(all(&doc, "MarkAsRead").len(), 1);
    assert_eq!(all(&doc, "StopProcessingRules").len(), 1);
}

#[tokio::test]
async fn category_master_list_round_trip() {
    use base64::Engine;
    let (app, user) = App::test();
    // A webmail category shows up in the master list with its Outlook colour index.
    app.store.create_category(user.id, "Work", "green").unwrap();
    let doc = call(&app, r#"<m:GetUserConfiguration><m:UserConfigurationName Name="CategoryList"><t:DistinguishedFolderId Id="calendar"/></m:UserConfigurationName><m:UserConfigurationProperties>All</m:UserConfigurationProperties></m:GetUserConfiguration>"#).await;
    let data = doc.find("XmlData").unwrap().text.clone();
    let xml = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(data.trim()).unwrap()).unwrap();
    assert!(xml.contains(r#"name="Work""#), "{xml}");
    assert!(xml.contains(r#"color="4""#), "{xml}");

    // A category created in Outlook is merged into the registry.
    let newxml = r#"<?xml version="1.0"?><categories xmlns="CategoryList.xsd"><category name="Urgent" color="0" guid="{11111111-1111-1111-1111-111111111111}"/></categories>"#;
    let b64 = base64::engine::general_purpose::STANDARD.encode(newxml.as_bytes());
    call(&app, &format!(r#"<m:UpdateUserConfiguration><m:UserConfiguration><t:UserConfigurationName Name="CategoryList"><t:DistinguishedFolderId Id="calendar"/></t:UserConfigurationName><t:XmlData>{b64}</t:XmlData></m:UserConfiguration></m:UpdateUserConfiguration>"#)).await;
    let cats = app.store.categories(user.id).unwrap();
    assert!(cats.iter().any(|(_, n, c)| n == "Urgent" && c == "red"), "{cats:?}");
}
