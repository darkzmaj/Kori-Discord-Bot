//! Kori bot: gateway event wiring and interaction dispatch to the ticket services.
use crate::ctx;
use crate::db::repositories::KoriRepository;
use crate::discord::Discord;
use crate::logger::{self, log_discord_error};
use crate::services::audit_log_service::{AuditEvent, AuditLogService, AuditSubject};
use crate::services::command_sync::CommandSynchronizer;
use crate::services::ticket_service::TicketService;
use crate::services::ticket_ui::TicketUi;
use crate::services::log_routing::LogRouter;
use serde_json::json;
use serenity::all::{
    ChannelId, CommandInteraction, ComponentInteraction, ComponentInteractionDataKind, Context, CreateInteractionResponse,
    CreateInteractionResponseMessage, EventHandler, GatewayIntents, GuildChannel, GuildId, Interaction, InteractionId, Message,
    ModalInteraction, Ready, Role, RoleId,
};
use serenity::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// GUILDS only: tickets need channels, roles and interactions; message text is read over REST for transcripts.
pub fn intents() -> GatewayIntents {
    GatewayIntents::GUILDS
}

pub fn compact_uptime(ms: u128) -> String {
    let total = (ms / 1000) as u64;
    if total < 60 {
        return format!("{total}s");
    }
    let mut parts = Vec::new();
    let mut remaining = total;
    for (suffix, divisor) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        let value = remaining / divisor;
        remaining %= divisor;
        if value != 0 {
            parts.push(format!("{value}{suffix}"));
        }
    }
    parts.join(" ")
}

/// One interaction regardless of its kind, so failures can be answered the same way.
#[derive(Clone, Copy)]
enum Ix<'a> {
    Command(&'a CommandInteraction),
    Component(&'a ComponentInteraction),
    Modal(&'a ModalInteraction),
}

impl Ix<'_> {
    fn id(&self) -> InteractionId {
        match self {
            Ix::Command(i) => i.id,
            Ix::Component(i) => i.id,
            Ix::Modal(i) => i.id,
        }
    }
    fn guild_id(&self) -> Option<GuildId> {
        match self {
            Ix::Command(i) => i.guild_id,
            Ix::Component(i) => i.guild_id,
            Ix::Modal(i) => i.guild_id,
        }
    }
    async fn respond(&self, discord: &Discord, response: CreateInteractionResponse) -> serenity::Result<()> {
        let http = &discord.http;
        match self {
            Ix::Command(i) => i.create_response(http, response).await,
            Ix::Component(i) => i.create_response(http, response).await,
            Ix::Modal(i) => i.create_response(http, response).await,
        }
    }
}

fn ephemeral(content: &str) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(content).ephemeral(true))
}

pub struct KoriBot {
    pub discord: Discord,
    repo: Arc<KoriRepository>,
    pub tickets: Arc<TicketService>,
    pub ticket_ui: Arc<TicketUi>,
    pub audit: Arc<AuditLogService>,
    command_sync: Arc<CommandSynchronizer>,
    started_at: Arc<Mutex<Instant>>,
    ready_once: AtomicBool,
    /// Notified when the ready sequence fails; `main` then exits with code 1.
    pub startup_failed: tokio::sync::Notify,
}

fn audit_event(title: &str, category: &str, color: &str) -> AuditEvent {
    AuditEvent { title: title.into(), category: category.into(), color: color.into(), ..Default::default() }
}

fn subject(id: impl ToString, name: Option<String>) -> Option<AuditSubject> {
    Some(AuditSubject { id: Some(id.to_string()), name })
}

impl KoriBot {
    pub fn new(repo: Arc<KoriRepository>, application_id: Option<String>, discord: Discord) -> Self {
        let audit = Arc::new(AuditLogService::new(Arc::new(LogRouter::new(repo.clone())), discord.http.clone()));
        let tickets = Arc::new(TicketService::new(repo.clone(), discord.clone(), audit.clone()));
        let command_sync = Arc::new(CommandSynchronizer::new(repo.clone(), application_id));
        let started_at = Arc::new(Mutex::new(Instant::now()));
        let uptime_start = started_at.clone();
        let ticket_ui = Arc::new(TicketUi::new(
            repo.clone(),
            tickets.clone(),
            move || compact_uptime(uptime_start.lock().unwrap().elapsed().as_millis()),
        ));
        Self {
            discord,
            repo,
            tickets,
            ticket_ui,
            audit,
            command_sync,
            started_at,
            ready_once: AtomicBool::new(false),
            startup_failed: tokio::sync::Notify::new(),
        }
    }

    // ---- gateway events ----

