use std::env;

/// Everything else, log channels included, is set per server in `/ticket`.
#[derive(Clone)]
pub struct Config {
    pub token: String,
    pub database_path: String,
    pub application_id: Option<String>,
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
        }
    }

    pub fn require(&self) -> anyhow::Result<()> {
        if self.token.trim().is_empty() {
            anyhow::bail!("DISCORD_TOKEN is required. See .env.example.");
        }
        if self.database_path.trim().is_empty() {
            anyhow::bail!("DATABASE_PATH cannot be empty. See .env.example.");
        }
        Ok(())
    }
}

/// The bot token prints only as whether it is set.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("token", &if self.token.is_empty() { "[unset]" } else { "[redacted]" })
            .field("database_path", &self.database_path)
            .field("application_id", &self.application_id)
            .finish()
    }
}
