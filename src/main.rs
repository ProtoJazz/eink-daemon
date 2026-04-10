use chrono::{NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use tokio::sync::watch;
use zbus::Connection;
use zbus::interface;
use zbus::message::Body;
use zbus::object_server::SignalEmitter;
use zbus::proxy::Builder;
use zbus::zvariant::Value;
// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Config {
    display: DisplayConfig,
    calendar: CalendarConfig,
    #[serde(default)]
    notifications: NotificationConfig,
}

#[derive(Deserialize)]
struct DisplayConfig {
    host: String,
    port: u16,
}

#[derive(Deserialize)]
struct CalendarConfig {
    uid: String,
    timezone: String,
}

#[derive(Deserialize)]
struct NotificationConfig {
    /// Path to log all received notifications for reference
    #[serde(default = "default_log_path")]
    log_path: String,
    /// Filter mode: "blacklist" or "whitelist"
    #[serde(default = "default_filter_mode")]
    filter_mode: String,
    /// Apps to exclude (blacklist mode) or include (whitelist mode).
    /// Matched against both app_name and desktop-entry hint.
    #[serde(default)]
    filter_apps: Vec<String>,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            log_path: default_log_path(),
            filter_mode: default_filter_mode(),
            filter_apps: Vec::new(),
        }
    }
}

fn default_log_path() -> String {
    "~/.local/share/eink-daemon/notifications.log".to_string()
}

fn default_filter_mode() -> String {
    "blacklist".to_string()
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let home = std::env::var("HOME")?;
    let config_path = format!("{}/.config/eink-daemon/config.toml", home);
    let content = fs::read_to_string(&config_path)?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}

struct TrayIcon;

#[interface(name = "org.kde.StatusNotifierItem")]
impl TrayIcon {
    // This becomes a D-Bus property called "IconName"
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "audio-subwoofer-symbolic"
    }

    #[zbus(property)]
    fn category(&self) -> &str {
        "SystemServices"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "eink-daemon"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "eink-daemon"
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
    #[zbus(property)]
    fn menu(&self) -> zbus::zvariant::ObjectPath<'_> {
        zbus::zvariant::ObjectPath::try_from("/Menu").unwrap()
    }
    #[zbus(signal)]
    async fn layout_updated(
        ctx: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
    // This would become a callable D-Bus method called "Activate"
    fn activate(&self, x: i32, y: i32) {
        println!("Someone clicked the tray icon!");
    }
}

struct TrayMenu;

#[interface(name = "com.canonical.dbusmenu")]
impl TrayMenu {
    fn get_layout(
        &self,
        _parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, (i32, HashMap<String, Value<'_>>, Vec<Value<'_>>)) {
        let item1 = Value::from((
            1u32,
            HashMap::from([
                ("label".to_string(), Value::from("eInk Display")),
                ("enabled".to_string(), Value::from(false)),
            ]),
            Vec::<Value>::new(),
        ));

        let item2 = Value::from((
            2u32,
            HashMap::from([("type".to_string(), Value::from("separator"))]),
            Vec::<Value>::new(),
        ));

        let item3 = Value::from((
            3u32,
            HashMap::from([("label".to_string(), Value::from("Quit"))]),
            Vec::<Value>::new(),
        ));

        (
            1u32, // revision
            (
                0i32,           // root id
                HashMap::new(), // root properties
                vec![item1, item2, item3],
            ),
        )
    }

    fn event(&self, id: i32, event_id: &str, _data: Value<'_>, _timestamp: u32) {
        if id == 3 && event_id == "clicked" {
            println!("Quit clicked!");
            std::process::exit(0);
        }
    }
}
// ── Calendar ────────────────────────────────────────────────────────────────

struct CalendarEvent {
    summary: String,
    start: chrono::DateTime<Tz>,
    end: chrono::DateTime<Tz>,
    recurring: bool,
}

