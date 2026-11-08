use crate::db::repositories::KoriRepository;
use crate::discord::Discord;
use serenity::model::guild::Member;
use serenity::model::Permissions;


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionLevel {
    Global,
    Administrator,
}

/// Kori Administrator means a current effective Discord Administrator permission, never guild ownership.
/// Ticket staff (role-based) is checked by the ticket service, not here.
pub fn is_discord_administrator(permissions: Permissions) -> bool {
    permissions.administrator()
}

/// Effective guild permissions of a member: the interaction-provided value when present,
/// otherwise computed from the cached guild's roles (empty if the guild is not cached).
/// Like discord.js `member.permissions`, the guild owner resolves to all permissions.
pub fn member_permissions(discord: &Discord, member: &Member) -> Permissions {
    if let Some(permissions) = member.permissions {
        return permissions;
    }
    discord.cache.guild(member.guild_id).map(|guild| guild.member_permissions(member)).unwrap_or_else(Permissions::empty)
}


/// Core check for interaction handlers (`interaction.member.permissions`).
pub async fn has_permission_with(repo: &KoriRepository, guild_id: &str, user_id: &str, permissions: Permissions, level: PermissionLevel) -> anyhow::Result<bool> {
    let _ = (repo, guild_id, user_id);
    Ok(level == PermissionLevel::Global || is_discord_administrator(permissions))
}

pub async fn has_permission(repo: &KoriRepository, discord: &Discord, member: &Member, level: PermissionLevel) -> anyhow::Result<bool> {
    let permissions = member_permissions(discord, member);
    has_permission_with(repo, &member.guild_id.to_string(), &member.user.id.to_string(), permissions, level).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> KoriRepository {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        KoriRepository::new(conn)
    }

    #[tokio::test]
    async fn defines_administrator_by_effective_permission_not_ownership() {
        let r = repo();
        assert!(has_permission_with(&r, "a", "management", Permissions::ADMINISTRATOR, PermissionLevel::Administrator).await.unwrap());
        assert!(!has_permission_with(&r, "a", "owner", Permissions::MANAGE_GUILD, PermissionLevel::Administrator).await.unwrap());
        assert!(has_permission_with(&r, "a", "anyone", Permissions::empty(), PermissionLevel::Global).await.unwrap());
    }
}
