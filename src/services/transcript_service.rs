//! Ticket transcripts: renders a closed ticket as plain text and delivers it, as a `.txt` file, to the
//! operator's transcript log webhook. The webhook's guild and channel are verified before anything is sent.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use serenity::all::Message;

use crate::config::LogDestination;
use crate::logger;
use crate::services::audit_log_service::{retry_delay_ms, validate_audit_log_config, BoxFut, ValidAuditLogConfig, WebhookResponse};

/// Discord's attachment limit for webhooks and DMs is 10 MiB; stay well under it.
pub const MAX_TRANSCRIPT_BYTES: usize = 8 * 1024 * 1024;
/// Tickets longer than this are transcribed up to the limit, newest messages last.
pub const MAX_TRANSCRIPT_MESSAGES: usize = 10_000;
const RETRIES: u32 = 3;

/// One file upload with its message payload.
#[derive(Clone, Debug)]
pub struct Upload {
    pub payload_json: String,
    pub filename: String,
    pub data: Vec<u8>,
}

/// `(url, None)` = GET (validation), `(url, Some(upload))` = multipart POST (delivery).
pub type UploadTransport = Arc<dyn Fn(String, Option<Upload>) -> BoxFut<anyhow::Result<WebhookResponse>> + Send + Sync>;

pub fn reqwest_upload_transport(client: reqwest::Client) -> UploadTransport {
    Arc::new(move |url, upload| {
        let client = client.clone();
        Box::pin(async move {
            let request = match upload {
                None => client.get(&url).timeout(Duration::from_secs(5)),
                Some(upload) => {
                    let file = reqwest::multipart::Part::bytes(upload.data).file_name(upload.filename).mime_str("text/plain; charset=utf-8")?;
                    let form = reqwest::multipart::Form::new().text("payload_json", upload.payload_json).part("files[0]", file);
                    client.post(&url).multipart(form).timeout(Duration::from_secs(30))
                }
            };
            let response = request.send().await?;
            let status = response.status().as_u16();
            let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(String::from);
            let body = response.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
            Ok(WebhookResponse { status, retry_after, body })
        })
    })
}

pub struct TranscriptLogService {
    config: Option<ValidAuditLogConfig>,
    validated: AtomicBool,
    request: UploadTransport,
}

impl TranscriptLogService {
    pub fn new(configured: LogDestination, client: reqwest::Client) -> Self {
        Self::with_transport(configured, reqwest_upload_transport(client))
    }

    pub fn with_transport(configured: LogDestination, request: UploadTransport) -> Self {
        let config = match validate_audit_log_config(&configured) {
            Ok(config) => Some(config),
            Err(reason) => {
                if reason != "not configured" {
                    logger::warn("[transcripts] transcript logging disabled", crate::ctx! {"reason" => reason});
                }
                None
            }
        };
        Self { config, validated: AtomicBool::new(false), request }
    }

    pub fn enabled(&self) -> bool {
        self.config.is_some() && self.validated.load(Ordering::SeqCst)
    }

    /// The operator's transcript webhook only takes transcripts from its own server.
    pub fn enabled_for(&self, guild_id: &str) -> bool {
        self.enabled() && self.config.as_ref().is_some_and(|c| c.guild_id == guild_id)
    }

    /// Called after ready: the webhook must belong to the configured guild and channel.
    pub async fn initialize(&self) {
        let Some(config) = &self.config else { return };
        for attempt in 0..=RETRIES {
            match (self.request)(config.webhook_url.clone(), None).await {
                Ok(response) if response.ok() => {
                    let webhook: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
                    let matches = webhook["guild_id"].as_str() == Some(config.guild_id.as_str())
                        && webhook["channel_id"].as_str() == Some(config.channel_id.as_str());
                    if matches {
                        self.validated.store(true, Ordering::SeqCst);
                        logger::info("[transcripts] transcript logging enabled", crate::ctx! {"guildId" => config.guild_id, "channelId" => config.channel_id});
                    } else {
                        logger::warn("[transcripts] webhook does not belong to TRANSCRIPT_GUILD_ID / TRANSCRIPT_CHANNEL_ID; transcript logging disabled", crate::ctx! {});
                    }
                    return;
                }
                Ok(response) if response.status != 429 && response.status < 500 => {
                    logger::warn("[transcripts] webhook unavailable; transcript logging disabled", crate::ctx! {"status" => response.status});
                    return;
                }
                _ if attempt < RETRIES => tokio::time::sleep(Duration::from_millis(2_000 * (attempt as u64 + 1))).await,
                _ => logger::warn("[transcripts] webhook validation failed after retries", crate::ctx! {}),
            }
        }
    }