fn parse_event(ical: &str) -> Option<CalendarEvent> {
    let mut summary = None;
    let mut dtstart_raw = None;
    let mut dtstart_tz = None;
    let mut dtend_raw = None;
    let mut dtend_tz = None;
    let mut recurring = false;

    for line in ical.lines() {
        if line.starts_with("SUMMARY:") {
            summary = Some(line.trim_start_matches("SUMMARY:").to_string());
        }
        if line.starts_with("DTSTART") {
            if let Some(tz_start) = line.find("TZID=") {
                let after_tzid = &line[tz_start + 5..];
                if let Some(colon) = after_tzid.find(':') {
                    dtstart_tz = Some(after_tzid[..colon].to_string());
                    dtstart_raw = Some(after_tzid[colon + 1..].to_string());
                }
            }
        }
        if line.starts_with("DTEND") {
            if let Some(tz_start) = line.find("TZID=") {
                let after_tzid = &line[tz_start + 5..];
                if let Some(colon) = after_tzid.find(':') {
                    dtend_tz = Some(after_tzid[..colon].to_string());
                    dtend_raw = Some(after_tzid[colon + 1..].to_string());
                }
            }
        }
        if line.starts_with("RRULE:") {
            recurring = true;
        }
    }

    let summary = summary?;
    let start_naive = NaiveDateTime::parse_from_str(&dtstart_raw?, "%Y%m%dT%H%M%S").ok()?;
    let end_naive = NaiveDateTime::parse_from_str(&dtend_raw?, "%Y%m%dT%H%M%S").ok()?;
    let start_tz: Tz = dtstart_tz?.parse().ok()?;
    let end_tz: Tz = dtend_tz?.parse().ok()?;

    Some(CalendarEvent {
        summary,
        start: start_tz.from_local_datetime(&start_naive).single()?,
        end: end_tz.from_local_datetime(&end_naive).single()?,
        recurring,
    })
}

fn format_countdown(minutes: i64) -> String {
    if minutes < 1 {
        "now".to_string()
    } else if minutes < 60 {
        format!("in {}m", minutes)
    } else {
        let hours = minutes / 60;
        let mins = minutes % 60;
        if mins == 0 {
            format!("in {}h", hours)
        } else {
            format!("in {}h{}m", hours, mins)
        }
    }
}

// ── Notifications ───────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Notification {
    app_name: String,
    desktop_entry: String,
    summary: String,
    body: String,
}

impl Notification {
    /// Returns the best identifier for this app — desktop_entry if available, else app_name
    fn app_id(&self) -> &str {
        if !self.desktop_entry.is_empty() {
            &self.desktop_entry
        } else {
            &self.app_name
        }
    }

    /// Check if this notification matches any entry in the given app list.
    /// Matches against both app_name and desktop_entry.
    fn matches_filter(&self, apps: &[String]) -> bool {
        apps.iter().any(|filter| {
            filter.eq_ignore_ascii_case(&self.app_name)
                || filter.eq_ignore_ascii_case(&self.desktop_entry)
        })
    }
}

fn log_notification(log_path: &PathBuf, notif: &Notification) {
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let line = format!(
        "[{}] app_name=\"{}\" desktop_entry=\"{}\" summary=\"{}\" body=\"{}\"\n",
        timestamp, notif.app_name, notif.desktop_entry, notif.summary, notif.body,
    );

    match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(mut f) => {
            let _ = f.write_all(line.as_bytes());
        }
        Err(e) => eprintln!("Failed to write notification log: {}", e),
    }
}

