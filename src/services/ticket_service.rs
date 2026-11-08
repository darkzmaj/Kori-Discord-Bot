//! Ticket lifecycle: panel button/selector, channel creation with locked-down overwrites, claiming,
//! `/reclaim`, `/add`, `/remove` and closing. Every action re-checks the actor's current roles and
//! Administrator permission from the interaction itself and the ticket from the database.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serenity::all::{
    ActionRowComponent, ButtonStyle, ChannelId, ChannelType, CommandDataOptionValue, CommandInteraction, ComponentInteraction,
    ComponentInteractionDataKind, CreateActionRow, CreateAllowedMentions, CreateAttachment, CreateButton, CreateChannel,
    CreateEmbed, CreateInputText, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, CreateModal,
    CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, EditInteractionResponse, EditMessage, GetMessages, GuildId,
    Http, InputTextStyle, Member, Message, MessageId, ModalInteraction, PermissionOverwrite, PermissionOverwriteType,
    Permissions, ReactionType, RoleId, User, UserId,
};
use serenity::model::id::TargetId;

use crate::ctx;
use crate::db::repositories::{GuildSettings, KoriRepository, PanelEmbed, Ticket, TicketType};
use crate::discord::Discord;
use crate::logger::{self, log_discord_error};
use crate::services::audit_log_service::{render_event, AuditEvent, AuditLogService, AuditSubject};
use crate::services::locks::KeyedLock;
use crate::services::transcript_service::{render_transcript, summary_embed, TranscriptLogService, TranscriptMeta, MAX_TRANSCRIPT_MESSAGES};

const COLOR: u32 = crate::discord::KORI_BLUE;
/// Per-member message cooldown in every ticket channel (Discord slowmode).
pub const SLOWMODE_SECONDS: u16 = 5;
/// A string select holds at most 25 options.
pub const MAX_TICKET_TYPES: usize = 25;
pub const MAX_ADDED_MEMBERS: usize = 20;
/// Time between closing and deleting the channel (instant in tests).
const CLOSE_DELAY: Duration = Duration::from_secs(if cfg!(test) { 0 } else { 5 });
/// Close reasons appear in the transcript, the log embed (1024-char fields) and the opener's DM.
pub const MAX_REASON: usize = 500;
/// Role pings in the welcome message stop here so its content stays under Discord's 2000 characters;
/// every staff role still gets access.
const MAX_WELCOME_PINGS: usize = 80;
/// After opening a ticket a member waits this long before opening another, so open/close loops cannot spam channels.
pub const OPEN_COOLDOWN: Duration = Duration::from_secs(30);

const NOT_A_TICKET: &str = "This channel is not an open ticket.";
const NOT_SET_UP: &str = "🟡 Tickets are not set up on this server yet. An administrator can configure them with `/ticket`.";

/// What ticket participants (the opener and added members) may do in their ticket, and nothing else.
pub fn participant_allow() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::ATTACH_FILES
        | Permissions::EMBED_LINKS
        | Permissions::ADD_REACTIONS
        | Permissions::USE_EXTERNAL_EMOJIS
        | Permissions::USE_EXTERNAL_STICKERS
}

/// Staff (unclaimed staff roles and the claimer) may additionally run Kori's slash commands in the ticket.
pub fn staff_allow() -> Permissions {
    participant_allow() | Permissions::USE_APPLICATION_COMMANDS
}

/// Denied explicitly in ticket channels so no role granted elsewhere widens what people can do there.
pub fn staff_deny() -> Permissions {
    Permissions::MENTION_EVERYONE
        | Permissions::MANAGE_MESSAGES
        | Permissions::MANAGE_CHANNELS
        | Permissions::MANAGE_ROLES
        | Permissions::MANAGE_WEBHOOKS
        | Permissions::MANAGE_THREADS
        | Permissions::CREATE_PUBLIC_THREADS
        | Permissions::CREATE_PRIVATE_THREADS
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::SEND_TTS_MESSAGES
        | Permissions::SEND_VOICE_MESSAGES
        | Permissions::SEND_POLLS
        | Permissions::USE_EXTERNAL_APPS
        | Permissions::CREATE_INSTANT_INVITE
}

pub fn participant_deny() -> Permissions {
    staff_deny() | Permissions::USE_APPLICATION_COMMANDS
}

/// Kori's own overwrite: it must keep seeing and managing the channel whatever the category says.
/// Manage Permissions (MANAGE_ROLES) on the channel also lets it set any bit on later overwrites.
pub fn bot_allow() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::EMBED_LINKS
        | Permissions::ATTACH_FILES
        | Permissions::MANAGE_CHANNELS
        | Permissions::MANAGE_ROLES
}

/// Discord refuses overwrite bits the bot does not hold at creation time; keep only what it has.
/// Unknown permissions (guild not cached) leave the bits untouched; View Channel is always kept.
pub fn creatable(bits: Permissions, bot: Permissions) -> Permissions {
    if bot.is_empty() || bot.administrator() {
        return bits;
    }
    bits & (bot | Permissions::VIEW_CHANNEL)
}

/// The overwrites a fresh ticket channel starts with. `@everyone` never sees it.
pub fn ticket_overwrites(guild_id: GuildId, bot_id: UserId, opener: UserId, staff_roles: &[RoleId], bot: Permissions) -> Vec<PermissionOverwrite> {
    let mut overwrites = vec![
        PermissionOverwrite { allow: Permissions::empty(), deny: Permissions::VIEW_CHANNEL, kind: PermissionOverwriteType::Role(RoleId::new(guild_id.get())) },
        PermissionOverwrite { allow: creatable(bot_allow(), bot), deny: Permissions::empty(), kind: PermissionOverwriteType::Member(bot_id) },
        PermissionOverwrite {
            allow: creatable(participant_allow(), bot),
            deny: creatable(participant_deny(), bot),
            kind: PermissionOverwriteType::Member(opener),
        },
    ];
    for role in staff_roles.iter().filter(|r| r.get() != guild_id.get()) {
        overwrites.push(PermissionOverwrite {
            allow: creatable(staff_allow(), bot),
            deny: creatable(staff_deny(), bot),
            kind: PermissionOverwriteType::Role(*role),
        });
    }
    overwrites
}

/// Discord channel name: `{type}-{username}` (or `ticket-{username}`), lowercase `a-z0-9-_`, at most 100 chars.
pub fn channel_name(type_label: Option<&str>, username: &str, user_id: UserId) -> String {
    fn slug(value: &str) -> String {
        let mut out = String::new();
        for c in value.to_lowercase().chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                out.push(c);
            } else if !out.ends_with('-') {
                out.push('-');
            }
        }
        out.trim_matches('-').to_string()
    }
    let prefix = type_label.map(slug).filter(|s| !s.is_empty()).unwrap_or_else(|| "ticket".into());
    let user = Some(slug(username)).filter(|s| !s.is_empty()).unwrap_or_else(|| user_id.to_string());
    let name = format!("{prefix}-{user}");
    name.chars().take(100).collect::<String>().trim_end_matches('-').to_string()
}

/// A custom emoji (`<:name:id>`, `<a:name:id>`) or a short non-ASCII string such as 🎫; anything else is refused
/// so a bad value can never make the panel unsendable.
pub fn valid_emoji(value: &str) -> bool {
    static CUSTOM: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let custom = CUSTOM.get_or_init(|| regex::Regex::new(r"^<a?:[A-Za-z0-9_]{2,32}:\d{17,20}>$").unwrap());
    custom.is_match(value) || (!value.is_empty() && value.chars().count() <= 8 && !value.chars().any(|c| c.is_ascii() || c.is_control()))
}

fn emoji(value: &str) -> Option<ReactionType> {
    valid_emoji(value).then(|| ReactionType::try_from(value).ok()).flatten()
}

pub const DEFAULT_PANEL_TITLE: &str = "🎫 Support Tickets";
pub const DEFAULT_PANEL_DESCRIPTION: &str = "Need help? Open a ticket below and our staff will assist you in a private channel.";

/// The panel embed from the guild's settings; unset fields use Kori's defaults.
pub fn panel_embed(s: &GuildSettings) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .color(s.panel_color.map(|c| c as u32).unwrap_or(COLOR))
        .title(s.panel_title.as_deref().unwrap_or(DEFAULT_PANEL_TITLE))
        .description(s.panel_description.as_deref().unwrap_or(DEFAULT_PANEL_DESCRIPTION));
    if let Some(url) = s.panel_image_url.as_deref() {
        embed = embed.image(url);
    }
    if let Some(footer) = s.panel_footer.as_deref() {
        embed = embed.footer(serenity::all::CreateEmbedFooter::new(footer));
    }
    embed
}

/// Panel embed fields from the editor: trimmed, empty = default, Discord's limits, https images, `#RRGGBB` colours.
pub fn parse_panel_embed(title: &str, description: &str, color: &str, image_url: &str, footer: &str) -> Result<PanelEmbed, &'static str> {
    let opt = |v: &str| Some(v.trim().to_string()).filter(|v| !v.is_empty());
    let (title, description, footer) = (opt(title), opt(description), opt(footer));
    let clean = |v: &Option<String>, newlines: bool| v.as_deref().is_none_or(|v| !v.chars().any(|c| c.is_control() && !(newlines && c == '\n')));
    if !clean(&title, false) || !clean(&description, true) || !clean(&footer, false) {
        return Err("Text cannot contain control characters.");
    }
    if title.as_deref().is_some_and(|t| t.chars().count() > 256) || footer.as_deref().is_some_and(|f| f.chars().count() > 2048) {
        return Err("The title can be at most 256 characters and the footer 2048.");
    }
    let color = match opt(color) {
        None => None,
        Some(hex) => match u32::from_str_radix(hex.trim_start_matches('#'), 16) {
            Ok(value) if hex.trim_start_matches('#').len() == 6 => Some(value as i64),
            _ => return Err("Use a hex colour such as #5DADE2."),
        },
    };
    let image_url = match opt(image_url) {
        None => None,
        Some(url) => match reqwest::Url::parse(&url) {
            Ok(parsed) if parsed.scheme() == "https" && url.len() <= 2000 => Some(url),
            _ => return Err("The image must be an https:// link (at most 2000 characters)."),
        },
    };
    Ok(PanelEmbed { title, description, color, image_url, footer })
}

