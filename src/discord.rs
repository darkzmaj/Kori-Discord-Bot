use serenity::cache::Cache;
use serenity::http::Http;
use std::sync::Arc;

/// Shared Discord handle for services and background tasks (the Rust analogue of discord.js `Client`).
#[derive(Clone)]
pub struct Discord {
    pub http: Arc<Http>,
    pub cache: Arc<Cache>,
}

/// Kori's default embed colour (ice blue, "kori" is Japanese for ice). Status colours (success/warning/danger) stay semantic.
pub const KORI_BLUE: u32 = 0x5dade2;
