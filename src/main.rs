use zbus::Connection;
use zbus::proxy::Builder;
use chrono::{NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use std::fs;

#[derive(Deserialize)]
struct Config {
    display: DisplayConfig,
    calendar: CalendarConfig,
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

fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let home = std::env::var("HOME")?;
    let config_path = format!("{}/.config/eink-daemon/config.toml", home);
    let content = fs::read_to_string(&config_path)?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}
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
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = load_config()?;
    let local_tz: Tz = config.calendar.timezone.parse()
        .map_err(|_| format!("Invalid timezone: {}", config.calendar.timezone))?;
    let esp32_url = format!("http://{}:{}/", config.display.host, config.display.port);

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

    println!("eink-daemon started, polling every 30s");

    loop {
        let now = Utc::now();
        let tomorrow = now + chrono::Duration::hours(24);
        let query = format!(
            r#"(occur-in-time-range? (make-time "{}") (make-time "{}"))"#,
            now.format("%Y%m%dT%H%M%SZ"),
            tomorrow.format("%Y%m%dT%H%M%SZ"),
        );

        let events: Vec<String> = calendar
            .call("GetObjectList", &(query.as_str(),))
            .await?;

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
            eprintln!("DEBUG: now_local = {:?}", now_local);
            eprintln!("DEBUG: local_start = {:?}", local_start);
            eprintln!("DEBUG: diff minutes = {}", (local_start - now_local).num_minutes());
            (
                next.summary.clone(),
                format!("{}-{}", local_start.format("%-H:%M"), local_end.format("%-H:%M")),
                format_countdown(minutes_until),
            )
        } else {
            ("No upcoming events".to_string(), String::new(), String::new())
        };

        let payload = serde_json::json!({
            "event_name": event_name,
            "event_time": event_time,
            "event_countdown": event_countdown,
            "notif_app": "",
            "notif_text": ""
        });

        println!("[{}] {}", now_local.format("%H:%M:%S"), event_name);

        match ureq::post(&esp32_url)
            .header("Content-Type", "application/json")
            .send(payload.to_string().as_bytes())
        {
            Ok(_) => {},
            Err(e) => eprintln!("Failed to reach ESP32: {}", e),
        }

        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}