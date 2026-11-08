use super::now;
use anyhow::{anyhow, bail, Result};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{params, Connection, Params, Row, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, MutexGuard};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuildSettings {
    pub guild_id: String,
    pub tickets_enabled: i64,
    pub ticket_category_id: Option<String>,
    pub panel_title: Option<String>,
    pub panel_description: Option<String>,
    pub panel_color: Option<i64>,
    pub panel_image_url: Option<String>,
    pub panel_footer: Option<String>,
    /// Where the panel was last sent; edits update that message in place.
    pub panel_channel_id: Option<String>,
    pub panel_message_id: Option<String>,
    /// The message at the top of every new ticket; `None` = Kori's default text.
    pub welcome_title: Option<String>,
    pub welcome_description: Option<String>,
    pub welcome_ping_opener: i64,
    pub welcome_ping_staff: i64,
    /// This server's own log and transcript channels; `None` = not set.
    pub log_channel_id: Option<String>,
    pub transcript_channel_id: Option<String>,
}
/// The editable panel embed. `None` fields fall back to Kori's defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PanelEmbed {
    pub title: Option<String>,
    pub description: Option<String>,
    pub color: Option<i64>,
    pub image_url: Option<String>,
    pub footer: Option<String>,
}
/// One selectable ticket type (Support, Billing, ...) shown in the panel selector.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TicketType {
    pub type_id: String,
    pub guild_id: String,
    pub label: String,
    pub description: Option<String>,
    pub emoji: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    pub channel_id: String,
    pub guild_id: String,
    pub opener_id: String,
    pub type_label: Option<String>,
    pub channel_name: Option<String>,
    pub claimed_by_id: Option<String>,
    pub status: String,
    pub created_at: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandSyncState {
    pub application_id: String,
    pub schema_fingerprint: String,
    pub guild_cleanup_complete: i64,
    pub updated_at: String,
}



// ---- row helpers ----

fn row_json(row: &Row) -> rusqlite::Result<serde_json::Value> {
    let stmt = row.as_ref();
    let mut map = serde_json::Map::new();
    for i in 0..stmt.column_count() {
        let v = match row.get_ref(i)? {
            ValueRef::Null => serde_json::Value::Null,
            ValueRef::Integer(n) => n.into(),
            ValueRef::Real(f) => f.into(),
            ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
            ValueRef::Blob(b) => b.iter().map(|&x| serde_json::Value::from(x)).collect(),
        };
        map.insert(stmt.column_name(i)?.to_string(), v);
    }
    Ok(serde_json::Value::Object(map))
}
/// Generic row mapping by column name (blob-free structs only; blob rows use explicit mappers).
fn all<T: DeserializeOwned>(c: &Connection, sql: &str, p: impl Params) -> Result<Vec<T>> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map(p, row_json)?.collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(|v| Ok(serde_json::from_value(v)?)).collect()
}
fn get<T: DeserializeOwned>(c: &Connection, sql: &str, p: impl Params) -> Result<Option<T>> {
    Ok(all(c, sql, p)?.into_iter().next())
}
fn strings(c: &Connection, sql: &str, p: impl Params) -> Result<Vec<String>> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map(p, |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}
fn is_unique(e: &rusqlite::Error) -> bool {
    e.to_string().contains("UNIQUE")
}
/// Column names are interpolated into SQL, so only whitelisted keys are accepted
/// (TS enforced this through its literal union types).
fn checked<'a>(key: &'a str, allowed: &[&str]) -> Result<&'a str> {
    if allowed.contains(&key) {
        Ok(key)
    } else {
        bail!("Invalid field {key}")
    }
}

fn ensure_guild(c: &Connection, guild_id: &str) -> Result<()> {
    let stamp = now();
    c.execute("INSERT OR IGNORE INTO guild_settings(guild_id,created_at,updated_at) VALUES (?,?,?)", params![guild_id, stamp, stamp])?;
    Ok(())
}

pub struct KoriRepository {
    db: Mutex<Connection>,
}