/// What happened to the posted panel after a change.
#[derive(Debug, PartialEq)]
pub enum PanelUpdate {
    /// The posted panel now shows the change.
    Updated,
    /// No panel has been sent yet.
    NotSent,
    /// The panel message was deleted; it must be sent again.
    Gone,
}

/// The components appended to the panel embed: a selector when types exist, otherwise one Open Ticket button.
pub fn panel_components(types: &[TicketType]) -> Vec<CreateActionRow> {
    if types.is_empty() {
        return vec![CreateActionRow::Buttons(vec![CreateButton::new("ticket:open")
            .label("Open Ticket")
            .style(ButtonStyle::Primary)
            .emoji(ReactionType::Unicode("🎫".into()))])];
    }
    let options = types
        .iter()
        .take(MAX_TICKET_TYPES)
        .map(|t| {
            let mut option = CreateSelectMenuOption::new(&t.label, &t.type_id);
            if let Some(description) = t.description.as_deref().filter(|d| !d.is_empty()) {
                option = option.description(description);
            }
            if let Some(e) = t.emoji.as_deref().and_then(emoji) {
                option = option.emoji(e);
            }
            option
        })
        .collect();
    vec![CreateActionRow::SelectMenu(
        CreateSelectMenu::new("ticket:select", CreateSelectMenuKind::String { options }).placeholder("Select a ticket type"),
    )]
}

pub const DEFAULT_WELCOME_TITLE: &str = "🎫 {type} Ticket";
pub const DEFAULT_WELCOME_DESCRIPTION: &str = "Welcome {user}! Thanks for reaching out — a member of staff will be with you shortly.\n\nPlease describe your request in as much detail as you can. Messages in this ticket have a 5-second cooldown.";

/// Fills the welcome placeholders: `{user}` (the opener, as a mention) and `{type}` (the ticket type, or General).
pub fn fill_welcome(template: &str, ticket: &Ticket) -> String {
    template.replace("{user}", &format!("<@{}>", ticket.opener_id)).replace("{type}", ticket.type_label.as_deref().unwrap_or("General"))
}

/// Welcome title and description from the editor: trimmed, empty = default, Discord's limits, no control characters.
pub fn parse_welcome(title: &str, description: &str) -> Result<(Option<String>, Option<String>), &'static str> {
    let opt = |v: &str| Some(v.trim().to_string()).filter(|v| !v.is_empty());
    let (title, description) = (opt(title), opt(description));
    if title.as_deref().is_some_and(|t| t.chars().any(char::is_control)) || description.as_deref().is_some_and(|d| d.chars().any(|c| c.is_control() && c != '\n')) {
        return Err("Text cannot contain control characters.");
    }
    // `{user}` expands to a ~22-character mention; keep the title under Discord's 256 characters after filling.
    if title.as_deref().is_some_and(|t| t.chars().count() > 200) {
        return Err("The title can be at most 200 characters.");
    }
    Ok((title, description))
}

/// The message content and allowed mentions for a new ticket's welcome, per the guild's ping switches.
pub fn welcome_pings(settings: &GuildSettings, opener: UserId, staff: &[RoleId]) -> (String, CreateAllowedMentions) {
    let opener_pinged = settings.welcome_ping_opener != 0;
    let roles: Vec<RoleId> = if settings.welcome_ping_staff != 0 { staff.iter().take(MAX_WELCOME_PINGS).copied().collect() } else { Vec::new() };
    let mut mentions: Vec<String> = Vec::new();
    if opener_pinged {
        mentions.push(format!("<@{opener}>"));
    }
    mentions.extend(roles.iter().map(|r| format!("<@&{r}>")));
    let allowed = CreateAllowedMentions::new().users(if opener_pinged { vec![opener] } else { vec![] }).roles(roles);
    (mentions.join(" "), allowed)
}

/// The pinned controls message at the top of every ticket.
pub fn welcome_view(ticket: &Ticket, settings: &GuildSettings) -> (CreateEmbed, Vec<CreateActionRow>) {
    let title = fill_welcome(settings.welcome_title.as_deref().unwrap_or(DEFAULT_WELCOME_TITLE), ticket);
    let description = fill_welcome(settings.welcome_description.as_deref().unwrap_or(DEFAULT_WELCOME_DESCRIPTION), ticket);
    let status = match ticket.claimed_by_id.as_deref() {
        Some(id) => format!("🟢 Claimed by <@{id}>"),
        None => "🟡 Waiting for staff".into(),
    };
    let embed = CreateEmbed::new()
        .color(COLOR)
        .title(title)
        .description(description)
        .field("Opened By", format!("<@{}>", ticket.opener_id), true)
        .field("Type", ticket.type_label.clone().unwrap_or_else(|| "General".into()), true)
        .field("Status", status, false);
    let claimed = ticket.claimed_by_id.is_some();
    let rows = vec![CreateActionRow::Buttons(vec![
        CreateButton::new("ticket:claim")
            .label(if claimed { "Claimed" } else { "Claim" })
            .style(ButtonStyle::Success)
            .disabled(claimed),
        CreateButton::new("ticket:close").label("Close").style(ButtonStyle::Danger),
    ])];
    (embed, rows)
}

/// Who is acting, from the interaction's own member payload (current roles and permissions).
#[derive(Clone, Debug)]
pub struct Actor {
    pub id: UserId,
    pub admin: bool,
    pub staff: bool,
}

pub fn actor(member: &Member, staff_roles: &[String]) -> Actor {
    let admin = member.permissions.is_some_and(|p| p.administrator());
    let staff = admin || member.roles.iter().any(|r| staff_roles.contains(&r.to_string()));
    Actor { id: member.user.id, admin, staff }
}

/// Claim, add/remove members: the claimer or an administrator; while unclaimed, any ticket staff.
pub fn can_manage(actor: &Actor, ticket: &Ticket) -> bool {
    actor.admin
        || match ticket.claimed_by_id.as_deref() {
            Some(claimer) => claimer == actor.id.to_string(),
            None => actor.staff,
        }
}

/// Closing: everyone who can manage the ticket, plus its opener.
pub fn can_close(actor: &Actor, ticket: &Ticket) -> bool {
    can_manage(actor, ticket) || ticket.opener_id == actor.id.to_string()
}

fn ephemeral(content: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(content).ephemeral(true))
}

fn notice(title: &str, description: String, color: u32) -> CreateEmbed {
    CreateEmbed::new().color(color).title(title).description(description)
}

fn snowflake(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().filter(|v| *v != 0)
}

pub struct TicketService {
    repo: Arc<KoriRepository>,
    discord: Discord,
    audit: Option<Arc<AuditLogService>>,
    transcripts: Arc<TranscriptLogService>,
    opening: KeyedLock,
    /// guild:user -> when their last ticket was created.
    recent: Mutex<HashMap<String, Instant>>,
}

impl TicketService {
    pub fn new(repo: Arc<KoriRepository>, discord: Discord, audit: Option<Arc<AuditLogService>>, transcripts: Arc<TranscriptLogService>) -> Self {
        TicketService { repo, discord, audit, transcripts, opening: KeyedLock::default(), recent: Mutex::default() }
    }

    /// Logs one event for `guild_id`: see [`route_log`].
    pub fn log(&self, guild_id: GuildId, event: AuditEvent) {
        route_log(&self.repo, &self.discord, self.audit.as_deref(), guild_id, event);
    }

    /// Whether the operator's `.env` webhooks cover this server: (main log, transcripts).
    pub fn operator_logs(&self, guild_id: GuildId) -> (bool, bool) {
        let gs = guild_id.to_string();
        (self.audit.as_ref().is_some_and(|a| a.covers(&gs)), self.transcripts.enabled_for(&gs))
    }

    fn record(&self, title: &str, color: &str, guild_id: GuildId, actor: Option<&User>, details: Vec<(&str, String)>) {
        let guild_name = self.discord.cache.guild(guild_id).map(|g| g.name.clone());
        self.log(guild_id, AuditEvent {
            title: title.into(),
            category: "Tickets".into(),
            color: color.into(),
            guild: Some(AuditSubject { id: Some(guild_id.to_string()), name: guild_name }),
            actor: actor.map(|u| AuditSubject::named(u.id.to_string(), u.tag())),
            details: details.into_iter().map(|(k, v)| (k.to_string(), v.into())).collect(),
            ..Default::default()
        });
    }

    fn bot_permissions(&self, guild_id: GuildId) -> Permissions {
        let me = self.discord.cache.current_user().id;
        let Some(guild) = self.discord.cache.guild(guild_id) else { return Permissions::empty() };
        guild.members.get(&me).map(|m| guild.member_permissions(m)).unwrap_or_else(Permissions::empty)
    }

    fn channel_exists(&self, guild_id: GuildId, channel_id: &str) -> bool {
        let Some(id) = snowflake(channel_id).map(ChannelId::new) else { return false };
        self.discord.cache.guild(guild_id).is_some_and(|g| g.channels.contains_key(&id))
    }

    /// The ticket category when it is configured and still exists.
    pub fn category(&self, guild_id: GuildId) -> anyhow::Result<Option<ChannelId>> {
        let settings = self.repo.settings(&guild_id.to_string())?;
        let Some(id) = settings.ticket_category_id.as_deref().and_then(snowflake).map(ChannelId::new) else { return Ok(None) };
        let exists = self
            .discord
            .cache
            .guild(guild_id)
            .is_some_and(|g| g.channels.get(&id).is_some_and(|c| c.kind == ChannelType::Category));
        Ok(exists.then_some(id))
    }

    async fn put(&self, channel: ChannelId, target: u64, is_role: bool, allow: Permissions, deny: Permissions, reason: &str) -> serenity::Result<()> {
        let body = serde_json::json!({ "allow": allow.bits().to_string(), "deny": deny.bits().to_string(), "type": if is_role { 0 } else { 1 } });
        self.discord.http.create_permission(channel, TargetId::new(target), &body, Some(reason)).await
    }

    async fn drop_overwrite(&self, channel: ChannelId, target: u64, reason: &str) {
        if let Err(error) = self.discord.http.delete_permission(channel, TargetId::new(target), Some(reason)).await {
            // 10009 Unknown Overwrite: already gone.
            if logger::discord_code(&error) != Some(10009) {
                log_discord_error("ticket.overwrite.delete", &error.into(), ctx! {"channelId" => channel.to_string(), "targetId" => target.to_string()});
            }
        }
    }

    // ---- panel ----