    async fn on_ready(self: &Arc<Self>) -> anyhow::Result<()> {
        *self.started_at.lock().unwrap() = Instant::now();
        let guilds = self.discord.cache.guilds();
        logger::info(
            "Kori started",
            ctx! {"version" => env!("CARGO_PKG_VERSION"), "guilds" => guilds.len(), "userId" => self.discord.cache.current_user().id.to_string(), "applicationId" => self.discord.http.application_id().map(|a| a.to_string())},
        );
        // A public bot is always per server. If Discord cannot say, stay per server until the next start.
        match self.discord.http.get_current_application_info().await {
            Ok(app) => self.audit.router().set_public(app.bot_public),
            Err(error) => logger::warn("could not read the application's Public Bot setting; global logging stays off", ctx! {"error" => error.to_string()}),
        }
        self.audit.record(AuditEvent {
            details: vec![("Guilds".into(), json!(guilds.len())), ("Version".into(), json!(env!("CARGO_PKG_VERSION")))],
            ..audit_event("Kori Started", "Bot", "success")
        });
        self.command_sync.synchronize(&self.discord, false).await?;
        for guild_id in guilds {
            self.repo.ensure_guild(&guild_id.to_string())?;
            self.tickets.reconcile(guild_id)?;
        }
        Ok(())
    }

    async fn on_guild_create(&self, guild_id: GuildId, name: String) -> anyhow::Result<()> {
        let id = guild_id.to_string();
        self.repo.ensure_guild(&id)?;
        self.command_sync.clear_guild_on_join(&self.discord, guild_id).await;
        self.audit.record(AuditEvent { guild: subject(&id, Some(name)), ..audit_event("Joined Guild", "Bot", "info") });
        Ok(())
    }

    fn on_guild_delete(&self, guild_id: GuildId, name: Option<String>) {
        let id = guild_id.to_string();
        self.audit.record(AuditEvent { priority: "critical".into(), guild: subject(&id, name), ..audit_event("Left Guild", "Bot", "danger") });
        match self.repo.purge_guild(&id) {
            Ok(()) => logger::info("guild data purged after removal", ctx! {"guildId" => id}),
            Err(_) => logger::error("guild cleanup failed", ctx! {"guildId" => id}),
        }
    }

    async fn on_channel_delete(&self, guild_id: GuildId, channel_id: ChannelId) -> anyhow::Result<()> {
        let (guild, channel) = (guild_id.to_string(), channel_id.to_string());
        let settings = self.repo.settings(&guild)?;
        if settings.ticket_category_id.as_deref() == Some(channel.as_str()) {
            self.repo.update_setting(&guild, "ticket_category_id", rusqlite::types::Value::Null)?;
            logger::warn("ticket category deleted; tickets require a new category", ctx! {"guildId" => guild.clone()});
        }
        if settings.panel_channel_id.as_deref() == Some(channel.as_str()) {
            self.repo.set_panel_message(&guild, None)?;
        }
        self.repo.forget_log_channel(&channel)?;
        self.tickets.on_channel_delete(guild_id, channel_id)?;
        Ok(())
    }

    async fn on_role_delete(&self, guild_id: GuildId, role_id: RoleId) -> anyhow::Result<()> {
        if self.repo.remove_staff_role(&guild_id.to_string(), &role_id.to_string())? {
            logger::warn("ticket staff role deleted", ctx! {"guildId" => guild_id.to_string(), "roleId" => role_id.to_string()});
        }
        Ok(())
    }

    // ---- interactions ----

    pub async fn on_interaction(&self, ctx: &Context, interaction: &Interaction) {
        let (ix, custom_id) = match interaction {
            Interaction::Command(i) => (Ix::Command(i), None),
            Interaction::Component(i) => (Ix::Component(i), Some(i.data.custom_id.clone())),
            Interaction::Modal(i) => (Ix::Modal(i), Some(i.data.custom_id.clone())),
            _ => return,
        };
        if let Err(error) = self.route(ctx, interaction).await {
            log_discord_error(
                "interaction.handle",
                &error,
                ctx! {"interactionId" => ix.id().to_string(), "guildId" => ix.guild_id().map(|g| g.to_string()), "customId" => custom_id},
            );
            // No replied/deferred tracking: Discord rejects the reply with 40060 when it was already acknowledged.
            if let Err(error) = ix.respond(&self.discord, ephemeral("Kori encountered an unexpected error. Please try again.")).await {
                if logger::discord_code(&error) != Some(40060) {
                    log_discord_error("interaction.reply", &error.into(), ctx! {"interactionId" => ix.id().to_string()});
                }
            }
        }
    }

