//! `/ticket` ephemeral dashboard: the whole current setup at a glance, plus Setup (status and category), Staff Roles,
//! Ticket Types and Panel (embed editor and sending). Edits to the panel or types update the posted panel in place.
//! Every component re-checks Discord Administrator before acting.
use std::sync::Arc;

use serenity::all::{
    ButtonStyle, ChannelId, ChannelType, CommandInteraction, ComponentInteraction, ComponentInteractionDataKind,
    Context, CreateActionRow, CreateButton, CreateChannel, CreateEmbed, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateModal, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption,
    GuildId, InputTextStyle, ModalInteraction, PermissionOverwrite, PermissionOverwriteType, Permissions, RoleId, User,
};

use crate::ctx;
use crate::db::repositories::{GuildSettings, KoriRepository, PanelEmbed, Ticket, TicketType};
use crate::discord::Discord;
use crate::logger::log_discord_error;
use crate::services::audit_log_service::{AuditEvent, AuditSubject};
use crate::services::authorization::{has_permission, PermissionLevel};
use crate::services::log_routing::{LogRouter, ACTIVITY, GLOBAL, PER_SERVER, TRANSCRIPTS};
use crate::services::ticket_service::{
    bot_allow, creatable, panel_components, panel_embed, parse_panel_embed, parse_welcome, valid_emoji, welcome_pings, welcome_view, PanelUpdate,
    TicketService, DEFAULT_PANEL_DESCRIPTION, DEFAULT_PANEL_TITLE, DEFAULT_WELCOME_DESCRIPTION, DEFAULT_WELCOME_TITLE, MAX_TICKET_TYPES,
    SLOWMODE_SECONDS,
};

const COLOR: u32 = crate::discord::KORI_BLUE;

fn snowflake(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().filter(|v| *v != 0)
}

const BOT: &str = "Kori";

fn guild_name(ctx: &Context, id: &str) -> String {
    snowflake(id).and_then(|g| ctx.cache.guild(GuildId::new(g)).map(|g| g.name.clone())).unwrap_or_else(|| format!("Server {id}"))
}

fn kind_what(kind: &str) -> &'static str {
    if kind == TRANSCRIPTS { "closed-ticket transcripts" } else { "ticket activity logs" }
}

fn kind_label(kind: &str) -> &'static str {
    if kind == TRANSCRIPTS { "Transcripts" } else { "Activity Log" }
}

fn channel_line(channel: &Option<String>) -> String {
    channel.as_ref().map(|id| format!("<#{id}>")).unwrap_or_else(|| "⚪ Not set".into())
}

/// An interaction payload: `None` content leaves the message's current content untouched.
#[derive(Default)]
struct View {
    content: Option<String>,
    embeds: Vec<CreateEmbed>,
    components: Vec<CreateActionRow>,
}

impl View {
    fn text(content: impl Into<String>, components: Vec<CreateActionRow>) -> Self {
        View { content: Some(content.into()), embeds: vec![], components }
    }
    fn embed(embed: CreateEmbed, components: Vec<CreateActionRow>) -> Self {
        View { content: Some(String::new()), embeds: vec![embed], components }
    }
    fn content(mut self, content: Option<String>) -> Self {
        if content.is_some() {
            self.content = content;
        }
        self
    }
    fn message(self) -> CreateInteractionResponseMessage {
        let mut out = CreateInteractionResponseMessage::new().embeds(self.embeds).components(self.components);
        if let Some(content) = self.content {
            out = out.content(content);
        }
        out
    }
}

fn ephemeral(content: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(content).ephemeral(true))
}

fn button(custom_id: String, label: &str, style: ButtonStyle) -> CreateButton {
    CreateButton::new(custom_id).label(label).style(style)
}

fn nav(section: &str, guild_id: GuildId, label: &str) -> CreateButton {
    nav_styled(section, guild_id, label, ButtonStyle::Primary)
}

fn nav_styled(section: &str, guild_id: GuildId, label: &str, style: ButtonStyle) -> CreateButton {
    button(format!("ticketui:nav:{section}:{guild_id}"), label, style)
}

fn act(kind: &str, action: &str, guild_id: GuildId, label: &str, style: ButtonStyle) -> CreateButton {
    button(format!("ticketui:{kind}:{action}:{guild_id}"), label, style)
}

fn row(buttons: Vec<CreateButton>) -> CreateActionRow {
    CreateActionRow::Buttons(buttons)
}

fn select(custom_id: String, kind: CreateSelectMenuKind, placeholder: &str) -> CreateActionRow {
    CreateActionRow::SelectMenu(CreateSelectMenu::new(custom_id, kind).placeholder(placeholder))
}

/// The `[_, kind, action, guildId]` parts of a `ticketui:` custom ID; missing parts are empty.
fn parts(custom_id: &str) -> (&str, &str, &str) {
    let mut split = custom_id.split(':').skip(1);
    (split.next().unwrap_or(""), split.next().unwrap_or(""), split.next().unwrap_or(""))
}

fn values(i: &ComponentInteraction) -> Vec<String> {
    match &i.data.kind {
        ComponentInteractionDataKind::StringSelect { values } => values.clone(),
        ComponentInteractionDataKind::RoleSelect { values } => values.iter().map(|v| v.to_string()).collect(),
        ComponentInteractionDataKind::ChannelSelect { values } => values.iter().map(|v| v.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// A native role picker that accepts up to 25 roles per pick (Discord's maximum); repeat to add more.
fn role_picker(custom_id: String, placeholder: &str) -> CreateActionRow {
    CreateActionRow::SelectMenu(
        CreateSelectMenu::new(custom_id, CreateSelectMenuKind::Role { default_roles: None }).placeholder(placeholder).min_values(1).max_values(25),
    )
}

fn modal_value(i: &ModalInteraction, id: &str) -> String {
    i.data
        .components
        .iter()
        .flat_map(|row| row.components.iter())
        .find_map(|c| match c {
            serenity::all::ActionRowComponent::InputText(input) if input.custom_id == id => input.value.clone(),
            _ => None,
        })
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Ticket type labels become channel prefixes and select options: 1–50 characters, no control characters.
pub fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.chars().count() <= 50 && !label.chars().any(char::is_control)
}

/// A modal text input, prefilled with the current value when there is one.
fn field(style: InputTextStyle, label: &str, id: &str, max: u16, required: bool, value: Option<&str>) -> CreateActionRow {
    let mut input = CreateInputText::new(style, label, id).max_length(max).required(required);
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        input = input.value(value);
    }
    CreateActionRow::InputText(input)
}

fn type_modal(custom_id: String, title: &str, t: Option<&TicketType>) -> CreateModal {
    CreateModal::new(custom_id, title).components(vec![
        field(InputTextStyle::Short, "Name (e.g. Support, Billing)", "label", 50, true, t.map(|t| t.label.as_str())),
        field(InputTextStyle::Short, "Description (optional)", "description", 100, false, t.and_then(|t| t.description.as_deref())),
        field(InputTextStyle::Short, "Emoji (optional, e.g. 💳)", "emoji", 64, false, t.and_then(|t| t.emoji.as_deref())),
    ])
}

fn type_line(t: &TicketType) -> String {
    format!("{}{}", t.emoji.as_deref().map(|e| format!("{e} ")).unwrap_or_default(), t.label)
}

fn ping_summary(s: &GuildSettings) -> &'static str {
    match (s.welcome_ping_opener != 0, s.welcome_ping_staff != 0) {
        (true, true) => "the creator and all staff roles",
        (true, false) => "the creator",
        (false, true) => "all staff roles",
        (false, false) => "nobody",
    }
}

fn panel_location(guild_id: GuildId, s: &GuildSettings) -> String {
    match (s.panel_channel_id.as_deref(), s.panel_message_id.as_deref()) {
        (Some(channel), Some(message)) => format!("<#{channel}> ([jump](https://discord.com/channels/{guild_id}/{channel}/{message}))"),
        _ => "🟡 Not sent yet".into(),
    }
}

/// Interaction details shared by buttons, selects and modals.
struct Actor<'a> {
    id: serenity::all::InteractionId,
    token: &'a str,
    custom_id: &'a str,
    user: &'a User,
    guild_id: GuildId,
}

impl Actor<'_> {
    async fn respond(&self, ctx: &Context, response: CreateInteractionResponse) -> anyhow::Result<()> {
        ctx.http.create_interaction_response(self.id, self.token, &response, vec![]).await?;
        Ok(())
    }
    async fn reply(&self, ctx: &Context, content: impl Into<String>) -> anyhow::Result<()> {
        self.respond(ctx, ephemeral(content)).await
    }
    async fn update(&self, ctx: &Context, view: View) -> anyhow::Result<()> {
        self.respond(ctx, CreateInteractionResponse::UpdateMessage(view.message())).await
    }
}

pub struct TicketUi {
    repo: Arc<KoriRepository>,
    tickets: Arc<TicketService>,
    uptime: Box<dyn Fn() -> String + Send + Sync>,
}

impl TicketUi {
    pub fn new(repo: Arc<KoriRepository>, tickets: Arc<TicketService>, uptime: impl Fn() -> String + Send + Sync + 'static) -> Self {
        TicketUi { repo, tickets, uptime: Box::new(uptime) }
    }

