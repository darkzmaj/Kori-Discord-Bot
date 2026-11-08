use std::env;


fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok().map(|value| value.trim().to_string())
}

/// A Discord channel Kori logs to through a webhook; the guild and channel IDs are checked against the webhook.
#[derive(Clone, Default)]
pub struct LogDestination {
    pub webhook_url: Option<String>,
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
}

#[derive(Clone)]
pub struct Config {
    pub token: String,
    pub database_path: String,
    pub application_id: Option<String>,
    /// Main log: member actions, staff actions and bot health.
    pub audit_logging: LogDestination,
    /// Transcript log: a .txt transcript of every closed ticket.
    pub transcript_logging: LogDestination,
}

impl Config {
    pub fn from_env() -> Self {
        let _ = dotenvy::dotenv();
        Self {
            token: env::var("DISCORD_TOKEN").unwrap_or_default(),
            database_path: env::var("DATABASE_PATH").unwrap_or_else(|_| "./data/kori.sqlite".into()),
            application_id: ["APPLICATION_ID", "CLIENT_ID", "DISCORD_CLIENT_ID"]
                .iter()
                .find_map(|name| env::var(name).ok()),
            audit_logging: LogDestination {
                webhook_url: optional_env("LOG_WEBHOOK_URL"),
                guild_id: optional_env("LOG_GUILD_ID"),
                channel_id: optional_env("LOG_CHANNEL_ID"),
            },
            transcript_logging: LogDestination {
                webhook_url: optional_env("TRANSCRIPT_WEBHOOK_URL"),
                guild_id: optional_env("TRANSCRIPT_GUILD_ID"),
                channel_id: optional_env("TRANSCRIPT_CHANNEL_ID"),
            },
        }
    }

    pub fn require(&self) -> anyhow::Result<()> {
        if self.token.trim().is_empty() {
            anyhow::bail!("DISCORD_TOKEN is required. See .env.example.");
        }
        if self.database_path.trim().is_empty() {
            anyhow::bail!("DATABASE_PATH cannot be empty. See .env.example.");
        }
        // Each log is optional, but a half-filled one is a mistake worth stopping for.
        for (name, prefix, destination) in [("Main log", "LOG", &self.audit_logging), ("Transcript log", "TRANSCRIPT", &self.transcript_logging)] {
            match crate::services::audit_log_service::validate_audit_log_config(destination) {
                Ok(_) | Err("not configured") => {}
                Err(reason) => anyhow::bail!(
                    "{name} has an {reason}: set all of {prefix}_WEBHOOK_URL, {prefix}_GUILD_ID and {prefix}_CHANNEL_ID, or none. See .env.example."
                ),
            }
        }
        Ok(())
    }
}


/// Secrets (bot token, webhook URLs) print only as whether they are set.
fn secret(value: Option<&str>) -> &'static str {
    if value.is_some_and(|v| !v.is_empty()) { "[redacted]" } else { "[unset]" }
}

impl std::fmt::Debug for LogDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogDestination")
            .field("webhook_url", &secret(self.webhook_url.as_deref()))
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .finish()
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("token", &secret(Some(&self.token)))
            .field("database_path", &self.database_path)
            .field("application_id", &self.application_id)
            .field("audit_logging", &self.audit_logging)
            .field("transcript_logging", &self.transcript_logging)
            .finish()
    }
}
