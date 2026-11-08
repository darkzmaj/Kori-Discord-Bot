-- The message posted at the top of every new ticket: editable title and description (NULL = Kori's default)
-- and who it pings.
ALTER TABLE guild_settings ADD COLUMN welcome_title TEXT;
ALTER TABLE guild_settings ADD COLUMN welcome_description TEXT;
ALTER TABLE guild_settings ADD COLUMN welcome_ping_opener INTEGER NOT NULL DEFAULT 1 CHECK (welcome_ping_opener IN (0,1));
ALTER TABLE guild_settings ADD COLUMN welcome_ping_staff INTEGER NOT NULL DEFAULT 1 CHECK (welcome_ping_staff IN (0,1));