    fn audit_configuration(&self, ctx: &Context, a: &Actor<'_>, title: &str, details: Vec<(&str, String)>) {
        let guild_name = ctx.cache.guild(a.guild_id).map(|g| g.name.clone()).unwrap_or_default();
        self.tickets.log(AuditEvent {
            title: title.into(),
            category: "Ticket Configuration".into(),
            color: "warning".into(),
            guild: Some(AuditSubject::named(a.guild_id.to_string(), guild_name)),
            actor: Some(AuditSubject::named(a.user.id.to_string(), a.user.tag())),
            details: details.into_iter().map(|(k, v)| (k.to_string(), v.into())).collect(),
            ..Default::default()
        });
    }

    async fn admin(&self, ctx: &Context, a: &Actor<'_>) -> anyhow::Result<bool> {
        let member = a.guild_id.member(ctx, a.user.id).await.ok();
        let allowed = match &member {
            Some(member) => {
                let discord = Discord { http: ctx.http.clone(), cache: ctx.cache.clone() };
                has_permission(&self.repo, &discord, member, PermissionLevel::Administrator).await?
            }
            None => false,
        };
        if !allowed {
            a.reply(ctx, "You need Discord Administrator permission to use Kori Tickets.").await?;
        }
        Ok(allowed)
    }

    /// `/ticket`
    pub async fn open(&self, ctx: &Context, i: &CommandInteraction) -> anyhow::Result<()> {
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        let a = Actor { id: i.id, token: &i.token, custom_id: "", user: &i.user, guild_id };
        if !self.admin(ctx, &a).await? {
            return Ok(());
        }
        // Best effort: a transient Discord error leaves the tracked panel as it is.
        let _ = self.tickets.verify_panel(guild_id).await;
        let view = if self.tickets.router().mode(guild_id).is_none() { self.log_mode_view(guild_id) } else { self.dashboard(ctx, guild_id)? };
        a.respond(ctx, CreateInteractionResponse::Message(view.message().ephemeral(true))).await
    }

