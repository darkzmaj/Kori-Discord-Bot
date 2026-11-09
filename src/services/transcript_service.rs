//! Ticket transcripts: renders a closed ticket as plain text. The ticket service posts it, as a `.txt` file, to the
//! transcript channel chosen in `/ticket` → **Logs**, and DMs it to the opener.
use serde_json::{json, Value};
use serenity::all::Message;

/// Discord's attachment limit is 10 MiB; stay well under it.
pub const MAX_TRANSCRIPT_BYTES: usize = 8 * 1024 * 1024;
/// Tickets longer than this are transcribed up to the limit, newest messages last.
pub const MAX_TRANSCRIPT_MESSAGES: usize = 10_000;

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
}
