-- Per-server log destinations chosen in /ticket. NULL = not set.
ALTER TABLE guild_settings ADD COLUMN log_channel_id TEXT;
ALTER TABLE guild_settings ADD COLUMN transcript_channel_id TEXT;