impl KoriRepository {
    pub fn new(conn: Connection) -> Self {
        Self { db: Mutex::new(conn) }
    }
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn ensure_guild(&self, guild_id: &str) -> Result<()> {
        ensure_guild(&self.conn(), guild_id)
    }
    pub fn settings(&self, guild_id: &str) -> Result<GuildSettings> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        get(&c, "SELECT * FROM guild_settings WHERE guild_id = ?", params![guild_id])?.ok_or_else(|| anyhow!("guild settings missing"))
    }
    /// key: "tickets_enabled" | "ticket_category_id" | "welcome_ping_opener" | "welcome_ping_staff" | "log_channel_id" | "transcript_channel_id"
    pub fn update_setting(&self, guild_id: &str, key: &str, value: Value) -> Result<()> {
        let key = checked(
            key,
            &["tickets_enabled", "ticket_category_id", "welcome_ping_opener", "welcome_ping_staff", "log_channel_id", "transcript_channel_id"],
        )?;
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        c.execute(&format!("UPDATE guild_settings SET {key} = ?, updated_at = ? WHERE guild_id = ?"), params![value, now(), guild_id])?;
        Ok(())
    }
    pub fn set_panel_embed(&self, guild_id: &str, e: &PanelEmbed) -> Result<()> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        c.execute(
            "UPDATE guild_settings SET panel_title=?,panel_description=?,panel_color=?,panel_image_url=?,panel_footer=?,updated_at=? WHERE guild_id=?",
            params![e.title, e.description, e.color, e.image_url, e.footer, now(), guild_id],
        )?;
        Ok(())
    }
    pub fn set_welcome_text(&self, guild_id: &str, title: Option<&str>, description: Option<&str>) -> Result<()> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        c.execute(
            "UPDATE guild_settings SET welcome_title=?,welcome_description=?,updated_at=? WHERE guild_id=?",
            params![title, description, now(), guild_id],
        )?;
        Ok(())
    }
    /// `(channel, message)` of the posted panel, or None when it is gone.
    pub fn set_panel_message(&self, guild_id: &str, at: Option<(&str, &str)>) -> Result<()> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        c.execute(
            "UPDATE guild_settings SET panel_channel_id=?,panel_message_id=?,updated_at=? WHERE guild_id=?",
            params![at.map(|a| a.0), at.map(|a| a.1), now(), guild_id],
        )?;
        Ok(())
    }

    pub fn purge_guild(&self, guild_id: &str) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for table in [
            "ticket_staff_roles",
            "ticket_types",
            "guild_settings",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE guild_id=?"), params![guild_id])?;
        }
        tx.execute("DELETE FROM ticket_members WHERE channel_id IN (SELECT channel_id FROM tickets WHERE guild_id=?1)", params![guild_id])?;
        tx.execute("DELETE FROM tickets WHERE guild_id=?", params![guild_id])?;
        tx.commit()?;
        Ok(())
    }
    // ---- tickets: staff roles, types and ticket channels, all scoped to the owning guild ----

    pub fn staff_roles(&self, guild_id: &str) -> Result<Vec<String>> {
        strings(&self.conn(), "SELECT role_id FROM ticket_staff_roles WHERE guild_id=? ORDER BY created_at, role_id", params![guild_id])
    }
    /// False when the role was already configured.
    pub fn add_staff_role(&self, guild_id: &str, role_id: &str, actor: &str) -> Result<bool> {
        let c = self.conn();
        ensure_guild(&c, guild_id)?;
        Ok(c.execute(
            "INSERT OR IGNORE INTO ticket_staff_roles(guild_id,role_id,added_by_id,created_at) VALUES (?,?,?,?)",
            params![guild_id, role_id, actor, now()],
        )? == 1)
    }
    pub fn remove_staff_role(&self, guild_id: &str, role_id: &str) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM ticket_staff_roles WHERE guild_id=? AND role_id=?", params![guild_id, role_id])? == 1)
    }
    pub fn ticket_types(&self, guild_id: &str) -> Result<Vec<TicketType>> {
        all(&self.conn(), "SELECT type_id,guild_id,label,description,emoji FROM ticket_types WHERE guild_id=? ORDER BY created_at, label", params![guild_id])
    }
    pub fn ticket_type(&self, guild_id: &str, type_id: &str) -> Result<Option<TicketType>> {
        get(&self.conn(), "SELECT type_id,guild_id,label,description,emoji FROM ticket_types WHERE guild_id=? AND type_id=?", params![guild_id, type_id])
    }
    /// False when a type with that label (case-insensitive) already exists in the guild.
    pub fn add_ticket_type(&self, t: &TicketType) -> Result<bool> {
        let c = self.conn();
        ensure_guild(&c, &t.guild_id)?;
        match c.execute(
            "INSERT INTO ticket_types(type_id,guild_id,label,description,emoji,created_at) VALUES (?,?,?,?,?,?)",
            params![t.type_id, t.guild_id, t.label, t.description, t.emoji, now()],
        ) {
            Ok(_) => Ok(true),
            Err(e) if is_unique(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    /// False when the type is gone or another type already uses that label.
    pub fn update_ticket_type(&self, t: &TicketType) -> Result<bool> {
        match self.conn().execute(
            "UPDATE ticket_types SET label=?,description=?,emoji=? WHERE guild_id=? AND type_id=?",
            params![t.label, t.description, t.emoji, t.guild_id, t.type_id],
        ) {
            Ok(n) => Ok(n == 1),
            Err(e) if is_unique(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    pub fn remove_ticket_type(&self, guild_id: &str, type_id: &str) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM ticket_types WHERE guild_id=? AND type_id=?", params![guild_id, type_id])? == 1)
    }
    /// False when the opener already has an open ticket in this guild (enforced by a partial unique index).
    pub fn open_ticket(&self, t: &Ticket) -> Result<bool> {
        match self.conn().execute(
            "INSERT INTO tickets(channel_id,guild_id,opener_id,type_label,channel_name,status,created_at) VALUES (?,?,?,?,?,'open',?)",
            params![t.channel_id, t.guild_id, t.opener_id, t.type_label, t.channel_name, t.created_at],
        ) {
            Ok(_) => Ok(true),
            Err(e) if is_unique(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    /// An open ticket in this guild, by channel.
    pub fn ticket(&self, guild_id: &str, channel_id: &str) -> Result<Option<Ticket>> {
        get(&self.conn(), "SELECT channel_id,guild_id,opener_id,type_label,channel_name,claimed_by_id,status,created_at FROM tickets WHERE guild_id=? AND channel_id=? AND status='open'", params![guild_id, channel_id])
    }
    pub fn open_ticket_for(&self, guild_id: &str, opener_id: &str) -> Result<Option<Ticket>> {
        get(&self.conn(), "SELECT channel_id,guild_id,opener_id,type_label,channel_name,claimed_by_id,status,created_at FROM tickets WHERE guild_id=? AND opener_id=? AND status='open'", params![guild_id, opener_id])
    }
    pub fn open_tickets(&self, guild_id: &str) -> Result<Vec<Ticket>> {
        all(&self.conn(), "SELECT channel_id,guild_id,opener_id,type_label,channel_name,claimed_by_id,status,created_at FROM tickets WHERE guild_id=? AND status='open'", params![guild_id])
    }
    pub fn open_ticket_count(&self, guild_id: &str) -> Result<i64> {
        Ok(self.conn().query_row("SELECT COUNT(*) FROM tickets WHERE guild_id=? AND status='open'", params![guild_id], |r| r.get(0))?)
    }
    /// Compare-and-set claim: succeeds only while the current claimer is still `expected` (None = unclaimed),
    /// so two staff clicking Claim at once cannot both win. `claimer: None` undoes a claim.
    pub fn claim_ticket(&self, guild_id: &str, channel_id: &str, claimer: Option<&str>, expected: Option<&str>) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE tickets SET claimed_by_id=?, claimed_at=? WHERE guild_id=? AND channel_id=? AND status='open' AND claimed_by_id IS ?",
            params![claimer, now(), guild_id, channel_id, expected],
        )? == 1)
    }
    /// False when the ticket was already closed (or never existed).
    pub fn close_ticket(&self, guild_id: &str, channel_id: &str, actor: Option<&str>) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE tickets SET status='closed', closed_at=?, closed_by_id=? WHERE guild_id=? AND channel_id=? AND status='open'",
            params![now(), actor, guild_id, channel_id],
        )? == 1)
    }
    pub fn set_welcome_message(&self, guild_id: &str, channel_id: &str, message_id: &str) -> Result<()> {
        self.conn().execute("UPDATE tickets SET welcome_message_id=? WHERE guild_id=? AND channel_id=?", params![message_id, guild_id, channel_id])?;
        Ok(())
    }
    pub fn welcome_message(&self, channel_id: &str) -> Result<Option<String>> {
        Ok(self.conn().query_row("SELECT welcome_message_id FROM tickets WHERE channel_id=?", params![channel_id], |r| r.get(0)).ok().flatten())
    }
    pub fn ticket_members(&self, channel_id: &str) -> Result<Vec<String>> {
        strings(&self.conn(), "SELECT user_id FROM ticket_members WHERE channel_id=? ORDER BY created_at, user_id", params![channel_id])
    }
    pub fn add_ticket_member(&self, channel_id: &str, user_id: &str, actor: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "INSERT OR IGNORE INTO ticket_members(channel_id,user_id,added_by_id,created_at) VALUES (?,?,?,?)",
            params![channel_id, user_id, actor, now()],
        )? == 1)
    }
    pub fn remove_ticket_member(&self, channel_id: &str, user_id: &str) -> Result<bool> {
        Ok(self.conn().execute("DELETE FROM ticket_members WHERE channel_id=? AND user_id=?", params![channel_id, user_id])? == 1)
    }
    pub fn command_sync_state(&self, application_id: &str) -> Result<Option<CommandSyncState>> {
        get(&self.conn(), "SELECT * FROM command_sync_state WHERE application_id=?", params![application_id])
    }
    pub fn save_command_sync_state(&self, application_id: &str, fingerprint: &str, cleanup_complete: bool) -> Result<()> {
        self.conn().execute(
            "INSERT INTO command_sync_state(application_id,schema_fingerprint,guild_cleanup_complete,updated_at) VALUES (?,?,?,?) ON CONFLICT(application_id) DO UPDATE SET schema_fingerprint=excluded.schema_fingerprint,guild_cleanup_complete=excluded.guild_cleanup_complete,updated_at=excluded.updated_at",
            params![application_id, fingerprint, cleanup_complete as i64, now()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> KoriRepository {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::db::migrate(&mut db).unwrap();
        KoriRepository::new(db)
    }

    // ---- tickets ----
    fn ticket(channel: &str, guild: &str, opener: &str) -> Ticket {
        Ticket { channel_id: channel.into(), guild_id: guild.into(), opener_id: opener.into(), created_at: now(), ..Default::default() }
    }
    #[test]
    fn one_open_ticket_per_member_per_guild() {
        let r = repo();
        assert!(r.open_ticket(&ticket("c1", "g", "u")).unwrap());
        assert!(!r.open_ticket(&ticket("c2", "g", "u")).unwrap(), "second open ticket refused");
        assert!(r.open_ticket(&ticket("c3", "other", "u")).unwrap(), "other guild is independent");
        assert!(r.close_ticket("g", "c1", Some("u")).unwrap());
        assert!(!r.close_ticket("g", "c1", Some("u")).unwrap(), "already closed");
        assert!(r.open_ticket(&ticket("c2", "g", "u")).unwrap(), "can open again after closing");
    }
    #[test]
    fn claims_are_compare_and_set_and_guild_scoped() {
        let r = repo();
        r.open_ticket(&ticket("c", "g", "u")).unwrap();
        assert!(!r.claim_ticket("other", "c", Some("s1"), None).unwrap(), "other guild cannot claim");
        assert!(r.claim_ticket("g", "c", Some("s1"), None).unwrap());
        assert!(!r.claim_ticket("g", "c", Some("s2"), None).unwrap(), "already claimed");
        assert!(r.claim_ticket("g", "c", Some("admin"), Some("s1")).unwrap(), "reclaim from the known claimer");
        assert_eq!(r.ticket("g", "c").unwrap().unwrap().claimed_by_id.as_deref(), Some("admin"));
        assert!(r.ticket("other", "c").unwrap().is_none());
    }
    #[test]
    fn ticket_types_and_staff_roles_are_per_guild_and_purged() {
        let r = repo();
        let t = |id: &str, g: &str, label: &str| TicketType { type_id: id.into(), guild_id: g.into(), label: label.into(), ..Default::default() };
        assert!(r.add_ticket_type(&t("t1", "g", "Billing")).unwrap());
        assert!(!r.add_ticket_type(&t("t2", "g", "billing")).unwrap(), "labels unique case-insensitively");
        assert!(r.add_ticket_type(&t("t3", "h", "Billing")).unwrap());
        assert!(r.ticket_type("h", "t1").unwrap().is_none(), "cannot read another guild's type");
        assert!(r.add_staff_role("g", "r", "a").unwrap());
        assert!(!r.add_staff_role("g", "r", "a").unwrap());
        r.open_ticket(&ticket("c", "g", "u")).unwrap();
        r.add_ticket_member("c", "m", "s").unwrap();
        r.purge_guild("g").unwrap();
        assert!(r.ticket_types("g").unwrap().is_empty());
        assert!(r.staff_roles("g").unwrap().is_empty());
        assert!(r.ticket("g", "c").unwrap().is_none());
        assert!(r.ticket_members("c").unwrap().is_empty());
        assert_eq!(r.ticket_types("h").unwrap().len(), 1);
    }

    #[test]
    fn panel_embed_and_message_round_trip_and_types_can_be_edited() {
        let r = repo();
        let e = PanelEmbed { title: Some("Help".into()), color: Some(0x123456), ..Default::default() };
        r.set_panel_embed("g", &e).unwrap();
        r.set_panel_message("g", Some(("c", "m"))).unwrap();
        let s = r.settings("g").unwrap();
        assert_eq!((s.panel_title.as_deref(), s.panel_color, s.panel_description), (Some("Help"), Some(0x123456), None));
        assert_eq!((s.panel_channel_id.as_deref(), s.panel_message_id.as_deref()), (Some("c"), Some("m")));
        assert_eq!((s.welcome_ping_opener, s.welcome_ping_staff, s.welcome_title), (1, 1, None), "welcome defaults");
        r.set_welcome_text("g", Some("Hi {user}"), None).unwrap();
        r.update_setting("g", "welcome_ping_staff", Value::Integer(0)).unwrap();
        let s = r.settings("g").unwrap();
        assert_eq!((s.welcome_title.as_deref(), s.welcome_ping_staff), (Some("Hi {user}"), 0));
        r.set_panel_message("g", None).unwrap();
        assert!(r.settings("g").unwrap().panel_message_id.is_none());
        let t = |id: &str, label: &str| TicketType { type_id: id.into(), guild_id: "g".into(), label: label.into(), ..Default::default() };
        r.add_ticket_type(&t("a", "Support")).unwrap();
        r.add_ticket_type(&t("b", "Billing")).unwrap();
        assert!(r.update_ticket_type(&TicketType { emoji: Some("🛟".into()), ..t("a", "Help") }).unwrap());
        assert!(!r.update_ticket_type(&t("a", "billing")).unwrap(), "label taken");
        assert!(!r.update_ticket_type(&TicketType { guild_id: "h".into(), ..t("a", "X") }).unwrap(), "other guild");
        assert_eq!(r.ticket_type("g", "a").unwrap().unwrap().label, "Help");
    }
}
