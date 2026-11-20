//! Admin CLI for NavaCast. Talks to navad's control API (localhost by default).
//!
//! Set NAVA_API and NAVA_TOKEN, then run `navactl stations list`.

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::exit;
use std::time::Duration;

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let mut api = env::var("NAVA_API").unwrap_or_else(|_| "http://127.0.0.1:8088".into());
    let mut token = env::var("NAVA_TOKEN").unwrap_or_default();
    while args.first().map(|s| s.starts_with("--")).unwrap_or(false) {
        match args[0].as_str() {
            "--api" => { api = take(&mut args, 1); }
            "--token" => { token = take(&mut args, 1); }
            "--help" | "-h" => { help(); return; }
            _ => break,
        }
    }
    if args.is_empty() {
        help();
        exit(1);
    }
    let result = match args[0].as_str() {
        "stations" => stations(&api, &token, &args[1..]),
        "mounts" => mounts(&api, &token, &args[1..]),
        "upload" => upload(&api, &token, &args[1..]),
        "playlists" => playlists(&api, &token, &args[1..]),
        "queue" => queue(&api, &token, &args[1..]),
        "seek" => seek(&api, &token, &args[1..]),
        "jump" => jump(&api, &token, &args[1..]),
        "play-next" => play_next(&api, &token, &args[1..]),
        "media" => media_cmd(&api, &token, &args[1..]),
        "schedules" => schedules(&api, &token, &args[1..]),
        "record" => record(&api, &token, &args[1..]),
        "listeners" => listeners(&api, &token, &args[1..]),
        "users" => users(&api, &token, &args[1..]),
        "login" => login(&api, &args[1..]),
        "live" => live(&api, &token),
        "logs" => logs(&api, &token, &args[1..]),
        "health" => health(&api, &token),
        "help" => { help(); Ok(()) }
        other => Err(format!("unknown command {other}")),
    };
    if let Err(err) = result {
        eprintln!("navactl: {err}");
        exit(1);
    }
}