    /// Sends one transcript file with its summary embed. False when disabled, for another server, or every attempt failed.
    pub async fn deliver(&self, guild_id: &str, embed: Value, filename: &str, data: &[u8]) -> bool {
        let Some(config) = self.config.as_ref().filter(|_| self.enabled_for(guild_id)) else { return false };
        let payload_json = json!({
            "embeds": [embed],
            "allowed_mentions": {"parse": [], "users": [], "roles": [], "replied_user": false},
        })
        .to_string();
        let upload = Upload { payload_json, filename: filename.to_string(), data: data.to_vec() };
        for attempt in 0..=RETRIES {
            match (self.request)(config.webhook_url.clone(), Some(upload.clone())).await {
                Ok(response) if response.ok() => return true,
                Ok(response) if response.status != 429 && response.status < 500 => {
                    logger::warn("[transcripts] transcript delivery rejected", crate::ctx! {"status" => response.status});
                    return false;
                }
                Ok(response) if attempt < RETRIES => {
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms(response.retry_after.as_deref(), attempt))).await
                }
                Err(_) if attempt < RETRIES => tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await,
                _ => {}
            }
        }
        logger::warn("[transcripts] transcript delivery retries exhausted", crate::ctx! {});
        false
    }
}

/// Everything the transcript header and summaries show about a closed ticket.
#[derive(Clone, Debug, Default)]
pub struct TranscriptMeta {
    pub guild_name: String,
    pub title: String,
    pub channel_name: String,
    pub opener: String,
    pub claimer: Option<String>,
    pub closer: String,
    pub reason: Option<String>,
    /// `name (id)` for every participant: opener, claimer, added members and everyone who wrote.
    pub participants: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub deleted_at: chrono::DateTime<chrono::Utc>,
}

pub fn utc(time: chrono::DateTime<chrono::Utc>) -> String {
    time.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn message_time(message: &Message) -> String {
    chrono::DateTime::from_timestamp(message.timestamp.unix_timestamp(), 0).map(utc).unwrap_or_default()
}

/// The `.txt` transcript: a header, then every message oldest first. Bounded by [`MAX_TRANSCRIPT_BYTES`].
pub fn render_transcript(meta: &TranscriptMeta, messages: &[Message], truncated: bool) -> String {
    let rule = "=".repeat(72);
    let mut out = format!(
        "{rule}\nKori Ticket Transcript\n{rule}\nTicket:       {}\nChannel:      #{}\nServer:       {}\nOpened by:    {}\nClaimed by:   {}\nClosed by:    {}\nReason:       {}\nCreated:      {}\nDeleted:      {}\nMessages:     {}{}\nParticipants:\n{}\n{rule}\n\n",
        meta.title,
        meta.channel_name,
        meta.guild_name,
        meta.opener,
        meta.claimer.as_deref().unwrap_or("Nobody"),
        meta.closer,
        meta.reason.as_deref().unwrap_or("No reason provided"),
        utc(meta.created_at),
        utc(meta.deleted_at),
        messages.len(),
        if truncated { format!(" (only the latest {MAX_TRANSCRIPT_MESSAGES} are included)") } else { String::new() },
        meta.participants.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"),
    );
    // User mentions read as names when anyone in the ticket wrote or was mentioned; unknown ids stay as they are.
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in messages {
        names.insert(m.author.id.to_string(), m.author.tag());
        for user in &m.mentions {
            names.insert(user.id.to_string(), user.tag());
        }
    }
    static MENTION: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"<@!?(\d+)>").unwrap());
    let readable = |text: &str| {
        MENTION.replace_all(text, |c: &regex::Captures| names.get(&c[1]).map(|n| format!("@{n}")).unwrap_or_else(|| c[0].to_string())).into_owned()
    };
    let humans_wrote = messages.iter().any(|m| !m.author.bot);
    let content_missing = humans_wrote && messages.iter().filter(|m| !m.author.bot).all(|m| m.content.is_empty() && m.attachments.is_empty() && m.embeds.is_empty() && m.sticker_items.is_empty());
    if content_missing {
        out.push_str("NOTE: message text is empty. Enable the Message Content Intent for this bot in the Discord Developer Portal.\n\n");
    }
    for message in messages {
        let mut entry = format!("[{}] {} ({})", message_time(message), message.author.tag(), message.author.id);
        if message.edited_timestamp.is_some() {
            entry.push_str(" (edited)");
        }
        entry.push_str(":\n");
        if !message.content.is_empty() {
            for line in readable(&message.content).lines() {
                entry.push_str(&format!("    {line}\n"));
            }
        }
        for embed in &message.embeds {
            let text = [embed.title.as_deref(), embed.description.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" — ");
            entry.push_str(&format!("    [embed] {}\n", readable(&text).replace('\n', " ")));
        }
        for attachment in &message.attachments {
            entry.push_str(&format!("    [attachment] {} {}\n", attachment.filename, attachment.url));
        }
        for sticker in &message.sticker_items {
            entry.push_str(&format!("    [sticker] {}\n", sticker.name));
        }
        if out.len() + entry.len() + 64 > MAX_TRANSCRIPT_BYTES {
            out.push_str("\n[transcript truncated: size limit reached]\n");
            break;
        }
        out.push_str(&entry);
        out.push('\n');
    }
    out
}