/// Parse a Notify method call body into a Notification.
/// D-Bus signature: (susssasa{sv}i)
fn parse_notify_body(body: &Body) -> Option<Notification> {
    // Deserialize the full Notify argument tuple.
    // The image-data in hints can be complex, so we destructure manually via zbus's
    // OwnedValue to avoid tripping on the giant image blobs.
    let (app_name, _replaces_id, _app_icon, summary, body_text, _actions, hints, _expire): (
        String,
        u32,
        String,
        String,
        String,
        Vec<String>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        i32,
    ) = body.deserialize().ok()?;

    let desktop_entry = hints
        .get("desktop-entry")
        .and_then(|v| <String>::try_from(v.clone()).ok())
        .unwrap_or_default();

    Some(Notification {
        app_name,
        desktop_entry,
        summary,
        body: body_text,
    })
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = load_config()?;
    let local_tz: Tz = config
        .calendar
        .timezone
        .parse()
        .map_err(|_| format!("Invalid timezone: {}", config.calendar.timezone))?;
    let esp32_url = format!("http://{}:{}/", config.display.host, config.display.port);

    let log_path = expand_tilde(&config.notifications.log_path);
    let filter_mode = config.notifications.filter_mode.clone();
    let filter_apps = config.notifications.filter_apps.clone();

    // Channel to pass the latest notification from the monitor task to the main loop
    let (notif_tx, notif_rx) = watch::channel::<Option<Notification>>(None);

    // ── Notification monitor task ───────────────────────────────────────
    let notif_connection = Connection::session().await?;

    // Add a match rule to eavesdrop on Notify calls.
    // Use BecomeMonitor to eavesdrop on Notify calls (same mechanism as dbus-monitor).
    let match_rule = "type='method_call',interface='org.freedesktop.Notifications',member='Notify'";
    let dbus_proxy: zbus::Proxy = Builder::new(&notif_connection)
        .destination("org.freedesktop.DBus")?
        .path("/org/freedesktop/DBus")?
        .interface("org.freedesktop.DBus.Monitoring")?
        .build()
        .await?;
    // BecomeMonitor(rules: Vec<String>, flags: u32)
    dbus_proxy
        .call::<_, _, ()>("BecomeMonitor", &(vec![match_rule], 0u32))
        .await?;

    use futures_util::StreamExt;
    let mut stream = zbus::MessageStream::from(&notif_connection);

    let log_path_clone = log_path.clone();
    let filter_mode_clone = filter_mode.clone();
    let filter_apps_clone = filter_apps.clone();

    tokio::spawn(async move {
        println!("[notif] monitor task started, waiting for messages...");
        while let Some(Ok(msg)) = stream.next().await {
            let header = msg.header();
            let member = header.member().map(|m| m.as_str().to_string());
            let iface = header.interface().map(|i| i.as_str().to_string());
            let msg_type = header.message_type();
            eprintln!(
                "[notif] D-Bus message: type={:?} interface={:?} member={:?}",
                msg_type, iface, member
            );

            // Only process method calls to Notify
            if member.as_deref() != Some("Notify") {
                continue;
            }
            if iface.as_deref() != Some("org.freedesktop.Notifications") {
                continue;
            }

            eprintln!("[notif] Got a Notify call, attempting to parse body...");
            let body = msg.body();
            if let Some(notif) = parse_notify_body(&body) {
                // Always log for reference
                log_notification(&log_path_clone, &notif);

                // Apply filter
                let dominated = notif.matches_filter(&filter_apps_clone);
                let pass = match filter_mode_clone.as_str() {
                    "whitelist" => dominated,
                    _ => !dominated, // blacklist: pass if NOT in list
                };

                if pass {
                    println!(
                        "[notif] {} | {} | {}",
                        notif.app_id(),
                        notif.summary,
                        notif.body
                    );
                    let _ = notif_tx.send(Some(notif));
                } else {
                    println!("[notif] FILTERED: {} | {}", notif.app_id(), notif.summary);
                }
            } else {
                eprintln!("[notif] Failed to parse Notify body");
            }
        }
        eprintln!("[notif] monitor task ended unexpectedly");
    });

    // ── Calendar + display loop ─────────────────────────────────────────
    let connection = Connection::session().await?;

    let factory: zbus::Proxy = Builder::new(&connection)
        .destination("org.gnome.evolution.dataserver.Calendar8")?
        .path("/org/gnome/evolution/dataserver/CalendarFactory")?
        .interface("org.gnome.evolution.dataserver.CalendarFactory")?
        .build()
        .await?;

    let (object_path, bus_name): (String, String) = factory
        .call("OpenCalendar", &(config.calendar.uid.as_str(),))
        .await?;

    let calendar: zbus::Proxy = Builder::new(&connection)
        .destination(bus_name.clone())?
        .path(object_path.as_str())?
        .interface("org.gnome.evolution.dataserver.Calendar")?
        .build()
        .await?;

    println!("eink-daemon started, polling every 60s");
    println!(
        "notification filter: mode={}, apps={:?}",
        filter_mode, filter_apps
    );

    connection
        .object_server()
        .at("/StatusNotifierItem", TrayIcon)
        .await?;
    connection.object_server().at("/Menu", TrayMenu).await?;

    let watcher: zbus::Proxy = Builder::new(&connection)
        .destination("org.kde.StatusNotifierWatcher")?
        .path("/StatusNotifierWatcher")?
        .interface("org.kde.StatusNotifierWatcher")?
        .build()
        .await?;

    watcher
        .call::<_, _, ()>(
            "RegisterStatusNotifierItem",
            &(connection.unique_name().unwrap().as_str(),),
        )
        .await?;

    loop {
        let now = Utc::now();
        let tomorrow = now + chrono::Duration::hours(24);
        let query = format!(
            r#"(occur-in-time-range? (make-time "{}") (make-time "{}"))"#,
            now.format("%Y%m%dT%H%M%SZ"),
            tomorrow.format("%Y%m%dT%H%M%SZ"),
        );

        let events: Vec<String> = calendar.call("GetObjectList", &(query.as_str(),)).await?;

        let now_local = now.with_timezone(&local_tz);
        let today = now_local.date_naive();

        let mut upcoming: Vec<CalendarEvent> = events
            .iter()
            .filter_map(|s| parse_event(s))
            .filter_map(|mut e| {
                if e.recurring {
                    let original_time = e.start.with_timezone(&local_tz).time();
                    let duration = e.end - e.start;
                    let today_start = local_tz
                        .from_local_datetime(&today.and_time(original_time))
                        .single()?;
                    let today_end = today_start + duration;
                    let tz = e.start.timezone();
                    let tz2 = e.end.timezone();
                    e.start = today_start.with_timezone(&tz);
                    e.end = today_end.with_timezone(&tz2);
                }
                let local_start = e.start.with_timezone(&local_tz);
                if local_start > now_local {
                    Some(e)
                } else {
                    None
                }
            })
            .collect();

        upcoming.sort_by_key(|e| e.start);

        let (event_name, event_time, event_countdown) = if let Some(next) = upcoming.first() {
            let local_start = next.start.with_timezone(&local_tz);
            let local_end = next.end.with_timezone(&local_tz);
            let minutes_until = (local_start - now_local).num_minutes();
            (
                next.summary.clone(),
                format!(
                    "{}-{}",
                    local_start.format("%-H:%M"),
                    local_end.format("%-H:%M")
                ),
                format_countdown(minutes_until),
            )
        } else {
            (
                "No upcoming events".to_string(),
                String::new(),
                String::new(),
            )
        };

        // Get the latest notification (if any)
        let (notif_app, notif_text) = match notif_rx.borrow().as_ref() {
            Some(n) => (n.app_id().to_string(), n.summary.clone()),
            None => (String::new(), String::new()),
        };

        let payload = serde_json::json!({
            "event_name": event_name,
            "event_time": event_time,
            "event_countdown": event_countdown,
            "notif_app": notif_app,
            "notif_text": notif_text,
        });

        println!("[{}] -> {}", now_local.format("%H:%M:%S"), payload);

        match ureq::post(&esp32_url)
            .header("Content-Type", "application/json")
            .send(payload.to_string().as_bytes())
        {
            Ok(_) => {}
            Err(e) => eprintln!("Failed to reach ESP32: {}", e),
        }

        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}