    /// Edits the posted panel in place (embed and selector) so changes never need a resend.
    pub async fn refresh_panel(&self, guild_id: GuildId) -> anyhow::Result<PanelUpdate> {
        let gs = guild_id.to_string();
        let settings = self.repo.settings(&gs)?;
        let (Some(channel), Some(message)) =
            (settings.panel_channel_id.as_deref().and_then(snowflake), settings.panel_message_id.as_deref().and_then(snowflake))
        else {
            return Ok(PanelUpdate::NotSent);
        };
        let edit = EditMessage::new().embed(panel_embed(&settings)).components(panel_components(&self.repo.ticket_types(&gs)?));
        match ChannelId::new(channel).edit_message(&self.discord.http, MessageId::new(message), edit).await {
            Ok(_) => Ok(PanelUpdate::Updated),
            // 10003 Unknown Channel, 10008 Unknown Message: the panel was deleted by hand.
            Err(error) if matches!(logger::discord_code(&error), Some(10003 | 10008)) => {
                self.repo.set_panel_message(&gs, None)?;
                Ok(PanelUpdate::Gone)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Forgets a tracked panel whose message was deleted by hand, so the dashboard never shows a dead link.
    pub async fn verify_panel(&self, guild_id: GuildId) -> anyhow::Result<()> {
        let gs = guild_id.to_string();
        let settings = self.repo.settings(&gs)?;
        let (Some(channel), Some(message)) =
            (settings.panel_channel_id.as_deref().and_then(snowflake), settings.panel_message_id.as_deref().and_then(snowflake))
        else {
            return Ok(());
        };
        if let Err(error) = self.discord.http.get_message(ChannelId::new(channel), MessageId::new(message)).await {
            if matches!(logger::discord_code(&error), Some(10003 | 10008)) {
                self.repo.set_panel_message(&gs, None)?;
            }
        }
        Ok(())
    }

    /// Posts the panel to `channel` and tracks it; a panel posted elsewhere before is removed so only one stays live.
    pub async fn send_panel(&self, guild_id: GuildId, channel: ChannelId) -> anyhow::Result<MessageId> {
        let gs = guild_id.to_string();
        let settings = self.repo.settings(&gs)?;
        let message = CreateMessage::new().embed(panel_embed(&settings)).components(panel_components(&self.repo.ticket_types(&gs)?));
        let sent = channel.send_message(&self.discord.http, message).await?;
        if let (Some(old_channel), Some(old_message)) =
            (settings.panel_channel_id.as_deref().and_then(snowflake), settings.panel_message_id.as_deref().and_then(snowflake))
        {
            let _ = self.discord.http.delete_message(ChannelId::new(old_channel), MessageId::new(old_message), Some("Kori panel moved")).await;
        }
        self.repo.set_panel_message(&gs, Some((&channel.to_string(), &sent.id.to_string())))?;
        Ok(sent.id)
    }

    // ---- panel: open ----

    /// `ticket:open` (button) and `ticket:select` (string select) on a panel message.
    pub async fn handle_component(&self, i: &ComponentInteraction) -> anyhow::Result<()> {
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        match (i.data.custom_id.as_str(), &i.data.kind) {
            ("ticket:open", ComponentInteractionDataKind::Button) => self.open(i, guild_id, None).await,
            ("ticket:select", ComponentInteractionDataKind::StringSelect { values }) => {
                let type_id = values.first().cloned().unwrap_or_default();
                self.open(i, guild_id, Some(type_id)).await
            }
            ("ticket:claim", ComponentInteractionDataKind::Button) => self.claim(i, guild_id).await,
            ("ticket:close", ComponentInteractionDataKind::Button) => self.close_prompt(i, guild_id).await,
            _ => Ok(()),
        }
    }

    async fn open(&self, i: &ComponentInteraction, guild_id: GuildId, type_id: Option<String>) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let gs = guild_id.to_string();
        let settings = self.repo.settings(&gs)?;
        if settings.tickets_enabled == 0 {
            i.create_response(http, ephemeral("🔴 Tickets are currently disabled on this server.")).await?;
            return Ok(());
        }
        let Some(category) = self.category(guild_id)? else {
            i.create_response(http, ephemeral(NOT_SET_UP)).await?;
            return Ok(());
        };
        let ticket_type = match type_id.as_deref() {
            Some(id) => match self.repo.ticket_type(&gs, id)? {
                Some(t) => Some(t),
                None => {
                    i.create_response(http, ephemeral("That ticket type no longer exists. Please choose another option.")).await?;
                    return Ok(());
                }
            },
            None => None,
        };
        let key = format!("{gs}:{}", i.user.id);
        let waited = self.recent.lock().unwrap().get(&key).map(Instant::elapsed);
        if let Some(remaining) = waited.and_then(|w| OPEN_COOLDOWN.checked_sub(w)).filter(|r| !r.is_zero()) {
            let message = format!("🟡 Please wait {} more seconds before opening another ticket.", remaining.as_secs().max(1));
            i.create_response(http, ephemeral(message)).await?;
            return Ok(());
        }
        i.create_response(http, CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new().ephemeral(true))).await?;
        let outcome = self.opening.run(&key, self.create_ticket(guild_id, &i.user, category, ticket_type)).await;
        let content = match outcome {
            Ok(Opened::Created(channel)) => {
                let mut recent = self.recent.lock().unwrap();
                recent.retain(|_, at| at.elapsed() < OPEN_COOLDOWN);
                recent.insert(key, Instant::now());
                format!("🟢 Your ticket is open: <#{channel}>")
            }
            Ok(Opened::Existing(channel)) => format!("🟡 You already have an open ticket: <#{channel}>"),
            Err(error) => {
                log_discord_error("ticket.open", &error, ctx! {"guildId" => gs.clone(), "userId" => i.user.id.to_string()});
                let reason = crate::services::audit_log_service::sanitize_audit_text(&error.to_string(), 300);
                self.record("Ticket Creation Failed", "danger", guild_id, Some(&i.user), vec![("Error", reason)]);
                "🔴 Kori could not create your ticket. Ask an administrator to check Kori's Manage Channels and Manage Roles permissions, and that the ticket category has room (Discord allows 50 channels per category).".into()
            }
        };
        i.edit_response(http, EditInteractionResponse::new().content(content)).await?;
        Ok(())
    }

    async fn create_ticket(&self, guild_id: GuildId, user: &User, category: ChannelId, ticket_type: Option<TicketType>) -> anyhow::Result<Opened> {
        let gs = guild_id.to_string();
        if let Some(existing) = self.repo.open_ticket_for(&gs, &user.id.to_string())? {
            if self.channel_exists(guild_id, &existing.channel_id) {
                return Ok(Opened::Existing(existing.channel_id));
            }
            // The channel vanished while Kori was not watching; the record is stale.
            self.repo.close_ticket(&gs, &existing.channel_id, None)?;
        }
        let staff: Vec<RoleId> = self.repo.staff_roles(&gs)?.iter().filter_map(|r| snowflake(r)).map(RoleId::new).collect();
        let label = ticket_type.as_ref().map(|t| t.label.clone());
        let bot = self.bot_permissions(guild_id);
        let me = self.discord.cache.current_user().id;
        let reason = format!("Kori ticket for {}", user.tag());
        let builder = CreateChannel::new(channel_name(label.as_deref(), &user.name, user.id))
            .kind(ChannelType::Text)
            .category(category)
            .topic(format!("Ticket opened by {} ({})", user.tag(), user.id))
            .rate_limit_per_user(SLOWMODE_SECONDS)
            .permissions(ticket_overwrites(guild_id, me, user.id, &staff, bot))
            .audit_log_reason(&reason);
        let channel = guild_id.create_channel(&self.discord.http, builder).await?;
        let ticket = Ticket {
            channel_id: channel.id.to_string(),
            guild_id: gs.clone(),
            opener_id: user.id.to_string(),
            type_label: label.clone(),
            channel_name: Some(channel.name.clone()),
            status: "open".into(),
            created_at: crate::db::now(),
            ..Default::default()
        };
        if !self.repo.open_ticket(&ticket)? {
            let _ = self.discord.http.delete_channel(channel.id, Some("Duplicate Kori ticket")).await;
            let existing = self.repo.open_ticket_for(&gs, &user.id.to_string())?.map(|t| t.channel_id).unwrap_or_default();
            return Ok(Opened::Existing(existing));
        }
        // Bits Discord refused at creation are applied now that Kori holds Manage Permissions on the channel.
        if !bot.is_empty() && !bot.administrator() {
            let strict = |target: u64, is_role: bool, allow: Permissions, deny: Permissions| self.put(channel.id, target, is_role, allow, deny, "Kori ticket permissions");
            if let Err(error) = strict(user.id.get(), false, participant_allow(), participant_deny()).await {
                log_discord_error("ticket.overwrite.strict", &error.into(), ctx! {"channelId" => channel.id.to_string()});
            }
            for role in &staff {
                if let Err(error) = strict(role.get(), true, staff_allow(), staff_deny()).await {
                    log_discord_error("ticket.overwrite.strict", &error.into(), ctx! {"channelId" => channel.id.to_string()});
                }
            }
        }
        let settings = self.repo.settings(&gs)?;
        let (embed, rows) = welcome_view(&ticket, &settings);
        let (pings, allowed) = welcome_pings(&settings, user.id, &staff);
        let mut message = CreateMessage::new().embed(embed).components(rows).allowed_mentions(allowed);
        if !pings.is_empty() {
            message = message.content(pings);
        }
        match channel.id.send_message(&self.discord.http, message).await {
            Ok(sent) => self.repo.set_welcome_message(&gs, &ticket.channel_id, &sent.id.to_string())?,
            Err(error) => log_discord_error("ticket.welcome", &error.into(), ctx! {"channelId" => channel.id.to_string()}),
        }
        self.record(
            "Ticket Opened",
            "success",
            guild_id,
            Some(user),
            vec![("Channel", format!("<#{}>", channel.id)), ("Type", label.unwrap_or_else(|| "General".into()))],
        );
        Ok(Opened::Created(ticket.channel_id))
    }

    /// Re-renders the welcome message's status and claim button after a claim change.
    async fn refresh_welcome(&self, guild_id: GuildId, channel: ChannelId) {
        let Ok(Some(ticket)) = self.repo.ticket(&guild_id.to_string(), &channel.to_string()) else { return };
        let Some(message) = self.repo.welcome_message(&ticket.channel_id).ok().flatten().as_deref().and_then(snowflake) else { return };
        let Ok(settings) = self.repo.settings(&guild_id.to_string()) else { return };
        let (embed, rows) = welcome_view(&ticket, &settings);
        if let Err(error) = channel.edit_message(&self.discord.http, MessageId::new(message), EditMessage::new().embed(embed).components(rows)).await {
            log_discord_error("ticket.welcome.edit", &error.into(), ctx! {"channelId" => channel.to_string()});
        }
    }

    // ---- claiming ----

    async fn claim(&self, i: &ComponentInteraction, guild_id: GuildId) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let gs = guild_id.to_string();
        let Some(ticket) = self.repo.ticket(&gs, &i.channel_id.to_string())? else {
            i.create_response(http, ephemeral(NOT_A_TICKET)).await?;
            return Ok(());
        };
        let Some(member) = i.member.as_ref() else { return Ok(()) };
        let actor = actor(member, &self.repo.staff_roles(&gs)?);
        if !actor.staff {
            i.create_response(http, ephemeral("Only ticket staff can claim tickets.")).await?;
            return Ok(());
        }
        if ticket.opener_id == actor.id.to_string() && !actor.admin {
            i.create_response(http, ephemeral("You cannot claim your own ticket.")).await?;
            return Ok(());
        }
        if let Some(claimer) = &ticket.claimed_by_id {
            i.create_response(http, ephemeral(format!("🟡 This ticket is already claimed by <@{claimer}>. An administrator can take it over with `/reclaim`."))).await?;
            return Ok(());
        }
        let me = actor.id.to_string();
        if !self.repo.claim_ticket(&gs, &ticket.channel_id, Some(&me), None)? {
            i.create_response(http, ephemeral("🟡 Someone else claimed this ticket first.")).await?;
            return Ok(());
        }
        i.create_response(http, CreateInteractionResponse::Acknowledge).await?;
        if let Err(error) = self.hand_over(guild_id, &ticket, actor.id).await {
            // Never leave a ticket recorded as claimed by someone Discord did not let in.
            self.repo.claim_ticket(&gs, &ticket.channel_id, None, Some(&me))?;
            log_discord_error("ticket.claim", &error, ctx! {"guildId" => gs, "channelId" => ticket.channel_id.clone()});
            let followup = serenity::all::CreateInteractionResponseFollowup::new()
                .content("🔴 Kori could not update this ticket's permissions, so the claim was undone. Check its Manage Roles permission.")
                .ephemeral(true);
            i.create_followup(http, followup).await?;
            return Ok(());
        }
        let embed = notice("🟢 Ticket Claimed", format!("<@{}> has claimed this ticket and will help you from here.", actor.id), 0x57f287);
        i.channel_id.send_message(http, CreateMessage::new().embed(embed)).await?;
        self.refresh_welcome(guild_id, i.channel_id).await;
        self.record("Ticket Claimed", "info", guild_id, Some(&i.user), vec![("Channel", format!("<#{}>", ticket.channel_id))]);
        Ok(())
    }