/// Summary embed for the transcript log and the opener's DM (mentions render; none ping).
pub fn summary_embed(meta: &TranscriptMeta, opener_id: &str, closer_id: &str, claimer_id: Option<&str>, participant_ids: &[String]) -> Value {
    let participants = participant_ids.iter().map(|id| format!("<@{id}>")).collect::<Vec<_>>().join(", ");
    let participants: String = if participants.chars().count() > 1024 { format!("{} participants", participant_ids.len()) } else { participants };
    let reason: String = meta.reason.clone().unwrap_or_else(|| "No reason provided".into()).chars().take(1024).collect();
    json!({
        "title": "🔒 Ticket Closed",
        "color": 0xed4245,
        "description": format!("**{}**\n#{} in **{}**", meta.title, meta.channel_name, meta.guild_name),
        "fields": [
            {"name": "Opened By", "value": format!("<@{opener_id}>"), "inline": true},
            {"name": "Claimed By", "value": claimer_id.map(|id| format!("<@{id}>")).unwrap_or_else(|| "Nobody".into()), "inline": true},
            {"name": "Closed By", "value": format!("<@{closer_id}>"), "inline": true},
            {"name": "Reason", "value": reason, "inline": false},
            {"name": "Participants", "value": if participants.is_empty() { "None".into() } else { participants }, "inline": false},
            {"name": "Created", "value": format!("<t:{}:F>", meta.created_at.timestamp()), "inline": true},
            {"name": "Deleted", "value": format!("<t:{}:F>", meta.deleted_at.timestamp()), "inline": true},
        ],
        "footer": {"text": "Kori • The full transcript is attached."},
        "timestamp": meta.deleted_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::user_json;
    use std::sync::Mutex;

    fn message(id: u64, author: u64, content: &str) -> Message {
        serde_json::from_value(json!({
            "id": id.to_string(), "channel_id": "5", "author": user_json(author), "content": content,
            "timestamp": "2026-11-08T10:00:00.000Z", "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0,
        }))
        .unwrap()
    }

    fn meta() -> TranscriptMeta {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-08T09:00:00Z").unwrap().with_timezone(&chrono::Utc);
        TranscriptMeta {
            guild_name: "Guild".into(),
            title: "Billing Ticket".into(),
            channel_name: "billing-user5".into(),
            opener: "user5 (5)".into(),
            claimer: Some("user6 (6)".into()),
            closer: "user6 (6)".into(),
            reason: Some("Resolved".into()),
            participants: vec!["user5 (5)".into(), "user6 (6)".into()],
            created_at: at,
            deleted_at: at + chrono::Duration::hours(2),
        }
    }

    #[test]
    fn transcript_has_header_and_every_message() {
        let text = render_transcript(&meta(), &[message(1, 5, "hello\nsecond line"), message(2, 6, "hi <@5> and <@!123456789012345678>")], false);
        for expected in [
            "Ticket:       Billing Ticket",
            "Closed by:    user6 (6)",
            "Reason:       Resolved",
            "Created:      2026-11-08 09:00:00 UTC",
            "Deleted:      2026-11-08 11:00:00 UTC",
            "  - user5 (5)",
            "[2026-11-08 10:00:00 UTC] user5 (5):\n    hello\n    second line",
            "user6 (6):\n    hi @user5 and <@!123456789012345678>",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(!text.contains("Message Content Intent"));
    }

    #[test]
    fn warns_when_message_content_is_unavailable() {
        let text = render_transcript(&meta(), &[message(1, 5, "")], false);
        assert!(text.contains("Message Content Intent"));
    }

    #[test]
    fn transcripts_stay_under_the_upload_limit() {
        let big = "x".repeat(1_900);
        let messages: Vec<Message> = (0..5_000).map(|i| message(i + 1, 5, &big)).collect();
        let text = render_transcript(&meta(), &messages, true);
        assert!(text.len() <= MAX_TRANSCRIPT_BYTES);
        assert!(text.ends_with("[transcript truncated: size limit reached]\n"));
        assert!(text.contains("only the latest 10000"));
    }

    #[test]
    fn summary_lists_who_and_when_without_pinging() {
        let embed = summary_embed(&meta(), "5", "6", Some("6"), &["5".into(), "6".into(), "7".into()]);
        let field = |name: &str| embed["fields"].as_array().unwrap().iter().find(|f| f["name"] == name).unwrap()["value"].clone();
        assert_eq!(field("Closed By"), "<@6>");
        assert_eq!(field("Reason"), "Resolved");
        assert_eq!(field("Participants"), "<@5>, <@6>, <@7>");
        assert_eq!(field("Created"), format!("<t:{}:F>", meta().created_at.timestamp()));
    }

    #[tokio::test]
    async fn delivers_only_to_a_verified_destination() {
        let sent = Arc::new(Mutex::new(Vec::<Upload>::new()));
        let sink = sent.clone();
        let transport: UploadTransport = Arc::new(move |_url, upload| {
            let sink = sink.clone();
            Box::pin(async move {
                let body = match upload {
                    None => json!({"guild_id": "123456789012345678", "channel_id": "223456789012345678"}).to_string(),
                    Some(upload) => {
                        sink.lock().unwrap().push(upload);
                        String::new()
                    }
                };
                Ok(WebhookResponse { status: 200, retry_after: None, body: body.into_bytes() })
            })
        });
        let config = |channel: &str| LogDestination {
            webhook_url: Some("https://discord.com/api/webhooks/123456789012345678/token".into()),
            guild_id: Some("123456789012345678".into()),
            channel_id: Some(channel.into()),
        };
        let wrong = TranscriptLogService::with_transport(config("323456789012345678"), transport.clone());
        wrong.initialize().await;
        let g = "123456789012345678";
        assert!(!wrong.deliver(g, json!({}), "t.txt", b"x").await, "webhook in another channel is refused");
        let service = TranscriptLogService::with_transport(config("223456789012345678"), transport);
        assert!(!service.deliver(g, json!({}), "t.txt", b"x").await, "nothing is sent before verification");
        service.initialize().await;
        assert!(!service.deliver("999999999999999999", json!({}), "t.txt", b"x").await, "another server's transcript never reaches the operator");
        assert!(service.deliver(g, json!({"title": "t"}), "ticket.txt", b"body").await);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].filename, "ticket.txt");
        assert!(sent[0].payload_json.contains("\"parse\":[]"));
    }
}