    async fn route(&self, ctx: &Context, interaction: &Interaction) -> anyhow::Result<()> {
        match interaction {
            Interaction::Component(i) => {
                let id = i.data.custom_id.as_str();
                if matches!(i.data.kind, ComponentInteractionDataKind::MentionableSelect { .. } | ComponentInteractionDataKind::Unknown(_)) {
                    return Ok(());
                }
                if id.starts_with("ticketui:") {
                    self.ticket_ui.handle(ctx, i).await?;
                } else if id.starts_with("ticket:") {
                    self.tickets.handle_component(i).await?;
                }
            }
            Interaction::Modal(i) => {
                let id = i.data.custom_id.as_str();
                if id.starts_with("ticketui:") {
                    self.ticket_ui.handle_modal(ctx, i).await?;
                } else if id.starts_with("ticket:") {
                    self.tickets.handle_modal(i).await?;
                }
            }
            Interaction::Command(i) => {
                if i.guild_id.is_none() {
                    i.create_response(&self.discord.http, ephemeral("Kori commands can only be used in a server.")).await?;
                    return Ok(());
                }
                match i.data.name.as_str() {
                    "ticket" => self.ticket_ui.open(ctx, i).await?,
                    "add" | "remove" | "reclaim" | "close" => self.tickets.handle_command(i).await?,
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// serenity event handler; the bot is installed after the client (and thus its Http/Cache) exists.
pub struct Handler {
    pub bot: Arc<OnceLock<Arc<KoriBot>>>,
}

impl Handler {
    fn bot(&self) -> Option<Arc<KoriBot>> {
        self.bot.get().cloned()
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn shard_stage_update(&self, _ctx: Context, event: serenity::all::ShardStageUpdateEvent) {
        crate::health::set_connected(event.new == serenity::all::ConnectionStage::Connected);
    }

    async fn ready(&self, ctx: Context, _ready: Ready) {
        crate::health::set_connected(true);
        let Some(bot) = self.bot() else { return };
        if bot.ready_once.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wait until every guild has arrived (or 15s), so startup reconciliation sees them all.
        let deadline = Instant::now() + Duration::from_secs(15);
        while ctx.cache.unavailable_guilds().len() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if let Err(error) = bot.on_ready().await {
            let message = error.to_string();
            logger::error("Kori startup failed", ctx! {"error" => message.clone()});
            bot.audit.record(AuditEvent { priority: "critical".into(), reason: Some(message), ..audit_event("Kori Startup Failed", "Bot", "danger") });
            bot.startup_failed.notify_one();
        }
    }

    async fn guild_create(&self, _ctx: Context, guild: serenity::all::Guild, is_new: Option<bool>) {
        // Initial guild loads after connecting are not joins.
        let (Some(bot), Some(true)) = (self.bot(), is_new) else { return };
        if let Err(error) = bot.on_guild_create(guild.id, guild.name.clone()).await {
            logger::error("guild initialization failed", ctx! {"guildId" => guild.id.to_string(), "error" => error.to_string()});
        }
    }

    async fn guild_delete(&self, _ctx: Context, incomplete: serenity::all::UnavailableGuild, full: Option<serenity::all::Guild>) {
        // `unavailable` means a Discord outage, not removal.
        let Some(bot) = self.bot() else { return };
        if incomplete.unavailable {
            return;
        }
        bot.on_guild_delete(incomplete.id, full.map(|g| g.name));
    }

    async fn channel_delete(&self, _ctx: Context, channel: GuildChannel, _messages: Option<Vec<Message>>) {
        let Some(bot) = self.bot() else { return };
        if let Err(error) = bot.on_channel_delete(channel.guild_id, channel.id).await {
            logger::error(
                "channel deletion handler failed",
                ctx! {"guildId" => channel.guild_id.to_string(), "channelId" => channel.id.to_string(), "error" => error.to_string()},
            );
        }
    }

    async fn guild_role_delete(&self, _ctx: Context, guild_id: GuildId, role_id: RoleId, _role: Option<Role>) {
        let Some(bot) = self.bot() else { return };
        if let Err(error) = bot.on_role_delete(guild_id, role_id).await {
            logger::error("role deletion handler failed", ctx! {"guildId" => guild_id.to_string(), "roleId" => role_id.to_string(), "error" => error.to_string()});
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Some(bot) = self.bot() else { return };
        bot.on_interaction(&ctx, &interaction).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_uptime_formats() {
        assert_eq!(compact_uptime(59_999), "59s");
        assert_eq!(compact_uptime(3_660_000), "1h 1m");
        assert_eq!(compact_uptime(90_000_000), "1d 1h");
    }
}