    /// Gives the ticket to `claimer`: the staff roles (or the previous claimer) lose access, the claimer gains it.
    async fn hand_over(&self, guild_id: GuildId, ticket: &Ticket, claimer: UserId) -> anyhow::Result<()> {
        let channel = ChannelId::new(snowflake(&ticket.channel_id).ok_or_else(|| anyhow::anyhow!("invalid ticket channel"))?);
        let reason = "Kori ticket claimed";
        self.put(channel, claimer.get(), false, staff_allow(), staff_deny(), reason).await?;
        match ticket.claimed_by_id.as_deref() {
            None => {
                for role in self.repo.staff_roles(&guild_id.to_string())?.iter().filter_map(|r| snowflake(r)) {
                    self.drop_overwrite(channel, role, reason).await;
                }
            }
            Some(previous) if previous != claimer.to_string() => {
                let members = self.repo.ticket_members(&ticket.channel_id)?;
                if previous != ticket.opener_id && !members.iter().any(|m| m == previous) {
                    if let Some(previous) = snowflake(previous) {
                        self.drop_overwrite(channel, previous, reason).await;
                    }
                }
            }
            Some(_) => {}
        }
        Ok(())
    }

    // ---- slash commands inside a ticket ----

    /// `/add`, `/remove`, `/reclaim` and `/close`.
    pub async fn handle_command(&self, i: &CommandInteraction) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        let gs = guild_id.to_string();
        let Some(member) = i.member.as_deref() else { return Ok(()) };
        let actor = actor(member, &self.repo.staff_roles(&gs)?);
        let Some(ticket) = self.repo.ticket(&gs, &i.channel_id.to_string())? else {
            i.create_response(http, ephemeral(format!("Use `/{}` inside an open ticket channel.", i.data.name))).await?;
            return Ok(());
        };
        match i.data.name.as_str() {
            "reclaim" => self.reclaim(i, guild_id, &actor, &ticket).await,
            "close" => {
                if !can_close(&actor, &ticket) {
                    i.create_response(http, ephemeral("You cannot close this ticket.")).await?;
                    return Ok(());
                }
                let reason = i.data.options.iter().find(|o| o.name == "reason").and_then(|o| match &o.value {
                    CommandDataOptionValue::String(text) => clean_reason(text),
                    _ => None,
                });
                match self.close(guild_id, &ticket, &i.user, reason.clone())? {
                    Some(embed) => {
                        i.create_response(http, CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().embed(embed))).await?;
                        self.spawn_teardown(guild_id, ticket, i.user.clone(), reason);
                    }
                    None => i.create_response(http, ephemeral(NOT_A_TICKET)).await?,
                }
                Ok(())
            }
            "add" | "remove" => self.membership(i, guild_id, &actor, &ticket).await,
            _ => Ok(()),
        }
    }

    async fn reclaim(&self, i: &CommandInteraction, guild_id: GuildId, actor: &Actor, ticket: &Ticket) -> anyhow::Result<()> {
        let http = &self.discord.http;
        if !actor.admin {
            i.create_response(http, ephemeral("You need Discord Administrator permission to use `/reclaim`.")).await?;
            return Ok(());
        }
        if ticket.claimed_by_id.as_deref() == Some(&actor.id.to_string()) {
            i.create_response(http, ephemeral("You already claim this ticket.")).await?;
            return Ok(());
        }
        let (gs, me) = (guild_id.to_string(), actor.id.to_string());
        if !self.repo.claim_ticket(&gs, &ticket.channel_id, Some(&me), ticket.claimed_by_id.as_deref())? {
            i.create_response(http, ephemeral("The ticket changed while reclaiming it. Please try again.")).await?;
            return Ok(());
        }
        i.create_response(http, CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new())).await?;
        if let Err(error) = self.hand_over(guild_id, ticket, actor.id).await {
            self.repo.claim_ticket(&gs, &ticket.channel_id, ticket.claimed_by_id.as_deref(), Some(&me))?;
            log_discord_error("ticket.reclaim", &error, ctx! {"guildId" => gs, "channelId" => ticket.channel_id.clone()});
            let content = "🔴 Kori could not update this ticket's permissions, so the reclaim was undone. Check its Manage Roles permission.";
            i.edit_response(http, EditInteractionResponse::new().content(content)).await?;
            return Ok(());
        }
        let from = ticket.claimed_by_id.as_deref().map(|id| format!(" from <@{id}>")).unwrap_or_default();
        let embed = notice("🟢 Ticket Reclaimed", format!("<@{}> has taken over this ticket{from}.", actor.id), 0x57f287);
        i.edit_response(http, EditInteractionResponse::new().embed(embed).allowed_mentions(CreateAllowedMentions::new())).await?;
        self.refresh_welcome(guild_id, i.channel_id).await;
        self.record(
            "Ticket Reclaimed",
            "warning",
            guild_id,
            Some(&i.user),
            vec![("Channel", format!("<#{}>", ticket.channel_id)), ("Previous Claimer", ticket.claimed_by_id.as_deref().map(|id| format!("<@{id}>")).unwrap_or_else(|| "None".into()))],
        );
        Ok(())
    }

    async fn membership(&self, i: &CommandInteraction, guild_id: GuildId, actor: &Actor, ticket: &Ticket) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let adding = i.data.name == "add";
        if !can_manage(actor, ticket) {
            let who = if ticket.claimed_by_id.is_some() { "the staff member who claimed this ticket or an administrator" } else { "ticket staff" };
            i.create_response(http, ephemeral(format!("Only {who} can {} members.", i.data.name))).await?;
            return Ok(());
        }
        let Some(target) = i.data.options.iter().find(|o| o.name == "user").and_then(|o| match o.value {
            CommandDataOptionValue::User(id) => Some(id),
            _ => None,
        }) else {
            return Ok(());
        };
        let ts = target.to_string();
        if ts == ticket.opener_id || ticket.claimed_by_id.as_deref() == Some(ts.as_str()) {
            i.create_response(http, ephemeral("The ticket opener and claimer always keep access.")).await?;
            return Ok(());
        }
        let members = self.repo.ticket_members(&ticket.channel_id)?;
        let present = members.contains(&ts);
        let channel = i.channel_id;
        if adding {
            if i.data.resolved.users.get(&target).is_some_and(|u| u.bot) {
                i.create_response(http, ephemeral("Bots cannot be added to tickets.")).await?;
                return Ok(());
            }
            if !i.data.resolved.members.contains_key(&target) {
                i.create_response(http, ephemeral("That user is not a member of this server.")).await?;
                return Ok(());
            }
            if present {
                i.create_response(http, ephemeral(format!("<@{target}> is already in this ticket."))).await?;
                return Ok(());
            }
            if members.len() >= MAX_ADDED_MEMBERS {
                i.create_response(http, ephemeral(format!("A ticket can have at most {MAX_ADDED_MEMBERS} added members."))).await?;
                return Ok(());
            }
            self.put(channel, target.get(), false, participant_allow(), participant_deny(), "Kori ticket member added").await?;
            self.repo.add_ticket_member(&ticket.channel_id, &ts, &actor.id.to_string())?;
        } else {
            if !present {
                i.create_response(http, ephemeral(format!("<@{target}> was not added to this ticket."))).await?;
                return Ok(());
            }
            self.drop_overwrite(channel, target.get(), "Kori ticket member removed").await;
            self.repo.remove_ticket_member(&ticket.channel_id, &ts)?;
        }
        let (title, verb) = if adding { ("🟢 Member Added", "added to") } else { ("🟡 Member Removed", "removed from") };
        let embed = notice(title, format!("<@{target}> was {verb} this ticket by <@{}>.", actor.id), if adding { 0x57f287 } else { 0xfee75c });
        let message = CreateInteractionResponseMessage::new().embed(embed).allowed_mentions(CreateAllowedMentions::new().users(vec![target]));
        i.create_response(http, CreateInteractionResponse::Message(message)).await?;
        self.record(
            if adding { "Ticket Member Added" } else { "Ticket Member Removed" },
            "info",
            guild_id,
            Some(&i.user),
            vec![("Channel", format!("<#{}>", ticket.channel_id)), ("Member", format!("<@{target}>"))],
        );
        Ok(())
    }

    // ---- closing ----

    async fn close_prompt(&self, i: &ComponentInteraction, guild_id: GuildId) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let gs = guild_id.to_string();
        let Some(ticket) = self.repo.ticket(&gs, &i.channel_id.to_string())? else {
            i.create_response(http, ephemeral(NOT_A_TICKET)).await?;
            return Ok(());
        };
        let Some(member) = i.member.as_ref() else { return Ok(()) };
        if !can_close(&actor(member, &self.repo.staff_roles(&gs)?), &ticket) {
            i.create_response(http, ephemeral("You cannot close this ticket.")).await?;
            return Ok(());
        }
        let reason = CreateInputText::new(InputTextStyle::Paragraph, "Reason (optional)", "reason")
            .placeholder("Shown in the transcript and sent to the ticket opener.")
            .max_length(MAX_REASON as u16)
            .required(false);
        let modal = CreateModal::new("ticket:closemodal", "Close Ticket").components(vec![CreateActionRow::InputText(reason)]);
        i.create_response(http, CreateInteractionResponse::Modal(modal)).await?;
        Ok(())
    }

    /// `ticket:closemodal`: submitting the reason modal is the confirmation.
    pub async fn handle_modal(&self, i: &ModalInteraction) -> anyhow::Result<()> {
        let http = &self.discord.http;
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        if i.data.custom_id != "ticket:closemodal" {
            return Ok(());
        }
        let gs = guild_id.to_string();
        let Some(ticket) = self.repo.ticket(&gs, &i.channel_id.to_string())? else {
            i.create_response(http, ephemeral(NOT_A_TICKET)).await?;
            return Ok(());
        };
        let Some(member) = i.member.as_ref() else { return Ok(()) };
        if !can_close(&actor(member, &self.repo.staff_roles(&gs)?), &ticket) {
            i.create_response(http, ephemeral("You cannot close this ticket.")).await?;
            return Ok(());
        }
        let reason = i
            .data
            .components
            .iter()
            .flat_map(|row| row.components.iter())
            .find_map(|c| match c {
                ActionRowComponent::InputText(input) if input.custom_id == "reason" => input.value.clone(),
                _ => None,
            })
            .and_then(|text| clean_reason(&text));
        match self.close(guild_id, &ticket, &i.user, reason.clone())? {
            Some(embed) => {
                i.create_response(http, CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().embed(embed))).await?;
                self.spawn_teardown(guild_id, ticket, i.user.clone(), reason);
            }
            None => i.create_response(http, ephemeral(NOT_A_TICKET)).await?,
        }
        Ok(())
    }

    /// Marks the ticket closed (exactly once) and returns the public notice; None when it was already closed.
    fn close(&self, guild_id: GuildId, ticket: &Ticket, closer: &User, reason: Option<String>) -> anyhow::Result<Option<CreateEmbed>> {
        if !self.repo.close_ticket(&guild_id.to_string(), &ticket.channel_id, Some(&closer.id.to_string()))? {
            return Ok(None);
        }
        self.record(
            "Ticket Closed",
            "danger",
            guild_id,
            Some(closer),
            vec![
                ("Channel", format!("#{}", ticket.channel_name.clone().unwrap_or_else(|| ticket.channel_id.clone()))),
                ("Opened By", format!("<@{}>", ticket.opener_id)),
                ("Reason", reason.clone().unwrap_or_else(|| "No reason provided".into())),
            ],
        );
        let mut description = format!("Closed by <@{}>.", closer.id);
        if let Some(reason) = &reason {
            description.push_str(&format!("\n**Reason:** {reason}"));
        }
        description.push_str(&format!("\n\nA transcript is being saved. This channel will be deleted in {} seconds.", CLOSE_DELAY.as_secs()));
        Ok(Some(notice("🔒 Ticket Closed", description, 0xed4245)))
    }

    /// Transcript, deletion, transcript log and the opener's DM run after the interaction is answered.
    fn spawn_teardown(&self, guild_id: GuildId, ticket: Ticket, closer: User, reason: Option<String>) {
        let job = Teardown {
            repo: self.repo.clone(),
            discord: self.discord.clone(),
            transcripts: self.transcripts.clone(),
            audit: self.audit.clone(),
            guild_id,
            ticket,
            closer,
            reason,
            started: Instant::now(),
        };
        tokio::spawn(job.run());
    }

    // ---- gateway reconciliation ----

    /// A ticket channel was deleted (by Kori or by hand): its record closes.
    pub fn on_channel_delete(&self, guild_id: GuildId, channel_id: ChannelId) -> anyhow::Result<()> {
        if self.repo.close_ticket(&guild_id.to_string(), &channel_id.to_string(), None)? {
            self.record("Ticket Channel Deleted", "warning", guild_id, None, vec![("Channel", channel_id.to_string())]);
        }
        Ok(())
    }

    /// Startup: closes records whose channels were deleted while Kori was offline.
    pub fn reconcile(&self, guild_id: GuildId) -> anyhow::Result<usize> {
        let gs = guild_id.to_string();
        let mut closed = 0;
        for ticket in self.repo.open_tickets(&gs)? {
            if !self.channel_exists(guild_id, &ticket.channel_id) && self.repo.close_ticket(&gs, &ticket.channel_id, None)? {
                closed += 1;
            }
        }
        if closed > 0 {
            logger::info("closed tickets whose channels no longer exist", ctx! {"guildId" => gs, "count" => closed});
        }
        Ok(closed)
    }
}

