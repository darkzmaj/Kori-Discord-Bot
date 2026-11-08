-- The panel embed lives in the ticket setup itself (no separate embed manager), and the posted
-- panel message is tracked so edits update it in place.
ALTER TABLE guild_settings ADD COLUMN panel_title TEXT;
ALTER TABLE guild_settings ADD COLUMN panel_description TEXT;
ALTER TABLE guild_settings ADD COLUMN panel_color INTEGER;
ALTER TABLE guild_settings ADD COLUMN panel_image_url TEXT;
ALTER TABLE guild_settings ADD COLUMN panel_footer TEXT;
ALTER TABLE guild_settings ADD COLUMN panel_channel_id TEXT;
ALTER TABLE guild_settings ADD COLUMN panel_message_id TEXT;
ALTER TABLE guild_settings DROP COLUMN panel_template_id;

DROP TABLE scheduled_embed_sends;
DROP TABLE sent_embed_messages;
DROP TABLE embed_attachments;
DROP TABLE embed_templates;