fn help() {
    println!(r#"navactl [--api URL] [--token TOKEN] <command>

NavaCast control CLI.

  login --username USER --password PASS
  stations list|create|delete
  mounts list|create|update|delete|password
    create/update: --encoder ffmpeg|native|opus  --relay URL
  upload --station ID --file PATH
  media search|tag|delete|replace
  playlists list|create|add|reorder|remove|schedule
  queue show|next|skip|previous|move|remove|clear
  seek --mount ID --position MS
  jump --mount ID --media ID
  play-next --mount ID --media ID
  schedules list|update
  record start|stop|list|delete
    start: --mount ID [--max-ms N]
    delete: --mount ID [--id RECORDING | --all]
  listeners list|kick|kick-all
  users create|token
  live
  logs [--limit N]
  health

Environment: NAVA_API (default http://127.0.0.1:8088), NAVA_TOKEN.
"#);
}

fn seek(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    let id = flag(args, "--mount")?;
    let position = flag(args, "--position")?.parse::<u64>().map_err(|e| e.to_string())?;
    let payload = serde_json::json!({"position_ms": position});
    let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/seek"), token, Some(payload.to_string()), Some("application/json"))?;
    expect(code, &body)?;
    println!("{}", body.trim());
    Ok(())
}

fn jump(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    let id = flag(args, "--mount")?;
    let media = flag(args, "--media")?;
    let payload = serde_json::json!({"media_id": media});
    let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/jump"), token, Some(payload.to_string()), Some("application/json"))?;
    expect(code, &body)?;
    println!("{}", body.trim());
    Ok(())
}

fn play_next(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    let id = flag(args, "--mount")?;
    let media = flag(args, "--media")?;
    let payload = serde_json::json!({"media_id": media});
    let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/play-next"), token, Some(payload.to_string()), Some("application/json"))?;
    expect(code, &body)?;
    println!("{}", body.trim());
    Ok(())
}

fn stations(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let (code, body) = request(api, "GET", "/api/stations", token, None, None)?;
            expect(code, &body)?;
            print_array(&body, &["id", "name", "slug", "genre"]);
        }
        Some("create") => {
            let name = flag(args, "--name")?;
            let slug = flag(args, "--slug")?;
            let description = flag_opt(args, "--description").unwrap_or_default();
            let genre = flag_opt(args, "--genre").unwrap_or_default();
            let payload = serde_json::json!({"name": name, "slug": slug, "description": description, "genre": genre});
            let (code, body) = request(api, "POST", "/api/stations", token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("delete") => {
            let id = flag(args, "--id")?;
            let (code, body) = request(api, "DELETE", &format!("/api/stations/{id}"), token, None, None)?;
            expect(code, &body)?;
            println!("deleted {id}");
        }
        _ => return Err("stations list|create|delete".into()),
    }
    Ok(())
}

fn mounts(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let q = flag_opt(args, "--station").map(|id| format!("?station={id}")).unwrap_or_default();
            let (code, body) = request(api, "GET", &format!("/api/mounts{q}"), token, None, None)?;
            expect(code, &body)?;
            print_array(&body, &["id", "path", "bitrate", "encoder", "relay_url", "listeners", "live"]);
        }
        Some("create") => {
            let payload = serde_json::json!({
                "station_id": flag(args, "--station")?,
                "path": flag(args, "--path")?,
                "source_password": flag(args, "--password")?,
                "bitrate": flag_opt(args, "--bitrate").and_then(|s| s.parse::<u32>().ok()).unwrap_or(128000),
                "max_listeners": flag_opt(args, "--max-listeners").and_then(|s| s.parse::<u32>().ok()).unwrap_or(100),
                "max_bitrate": flag_opt(args, "--max-bitrate").and_then(|s| s.parse::<u32>().ok()).unwrap_or(320000),
                "allow_takeover": flag_opt(args, "--takeover").unwrap_or_else(|| "false".into()) == "true",
                "fallback_playlist_id": flag_opt(args, "--fallback"),
                "encoder": flag_opt(args, "--encoder").unwrap_or_default(),
                "relay_url": flag_opt(args, "--relay").unwrap_or_default(),
            });
            let (code, body) = request(api, "POST", "/api/mounts", token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("update") => {
            let id = flag(args, "--id")?;
            let mut payload = serde_json::Map::new();
            for (k, json_key) in [("--path", "path"), ("--password", "source_password"), ("--fallback", "fallback_playlist_id")] {
                if let Some(v) = flag_opt(args, k) { payload.insert(json_key.into(), serde_json::Value::String(v)); }
            }
            if let Some(v) = flag_opt(args, "--bitrate") { payload.insert("bitrate".into(), serde_json::json!(v.parse::<u32>().map_err(|e| e.to_string())?)); }
            if let Some(v) = flag_opt(args, "--max-listeners") { payload.insert("max_listeners".into(), serde_json::json!(v.parse::<u32>().map_err(|e| e.to_string())?)); }
            if let Some(v) = flag_opt(args, "--takeover") { payload.insert("allow_takeover".into(), serde_json::json!(v == "true")); }
            if let Some(v) = flag_opt(args, "--crossfade") { payload.insert("crossfade_ms".into(), serde_json::json!(v.parse::<u64>().map_err(|e| e.to_string())?)); }
            if let Some(v) = flag_opt(args, "--autodj") { payload.insert("autodj".into(), serde_json::json!(v == "true")); }
            if let Some(v) = flag_opt(args, "--shuffle") { payload.insert("shuffle".into(), serde_json::json!(v == "true")); }
            if let Some(v) = flag_opt(args, "--repeat") {
                if !matches!(v.as_str(), "off" | "one" | "all") {
                    return Err("repeat must be off, one, or all".into());
                }
                payload.insert("repeat".into(), serde_json::Value::String(v));
            }
            if let Some(v) = flag_opt(args, "--gain") { payload.insert("gain_db".into(), serde_json::json!(v.parse::<f64>().map_err(|e| e.to_string())?)); }
            if let Some(v) = flag_opt(args, "--sample-rate") { payload.insert("sample_rate".into(), serde_json::json!(v.parse::<u32>().map_err(|e| e.to_string())?)); }
            if let Some(v) = flag_opt(args, "--channels") { payload.insert("channels".into(), serde_json::json!(v.parse::<u16>().map_err(|e| e.to_string())?)); }
            if let Some(v) = flag_opt(args, "--buffer") { payload.insert("buffer_ms".into(), serde_json::json!(v.parse::<u64>().map_err(|e| e.to_string())?)); }
            if let Some(v) = flag_opt(args, "--hls-segment") { payload.insert("hls_segment_ms".into(), serde_json::json!(v.parse::<u64>().map_err(|e| e.to_string())?)); }
            if let Some(v) = flag_opt(args, "--jingle-every") { payload.insert("jingle_every".into(), serde_json::json!(v.parse::<u32>().map_err(|e| e.to_string())?)); }
            if let Some(v) = flag_opt(args, "--jingle") { payload.insert("jingle_media_id".into(), serde_json::Value::String(v)); }
            if let Some(v) = flag_opt(args, "--intro") { payload.insert("intro_media_id".into(), serde_json::Value::String(v)); }
            if let Some(v) = flag_opt(args, "--encoder") { payload.insert("encoder".into(), serde_json::Value::String(v)); }
            if let Some(v) = flag_opt(args, "--relay") { payload.insert("relay_url".into(), serde_json::Value::String(v)); }
            let (code, body) = request(api, "PATCH", &format!("/api/mounts/{id}"), token, Some(serde_json::Value::Object(payload).to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("delete") => {
            let id = flag(args, "--id")?;
            let (code, body) = request(api, "DELETE", &format!("/api/mounts/{id}"), token, None, None)?;
            expect(code, &body)?;
            println!("deleted {id}");
        }
        Some("password") => {
            let id = flag(args, "--id")?;
            let payload = serde_json::json!({"password": flag(args, "--password")?});
            let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/password"), token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        _ => return Err("mounts list|create|update|delete|password".into()),
    }
    Ok(())
}

fn upload(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    let station = flag(args, "--station")?;
    let path = flag(args, "--file")?;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let filename = std::path::Path::new(&path).file_name().and_then(|s| s.to_str()).unwrap_or("track");
    let mut body = Vec::new();
    body.extend_from_slice(b"------r\r\nContent-Disposition: form-data; name=\"station_id\"\r\n\r\n");
    body.extend_from_slice(station.as_bytes());
    body.extend_from_slice(format!("\r\n------r\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
    body.extend_from_slice(&bytes);
    body.extend_from_slice(b"\r\n------r--\r\n");
    let (code, resp) = request_raw(api, "POST", "/api/media", token, &body, "multipart/form-data; boundary=----r")?;
    expect(code, &resp)?;
    println!("{}", resp.trim());
    Ok(())
}

fn playlists(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let station = flag(args, "--station")?;
            let (code, body) = request(api, "GET", &format!("/api/playlists?station={station}"), token, None, None)?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("create") => {
            let payload = serde_json::json!({"station_id": flag(args, "--station")?, "name": flag(args, "--name")?});
            let (code, body) = request(api, "POST", "/api/playlists", token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("add") => {
            let id = flag(args, "--playlist")?;
            let payload = serde_json::json!({"media_id": flag(args, "--media")?});
            let (code, body) = request(api, "POST", &format!("/api/playlists/{id}/items"), token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("reorder") => {
            let id = flag(args, "--playlist")?;
            let payload = serde_json::json!({
                "from": flag(args, "--from")?.parse::<usize>().map_err(|e| e.to_string())?,
                "to": flag(args, "--to")?.parse::<usize>().map_err(|e| e.to_string())?,
            });
            let (code, body) = request(api, "POST", &format!("/api/playlists/{id}/reorder"), token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("remove") => {
            let id = flag(args, "--playlist")?;
            let item = flag(args, "--item")?;
            let (code, body) = request(api, "DELETE", &format!("/api/playlists/{id}/items/{item}"), token, None, None)?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("schedule") => {
            let days: Vec<String> = flag(args, "--days")?.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            let payload = serde_json::json!({
                "mount_id": flag(args, "--mount")?,
                "playlist_id": flag(args, "--playlist")?,
                "days": days,
                "start": flag(args, "--start")?,
                "end": flag(args, "--end")?,
                "priority": flag_opt(args, "--priority").and_then(|s| s.parse::<i32>().ok()).unwrap_or(1),
            });
            let (code, body) = request(api, "POST", "/api/schedules", token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        _ => return Err("playlists list|create|add|reorder|remove|schedule".into()),
    }
    Ok(())
}

fn queue(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("");
    let id = flag(args, "--mount")?;
    match action {
        "show" => {
            let (code, body) = request(api, "GET", &format!("/api/mounts/{id}/queue"), token, None, None)?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        "skip" | "previous" | "clear" => {
            let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/queue/{action}"), token, Some("{}".into()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        "next" => {
            let payload = serde_json::json!({"media_id": flag_opt(args, "--media").unwrap_or_default()});
            let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/queue/next"), token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        "move" => {
            let payload = serde_json::json!({
                "from": flag(args, "--from")?.parse::<usize>().map_err(|e| e.to_string())?,
                "to": flag(args, "--to")?.parse::<usize>().map_err(|e| e.to_string())?,
            });
            let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/queue/move"), token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        "remove" => {
            let payload = serde_json::json!({"index": flag(args, "--index")?.parse::<usize>().map_err(|e| e.to_string())?});
            let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/queue/remove"), token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        _ => return Err("queue show|next|skip|previous|move|remove|clear --mount ID".into()),
    }
    Ok(())
}

fn media_cmd(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("search") => {
            let station = flag(args, "--station")?;
            let q = flag_opt(args, "--query").unwrap_or_default();
            let (code, body) = request(api, "GET", &format!("/api/media?station={station}&q={q}"), token, None, None)?;
            expect(code, &body)?;
            print_array(&body, &["id", "artist", "title", "filename"]);
        }
        Some("tag") => {
            let id = flag(args, "--id")?;
            let payload = serde_json::json!({"title": flag(args, "--title")?, "artist": flag_opt(args, "--artist").unwrap_or_default()});
            let (code, body) = request(api, "PATCH", &format!("/api/media/{id}"), token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("delete") => {
            let id = flag(args, "--id")?;
            let (code, body) = request(api, "DELETE", &format!("/api/media/{id}"), token, None, None)?;
            expect(code, &body)?;
            println!("deleted {id}");
        }
        Some("replace") => {
            let id = flag(args, "--id")?;
            let path = flag(args, "--file")?;
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            let filename = std::path::Path::new(&path).file_name().and_then(|s| s.to_str()).unwrap_or("track");
            let mut body = Vec::new();
            body.extend_from_slice(format!("------r\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
            body.extend_from_slice(&bytes);
            body.extend_from_slice(b"\r\n------r--\r\n");
            let (code, resp) = request_raw(api, "POST", &format!("/api/media/{id}/replace"), token, &body, "multipart/form-data; boundary=----r")?;
            expect(code, &resp)?;
            println!("{}", resp.trim());
        }
        _ => return Err("media search|tag|delete|replace".into()),
    }
    Ok(())
}

fn schedules(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let mount = flag(args, "--mount")?;
            let (code, body) = request(api, "GET", &format!("/api/schedules?mount={mount}"), token, None, None)?;
            expect(code, &body)?;
            print_array(&body, &["id", "playlist_id", "priority", "enabled", "start_min", "end_min"]);
        }
        Some("update") => {
            let id = flag(args, "--id")?;
            let mut payload = serde_json::Map::new();
            if let Some(v) = flag_opt(args, "--enabled") { payload.insert("enabled".into(), serde_json::json!(v == "true")); }
            if let Some(v) = flag_opt(args, "--priority") { payload.insert("priority".into(), serde_json::json!(v.parse::<i64>().map_err(|e| e.to_string())?)); }
            let (code, body) = request(api, "PATCH", &format!("/api/schedules/{id}"), token, Some(serde_json::Value::Object(payload).to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        _ => return Err("schedules list|update".into()),
    }
    Ok(())
}

fn listeners(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let id = flag(args, "--mount")?;
            let (code, body) = request(api, "GET", &format!("/api/mounts/{id}/listeners"), token, None, None)?;
            expect(code, &body)?;
            print_array(&body, &["id", "ip", "connected_sec", "user_agent"]);
        }
        Some("kick") => {
            let id = flag(args, "--mount")?;
            let lid = flag(args, "--id")?;
            let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/listeners/{lid}/kick"), token, Some("{}".into()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("kick-all") => {
            let id = flag(args, "--mount")?;
            let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/listeners/kick"), token, Some("{}".into()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        _ => return Err("listeners list|kick|kick-all --mount ID".into()),
    }
    Ok(())
}

fn users(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("create") => {
            let payload = serde_json::json!({"username": flag(args, "--username")?, "password": flag(args, "--password")?});
            let (code, body) = request(api, "POST", "/api/users", token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("token") => {
            let (code, body) = request(api, "POST", "/api/tokens/rotate", token, Some("{}".into()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        _ => return Err("users create|token".into()),
    }
    Ok(())
}

fn record(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    let id = flag(args, "--mount")?;
    match args.first().map(String::as_str) {
        Some("start") => {
            let max_ms = flag_opt(args, "--max-ms").unwrap_or_else(|| "0".into());
            let payload = serde_json::json!({"max_duration_ms": max_ms.parse::<u64>().map_err(|e| e.to_string())?});
            let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/record/start"), token, Some(payload.to_string()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("stop") => {
            let (code, body) = request(api, "POST", &format!("/api/mounts/{id}/record/stop"), token, Some("{}".into()), Some("application/json"))?;
            expect(code, &body)?;
            println!("{}", body.trim());
        }
        Some("list") => {
            let (code, body) = request(api, "GET", &format!("/api/mounts/{id}/recordings"), token, None, None)?;
            expect(code, &body)?;
            print_array(&body, &["id", "started_at", "duration_ms", "format", "bytes", "mount", "active", "error"]);
        }
        Some("delete") => {
            if flag_opt(args, "--all").is_some() || args.iter().any(|s| s == "--all") {
                let (code, body) = request(api, "DELETE", &format!("/api/mounts/{id}/recordings"), token, None, None)?;
                expect(code, &body)?;
                println!("{}", body.trim());
            } else {
                let rid = flag(args, "--id")?;
                let (code, body) = request(api, "DELETE", &format!("/api/mounts/{id}/recordings/{rid}"), token, None, None)?;
                expect(code, &body)?;
                println!("{}", body.trim());
            }
        }
        _ => return Err("record start|stop|list|delete".into()),
    }
    Ok(())
}

fn login(api: &str, args: &[String]) -> Result<(), String> {
    let payload = serde_json::json!({"username": flag(args, "--username")?, "password": flag(args, "--password")?});
    let (code, body) = request(api, "POST", "/api/login", "", Some(payload.to_string()), Some("application/json"))?;
    expect(code, &body)?;
    println!("{}", body.trim());
    Ok(())
}

fn live(api: &str, token: &str) -> Result<(), String> {
    let (code, body) = request(api, "GET", "/api/live", token, None, None)?;
    expect(code, &body)?;
    print_array(&body, &["path", "station_name", "mode", "listeners", "peak_listeners", "bytes_sent"]);
    Ok(())
}

fn logs(api: &str, token: &str, args: &[String]) -> Result<(), String> {
    let limit = flag_opt(args, "--limit").unwrap_or_else(|| "50".into());
    let (code, body) = request(api, "GET", &format!("/api/audit?limit={limit}"), token, None, None)?;
    expect(code, &body)?;
    print_array(&body, &["at", "actor", "action", "detail"]);
    Ok(())
}

fn health(api: &str, token: &str) -> Result<(), String> {
    let (code, body) = request(api, "GET", "/api/health", token, None, None)?;
    expect(code, &body)?;
    println!("{}", body.trim());
    Ok(())
}

fn request(api: &str, method: &str, path: &str, token: &str, body: Option<String>, content_type: Option<&str>) -> Result<(u16, String), String> {
    let bytes = body.unwrap_or_default().into_bytes();
    let (code, text) = request_raw(api, method, path, token, &bytes, content_type.unwrap_or("application/json"))?;
    Ok((code, text))
}

fn request_raw(api: &str, method: &str, path: &str, token: &str, body: &[u8], content_type: &str) -> Result<(u16, String), String> {
    let (host, port) = parse_api(api)?;
    let mut sock = TcpStream::connect((host.as_str(), port)).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_secs(30))).map_err(|e| e.to_string())?;
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n");
    if !token.is_empty() {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!("Content-Type: {content_type}\r\nContent-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    sock.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    sock.write_all(body).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let code = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let resp = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    Ok((code, resp))
}

fn parse_api(api: &str) -> Result<(String, u16), String> {
    let rest = api.strip_prefix("http://").ok_or("NAVA_API must be http://host:port (TLS belongs on a reverse proxy)")?;
    let hostport = rest.split('/').next().unwrap_or(rest);
    if let Some((host, port)) = hostport.rsplit_once(':') {
        let port: u16 = port.parse().map_err(|_| "bad port".to_string())?;
        Ok((host.to_string(), port))
    } else {
        Ok((hostport.to_string(), 80))
    }
}

fn expect(code: u16, body: &str) -> Result<(), String> {
    if (200..300).contains(&code) { Ok(()) } else { Err(format!("HTTP {code}: {}", body.trim())) }
}

fn print_array(body: &str, cols: &[&str]) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        println!("{}", body.trim());
        return;
    };
    let Some(rows) = value.as_array() else {
        println!("{}", body.trim());
        return;
    };
    println!("{}", cols.join("\t"));
    for row in rows {
        let line = cols.iter().map(|c| cell(row.get(*c))).collect::<Vec<_>>().join("\t");
        println!("{line}");
    }
}

fn cell(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn flag(args: &[String], name: &str) -> Result<String, String> {
    flag_opt(args, name).ok_or_else(|| format!("missing {name}"))
}

fn flag_opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|s| s == name).and_then(|i| args.get(i + 1)).cloned()
}

fn take(args: &mut Vec<String>, index: usize) -> String {
    let value = args.get(index).cloned().unwrap_or_default();
    if args.len() >= 2 { args.drain(..2); }
    value
}