enum Opened {
    Created(String),
    Existing(String),
}

/// Trimmed, non-empty, at most [`MAX_REASON`] characters, no control characters except newlines.
pub fn clean_reason(text: &str) -> Option<String> {
    let cleaned: String = text.trim().chars().filter(|c| *c == '\n' || !c.is_control()).take(MAX_REASON).collect();
    Some(cleaned).filter(|c| !c.is_empty())
}

/// Every message of a channel, oldest first; the newest [`MAX_TRANSCRIPT_MESSAGES`] when longer (flag set).
async fn fetch_messages(http: &Arc<Http>, channel: ChannelId) -> (Vec<Message>, bool) {
    let mut all: Vec<Message> = Vec::new();
    let mut before: Option<MessageId> = None;
    let mut truncated = false;
    loop {
        let mut request = GetMessages::new().limit(100);
        if let Some(id) = before {
            request = request.before(id);
        }
        match channel.messages(http, request).await {
            Ok(page) => {
                let full = page.len() == 100;
                before = page.last().map(|m| m.id);
                all.extend(page);
                if all.len() >= MAX_TRANSCRIPT_MESSAGES {
                    truncated = full || all.len() > MAX_TRANSCRIPT_MESSAGES;
                    all.truncate(MAX_TRANSCRIPT_MESSAGES);
                    break;
                }
                if !full {
                    break;
                }
            }
            Err(error) => {
                log_discord_error("ticket.transcript.fetch", &error.into(), ctx! {"channelId" => channel.to_string()});
                break;
            }
        }
    }
    all.reverse();
    (all, truncated)
}

/// Posts a server's event to the log channel it chose in `/ticket`. Without one, the event goes to the operator's
/// log webhook, which only accepts the operator's own server (and bot health).
pub fn route_log(repo: &Arc<KoriRepository>, discord: &Discord, audit: Option<&AuditLogService>, guild_id: GuildId, event: AuditEvent) {
    let gs = guild_id.to_string();
    let Some(channel) = repo.settings(&gs).ok().and_then(|s| s.log_channel_id).and_then(|c| snowflake(&c)).map(ChannelId::new) else {
        if let Some(audit) = audit {
            audit.record(event);
        }
        return;
    };
    let body = serde_json::json!({"embeds": [render_event(event)], "allowed_mentions": {"parse": []}});
    let (http, repo) = (discord.http.clone(), repo.clone());
    tokio::spawn(async move {
        if let Err(error) = http.send_message(channel, vec![], &body).await {
            if logger::discord_code(&error) == Some(10003) {
                let _ = repo.update_setting(&gs, "log_channel_id", rusqlite::types::Value::Null);
            }
            log_discord_error("ticket.log.channel", &error.into(), ctx! {"guildId" => gs, "channelId" => channel.to_string()});
        }
    });
}

struct Teardown {
    repo: Arc<KoriRepository>,
    discord: Discord,
    transcripts: Arc<TranscriptLogService>,
    audit: Option<Arc<AuditLogService>>,
    guild_id: GuildId,
    ticket: Ticket,
    closer: User,
    reason: Option<String>,
    started: Instant,
}

