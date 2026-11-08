-- Kori schema. Every guild-specific row is keyed by guild_id.
CREATE TABLE guild_settings (
  guild_id TEXT PRIMARY KEY,
  tickets_enabled INTEGER NOT NULL DEFAULT 1 CHECK (tickets_enabled IN (0,1)),
  ticket_category_id TEXT,
  panel_template_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE ticket_staff_roles (
  guild_id TEXT NOT NULL REFERENCES guild_settings(guild_id) ON DELETE CASCADE,
  role_id TEXT NOT NULL,
  added_by_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (guild_id, role_id)
);

CREATE TABLE ticket_types (
  type_id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL REFERENCES guild_settings(guild_id) ON DELETE CASCADE,
  label TEXT NOT NULL COLLATE NOCASE,
  description TEXT,
  emoji TEXT,
  created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_ticket_types_guild_label ON ticket_types(guild_id, label COLLATE NOCASE);

CREATE TABLE tickets (
  channel_id TEXT PRIMARY KEY,
  guild_id TEXT NOT NULL,
  opener_id TEXT NOT NULL,
  type_label TEXT,
  channel_name TEXT,
  claimed_by_id TEXT,
  welcome_message_id TEXT,
  status TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open','closed')),
  created_at TEXT NOT NULL,
  claimed_at TEXT,
  closed_at TEXT,
  closed_by_id TEXT
);
-- One open ticket per member per guild.
CREATE UNIQUE INDEX idx_tickets_one_open ON tickets(guild_id, opener_id) WHERE status = 'open';
CREATE INDEX idx_tickets_guild_status ON tickets(guild_id, status);

CREATE TABLE ticket_members (
  channel_id TEXT NOT NULL REFERENCES tickets(channel_id) ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  added_by_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (channel_id, user_id)
);

CREATE TABLE command_sync_state (
  application_id TEXT PRIMARY KEY,
  schema_fingerprint TEXT NOT NULL,
  guild_cleanup_complete INTEGER NOT NULL DEFAULT 0 CHECK(guild_cleanup_complete IN (0,1)),
  updated_at TEXT NOT NULL
);

CREATE TABLE embed_templates (
  template_id TEXT PRIMARY KEY,
  guild_id TEXT,
  name TEXT NOT NULL COLLATE NOCASE,
  payload_json TEXT NOT NULL,
  created_by_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_embed_templates_guild_name ON embed_templates(guild_id, name COLLATE NOCASE);

CREATE TABLE embed_attachments (
  id TEXT PRIMARY KEY,
  template_id TEXT NOT NULL REFERENCES embed_templates(template_id) ON DELETE CASCADE,
  filename TEXT NOT NULL,
  content_type TEXT NOT NULL,
  size_bytes INTEGER NOT NULL CHECK(size_bytes > 0 AND size_bytes <= 8388608),
  sha256 TEXT NOT NULL,
  data BLOB NOT NULL,
  created_by_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(template_id, filename)
);
CREATE INDEX idx_embed_attachments_template ON embed_attachments(template_id);

CREATE TABLE sent_embed_messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  embed_id TEXT NOT NULL REFERENCES embed_templates(template_id) ON DELETE CASCADE,
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  message_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(embed_id, guild_id, channel_id, message_id)
);
CREATE INDEX idx_sent_embed_messages_guild_embed ON sent_embed_messages(guild_id, embed_id);

CREATE TABLE scheduled_embed_sends (
  id TEXT PRIMARY KEY,
  template_id TEXT NOT NULL REFERENCES embed_templates(template_id) ON DELETE RESTRICT,
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  scheduled_for_utc TEXT NOT NULL,
  timezone TEXT NOT NULL,
  created_by_user_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_by_user_id TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('scheduled','processing','sent','cancelled','failed','missed')),
  sent_at TEXT,
  sent_message_id TEXT,
  failure_code TEXT,
  failure_reason TEXT,
  attempt_count INTEGER NOT NULL DEFAULT 0,
  last_attempt_at TEXT,
  processing_started_at TEXT,
  template_name_snapshot TEXT NOT NULL,
  mention_json TEXT NOT NULL DEFAULT '{"roles":[],"everyone":false,"here":false}',
  mention_outcome TEXT
);
CREATE INDEX idx_scheduled_embed_sends_due ON scheduled_embed_sends(status, scheduled_for_utc);
CREATE INDEX idx_scheduled_embed_sends_guild_status ON scheduled_embed_sends(guild_id, status, scheduled_for_utc);
