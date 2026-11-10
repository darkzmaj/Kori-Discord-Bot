//! Load test, run explicitly: `STRESS_USERS=1000 cargo test --release stress -- --ignored --nocapture`.
//! Every user opens a ticket at the same moment, then every user closes theirs at the same moment. The bot
//! runs its real code paths against a real SQLite file (WAL); only Discord is faked, in the same process,
//! so the reported peak memory includes the fake and is an upper bound for the bot alone.
use crate::db::repositories::KoriRepository;
use crate::test_support::*;
use serenity::all::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

const STAFF: u64 = 30;
const FIRST_USER: u64 = 100_000;

fn peak_rss_mib() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|l| l.starts_with("VmHWM"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or("n/a (not Linux)".into(), |kb| format!("{} MiB", kb / 1024))
}

fn report(phase: &str, mut latencies: Vec<Duration>, wall: Duration) {
    latencies.sort();
    let at = |p: f64| latencies[((latencies.len() - 1) as f64 * p) as usize].as_millis();
    println!(
        "STRESS {phase}: {} interactions in {:.2}s = {:.0}/s | latency p50 {}ms p95 {}ms p99 {}ms max {}ms | peak memory {}",
        latencies.len(),
        wall.as_secs_f64(),
        latencies.len() as f64 / wall.as_secs_f64(),
        at(0.50),
        at(0.95),
        at(0.99),
        at(1.0),
        peak_rss_mib()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "load test; run explicitly"]
async fn stress_open_and_close_tickets() {
    let users: u64 = std::env::var("STRESS_USERS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let mut h = Harness::start().await;
    let dir = std::env::temp_dir().join(format!("kori-stress-{}", std::process::id()));
    let db = crate::db::create_database(dir.join("kori.sqlite").to_str().unwrap()).unwrap();
    h.repo = Arc::new(KoriRepository::new(db));
    let bot_perms = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::MANAGE_CHANNELS | Permissions::MANAGE_ROLES;
    h.seed(
        GuildSeed::new(1, "Guild")
            .role(20, "Kori", 10, bot_perms)
            .role(STAFF, "Staff", 5, Permissions::empty())
            .channel(50, ChannelType::Category, None)
            .bot(&[20]),
    );
    h.repo.update_setting("1", "ticket_category_id", rusqlite::types::Value::Text("50".into())).unwrap();
    h.repo.add_staff_role("1", &STAFF.to_string(), "8").unwrap();
    h.repo.update_setting("1", "log_mode", rusqlite::types::Value::Text("server".into())).unwrap();
    // Closing reads the channel history for the transcript, then deletes the channel.
    h.on("GET", "/channels/*/messages", 200, serde_json::json!([]));
    let deleted = serde_json::json!({"id": "1", "type": 0, "guild_id": "1", "name": "closed", "position": 0, "permission_overwrites": [], "nsfw": false});
    h.on("DELETE", "/channels/*", 200, deleted);
    let bot = Arc::new(h.bot());
    let ctx = h.ctx();
    println!("STRESS start: {users} users, baseline memory {}", peak_rss_mib());

    let wall = Instant::now();
    let opens: Vec<_> = (0..users)
        .map(|i| {
            let (bot, ctx) = (bot.clone(), ctx.clone());
            let ix = Interaction::Component(h.button(1, FIRST_USER + i, "ticket:open"));
            tokio::spawn(async move {
                let t = Instant::now();
                bot.on_interaction(&ctx, &ix).await;
                t.elapsed()
            })
        })
        .collect();
    let mut latencies = Vec::new();
    for task in opens {
        latencies.push(task.await.unwrap());
    }
    report("open", latencies, wall.elapsed());

    let channels: Vec<(u64, u64)> = (0..users)
        .map(|i| {
            let user = FIRST_USER + i;
            let ticket = h.repo.open_ticket_for("1", &user.to_string()).unwrap().unwrap_or_else(|| panic!("user {user} got no ticket"));
            (user, ticket.channel_id.parse().unwrap())
        })
        .collect();

    let wall = Instant::now();
    let closes: Vec<_> = channels
        .iter()
        .map(|&(user, channel)| {
            let (bot, ctx) = (bot.clone(), ctx.clone());
            let mut button = h.button(1, user, "ticket:close");
            button.channel_id = ChannelId::new(channel);
            let mut modal = h.modal(1, user, "ticket:closemodal", &[("reason", "load test")]);
            modal.channel_id = ChannelId::new(channel);
            tokio::spawn(async move {
                let t = Instant::now();
                bot.on_interaction(&ctx, &Interaction::Component(button)).await;
                bot.on_interaction(&ctx, &Interaction::Modal(modal)).await;
                t.elapsed()
            })
        })
        .collect();
    let mut latencies = Vec::new();
    for task in closes {
        latencies.push(task.await.unwrap());
    }
    report("close (button + reason)", latencies, wall.elapsed());

    let still_open = (0..users).filter(|i| h.repo.open_ticket_for("1", &(FIRST_USER + i).to_string()).unwrap().is_some()).count();
    let created = h.calls_to("POST", "/guilds/1/channels").len();
    println!("STRESS result: {created} channels created, {still_open} tickets left open, {} Discord calls total", h.calls().len());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(created as u64, users, "every user got exactly one ticket channel");
    assert_eq!(still_open, 0, "every ticket closed");
}