impl Teardown {
    async fn run(self) {
        let Some(channel) = snowflake(&self.ticket.channel_id).map(ChannelId::new) else { return };
        let http = &self.discord.http;
        let (messages, truncated) = fetch_messages(http, channel).await;
        if let Some(wait) = CLOSE_DELAY.checked_sub(self.started.elapsed()) {
            tokio::time::sleep(wait).await;
        }
        let deleted = match http.delete_channel(channel, Some(&format!("Kori ticket closed by {}", self.closer.tag()))).await {
            Ok(_) => true,
            Err(error) => {
                log_discord_error("ticket.close.delete", &error.into(), ctx! {"channelId" => channel.to_string()});
                false
            }
        };
        let deleted_at = chrono::Utc::now();

        // Participants: opener, claimer, added members and everyone who wrote, in that order.
        let mut ids: Vec<String> = vec![self.ticket.opener_id.clone()];
        ids.extend(self.ticket.claimed_by_id.clone());
        ids.extend(self.repo.ticket_members(&self.ticket.channel_id).unwrap_or_default());
        ids.extend(messages.iter().filter(|m| !m.author.bot).map(|m| m.author.id.to_string()));
        let mut seen = std::collections::HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));
        let mut names: HashMap<String, String> = messages.iter().map(|m| (m.author.id.to_string(), m.author.tag())).collect();
        names.insert(self.closer.id.to_string(), self.closer.tag());
        for id in ids.iter().filter(|id| !names.contains_key(*id)).take(25).cloned().collect::<Vec<_>>() {
            if let Some(user) = snowflake(&id).map(UserId::new) {
                if let Ok(user) = http.get_user(user).await {
                    names.insert(id, user.tag());
                }
            }
        }
        let label = |id: &str| names.get(id).map(|name| format!("{name} ({id})")).unwrap_or_else(|| format!("Unknown user ({id})"));
        let channel_name = self.ticket.channel_name.clone().unwrap_or_else(|| self.ticket.channel_id.clone());
        let meta = TranscriptMeta {
            guild_name: self.discord.cache.guild(self.guild_id).map(|g| g.name.clone()).unwrap_or_else(|| self.guild_id.to_string()),
            title: match self.ticket.type_label.as_deref() {
                Some(label) => format!("{label} Ticket"),
                None => "Ticket".into(),
            },
            channel_name: channel_name.clone(),
            opener: label(&self.ticket.opener_id),
            claimer: self.ticket.claimed_by_id.as_deref().map(label),
            closer: label(&self.closer.id.to_string()),
            reason: self.reason.clone(),
            participants: ids.iter().map(|id| label(id)).collect(),
            created_at: chrono::DateTime::parse_from_rfc3339(&self.ticket.created_at).map(|t| t.with_timezone(&chrono::Utc)).unwrap_or(deleted_at),
            deleted_at,
        };
        let text = render_transcript(&meta, &messages, truncated);
        let filename = format!("transcript-{channel_name}.txt");
        let embed = summary_embed(&meta, &self.ticket.opener_id, &self.closer.id.to_string(), self.ticket.claimed_by_id.as_deref(), &ids);

        let gs = self.guild_id.to_string();
        // The server's own transcript channel; otherwise the operator's webhook (own server only).
        let own = self.repo.settings(&gs).ok().and_then(|s| s.transcript_channel_id).and_then(|c| snowflake(&c)).map(ChannelId::new);
        let (configured, logged) = match own {
            Some(channel) => {
                let body = serde_json::json!({"embeds": [embed.clone()], "allowed_mentions": {"parse": []}});
                let sent = http.send_message(channel, vec![CreateAttachment::bytes(text.as_bytes().to_vec(), filename.clone())], &body).await;
                if let Err(error) = &sent {
                    if logger::discord_code(error) == Some(10003) {
                        let _ = self.repo.update_setting(&gs, "transcript_channel_id", rusqlite::types::Value::Null);
                    }
                }
                (true, sent.map_err(|e| log_discord_error("ticket.transcript.channel", &e.into(), ctx! {"guildId" => gs.clone()})).is_ok())
            }
            None => (self.transcripts.enabled_for(&gs), self.transcripts.deliver(&gs, embed.clone(), &filename, text.as_bytes()).await),
        };
        let dm = async {
            let opener = UserId::new(snowflake(&self.ticket.opener_id).ok_or_else(|| anyhow::anyhow!("invalid opener"))?);
            let dm = opener.create_dm_channel(http).await?;
            let body = serde_json::json!({"embeds": [embed], "allowed_mentions": {"parse": []}});
            http.send_message(dm.id, vec![CreateAttachment::bytes(text.as_bytes().to_vec(), filename.clone())], &body).await?;
            anyhow::Ok(())
        };
        let dm_sent = match dm.await {
            Ok(()) => true,
            Err(error) => {
                // 50007: the member has DMs closed or left every shared server.
                log_discord_error("ticket.transcript.dm", &error, ctx! {"guildId" => self.guild_id.to_string(), "userId" => self.ticket.opener_id.clone()});
                false
            }
        };
        {
            let guild_name = meta.guild_name.clone();
            route_log(&self.repo, &self.discord, self.audit.as_deref(), self.guild_id, AuditEvent {
                title: "Ticket Deleted".into(),
                category: "Tickets".into(),
                color: if deleted && dm_sent && (logged || !configured) { "info".into() } else { "warning".into() },
                guild: Some(AuditSubject::named(self.guild_id.to_string(), guild_name)),
                actor: Some(AuditSubject::named(self.closer.id.to_string(), self.closer.tag())),
                details: vec![
                    ("Channel".into(), format!("#{channel_name}").into()),
                    ("Messages".into(), messages.len().into()),
                    ("Channel Deleted".into(), if deleted { "Yes" } else { "Failed — delete it manually" }.into()),
                    (
                        "Transcript Log".into(),
                        if logged { "Sent" } else if configured { "Failed" } else { "Not configured" }.into(),
                    ),
                    ("Opener DM".into(), if dm_sent { "Delivered" } else { "Not delivered (DMs closed?)" }.into()),
                ],
                ..Default::default()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(claimed: Option<&str>) -> Ticket {
        Ticket { channel_id: "5".into(), guild_id: "1".into(), opener_id: "10".into(), claimed_by_id: claimed.map(String::from), status: "open".into(), ..Default::default() }
    }
    fn who(id: u64, admin: bool, staff: bool) -> Actor {
        Actor { id: UserId::new(id), admin, staff: staff || admin }
    }

    #[test]
    fn names_channels_by_type_and_username() {
        let user = UserId::new(42);
        assert_eq!(channel_name(Some("Billing"), "Dark_Zmaj", user), "billing-dark_zmaj");
        assert_eq!(channel_name(None, "dark.zmaj", user), "ticket-dark-zmaj");
        assert_eq!(channel_name(Some("Bug Reports!"), "名前", user), "bug-reports-42");
        assert_eq!(channel_name(Some("🎫"), "a", user), "ticket-a");
        assert!(channel_name(Some(&"x".repeat(120)), "user", user).len() <= 100);
    }

    #[test]
    fn participants_get_exactly_the_ticket_permissions() {
        let allow = participant_allow();
        for p in [
            Permissions::VIEW_CHANNEL,
            Permissions::SEND_MESSAGES,
            Permissions::READ_MESSAGE_HISTORY,
            Permissions::ATTACH_FILES,
            Permissions::EMBED_LINKS,
            Permissions::ADD_REACTIONS,
            Permissions::USE_EXTERNAL_EMOJIS,
            Permissions::USE_EXTERNAL_STICKERS,
        ] {
            assert!(allow.contains(p));
        }
        assert_eq!(allow.bits().count_ones(), 8, "nothing else is granted");
        assert!((allow & participant_deny()).is_empty());
        assert!((staff_allow() & staff_deny()).is_empty());
        assert!(participant_deny().contains(Permissions::MENTION_EVERYONE | Permissions::MANAGE_MESSAGES | Permissions::USE_APPLICATION_COMMANDS));
    }

    #[test]
    fn new_ticket_hides_from_everyone_and_admits_opener_staff_and_bot() {
        let g = GuildId::new(1);
        let overwrites = ticket_overwrites(g, UserId::new(2), UserId::new(10), &[RoleId::new(7), RoleId::new(1)], Permissions::all());
        assert_eq!(overwrites.len(), 4, "@everyone listed as staff is ignored");
        assert_eq!(overwrites[0].kind, PermissionOverwriteType::Role(RoleId::new(1)));
        assert_eq!(overwrites[0].deny, Permissions::VIEW_CHANNEL);
        assert_eq!(overwrites[2].kind, PermissionOverwriteType::Member(UserId::new(10)));
        assert_eq!(overwrites[2].allow, participant_allow());
        assert_eq!(overwrites[3].allow, staff_allow());
    }

    #[test]
    fn creation_keeps_only_bits_the_bot_holds_but_always_hides_the_channel() {
        let bot = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES;
        assert_eq!(creatable(participant_allow(), bot), bot);
        assert_eq!(creatable(participant_deny(), bot), Permissions::empty());
        assert_eq!(creatable(participant_allow(), Permissions::empty()), participant_allow(), "unknown permissions: unmasked");
        assert_eq!(creatable(participant_deny(), Permissions::ADMINISTRATOR), participant_deny());
        let overwrites = ticket_overwrites(GuildId::new(1), UserId::new(2), UserId::new(10), &[], Permissions::SEND_MESSAGES);
        assert_eq!(overwrites[0].deny, Permissions::VIEW_CHANNEL);
    }

    #[test]
    fn who_may_manage_and_close() {
        let open = ticket(None);
        assert!(can_manage(&who(3, false, true), &open), "any staff while unclaimed");
        assert!(!can_manage(&who(10, false, false), &open), "opener cannot manage");
        assert!(can_close(&who(10, false, false), &open), "opener can close");
        assert!(!can_close(&who(11, false, false), &open));
        let claimed = ticket(Some("3"));
        assert!(can_manage(&who(3, false, true), &claimed), "claimer");
        assert!(!can_manage(&who(4, false, true), &claimed), "other staff lose control once claimed");
        assert!(can_manage(&who(99, true, false), &claimed), "administrator");
    }

    #[test]
    fn panel_embed_editor_validates_and_defaults() {
        let e = parse_panel_embed(" Help ", "", "#5dade2", "https://i.imgur.com/a.png", "").unwrap();
        assert_eq!((e.title.as_deref(), e.description, e.color, e.footer), (Some("Help"), None, Some(0x5dade2), None));
        assert!(parse_panel_embed("", "", "blue", "", "").is_err());
        assert!(parse_panel_embed("", "", "", "http://x.com/a.png", "").is_err(), "https only");
        assert!(parse_panel_embed("", "", "", "javascript:alert(1)", "").is_err());
        assert!(parse_panel_embed("bad\u{7}", "", "", "", "").is_err());
        assert!(parse_panel_embed("", "line one\nline two", "", "", "").is_ok(), "descriptions keep newlines");
        let json = serde_json::to_value(panel_embed(&GuildSettings::default())).unwrap();
        assert_eq!(json["title"], DEFAULT_PANEL_TITLE);
    }

    #[test]
    fn welcome_text_placeholders_defaults_and_pings() {
        let t = Ticket { opener_id: "10".into(), type_label: Some("Billing".into()), ..Default::default() };
        let defaults = GuildSettings { welcome_ping_opener: 1, welcome_ping_staff: 1, ..Default::default() };
        let json = serde_json::to_value(welcome_view(&t, &defaults).0).unwrap();
        assert_eq!(json["title"], "🎫 Billing Ticket");
        assert!(json["description"].as_str().unwrap().starts_with("Welcome <@10>!"));
        let custom = GuildSettings { welcome_title: Some("{type} help for {user}".into()), welcome_description: Some("Hi".into()), ..defaults.clone() };
        let json = serde_json::to_value(welcome_view(&Ticket { type_label: None, ..t.clone() }, &custom).0).unwrap();
        assert_eq!((json["title"].as_str(), json["description"].as_str()), (Some("General help for <@10>"), Some("Hi")));

        let staff = [RoleId::new(7), RoleId::new(8)];
        let (content, allowed) = welcome_pings(&defaults, UserId::new(10), &staff);
        assert_eq!(content, "<@10> <@&7> <@&8>");
        let allowed = serde_json::to_value(allowed).unwrap();
        assert_eq!((allowed["users"].as_array().unwrap().len(), allowed["roles"].as_array().unwrap().len()), (1, 2));
        let (content, _) = welcome_pings(&GuildSettings { welcome_ping_opener: 0, ..defaults.clone() }, UserId::new(10), &staff);
        assert_eq!(content, "<@&7> <@&8>");
        let (content, allowed) = welcome_pings(&GuildSettings::default(), UserId::new(10), &staff);
        assert!(content.is_empty());
        assert!(serde_json::to_value(allowed).unwrap()["roles"].as_array().unwrap().is_empty(), "nothing can ping");

        assert_eq!(parse_welcome("  ", "").unwrap(), (None, None));
        assert!(parse_welcome(&"x".repeat(201), "").is_err());
        assert!(parse_welcome("", "two\nlines").is_ok());
        assert!(parse_welcome("bad\u{7}", "").is_err());
    }

    #[test]
    fn validates_type_emoji() {
        assert!(valid_emoji("🎫"));
        assert!(valid_emoji("<:kori:123456789012345678>"));
        assert!(valid_emoji("<a:spin:123456789012345678>"));
        assert!(!valid_emoji("ticket"));
        assert!(!valid_emoji(""));
        assert!(!valid_emoji("<:bad:12>"));
    }

    #[test]
    fn panel_is_a_selector_with_types_and_a_button_without() {
        let json = |rows: Vec<CreateActionRow>| serde_json::to_value(&rows).unwrap();
        assert_eq!(json(panel_components(&[]))[0]["components"][0]["custom_id"], "ticket:open");
        let types = vec![TicketType { type_id: "t1".into(), label: "Billing".into(), emoji: Some("💳".into()), ..Default::default() }];
        let select = json(panel_components(&types));
        assert_eq!(select[0]["components"][0]["custom_id"], "ticket:select");
        assert_eq!(select[0]["components"][0]["options"][0]["value"], "t1");
    }

    // ---- end to end against the fake Discord REST API ----
    use crate::test_support::{user_json, GuildSeed, Harness, TIMESTAMP};
    use serde_json::json;
    use serenity::all::{CommandInteraction, Interaction};

    const STAFF: u64 = 30;

    async fn harness() -> (Harness, crate::bot::KoriBot) {
        let h = Harness::start().await;
        let bot_perms = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::MANAGE_CHANNELS | Permissions::MANAGE_ROLES;
        h.seed(
            GuildSeed::new(1, "Guild")
                .role(20, "Kori", 10, bot_perms)
                .role(STAFF, "Staff", 5, Permissions::empty())
                .role(40, "Admin", 8, Permissions::ADMINISTRATOR)
                .channel(50, ChannelType::Category, None)
                .channel(60, ChannelType::Text, Some(50))
                .bot(&[20])
                .member(5, &[])
                .member(6, &[STAFF])
                .member(7, &[STAFF])
                .member(8, &[40])
                .member(9, &[]),
        );
        h.repo.update_setting("1", "ticket_category_id", rusqlite::types::Value::Text("50".into())).unwrap();
        h.repo.add_staff_role("1", &STAFF.to_string(), "8").unwrap();
        let bot = h.bot();
        (h, bot)
    }

    fn in_channel(mut i: ComponentInteraction, channel: u64) -> Interaction {
        i.channel_id = ChannelId::new(channel);
        Interaction::Component(i)
    }

    fn command(h: &Harness, user: u64, name: &str, target: Option<u64>, channel: u64) -> Interaction {
        let mut data = json!({"id": "77", "name": name, "type": 1, "options": []});
        if let Some(t) = target {
            data["options"] = json!([{"type": 6, "name": "user", "value": t.to_string()}]);
            data["resolved"] = json!({
                "users": {t.to_string(): user_json(t)},
                "members": {t.to_string(): {"roles": [], "joined_at": TIMESTAMP, "deaf": false, "mute": false, "flags": 0}},
            });
        }
        let mut i: CommandInteraction = h.interaction(2, 1, user, data).into_command().unwrap();
        i.channel_id = ChannelId::new(channel);
        Interaction::Command(i)
    }

    fn command_with(h: &Harness, user: u64, name: &str, options: serde_json::Value, channel: u64) -> Interaction {
        let mut i: CommandInteraction = h.interaction(2, 1, user, json!({"id": "78", "name": name, "type": 1, "options": options})).into_command().unwrap();
        i.channel_id = ChannelId::new(channel);
        Interaction::Command(i)
    }

    fn last_reply(h: &Harness) -> String {
        h.replies().last().map(|r| r["data"]["content"].as_str().unwrap_or_default().to_string()).unwrap_or_default()
    }

    fn last_edit(h: &Harness) -> String {
        h.calls_to("PATCH", "/webhooks/*/*/messages/@original").last().map(|c| c.body["content"].as_str().unwrap_or_default().to_string()).unwrap_or_default()
    }

    fn close_modal(h: &Harness, user: u64, channel: u64, reason: &str) -> Interaction {
        let mut i = h.modal(1, user, "ticket:closemodal", &[("reason", reason)]);
        i.channel_id = ChannelId::new(channel);
        Interaction::Modal(i)
    }

    fn open(channel: &str, opener: &str) -> Ticket {
        Ticket { channel_id: channel.into(), guild_id: "1".into(), opener_id: opener.into(), created_at: crate::db::now(), ..Default::default() }
    }

    #[tokio::test]
    async fn opens_a_locked_down_ticket_channel_with_slowmode() {
        let (h, bot) = harness().await;
        bot.on_interaction(&h.ctx(), &Interaction::Component(h.button(1, 5, "ticket:open"))).await;
        let created = h.calls_to("POST", "/guilds/1/channels");
        assert_eq!(created.len(), 1);
        let body = &created[0].body;
        assert_eq!(body["name"], "ticket-user5");
        assert_eq!(body["rate_limit_per_user"], SLOWMODE_SECONDS);
        assert_eq!(body["parent_id"], "50");
        let everyone = body["permission_overwrites"].as_array().unwrap().iter().find(|o| o["id"] == "1").unwrap();
        assert_eq!(everyone["deny"], Permissions::VIEW_CHANNEL.bits().to_string());
        let ticket = h.repo.open_ticket_for("1", "5").unwrap().expect("ticket recorded");
        assert!(last_edit(&h).contains("Your ticket is open"), "{}", last_edit(&h));
        // Bits Discord would refuse at creation are applied once Kori holds Manage Permissions on the channel.
        let strict = h.calls_to("PUT", &format!("/channels/{}/permissions/5", ticket.channel_id));
        assert_eq!(strict[0].body["deny"], participant_deny().bits().to_string());
        assert_eq!(h.calls_to("PUT", &format!("/channels/{}/permissions/{STAFF}", ticket.channel_id)).len(), 1);
        assert!(h.repo.welcome_message(&ticket.channel_id).unwrap().is_some(), "welcome message tracked");
    }

    #[tokio::test]
    async fn claim_add_reclaim_remove_close_enforce_who_may_act() {
        let (h, bot) = harness().await;
        let ctx = h.ctx();
        let c: u64 = 777;
        h.repo.open_ticket(&open(&c.to_string(), "5")).unwrap();
        let claimer = |h: &Harness| h.repo.ticket("1", &c.to_string()).unwrap().unwrap().claimed_by_id;

        bot.on_interaction(&ctx, &in_channel(h.button(1, 9, "ticket:claim"), c)).await;
        assert_eq!(last_reply(&h), "Only ticket staff can claim tickets.");
        bot.on_interaction(&ctx, &in_channel(h.button(1, 6, "ticket:claim"), c)).await;
        assert_eq!(claimer(&h).as_deref(), Some("6"));
        assert_eq!(h.calls_to("PUT", &format!("/channels/{c}/permissions/6")).len(), 1, "claimer gains access");
        assert_eq!(h.calls_to("DELETE", &format!("/channels/{c}/permissions/{STAFF}")).len(), 1, "staff role loses access");
        bot.on_interaction(&ctx, &in_channel(h.button(1, 7, "ticket:claim"), c)).await;
        assert!(last_reply(&h).contains("already claimed by <@6>"));

        bot.on_interaction(&ctx, &command(&h, 7, "add", Some(9), c)).await;
        assert!(last_reply(&h).starts_with("Only the staff member who claimed"), "other staff cannot add once claimed");
        bot.on_interaction(&ctx, &command(&h, 6, "add", Some(9), c)).await;
        assert_eq!(h.repo.ticket_members(&c.to_string()).unwrap(), ["9"]);
        let put = h.calls_to("PUT", &format!("/channels/{c}/permissions/9"));
        assert_eq!(put[0].body["allow"], participant_allow().bits().to_string());

        bot.on_interaction(&ctx, &command(&h, 6, "reclaim", None, c)).await;
        assert!(last_reply(&h).contains("Administrator"));
        bot.on_interaction(&ctx, &command(&h, 8, "reclaim", None, c)).await;
        assert_eq!(claimer(&h).as_deref(), Some("8"));
        assert_eq!(h.calls_to("DELETE", &format!("/channels/{c}/permissions/6")).len(), 1, "previous claimer loses access");

        bot.on_interaction(&ctx, &command(&h, 6, "remove", Some(9), c)).await;
        assert!(last_reply(&h).starts_with("Only the staff member who claimed"));
        bot.on_interaction(&ctx, &command(&h, 8, "remove", Some(5), c)).await;
        assert_eq!(last_reply(&h), "The ticket opener and claimer always keep access.");
        bot.on_interaction(&ctx, &command(&h, 8, "remove", Some(9), c)).await;
        assert!(h.repo.ticket_members(&c.to_string()).unwrap().is_empty());

        bot.on_interaction(&ctx, &in_channel(h.button(1, 9, "ticket:close"), c)).await;
        assert_eq!(last_reply(&h), "You cannot close this ticket.");
        bot.on_interaction(&ctx, &close_modal(&h, 9, c, "")).await;
        assert_eq!(last_reply(&h), "You cannot close this ticket.", "a forged modal is refused too");
        bot.on_interaction(&ctx, &in_channel(h.button(1, 5, "ticket:close"), c)).await;
        assert_eq!(h.replies().last().unwrap()["type"], 9, "the opener gets the reason modal");
        bot.on_interaction(&ctx, &close_modal(&h, 5, c, "")).await;
        assert!(h.repo.ticket("1", &c.to_string()).unwrap().is_none(), "opener closed it");
        bot.on_interaction(&ctx, &command(&h, 8, "close", None, c)).await;
        assert!(last_reply(&h).contains("inside an open ticket channel"));
    }

    #[tokio::test]
    async fn refuses_a_second_ticket_and_disabled_or_unconfigured_guilds() {
        let (h, bot) = harness().await;
        let ctx = h.ctx();
        h.repo.open_ticket(&open("60", "5")).unwrap();
        bot.on_interaction(&ctx, &Interaction::Component(h.button(1, 5, "ticket:open"))).await;
        assert!(last_edit(&h).contains("already have an open ticket: <#60>"), "{}", last_edit(&h));
        assert!(h.calls_to("POST", "/guilds/1/channels").is_empty());

        h.repo.update_setting("1", "tickets_enabled", rusqlite::types::Value::Integer(0)).unwrap();
        bot.on_interaction(&ctx, &Interaction::Component(h.button(1, 6, "ticket:open"))).await;
        assert!(last_reply(&h).contains("disabled"));
        h.repo.update_setting("1", "tickets_enabled", rusqlite::types::Value::Integer(1)).unwrap();
        h.repo.update_setting("1", "ticket_category_id", rusqlite::types::Value::Null).unwrap();
        bot.on_interaction(&ctx, &Interaction::Component(h.button(1, 6, "ticket:open"))).await;
        assert_eq!(last_reply(&h), NOT_SET_UP);

        h.repo.update_setting("1", "ticket_category_id", rusqlite::types::Value::Text("50".into())).unwrap();
        bot.on_interaction(&ctx, &Interaction::Component(h.select(1, 6, "ticket:select", 3, &["missing"]))).await;
        assert!(last_reply(&h).contains("no longer exists"));
    }

    #[tokio::test]
    async fn non_admins_cannot_open_the_ticket_dashboard_or_forge_its_controls() {
        let (h, bot) = harness().await;
        let ctx = h.ctx();
        bot.on_interaction(&ctx, &command(&h, 6, "ticket", None, 1)).await;
        assert_eq!(last_reply(&h), "You need Discord Administrator permission to use Kori Tickets.");
        bot.on_interaction(&ctx, &Interaction::Component(h.select(1, 6, "ticketui:staffadd:x:1", 6, &["9"]))).await;
        assert_eq!(last_reply(&h), "You need Discord Administrator permission to use Kori Tickets.");
        assert_eq!(h.repo.staff_roles("1").unwrap(), [STAFF.to_string()]);
        bot.on_interaction(&ctx, &Interaction::Component(h.button(1, 8, "ticketui:category:remove:2"))).await;
        assert_eq!(last_reply(&h), "That control belongs to a different server.");
    }

    #[tokio::test]
    async fn a_claim_discord_refuses_is_rolled_back() {
        let (h, bot) = harness().await;
        let c: u64 = 778;
        h.repo.open_ticket(&open(&c.to_string(), "5")).unwrap();
        h.on("PUT", &format!("/channels/{c}/permissions/6"), 403, json!({"code": 50013, "message": "Missing Permissions"}));
        bot.on_interaction(&h.ctx(), &in_channel(h.button(1, 6, "ticket:claim"), c)).await;
        assert_eq!(h.repo.ticket("1", &c.to_string()).unwrap().unwrap().claimed_by_id, None, "claim undone");
        assert!(h.calls_to("DELETE", &format!("/channels/{c}/permissions/{STAFF}")).is_empty(), "staff keep access");
    }

    #[tokio::test]
    async fn opening_again_right_after_a_ticket_is_rate_limited() {
        let (h, bot) = harness().await;
        let ctx = h.ctx();
        bot.on_interaction(&ctx, &Interaction::Component(h.button(1, 5, "ticket:open"))).await;
        let first = h.repo.open_ticket_for("1", "5").unwrap().unwrap();
        h.repo.close_ticket("1", &first.channel_id, Some("5")).unwrap();
        bot.on_interaction(&ctx, &Interaction::Component(h.button(1, 5, "ticket:open"))).await;
        assert!(last_reply(&h).starts_with("🟡 Please wait"), "{}", last_reply(&h));
        assert_eq!(h.calls_to("POST", "/guilds/1/channels").len(), 1);
    }

    #[tokio::test]
    async fn closing_saves_a_transcript_deletes_the_channel_and_dms_the_opener() {
        let (h, bot) = harness().await;
        let c: u64 = 779;
        let created = "2026-11-08T09:00:00.000Z";
        h.repo
            .open_ticket(&Ticket { channel_name: Some("billing-user5".into()), type_label: Some("Billing".into()), created_at: created.into(), ..open(&c.to_string(), "5") })
            .unwrap();
        h.repo.claim_ticket("1", &c.to_string(), Some("6"), None).unwrap();
        h.repo.add_ticket_member(&c.to_string(), "9", "6").unwrap();
        h.repo.update_setting("1", "transcript_channel_id", rusqlite::types::Value::Text("60".into())).unwrap();
        h.repo.update_setting("1", "log_channel_id", rusqlite::types::Value::Text("61".into())).unwrap();
        let message = |id: u64, author: u64, content: &str| {
            json!({"id": id.to_string(), "channel_id": c.to_string(), "author": user_json(author), "content": content,
                "timestamp": "2026-11-08T10:00:00.000Z", "edited_timestamp": null, "tts": false, "mention_everyone": false,
                "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0})
        };
        // Discord returns newest first.
        h.on("GET", &format!("/channels/{c}/messages"), 200, json!([message(3, 6, "Fixed, closing."), message(2, 5, "My invoice is wrong")]));
        h.on("DELETE", &format!("/channels/{c}"), 200, json!({"id": c.to_string(), "type": 0, "guild_id": "1", "name": "billing-user5", "position": 0, "permission_overwrites": [], "nsfw": false}));

        bot.on_interaction(&h.ctx(), &command_with(&h, 6, "close", json!([{"type": 3, "name": "reason", "value": "Refund issued"}]), c)).await;
        let notice = h.replies().last().unwrap()["data"]["embeds"][0]["description"].as_str().unwrap().to_string();
        assert!(notice.contains("Closed by <@6>") && notice.contains("**Reason:** Refund issued"), "{notice}");

        let deleted_log = |h: &Harness| h.calls_to("POST", "/channels/61/messages").into_iter().any(|call| call.body["embeds"][0]["title"] == "Ticket Deleted");
        let mut waited = 0;
        while !(h.calls_to("POST", "/users/@me/channels").len() == 1 && deleted_log(&h)) && waited < 100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += 1;
        }
        assert_eq!(h.calls_to("DELETE", &format!("/channels/{c}")).len(), 1, "channel deleted");
        assert_eq!(h.calls_to("POST", "/users/@me/channels").len(), 1, "DM channel opened with the opener");
        let own = [format!("/channels/{c}/messages"), "/channels/60/messages".into(), "/channels/61/messages".into()];
        let dm = h.calls_to("POST", "/channels/*/messages").into_iter().find(|call| !own.contains(&call.path)).expect("DM sent");
        assert!(dm.body.is_null(), "the DM is multipart: it carries the transcript file");
        let transcript = h.calls_to("POST", "/channels/60/messages");
        assert_eq!(transcript.len(), 1, "transcript posted in the server's own transcript channel");
        assert!(transcript[0].body.is_null(), "with the .txt attached");
        assert!(deleted_log(&h), "the server's own log channel records the deletion");
        assert!(h.repo.ticket("1", &c.to_string()).unwrap().is_none());
    }

    #[tokio::test]
    async fn new_tickets_use_the_edited_welcome_and_ping_switches() {
        let (h, bot) = harness().await;
        let ctx = h.ctx();
        // Configured through the dashboard, as an administrator would.
        bot.on_interaction(&ctx, &Interaction::Modal(h.modal(1, 8, "ticketui:welcomeedit:x:1", &[("title", "Help for {user}"), ("description", "We got your {type} request.")]))).await;
        bot.on_interaction(&ctx, &Interaction::Component(h.button(1, 8, "ticketui:welcome:pingstaff:1"))).await;
        let s = h.repo.settings("1").unwrap();
        assert_eq!((s.welcome_title.as_deref(), s.welcome_ping_staff, s.welcome_ping_opener), (Some("Help for {user}"), 0, 1));

        bot.on_interaction(&ctx, &Interaction::Component(h.button(1, 5, "ticket:open"))).await;
        let ticket = h.repo.open_ticket_for("1", "5").unwrap().expect("ticket opened");
        let welcome = h.calls_to("POST", &format!("/channels/{}/messages", ticket.channel_id));
        let body = &welcome[0].body;
        assert_eq!(body["content"], "<@5>", "creator pinged, staff roles not");
        assert_eq!(body["allowed_mentions"]["roles"], json!([]));
        assert_eq!(body["embeds"][0]["title"], "Help for <@5>");
        assert_eq!(body["embeds"][0]["description"], "We got your General request.");

        // A non-admin cannot change it.
        bot.on_interaction(&ctx, &Interaction::Modal(h.modal(1, 9, "ticketui:welcomeedit:x:1", &[("title", "pwned"), ("description", "")]))).await;
        assert_eq!(h.repo.settings("1").unwrap().welcome_title.as_deref(), Some("Help for {user}"));
    }
}