    /// Guard shared by every component and modal: in a guild, still Administrator, control belongs to this guild.
    async fn guard<'a>(&self, ctx: &Context, a: Actor<'a>) -> anyhow::Result<Option<Actor<'a>>> {
        if !self.admin(ctx, &a).await? {
            return Ok(None);
        }
        if parts(a.custom_id).2 != a.guild_id.to_string() {
            a.reply(ctx, "That control belongs to a different server.").await?;
            return Ok(None);
        }
        Ok(Some(a))
    }

    pub async fn handle(&self, ctx: &Context, i: &ComponentInteraction) -> anyhow::Result<()> {
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        let actor = Actor { id: i.id, token: &i.token, custom_id: &i.data.custom_id, user: &i.user, guild_id };
        let Some(a) = self.guard(ctx, actor).await? else { return Ok(()) };
        let (kind, action, _) = parts(a.custom_id);
        let g = a.guild_id;
        let gs = g.to_string();
        if !matches!(i.data.kind, ComponentInteractionDataKind::Button) {
            return self.handle_select(ctx, &a, kind, action, &values(i)).await;
        }
        match (kind, action) {
            ("nav", section) => a.update(ctx, self.view(ctx, g, section)?).await,
            ("toggle", "tickets") => {
                let enabled = self.repo.settings(&gs)?.tickets_enabled == 0;
                self.repo.update_setting(&gs, "tickets_enabled", rusqlite::types::Value::Integer(enabled.into()))?;
                self.audit_configuration(ctx, &a, if enabled { "Tickets Enabled" } else { "Tickets Disabled" }, vec![]);
                a.update(ctx, self.view(ctx, g, "setup")?).await
            }
            ("category", "create") => self.create_category(ctx, &a).await,
            ("category", "pick") => {
                let picker = select(
                    format!("ticketui:categoryset:x:{g}"),
                    CreateSelectMenuKind::Channel { channel_types: Some(vec![ChannelType::Category]), default_channels: None },
                    "Select a category",
                );
                a.update(ctx, View::text("Select the category new tickets are created in.", vec![picker, row(vec![nav("setup", g, "← Back")])])).await
            }
            ("category", "remove") => {
                self.repo.update_setting(&gs, "ticket_category_id", rusqlite::types::Value::Null)?;
                self.audit_configuration(ctx, &a, "Ticket Category Changed", vec![("After", "None".into())]);
                a.update(ctx, self.view(ctx, g, "setup")?).await
            }
            ("staff", "add") => {
                let picker = role_picker(format!("ticketui:staffadd:x:{g}"), "Select roles to add");
                a.update(ctx, View::text("Select roles whose members can see, claim and manage tickets. There is no limit; pick again to add more.", vec![picker, row(vec![nav("staff", g, "← Back")])])).await
            }
            ("staff", "remove") => {
                if self.repo.staff_roles(&gs)?.is_empty() {
                    return a.update(ctx, self.view(ctx, g, "staff")?).await;
                }
                let picker = role_picker(format!("ticketui:staffremove:x:{g}"), "Select staff roles to remove");
                a.update(ctx, View::text("Select the staff roles to remove. Deleted roles are removed automatically.", vec![picker, row(vec![nav("staff", g, "← Back")])])).await
            }
            ("types", "add") => {
                if self.repo.ticket_types(&gs)?.len() >= MAX_TICKET_TYPES {
                    return a.reply(ctx, format!("A server can have at most {MAX_TICKET_TYPES} ticket types.")).await;
                }
                a.respond(ctx, CreateInteractionResponse::Modal(type_modal(format!("ticketui:typeadd:x:{g}"), "Add Ticket Type", None))).await
            }
            ("types", "edit") => {
                let options: Vec<CreateSelectMenuOption> =
                    self.repo.ticket_types(&gs)?.into_iter().map(|t| CreateSelectMenuOption::new(type_line(&t), t.type_id)).collect();
                if options.is_empty() {
                    return a.update(ctx, self.view(ctx, g, "types")?).await;
                }
                let picker = select(format!("ticketui:typeedit:x:{g}"), CreateSelectMenuKind::String { options }, "Select a ticket type to edit");
                a.update(ctx, View::text("Select the ticket type to edit.", vec![picker, row(vec![nav("types", g, "← Back")])])).await
            }
            ("types", "remove") => {
                let options: Vec<CreateSelectMenuOption> =
                    self.repo.ticket_types(&gs)?.into_iter().map(|t| CreateSelectMenuOption::new(t.label, t.type_id)).collect();
                if options.is_empty() {
                    return a.update(ctx, self.view(ctx, g, "types")?).await;
                }
                let picker = select(format!("ticketui:typeremove:x:{g}"), CreateSelectMenuKind::String { options }, "Select a ticket type to remove");
                a.update(ctx, View::text("Select the ticket type to remove.", vec![picker, row(vec![nav("types", g, "← Back")])])).await
            }
            ("panel", "embed") => {
                let s = self.repo.settings(&gs)?;
                let color = s.panel_color.map(|c| format!("#{c:06X}"));
                let modal = CreateModal::new(format!("ticketui:panelembed:x:{g}"), "Edit Panel Embed").components(vec![
                    field(InputTextStyle::Short, "Title", "title", 256, false, Some(s.panel_title.as_deref().unwrap_or(DEFAULT_PANEL_TITLE))),
                    field(InputTextStyle::Paragraph, "Description", "description", 4000, false, Some(s.panel_description.as_deref().unwrap_or(DEFAULT_PANEL_DESCRIPTION))),
                    field(InputTextStyle::Short, "Colour (hex, e.g. #5DADE2)", "color", 7, false, color.as_deref()),
                    field(InputTextStyle::Short, "Image URL (https, optional)", "image", 2000, false, s.panel_image_url.as_deref()),
                    field(InputTextStyle::Short, "Footer (optional)", "footer", 2048, false, s.panel_footer.as_deref()),
                ]);
                a.respond(ctx, CreateInteractionResponse::Modal(modal)).await
            }
            ("panel", "default") => {
                self.repo.set_panel_embed(&gs, &PanelEmbed::default())?;
                self.audit_configuration(ctx, &a, "Ticket Panel Embed Reset", vec![]);
                let notice = self.panel_note(g).await;
                a.update(ctx, self.view(ctx, g, "panel")?.content(notice)).await
            }
            ("panel", "preview") => {
                let preview = CreateInteractionResponseMessage::new()
                    .content("Preview — the ticket controls below are live only on the sent panel.")
                    .embed(panel_embed(&self.repo.settings(&gs)?))
                    .components(panel_components(&self.repo.ticket_types(&gs)?))
                    .ephemeral(true);
                a.respond(ctx, CreateInteractionResponse::Message(preview)).await
            }
            ("welcome", "edit") => {
                let s = self.repo.settings(&gs)?;
                let modal = CreateModal::new(format!("ticketui:welcomeedit:x:{g}"), "Edit Ticket Welcome").components(vec![
                    field(InputTextStyle::Short, "Title ({type} and {user} work)", "title", 200, false, Some(s.welcome_title.as_deref().unwrap_or(DEFAULT_WELCOME_TITLE))),
                    field(InputTextStyle::Paragraph, "Description ({user} mentions the creator)", "description", 4000, false, Some(s.welcome_description.as_deref().unwrap_or(DEFAULT_WELCOME_DESCRIPTION))),
                ]);
                a.respond(ctx, CreateInteractionResponse::Modal(modal)).await
            }
            ("welcome", key @ ("pingopener" | "pingstaff")) => {
                let column = if key == "pingopener" { "welcome_ping_opener" } else { "welcome_ping_staff" };
                let s = self.repo.settings(&gs)?;
                let on = if key == "pingopener" { s.welcome_ping_opener == 0 } else { s.welcome_ping_staff == 0 };
                self.repo.update_setting(&gs, column, rusqlite::types::Value::Integer(on.into()))?;
                let who = if key == "pingopener" { "Ticket Creator" } else { "Staff Roles" };
                self.audit_configuration(ctx, &a, &format!("Welcome Ping {who} {}", if on { "Enabled" } else { "Disabled" }), vec![]);
                a.update(ctx, self.view(ctx, g, "welcome")?).await
            }
            ("welcome", "reset") => {
                self.repo.set_welcome_text(&gs, None, None)?;
                self.audit_configuration(ctx, &a, "Ticket Welcome Reset", vec![]);
                a.update(ctx, self.view(ctx, g, "welcome")?).await
            }
            ("welcome", "preview") => {
                let s = self.repo.settings(&gs)?;
                let sample = Ticket { opener_id: a.user.id.to_string(), type_label: self.repo.ticket_types(&gs)?.first().map(|t| t.label.clone()), ..Default::default() };
                let (embed, _) = welcome_view(&sample, &s);
                let staff: Vec<RoleId> = self.repo.staff_roles(&gs)?.iter().filter_map(|r| snowflake(r)).map(RoleId::new).collect();
                let (pings, _) = welcome_pings(&s, a.user.id, &staff);
                let preview = CreateInteractionResponseMessage::new()
                    .content(format!("Preview (nobody is pinged by a preview). A new ticket would ping: {}", if pings.is_empty() { "nobody".into() } else { pings }))
                    .embed(embed)
                    .allowed_mentions(serenity::all::CreateAllowedMentions::new())
                    .ephemeral(true);
                a.respond(ctx, CreateInteractionResponse::Message(preview)).await
            }
            ("logmode", choice @ (GLOBAL | PER_SERVER)) => self.set_log_mode(ctx, &a, choice).await,
            ("logmode", "change") => a.update(ctx, self.log_mode_view(g)).await,
            ("logs", "hub") => self.make_hub(ctx, &a).await,
            ("logs", which @ ("pick" | "pickt")) => {
                if !self.edits_own_channels(g) {
                    return a.update(ctx, self.view(ctx, g, "logs")?).await;
                }
                let (kind, what) = if which == "pick" { ("logset", kind_what(ACTIVITY)) } else { ("transcriptset", kind_what(TRANSCRIPTS)) };
                let picker = select(
                    format!("ticketui:{kind}:x:{g}"),
                    CreateSelectMenuKind::Channel { channel_types: Some(vec![ChannelType::Text]), default_channels: None },
                    "Select a text channel",
                );
                a.update(ctx, View::text(format!("Select the channel for {what}. Keep it private to staff."), vec![picker, row(vec![nav("logs", g, "← Back")])])).await
            }
            ("logs", which @ ("clear" | "cleart")) => {
                if self.edits_own_channels(g) {
                    let (key, title) = if which == "clear" { (ACTIVITY, "Ticket Log Channel Cleared") } else { (TRANSCRIPTS, "Ticket Transcript Channel Cleared") };
                    self.repo.update_setting(&gs, key, rusqlite::types::Value::Null)?;
                    self.audit_configuration(ctx, &a, title, vec![]);
                }
                a.update(ctx, self.view(ctx, g, "logs")?).await
            }
            ("logs", "servers") => {
                let options: Vec<CreateSelectMenuOption> = if self.tickets.router().hub() == Some(g) {
                    self.repo.global_guilds()?.into_iter().take(25).map(|id| CreateSelectMenuOption::new(guild_name(ctx, &id), id)).collect()
                } else {
                    Vec::new()
                };
                if options.is_empty() {
                    return a.update(ctx, self.view(ctx, g, "logs")?).await;
                }
                let picker = select(format!("ticketui:routesrc:x:{g}"), CreateSelectMenuKind::String { options }, "Select a server");
                a.update(ctx, View::text("Select a server in global mode to give its logs their own channels here.", vec![picker, row(vec![nav("logs", g, "← Back")])])).await
            }
            ("routeview", source) => a.update(ctx, self.route_view(ctx, g, source)?).await,
            ("route", spec) => {
                let Some((kind, source)) = self.route_spec(g, spec) else { return a.update(ctx, self.view(ctx, g, "logs")?).await };
                let picker = select(
                    format!("ticketui:routeset:{spec}:{g}"),
                    CreateSelectMenuKind::Channel { channel_types: Some(vec![ChannelType::Text]), default_channels: None },
                    "Select a text channel",
                );
                let back = button(format!("ticketui:routeview:{source}:{g}"), "← Back", ButtonStyle::Primary);
                a.update(ctx, View::text(format!("Select the channel in this server for {} from **{}**.", kind_what(kind), guild_name(ctx, &source)), vec![picker, row(vec![back])])).await
            }
            ("routeclr", spec) => match self.route_spec(g, spec) {
                Some((kind, source)) => {
                    self.repo.set_log_route(&source, kind, None)?;
                    self.audit_configuration(ctx, &a, "Server Log Channel Cleared", vec![("Server", guild_name(ctx, &source)), ("Log", kind_label(kind).into())]);
                    a.update(ctx, self.route_view(ctx, g, &source)?).await
                }
                None => a.update(ctx, self.view(ctx, g, "logs")?).await,
            },
            ("panel", "send") => {
                let picker = select(
                    format!("ticketui:panelsend:x:{g}"),
                    CreateSelectMenuKind::Channel { channel_types: Some(vec![ChannelType::Text, ChannelType::News]), default_channels: None },
                    "Select a text channel",
                );
                a.update(ctx, View::text("Select the channel to send the ticket panel to.", vec![picker, row(vec![nav("panel", g, "← Back")])])).await
            }
            _ => Ok(()),
        }
    }

    async fn handle_select(&self, ctx: &Context, a: &Actor<'_>, kind: &str, action: &str, values: &[String]) -> anyhow::Result<()> {
        let g = a.guild_id;
        let gs = g.to_string();
        let value = values.first().map(String::as_str).unwrap_or_default();
        match kind {
            "categoryset" => {
                let id = snowflake(value).map(ChannelId::new);
                let valid = id.is_some_and(|id| ctx.cache.guild(g).is_some_and(|guild| guild.channels.get(&id).is_some_and(|c| c.kind == ChannelType::Category)));
                if !valid {
                    return a.update(ctx, self.view(ctx, g, "setup")?.content(Some("That category no longer exists.".into()))).await;
                }
                self.repo.update_setting(&gs, "ticket_category_id", rusqlite::types::Value::Text(value.into()))?;
                self.audit_configuration(ctx, a, "Ticket Category Changed", vec![("After", format!("<#{value}>"))]);
                a.update(ctx, self.view(ctx, g, "setup")?).await
            }
            "staffadd" => {
                let mut added = Vec::new();
                let mut refused = Vec::new();
                for id in values {
                    let role = snowflake(id).and_then(|r| ctx.cache.guild(g).and_then(|guild| guild.roles.get(&RoleId::new(r)).cloned()));
                    match role {
                        Some(role) if role.id.get() != g.get() && !role.managed => {
                            if self.repo.add_staff_role(&gs, id, &a.user.id.to_string())? {
                                added.push(format!("<@&{id}>"));
                            }
                        }
                        _ => refused.push(id.clone()),
                    }
                }
                if !added.is_empty() {
                    self.audit_configuration(ctx, a, "Ticket Staff Roles Added", vec![("Roles", added.join(", "))]);
                }
                let notice = (!refused.is_empty()).then(|| {
                    format!("🟡 Skipped {} role(s): @everyone, integration-managed and deleted roles cannot be ticket staff.", refused.len())
                });
                a.update(ctx, self.view(ctx, g, "staff")?.content(notice)).await
            }
            "staffremove" => {
                let mut removed = Vec::new();
                for id in values {
                    if self.repo.remove_staff_role(&gs, id)? {
                        removed.push(format!("<@&{id}>"));
                    }
                }
                if !removed.is_empty() {
                    self.audit_configuration(ctx, a, "Ticket Staff Roles Removed", vec![("Roles", removed.join(", "))]);
                }
                a.update(ctx, self.view(ctx, g, "staff")?).await
            }
            "typeremove" => {
                let mut notice = None;
                if let Some(t) = self.repo.ticket_type(&gs, value)? {
                    self.repo.remove_ticket_type(&gs, value)?;
                    self.audit_configuration(ctx, a, "Ticket Type Removed", vec![("Type", t.label)]);
                    notice = self.panel_note(g).await;
                }
                a.update(ctx, self.view(ctx, g, "types")?.content(notice)).await
            }
            "typeedit" => match self.repo.ticket_type(&gs, value)? {
                Some(t) => a.respond(ctx, CreateInteractionResponse::Modal(type_modal(format!("ticketui:typeupdate:{}:{g}", t.type_id), "Edit Ticket Type", Some(&t)))).await,
                None => a.update(ctx, self.view(ctx, g, "types")?.content(Some("That ticket type no longer exists.".into()))).await,
            },
            "panelsend" => self.send_panel(ctx, a, value).await,
            "logset" | "transcriptset" => self.set_log_channel(ctx, a, kind == "logset", value).await,
            "routesrc" => a.update(ctx, self.route_view(ctx, g, value)?).await,
            "routeset" => self.set_route_channel(ctx, a, action, value).await,
            _ => Ok(()),
        }
    }

    pub async fn handle_modal(&self, ctx: &Context, i: &ModalInteraction) -> anyhow::Result<()> {
        let Some(guild_id) = i.guild_id else { return Ok(()) };
        let actor = Actor { id: i.id, token: &i.token, custom_id: &i.data.custom_id, user: &i.user, guild_id };
        let Some(a) = self.guard(ctx, actor).await? else { return Ok(()) };
        let (kind, action, _) = parts(a.custom_id);
        let g = a.guild_id;
        let gs = g.to_string();
        if kind == "panelembed" {
            let parsed = parse_panel_embed(&modal_value(i, "title"), &modal_value(i, "description"), &modal_value(i, "color"), &modal_value(i, "image"), &modal_value(i, "footer"));
            let notice = match parsed {
                Err(reason) => Some(format!("🔴 Not saved: {reason}")),
                Ok(embed) => {
                    self.repo.set_panel_embed(&gs, &embed)?;
                    self.audit_configuration(ctx, &a, "Ticket Panel Embed Edited", vec![("Title", embed.title.unwrap_or_else(|| DEFAULT_PANEL_TITLE.into()))]);
                    Some(self.panel_note(g).await.unwrap_or_else(|| "🟢 Panel embed saved. Send the panel to post it.".into()))
                }
            };
            return a.update(ctx, self.view(ctx, g, "panel")?.content(notice)).await;
        }
        if kind == "welcomeedit" {
            let notice = match parse_welcome(&modal_value(i, "title"), &modal_value(i, "description")) {
                Err(reason) => Some(format!("🔴 Not saved: {reason}")),
                Ok((title, description)) => {
                    self.repo.set_welcome_text(&gs, title.as_deref(), description.as_deref())?;
                    self.audit_configuration(ctx, &a, "Ticket Welcome Edited", vec![("Title", title.unwrap_or_else(|| DEFAULT_WELCOME_TITLE.into()))]);
                    Some("🟢 Saved. New tickets use this welcome message.".into())
                }
            };
            return a.update(ctx, self.view(ctx, g, "welcome")?.content(notice)).await;
        }
        if kind != "typeadd" && kind != "typeupdate" {
            return Ok(());
        }
        let label = modal_value(i, "label");
        let description = Some(modal_value(i, "description")).filter(|d| !d.is_empty());
        let emoji = Some(modal_value(i, "emoji")).filter(|e| !e.is_empty());
        let notice = if !valid_label(&label) || description.as_deref().is_some_and(|d| d.chars().any(char::is_control)) {
            Some("Ticket type names must be 1–50 characters without control characters.".to_string())
        } else if emoji.as_deref().is_some_and(|e| !valid_emoji(e)) {
            Some("Use a single emoji such as 💳, or a custom emoji like `<:name:123456789012345678>`.".into())
        } else if kind == "typeupdate" {
            let t = TicketType { type_id: action.to_string(), guild_id: gs.clone(), label: label.clone(), description, emoji };
            if self.repo.update_ticket_type(&t)? {
                self.audit_configuration(ctx, &a, "Ticket Type Edited", vec![("Type", label)]);
                self.panel_note(g).await
            } else {
                Some(format!("Not saved: that type no longer exists, or another type is already named **{label}**."))
            }
        } else if self.repo.ticket_types(&gs)?.len() >= MAX_TICKET_TYPES {
            Some(format!("A server can have at most {MAX_TICKET_TYPES} ticket types."))
        } else {
            let t = TicketType { type_id: uuid::Uuid::new_v4().to_string(), guild_id: gs.clone(), label: label.clone(), description, emoji };
            if self.repo.add_ticket_type(&t)? {
                self.audit_configuration(ctx, &a, "Ticket Type Added", vec![("Type", label)]);
                self.panel_note(g).await
            } else {
                Some(format!("A ticket type named **{label}** already exists."))
            }
        };
        a.update(ctx, self.view(ctx, g, "types")?.content(notice)).await
    }

    /// Refreshes the posted panel after a change and says what happened (None when no panel was sent).
    async fn panel_note(&self, guild_id: GuildId) -> Option<String> {
        match self.tickets.refresh_panel(guild_id).await {
            Ok(PanelUpdate::Updated) => Some("🟢 Saved. The posted panel was updated in place.".into()),
            Ok(PanelUpdate::NotSent) => None,
            Ok(PanelUpdate::Gone) => Some("🟡 Saved, but the posted panel was deleted. Send it again from **Panel**.".into()),
            Err(error) => {
                log_discord_error("ticket.panel.refresh", &error, ctx! {"guildId" => guild_id.to_string()});
                Some("🔴 Saved, but Kori could not update the posted panel. Check its permissions in that channel.".into())
            }
        }
    }

    async fn create_category(&self, ctx: &Context, a: &Actor<'_>) -> anyhow::Result<()> {
        let g = a.guild_id;
        let me = ctx.cache.current_user().id;
        let bot = ctx.cache.guild(g).and_then(|guild| guild.members.get(&me).map(|m| guild.member_permissions(m))).unwrap_or_else(Permissions::empty);
        let builder = CreateChannel::new("Tickets")
            .kind(ChannelType::Category)
            .permissions(vec![
                PermissionOverwrite { allow: Permissions::empty(), deny: Permissions::VIEW_CHANNEL, kind: PermissionOverwriteType::Role(RoleId::new(g.get())) },
                PermissionOverwrite { allow: creatable(bot_allow(), bot), deny: Permissions::empty(), kind: PermissionOverwriteType::Member(me) },
            ])
            .audit_log_reason("Kori ticket category");
        let notice = match g.create_channel(&ctx.http, builder).await {
            Ok(category) => {
                self.repo.update_setting(&g.to_string(), "ticket_category_id", rusqlite::types::Value::Text(category.id.to_string()))?;
                self.audit_configuration(ctx, a, "Ticket Category Created", vec![("Category", format!("<#{}>", category.id))]);
                format!("🟢 Created <#{}>. New tickets will be created there.", category.id)
            }
            Err(error) => {
                log_discord_error("ticket.category.create", &error.into(), ctx! {"guildId" => g.to_string()});
                "🔴 Kori could not create the category. Check its Manage Channels and Manage Roles permissions.".into()
            }
        };
        a.update(ctx, self.view(ctx, g, "setup")?.content(Some(notice))).await
    }

    /// The first menu of `/ticket` (the same as Gaia's `/admin`): global or per-server logging.
    fn log_mode_view(&self, g: GuildId) -> View {
        let router = self.tickets.router();
        let public = router.is_public();
        let mut text = format!(
            "**How should {BOT} log?**\n\n🌐 **Global**: logs from every server {BOT} is in go to one log server, and each log shows which server it came from. You can give each server its own channels there. Choose this when {BOT} is your own bot.\n\n🏠 **Per server**: this server's logs stay in this server. Nothing is shared with other servers."
        );
        if public {
            text.push_str(&format!("\n\n🔒 {BOT} is a public bot, so logs always stay per server."));
        }
        text.push_str("\n\nOnly the server owner can choose. You can change it later in **Logs**.");
        let mut buttons = vec![
            button(format!("ticketui:logmode:{GLOBAL}:{g}"), "Global", ButtonStyle::Primary).disabled(public),
            button(format!("ticketui:logmode:{PER_SERVER}:{g}"), "Per server", ButtonStyle::Primary),
        ];
        if router.mode(g).is_some() {
            buttons.push(nav("logs", g, "← Back"));
        }
        View::embed(CreateEmbed::new().color(COLOR).title("Logging Setup").description(text), vec![row(buttons)])
    }

    async fn set_log_mode(&self, ctx: &Context, a: &Actor<'_>, choice: &str) -> anyhow::Result<()> {
        let g = a.guild_id;
        let router = self.tickets.router().clone();
        if !LogRouter::is_owner(&ctx.cache, g, a.user.id) {
            return a.reply(ctx, format!("Only the server owner can choose how {BOT} logs.")).await;
        }
        if choice == GLOBAL && router.is_public() {
            return a.reply(ctx, format!("{BOT} is a public bot, so logs always stay per server.")).await;
        }
        let gs = g.to_string();
        let hub = router.hub();
        self.repo.update_setting(&gs, "log_mode", rusqlite::types::Value::Text(choice.into()))?;
        let notice = if choice == GLOBAL {
            match hub {
                None => {
                    self.repo.set_log_hub(Some(&gs))?;
                    "🟢 Global. This server is the log server: set its channels below. Logs from every server in global mode arrive here.".to_string()
                }
                Some(h) if h == g => "🟢 Global. This server is the log server.".into(),
                Some(h) => format!("🟢 Global. This server's logs go to the log server **{}**.", guild_name(ctx, &h.to_string())),
            }
        } else {
            if hub == Some(g) {
                self.repo.set_log_hub(None)?;
            }
            "🟢 Per server. Set this server's log channels below.".into()
        };
        self.audit_configuration(ctx, a, "Log Mode Set", vec![("Mode", if choice == GLOBAL { "Global" } else { "Per server" }.into())]);
        a.update(ctx, self.view(ctx, g, "logs")?.content(Some(notice))).await
    }

    /// Makes this global-mode server the log server. Moving it away from another server takes that server's owner.
    async fn make_hub(&self, ctx: &Context, a: &Actor<'_>) -> anyhow::Result<()> {
        let g = a.guild_id;
        let router = self.tickets.router().clone();
        if router.mode(g) != Some(GLOBAL) {
            return a.update(ctx, self.view(ctx, g, "logs")?).await;
        }
        if !LogRouter::is_owner(&ctx.cache, g, a.user.id) {
            return a.reply(ctx, "Only the server owner can make this the log server.").await;
        }
        if let Some(hub) = router.hub().filter(|h| *h != g && ctx.cache.guild(*h).is_some()) {
            if !LogRouter::is_owner(&ctx.cache, hub, a.user.id) {
                return a.reply(ctx, format!("Only the owner of the current log server (**{}**) can move it.", guild_name(ctx, &hub.to_string()))).await;
            }
        }
        self.repo.set_log_hub(Some(&g.to_string()))?;
        self.audit_configuration(ctx, a, "Log Server Changed", vec![]);
        a.update(ctx, self.view(ctx, g, "logs")?.content(Some("🟢 This is now the log server. Set its channels below.".into()))).await
    }

    /// Whether this server sets its own log channels: per server, or the log server in global mode.
    fn edits_own_channels(&self, g: GuildId) -> bool {
        let router = self.tickets.router();
        match router.mode(g) {
            Some(PER_SERVER) => true,
            Some(_) => router.hub() == Some(g),
            None => false,
        }
    }

    /// `a<id>` / `t<id>`: a log kind for a global-mode server, valid only in the log server.
    fn route_spec(&self, g: GuildId, spec: &str) -> Option<(&'static str, String)> {
        if self.tickets.router().hub() != Some(g) {
            return None;
        }
        let kind = match spec.chars().next()? {
            'a' => ACTIVITY,
            't' => TRANSCRIPTS,
            _ => return None,
        };
        let source = &spec[1..];
        let global = self.repo.global_guilds().ok()?;
        global.iter().any(|id| id == source).then(|| (kind, source.to_string()))
    }

    fn route_view(&self, ctx: &Context, g: GuildId, source: &str) -> anyhow::Result<View> {
        if self.route_spec(g, &format!("a{source}")).is_none() {
            return self.view(ctx, g, "logs");
        }
        let hub = self.repo.settings(&g.to_string())?;
        let line = |kind: &str, default: &Option<String>| match self.repo.log_route(source, kind).ok().flatten() {
            Some(id) => format!("<#{id}>"),
            None => format!("Default: {}", channel_line(default)),
        };
        let has = |kind: &str| self.repo.log_route(source, kind).ok().flatten().is_some();
        let text = format!(
            "**{}** is in global mode, so its logs arrive in this server. Give them their own channels here, or leave them on this server's defaults.\n\n**Activity Log**\n{}\n\n**Transcripts**\n{}",
            guild_name(ctx, source),
            line(ACTIVITY, &hub.log_channel_id),
            line(TRANSCRIPTS, &hub.transcript_channel_id),
        );
        let b = |kind: &str, action: &str, label: &str, style: ButtonStyle| button(format!("ticketui:{kind}:{action}{source}:{g}"), label, style);
        Ok(View::embed(
            CreateEmbed::new().color(COLOR).title("Server Channels").description(text),
            vec![row(vec![
                b("route", "a", "Set Activity Channel", ButtonStyle::Primary),
                b("route", "t", "Set Transcript Channel", ButtonStyle::Primary),
                b("routeclr", "a", "Use Default Activity", ButtonStyle::Secondary).disabled(!has(ACTIVITY)),
                b("routeclr", "t", "Use Default Transcripts", ButtonStyle::Secondary).disabled(!has(TRANSCRIPTS)),
                nav("logs", g, "← Back"),
            ])],
        ))
    }

    /// Posts a confirmation in `channel` (a text channel of this server): proof that Kori can write there.
    async fn confirm_channel(&self, ctx: &Context, a: &Actor<'_>, channel: &str, what: &str) -> Result<ChannelId, String> {
        let g = a.guild_id;
        let Some(channel_id) = snowflake(channel)
            .map(ChannelId::new)
            .filter(|id| ctx.cache.guild(g).is_some_and(|guild| guild.channels.get(id).is_some_and(|c| c.kind == ChannelType::Text)))
        else {
            return Err("That channel no longer exists.".into());
        };
        let confirm = serde_json::json!({
            "embeds": [CreateEmbed::new().color(COLOR).title("Kori Tickets").description(format!("Kori will post {what} in this channel. Set by <@{}>.", a.user.id))],
            "allowed_mentions": {"parse": []},
        });
        match ctx.http.send_message(channel_id, vec![], &confirm).await {
            Ok(_) => Ok(channel_id),
            Err(error) => {
                log_discord_error("ticket.logs.set", &error.into(), ctx! {"guildId" => g.to_string(), "channelId" => channel_id.to_string()});
                Err(format!("🔴 Not saved: Kori could not post in <#{channel_id}>. Give it View Channel, Send Messages, Embed Links and Attach Files there."))
            }
        }
    }

    /// Saves this server's own (or, on the log server, default) log channel once Kori has posted there.
    async fn set_log_channel(&self, ctx: &Context, a: &Actor<'_>, activity: bool, channel: &str) -> anyhow::Result<()> {
        let g = a.guild_id;
        if !self.edits_own_channels(g) {
            return a.update(ctx, self.view(ctx, g, "logs")?).await;
        }
        let (key, title) = if activity { (ACTIVITY, "Ticket Log Channel Changed") } else { (TRANSCRIPTS, "Ticket Transcript Channel Changed") };
        let scope = if self.tickets.router().hub() == Some(g) { " from every server in global mode" } else { "" };
        let what = format!("{}{scope}", kind_what(key));
        let notice = match self.confirm_channel(ctx, a, channel, &what).await {
            Ok(id) => {
                self.repo.update_setting(&g.to_string(), key, rusqlite::types::Value::Text(id.to_string()))?;
                self.audit_configuration(ctx, a, title, vec![("Channel", format!("<#{id}>"))]);
                format!("🟢 Kori will post {what} in <#{id}>.")
            }
            Err(notice) => notice,
        };
        a.update(ctx, self.view(ctx, g, "logs")?.content(Some(notice))).await
    }

    async fn set_route_channel(&self, ctx: &Context, a: &Actor<'_>, spec: &str, channel: &str) -> anyhow::Result<()> {
        let g = a.guild_id;
        let Some((kind, source)) = self.route_spec(g, spec) else { return a.update(ctx, self.view(ctx, g, "logs")?).await };
        let what = format!("{} from {}", kind_what(kind), guild_name(ctx, &source));
        let notice = match self.confirm_channel(ctx, a, channel, &what).await {
            Ok(id) => {
                self.repo.set_log_route(&source, kind, Some(&id.to_string()))?;
                self.audit_configuration(ctx, a, "Server Log Channel Changed", vec![("Server", guild_name(ctx, &source)), ("Channel", format!("<#{id}>"))]);
                format!("🟢 Kori will post {what} in <#{id}>.")
            }
            Err(notice) => notice,
        };
        a.update(ctx, self.route_view(ctx, g, &source)?.content(Some(notice))).await
    }

    fn logs_summary(&self, ctx: &Context, g: GuildId) -> anyhow::Result<String> {
        let router = self.tickets.router();
        Ok(match router.mode(g) {
            None => "🟡 Not set up: open **Logs**".into(),
            Some(PER_SERVER) => {
                let s = self.repo.settings(&g.to_string())?;
                format!("🏠 Per server • Activity: {} • Transcripts: {}", channel_line(&s.log_channel_id), channel_line(&s.transcript_channel_id))
            }
            Some(_) => match router.hub() {
                Some(h) if h == g => "🌐 Global • this is the log server".into(),
                Some(h) => format!("🌐 Global • logs go to **{}**", guild_name(ctx, &h.to_string())),
                None => "🌐 Global • 🟡 no log server yet".into(),
            },
        })
    }

    async fn send_panel(&self, ctx: &Context, a: &Actor<'_>, channel: &str) -> anyhow::Result<()> {
        let g = a.guild_id;
        let gs = g.to_string();
        let Some(channel_id) = snowflake(channel).map(ChannelId::new).filter(|id| ctx.cache.guild(g).is_some_and(|guild| guild.channels.contains_key(id))) else {
            return a.update(ctx, self.view(ctx, g, "panel")?.content(Some("That channel no longer exists.".into()))).await;
        };
        let warning = if self.tickets.category(g)?.is_none() { "\n🟡 Set a ticket category in **Setup** before members can open tickets." } else { "" };
        let notice = match self.tickets.send_panel(g, channel_id).await {
            Ok(_) => {
                self.audit_configuration(ctx, a, "Ticket Panel Sent", vec![("Channel", format!("<#{channel_id}>"))]);
                format!("🟢 Ticket panel sent to <#{channel_id}>. Later edits update it in place.{warning}")
            }
            Err(error) => {
                log_discord_error("ticket.panel.send", &error, ctx! {"guildId" => gs, "channelId" => channel_id.to_string()});
                format!("🔴 Kori could not send the panel to <#{channel_id}>. Check its View Channel, Send Messages and Embed Links permissions there.")
            }
        };
        a.update(ctx, self.view(ctx, g, "panel")?.content(Some(notice))).await
    }

    fn dashboard(&self, ctx: &Context, guild_id: GuildId) -> anyhow::Result<View> {
        let gs = guild_id.to_string();
        let s = self.repo.settings(&gs)?;
        let staff = self.repo.staff_roles(&gs)?;
        let types = self.repo.ticket_types(&gs)?;
        let open = self.repo.open_ticket_count(&gs)?;
        let category = self.tickets.category(guild_id)?;
        let staff_list = if staff.is_empty() {
            "🟡 None — only Administrators handle tickets".to_string()
        } else {
            let mut list = staff.iter().take(20).map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" ");
            if staff.len() > 20 {
                list.push_str(&format!(" +{} more", staff.len() - 20));
            }
            list
        };
        let logs = self.logs_summary(ctx, guild_id)?;
        let type_list = if types.is_empty() { "⚪ None — single **Open Ticket** button".to_string() } else { types.iter().map(type_line).collect::<Vec<_>>().join(", ") };
        let description = format!(
            "Managing **{}**\n🟢 **Kori** online • Uptime: {}\n\n**Status**\n{}\n\n**Ticket Category**\n{}\n\n**Staff Roles** ({})\n{staff_list}\n\n**Ticket Types** ({})\n{type_list}\n\n**Panel**\n**{}** — {}\n\n**Ticket Welcome**\n**{}** — pings {}\n\n**Logs**\n{logs}\n\n**Open Tickets**\n{open}",
            ctx.cache.guild(guild_id).map(|g| g.name.clone()).unwrap_or_default(),
            (self.uptime)(),
            if s.tickets_enabled != 0 { "🟢 Enabled — members can open tickets" } else { "🔴 Disabled — new tickets are paused" },
            category.map(|id| format!("<#{id}>")).unwrap_or_else(|| "🟡 Not configured".into()),
            staff.len(),
            types.len(),
            s.panel_title.as_deref().unwrap_or(DEFAULT_PANEL_TITLE),
            panel_location(guild_id, &s),
            s.welcome_title.as_deref().unwrap_or(DEFAULT_WELCOME_TITLE),
            ping_summary(&s),
        );
        Ok(View::embed(
            CreateEmbed::new().color(COLOR).title("🎫 Kori Tickets").description(description),
            vec![row(vec![
                nav("setup", guild_id, "Setup"),
                nav("staff", guild_id, "Staff Roles"),
                nav("types", guild_id, "Ticket Types"),
                nav("panel", guild_id, "Panel"),
                nav("welcome", guild_id, "Ticket Welcome"),
            ]), row(vec![nav("logs", guild_id, "Logs"), nav_styled("dashboard", guild_id, "Refresh", ButtonStyle::Secondary)])],
        ))
    }

    fn view(&self, ctx: &Context, guild_id: GuildId, section: &str) -> anyhow::Result<View> {
        let g = guild_id;
        let gs = g.to_string();
        let back = || nav("dashboard", g, "← Back");
        match section {
            "setup" => {
                let s = self.repo.settings(&gs)?;
                let category = self.tickets.category(g)?;
                let enabled = s.tickets_enabled != 0;
                let description = format!(
                    "Status: {}\n\nTicket Category: {}\n\nNew tickets are private channels in this category, named `type-username` (or `ticket-username` without types). Every ticket has a {SLOWMODE_SECONDS}-second message cooldown.\n\n**Create Category** makes a private **Tickets** category for you.",
                    if enabled { "🟢 Enabled" } else { "🔴 Disabled" },
                    category.map(|id| format!("<#{id}>")).unwrap_or_else(|| "🟡 Not configured".into()),
                );
                Ok(View::embed(
                    CreateEmbed::new().color(COLOR).title("Setup").description(description),
                    vec![row(vec![
                        button(format!("ticketui:toggle:tickets:{g}"), if enabled { "Disable" } else { "Enable" }, if enabled { ButtonStyle::Danger } else { ButtonStyle::Success }),
                        act("category", "create", g, "Create Category", ButtonStyle::Success),
                        act("category", "pick", g, "Set Category", ButtonStyle::Primary),
                        act("category", "remove", g, "Remove Category", ButtonStyle::Secondary).disabled(s.ticket_category_id.is_none()),
                        back(),
                    ])],
                ))
            }
            "staff" => {
                let roles = self.repo.staff_roles(&gs)?;
                // The embed description holds 4096 characters; long lists are summarised.
                let mut list = if roles.is_empty() { "None".to_string() } else { roles.iter().take(100).map(|id| format!("• <@&{id}>")).collect::<Vec<_>>().join("\n") };
                if roles.len() > 100 {
                    list.push_str(&format!("\n…and {} more", roles.len() - 100));
                }
                Ok(View::embed(
                    CreateEmbed::new().color(COLOR).title("Staff Roles").description(format!(
                        "Members with a staff role see new tickets, can claim them, and can `/add` or `/remove` members. Once a ticket is claimed, only its participants and the claimer keep access. Discord Administrators always count as staff and can take over with `/reclaim`.\n\n**Configured Roles** ({})\n{list}",
                        roles.len()
                    )),
                    vec![row(vec![
                        act("staff", "add", g, "Add Roles", ButtonStyle::Success),
                        act("staff", "remove", g, "Remove Roles", ButtonStyle::Danger).disabled(roles.is_empty()),
                        back(),
                    ])],
                ))
            }
            "types" => {
                let types = self.repo.ticket_types(&gs)?;
                let list = if types.is_empty() {
                    "None — the panel shows a single **Open Ticket** button.".to_string()
                } else {
                    types
                        .iter()
                        .map(|t| format!("• {}{}", type_line(t), t.description.as_deref().map(|d| format!(" — {d}")).unwrap_or_default()))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                Ok(View::embed(
                    CreateEmbed::new().color(COLOR).title("Ticket Types").description(format!(
                        "Ticket types appear in the panel's selector and prefix the channel name, such as `billing-username`. Types are optional.\n\n**Configured Types** ({}/{MAX_TICKET_TYPES})\n{list}",
                        types.len()
                    )),
                    vec![row(vec![
                        act("types", "add", g, "Add Type", ButtonStyle::Success).disabled(types.len() >= MAX_TICKET_TYPES),
                        act("types", "edit", g, "Edit Type", ButtonStyle::Primary).disabled(types.is_empty()),
                        act("types", "remove", g, "Remove Type", ButtonStyle::Danger).disabled(types.is_empty()),
                        back(),
                    ])],
                ))
            }
            "panel" => {
                let s = self.repo.settings(&gs)?;
                let or_default = |v: Option<&str>, default: &str| v.map(String::from).unwrap_or_else(|| format!("{default} *(default)*"));
                let description: String = or_default(s.panel_description.as_deref(), DEFAULT_PANEL_DESCRIPTION).chars().take(700).collect();
                let sent = s.panel_message_id.is_some();
                let text = format!(
                    "The panel is the message members use to open tickets. **Edit Embed** changes it, and a posted panel updates in place: no resending. Ticket types appear as its selector.\n\n**Posted In**\n{}\n\n**Title**\n{}\n\n**Description**\n{description}\n\n**Colour**\n{}\n\n**Image**\n{}\n\n**Footer**\n{}",
                    panel_location(g, &s),
                    or_default(s.panel_title.as_deref(), DEFAULT_PANEL_TITLE),
                    s.panel_color.map(|c| format!("#{c:06X}")).unwrap_or_else(|| format!("#{COLOR:06X} *(default)*")),
                    s.panel_image_url.as_deref().unwrap_or("None"),
                    s.panel_footer.as_deref().unwrap_or("None"),
                );
                let customised = s.panel_title.is_some() || s.panel_description.is_some() || s.panel_color.is_some() || s.panel_image_url.is_some() || s.panel_footer.is_some();
                Ok(View::embed(
                    CreateEmbed::new().color(COLOR).title("Ticket Panel").description(text),
                    vec![row(vec![
                        act("panel", "embed", g, "Edit Embed", ButtonStyle::Primary),
                        act("panel", "default", g, "Reset Embed", ButtonStyle::Secondary).disabled(!customised),
                        act("panel", "preview", g, "Preview", ButtonStyle::Secondary),
                        act("panel", "send", g, if sent { "Move Panel" } else { "Send Panel" }, ButtonStyle::Success),
                        back(),
                    ])],
                ))
            }
            "welcome" => {
                let s = self.repo.settings(&gs)?;
                let or_default = |v: Option<&str>, default: &str| v.map(String::from).unwrap_or_else(|| format!("{default} *(default)*"));
                let description: String = or_default(s.welcome_description.as_deref(), DEFAULT_WELCOME_DESCRIPTION).chars().take(1500).collect();
                let switch = |on: i64| if on != 0 { "🟢 On" } else { "⚪ Off" };
                let text = format!(
                    "The message Kori posts at the top of every new ticket, above the Claim and Close buttons. `{{user}}` becomes a mention of the ticket creator and `{{type}}` the ticket type (or General). The Opened By, Type and Status fields are always shown.\n\n**Title**\n{}\n\n**Description**\n{description}\n\n**Pings**\nTicket creator: {}\nAll staff roles: {}",
                    or_default(s.welcome_title.as_deref(), DEFAULT_WELCOME_TITLE),
                    switch(s.welcome_ping_opener),
                    switch(s.welcome_ping_staff),
                );
                let toggle = |key: &str, label: &str, on: i64| act("welcome", key, g, label, if on != 0 { ButtonStyle::Success } else { ButtonStyle::Secondary });
                let customised = s.welcome_title.is_some() || s.welcome_description.is_some();
                Ok(View::embed(
                    CreateEmbed::new().color(COLOR).title("Ticket Welcome").description(text),
                    vec![
                        row(vec![
                            act("welcome", "edit", g, "Edit Message", ButtonStyle::Primary),
                            toggle("pingopener", "Ping Creator", s.welcome_ping_opener),
                            toggle("pingstaff", "Ping Staff Roles", s.welcome_ping_staff),
                            act("welcome", "preview", g, "Preview", ButtonStyle::Secondary),
                        ]),
                        row(vec![act("welcome", "reset", g, "Reset Message", ButtonStyle::Secondary).disabled(!customised), back()]),
                    ],
                ))
            }
            "logs" => {
                let router = self.tickets.router();
                let Some(mode) = router.mode(g) else { return Ok(self.log_mode_view(g)) };
                let s = self.repo.settings(&gs)?;
                let own = row(vec![
                    act("logs", "pick", g, "Set Log Channel", ButtonStyle::Primary),
                    act("logs", "pickt", g, "Set Transcript Channel", ButtonStyle::Primary),
                    act("logs", "clear", g, "Clear Log", ButtonStyle::Secondary).disabled(s.log_channel_id.is_none()),
                    act("logs", "cleart", g, "Clear Transcripts", ButtonStyle::Secondary).disabled(s.transcript_channel_id.is_none()),
                ]);
                let change = act("logmode", "change", g, "Change Mode", ButtonStyle::Secondary);
                let private = "Transcripts contain every ticket message, so keep these channels private to staff. Members still get their own transcript by DM.";
                let hub = router.hub();
                let (text, rows) = if mode == PER_SERVER {
                    (
                        format!(
                            "**Mode:** 🏠 Per server. This server's ticket activity (opened, claimed, members added or removed, closed, setup changes) and the `.txt` transcript of every closed ticket go to channels here, and stay in this server.\n\n**Activity Log**\n{}\n\n**Transcripts**\n{}\n\n{private}",
                            channel_line(&s.log_channel_id),
                            channel_line(&s.transcript_channel_id),
                        ),
                        vec![own, row(vec![change, back()])],
                    )
                } else if hub == Some(g) {
                    let servers = self.repo.global_guilds()?;
                    let mut list = servers.iter().take(20).map(|id| format!("• {}", guild_name(ctx, id))).collect::<Vec<_>>().join("\n");
                    if servers.len() > 20 {
                        list.push_str(&format!("\n…and {} more", servers.len() - 20));
                    }
                    (
                        format!(
                            "**Mode:** 🌐 Global. **This is the log server.** Logs from every server in global mode arrive here, and each log shows which server it came from. Bot health (started, stopped, joined or left a server) arrives here too.\n\n**Activity Log** (default)\n{}\n\n**Transcripts** (default)\n{}\n\n**Servers in global mode** ({})\n{list}\n\n**Server Channels** gives a server its own channels here. {private}",
                            channel_line(&s.log_channel_id),
                            channel_line(&s.transcript_channel_id),
                            servers.len(),
                        ),
                        vec![own, row(vec![act("logs", "servers", g, "Server Channels", ButtonStyle::Primary).disabled(servers.is_empty()), change, back()])],
                    )
                } else {
                    let destination = match hub {
                        Some(h) => format!("This server's logs go to the log server **{}**, where its owner sets the channels.", guild_name(ctx, &h.to_string())),
                        None => "🟡 There is no log server yet, so nothing is logged. Make one server in global mode the log server.".into(),
                    };
                    (
                        format!("**Mode:** 🌐 Global. {destination}"),
                        vec![row(vec![act("logs", "hub", g, "Make This the Log Server", ButtonStyle::Success), change, back()])],
                    )
                };
                Ok(View::embed(CreateEmbed::new().color(COLOR).title("Logs").description(text), rows))
            }
            _ => self.dashboard(ctx, g),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_id_parts() {
        assert_eq!(parts("ticketui:nav:setup:123"), ("nav", "setup", "123"));
        assert_eq!(parts("ticketui:typeadd:x:9"), ("typeadd", "x", "9"));
        assert_eq!(parts("ticketui"), ("", "", ""));
    }

    #[test]
    fn type_labels_are_bounded() {
        assert!(valid_label("Billing"));
        assert!(!valid_label(""));
        assert!(!valid_label(&"x".repeat(51)));
        assert!(!valid_label("bad\nname"));
    }

    // ---- end to end against the fake Discord REST API ----
    use crate::test_support::{GuildSeed, Harness};
    use serde_json::json;
    use serenity::all::Interaction;

    async fn harness() -> (Harness, crate::bot::KoriBot) {
        let h = Harness::start().await;
        h.seed(
            GuildSeed::new(1, "Guild")
                .role(20, "Kori", 10, Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::MANAGE_CHANNELS | Permissions::MANAGE_ROLES)
                .role(40, "Admin", 8, Permissions::ADMINISTRATOR)
                .channel(50, ChannelType::Category, None)
                .channel(60, ChannelType::Text, None)
                .bot(&[20])
                .member(8, &[40])
                .member(9, &[]),
        );
        h.repo.update_setting("1", "log_mode", rusqlite::types::Value::Text("server".into())).unwrap();
        let bot = h.bot();
        (h, bot)
    }

    fn reply(h: &Harness) -> serde_json::Value {
        h.replies().last().cloned().unwrap_or_default()
    }

    #[tokio::test]
    async fn panel_is_sent_once_and_every_later_edit_updates_it_in_place() {
        let (h, bot) = harness().await;
        let ctx = h.ctx();
        bot.on_interaction(&ctx, &Interaction::Component(h.select(1, 8, "ticketui:panelsend:x:1", 8, &["60"]))).await;
        let posted = h.calls_to("POST", "/channels/60/messages");
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].body["components"][0]["components"][0]["custom_id"], "ticket:open");
        let s = h.repo.settings("1").unwrap();
        assert_eq!(s.panel_channel_id.as_deref(), Some("60"));
        let message = s.panel_message_id.clone().expect("panel tracked");

        let edit = |title: &str, color: &str| h.modal(1, 8, "ticketui:panelembed:x:1", &[("title", title), ("description", "Ask us anything"), ("color", color), ("image", ""), ("footer", "Kori")]);
        bot.on_interaction(&ctx, &Interaction::Modal(edit("Get Help", "#FF0000"))).await;
        let s = h.repo.settings("1").unwrap();
        assert_eq!((s.panel_title.as_deref(), s.panel_color, s.panel_footer.as_deref()), (Some("Get Help"), Some(0xFF0000), Some("Kori")));
        let patched = h.calls_to("PATCH", &format!("/channels/60/messages/{message}"));
        assert_eq!(patched.len(), 1, "edited in place, not re-sent");
        assert_eq!(patched[0].body["embeds"][0]["title"], "Get Help");
        assert!(reply(&h)["data"]["content"].as_str().unwrap().contains("updated in place"));
        assert_eq!(h.calls_to("POST", "/channels/60/messages").len(), 1);

        bot.on_interaction(&ctx, &Interaction::Modal(edit("Nope", "red"))).await;
        assert!(reply(&h)["data"]["content"].as_str().unwrap().starts_with("🔴 Not saved"));
        assert_eq!(h.repo.settings("1").unwrap().panel_title.as_deref(), Some("Get Help"), "invalid edit changes nothing");

        // Adding a type refreshes the posted panel's selector too.
        bot.on_interaction(&ctx, &Interaction::Modal(h.modal(1, 8, "ticketui:typeadd:x:1", &[("label", "Billing"), ("description", ""), ("emoji", "💳")]))).await;
        let patched = h.calls_to("PATCH", &format!("/channels/60/messages/{message}"));
        assert_eq!(patched.last().unwrap().body["components"][0]["components"][0]["custom_id"], "ticket:select");

        // Editing an existing type keeps its id and updates the panel again.
        let id = h.repo.ticket_types("1").unwrap()[0].type_id.clone();
        bot.on_interaction(&ctx, &Interaction::Modal(h.modal(1, 8, &format!("ticketui:typeupdate:{id}:1"), &[("label", "Payments"), ("description", "Refunds"), ("emoji", "")]))).await;
        let t = h.repo.ticket_type("1", &id).unwrap().unwrap();
        assert_eq!((t.label.as_str(), t.description.as_deref(), t.emoji), ("Payments", Some("Refunds"), None));
        let patched = h.calls_to("PATCH", &format!("/channels/60/messages/{message}"));
        assert_eq!(patched.last().unwrap().body["components"][0]["components"][0]["options"][0]["label"], "Payments");

        // A panel deleted by hand is noticed and forgotten instead of failing.
        h.on("PATCH", &format!("/channels/60/messages/{message}"), 404, json!({"code": 10008, "message": "Unknown Message"}));
        bot.on_interaction(&ctx, &Interaction::Component(h.button(1, 8, "ticketui:panel:default:1"))).await;
        assert!(reply(&h)["data"]["content"].as_str().unwrap().contains("was deleted"));
        assert!(h.repo.settings("1").unwrap().panel_message_id.is_none());
        assert!(h.repo.settings("1").unwrap().panel_title.is_none(), "reset to defaults");
    }

    fn message_json(channel: &str) -> serde_json::Value {
        json!({"id": "900", "channel_id": channel, "author": crate::test_support::user_json(crate::test_support::BOT_ID),
            "content": "", "timestamp": "2026-01-01T00:00:00.000Z", "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0})
    }

    async fn posted_with_title(h: &Harness, channel: &str, title: &str) -> Vec<serde_json::Value> {
        for _ in 0..100 {
            let found: Vec<_> = h.calls_to("POST", &format!("/channels/{channel}/messages")).into_iter().map(|c| c.body).filter(|b| b["embeds"][0]["title"] == title).collect();
            if !found.is_empty() {
                return found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Vec::new()
    }

    #[tokio::test]
    async fn first_ticket_command_asks_how_to_log_and_only_the_owner_answers() {
        let h = Harness::start().await;
        let admin = Permissions::ADMINISTRATOR;
        // Two servers owned by 8; 7 is an administrator but not an owner.
        h.seed(GuildSeed::new(11, "Main").owner(8).role(111, "Admin", 5, admin).channel(1160, ChannelType::Text, None).channel(1161, ChannelType::Text, None).bot(&[]).member(8, &[111]).member(7, &[111]));
        h.seed(GuildSeed::new(12, "Other").owner(8).role(121, "Admin", 5, admin).bot(&[]).member(8, &[121]));
        h.seed(GuildSeed::new(13, "Third").owner(8).role(131, "Admin", 5, admin).bot(&[]).member(8, &[131]));
        for channel in ["1160", "1161"] {
            h.on("POST", &format!("/channels/{channel}/messages"), 200, message_json(channel));
        }
        let bot = h.bot();
        let ctx = h.ctx();

        bot.on_interaction(&ctx, &Interaction::Command(h.command(11, 8, "ticket", json!([])))).await;
        let first = reply(&h);
        assert_eq!(first["data"]["embeds"][0]["title"], "Logging Setup", "the first menu asks how to log");
        assert!(first["data"]["embeds"][0]["description"].as_str().unwrap().starts_with("**How should Kori log?**"));

        bot.on_interaction(&ctx, &Interaction::Component(h.button(11, 7, "ticketui:logmode:global:11"))).await;
        assert_eq!(reply(&h)["data"]["content"], "Only the server owner can choose how Kori logs.");
        assert!(h.repo.settings("11").unwrap().log_mode.is_none());

        // The owner picks global: with no log server yet, this server becomes it.
        bot.on_interaction(&ctx, &Interaction::Component(h.button(11, 8, "ticketui:logmode:global:11"))).await;
        assert_eq!(h.repo.settings("11").unwrap().log_mode.as_deref(), Some("global"));
        assert_eq!(h.repo.log_hub().unwrap().as_deref(), Some("11"));
        bot.on_interaction(&ctx, &Interaction::Component(h.select(11, 8, "ticketui:logset:x:11", 8, &["1160"]))).await;
        assert_eq!(h.repo.settings("11").unwrap().log_channel_id.as_deref(), Some("1160"));

        // Another server joins global mode: its logs arrive in the log server and say where they came from.
        bot.on_interaction(&ctx, &Interaction::Component(h.button(12, 8, "ticketui:logmode:global:12"))).await;
        let logged = posted_with_title(&h, "1160", "Log Mode Set").await;
        assert!(logged.iter().any(|b| b.to_string().contains("Other")), "{logged:?}");
        // It cannot set channels of its own while in global mode.
        bot.on_interaction(&ctx, &Interaction::Component(h.select(12, 8, "ticketui:logset:x:12", 8, &["1160"]))).await;
        assert!(h.repo.settings("12").unwrap().log_channel_id.is_none());

        // The log server gives that server its own channel.
        bot.on_interaction(&ctx, &Interaction::Component(h.select(11, 8, "ticketui:routeset:a12:11", 8, &["1161"]))).await;
        assert_eq!(h.repo.log_route("12", "log_channel_id").unwrap().as_deref(), Some("1161"));
        bot.on_interaction(&ctx, &Interaction::Component(h.button(12, 8, "ticketui:toggle:tickets:12"))).await;
        assert!(!posted_with_title(&h, "1161", "Tickets Disabled").await.is_empty(), "server-specific channel used");
        // Routes can only be set from the log server.
        bot.on_interaction(&ctx, &Interaction::Component(h.select(12, 8, "ticketui:routeset:a11:12", 8, &["1161"]))).await;
        assert!(h.repo.log_route("11", "log_channel_id").unwrap().is_none());

        // A public bot offers no global mode.
        bot.audit.router().set_public(true);
        bot.on_interaction(&ctx, &Interaction::Command(h.command(13, 8, "ticket", json!([])))).await;
        let menu = reply(&h);
        assert!(menu["data"]["embeds"][0]["description"].as_str().unwrap().contains("public bot"));
        assert_eq!(menu["data"]["components"][0]["components"][0]["disabled"], true);
        bot.on_interaction(&ctx, &Interaction::Component(h.button(13, 8, "ticketui:logmode:global:13"))).await;
        assert!(h.repo.settings("13").unwrap().log_mode.is_none());
    }

    #[tokio::test]
    async fn log_channels_are_saved_only_after_kori_posts_there() {
        let (h, bot) = harness().await;
        let ctx = h.ctx();
        // Kori cannot post there: nothing is saved and the admin is told why.
        h.on("POST", "/channels/60/messages", 403, json!({"code": 50013, "message": "Missing Permissions"}));
        bot.on_interaction(&ctx, &Interaction::Component(h.select(1, 8, "ticketui:logset:x:1", 8, &["60"]))).await;
        assert!(h.repo.settings("1").unwrap().log_channel_id.is_none());
        assert!(reply(&h)["data"]["content"].as_str().unwrap().starts_with("🔴 Not saved"));

        h.on("POST", "/channels/60/messages", 200, json!({"id": "900", "channel_id": "60", "author": crate::test_support::user_json(crate::test_support::BOT_ID),
            "content": "", "timestamp": "2026-01-01T00:00:00.000Z", "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0}));
        bot.on_interaction(&ctx, &Interaction::Component(h.select(1, 8, "ticketui:logset:x:1", 8, &["60"]))).await;
        bot.on_interaction(&ctx, &Interaction::Component(h.select(1, 8, "ticketui:transcriptset:x:1", 8, &["60"]))).await;
        let s = h.repo.settings("1").unwrap();
        assert_eq!((s.log_channel_id.as_deref(), s.transcript_channel_id.as_deref()), (Some("60"), Some("60")));
        let text = reply(&h)["data"]["embeds"][0]["description"].as_str().unwrap().to_string();
        assert!(text.contains("**Activity Log**\n<#60>") && text.contains("**Transcripts**\n<#60>"), "{text}");

        // Setup changes now land in the server's own log channel.
        let mut waited = 0;
        while !h.calls_to("POST", "/channels/60/messages").iter().any(|c| c.body["embeds"][0]["title"] == "Ticket Transcript Channel Changed") && waited < 100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            waited += 1;
        }
        assert!(waited < 100, "configuration change logged in the server's channel");

        bot.on_interaction(&ctx, &Interaction::Component(h.button(1, 8, "ticketui:logs:clear:1"))).await;
        assert!(h.repo.settings("1").unwrap().log_channel_id.is_none());
        // Non-admins cannot redirect a server's logs.
        bot.on_interaction(&ctx, &Interaction::Component(h.select(1, 9, "ticketui:logset:x:1", 8, &["60"]))).await;
        assert!(h.repo.settings("1").unwrap().log_channel_id.is_none());
    }

    #[tokio::test]
    async fn dashboard_shows_the_whole_current_setup_and_refuses_non_admins() {
        let (h, bot) = harness().await;
        let ctx = h.ctx();
        h.repo.update_setting("1", "ticket_category_id", rusqlite::types::Value::Text("50".into())).unwrap();
        h.repo.add_staff_role("1", "40", "8").unwrap();
        h.repo.add_ticket_type(&TicketType { type_id: "t".into(), guild_id: "1".into(), label: "Support".into(), emoji: Some("🛟".into()), ..Default::default() }).unwrap();
        h.repo.set_panel_embed("1", &PanelEmbed { title: Some("Get Help".into()), ..Default::default() }).unwrap();
        h.repo.set_panel_message("1", Some(("60", "777"))).unwrap();
        h.on("GET", "/channels/60/messages/777", 200, json!({"id": "777", "channel_id": "60", "author": crate::test_support::user_json(crate::test_support::BOT_ID),
            "content": "", "timestamp": "2026-01-01T00:00:00.000Z", "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0}));
        let command = h.command(1, 8, "ticket", json!([]));
        bot.on_interaction(&ctx, &Interaction::Command(command)).await;
        let text = reply(&h)["data"]["embeds"][0]["description"].as_str().unwrap().to_string();
        for expected in ["<#50>", "<@&40>", "🛟 Support", "**Get Help**", "https://discord.com/channels/1/60/777", "🟢 Enabled"] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        // A panel deleted by hand disappears from the overview the next time /ticket opens.
        h.on("GET", "/channels/60/messages/777", 404, json!({"code": 10008, "message": "Unknown Message"}));
        bot.on_interaction(&ctx, &Interaction::Command(h.command(1, 8, "ticket", json!([])))).await;
        assert!(reply(&h)["data"]["embeds"][0]["description"].as_str().unwrap().contains("Not sent yet"));
        assert!(h.repo.settings("1").unwrap().panel_message_id.is_none());
        bot.on_interaction(&ctx, &Interaction::Command(h.command(1, 9, "ticket", json!([])))).await;
        assert_eq!(reply(&h)["data"]["content"], "You need Discord Administrator permission to use Kori Tickets.");
        bot.on_interaction(&ctx, &Interaction::Modal(h.modal(1, 9, "ticketui:panelembed:x:1", &[("title", "Hacked")]))).await;
        assert_eq!(h.repo.settings("1").unwrap().panel_title.as_deref(), Some("Get Help"), "forged modal from a non-admin is refused");
    }
}
