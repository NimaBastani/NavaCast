//! Ctrl+C must return. On Windows the same handler runs for the console
//! control event; this process test sends SIGINT and is executed on Linux.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn sigint_exits_within_a_few_seconds() {
    let dir = std::env::temp_dir().join(format!("radio-sigint-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = dir.join("navad.json");
    let data = dir.join("data");
    std::fs::write(
        &cfg_path,
        format!(
            r#"{{"data_dir":"{}","stream_listen":"127.0.0.1:0","http_listen":"127.0.0.1:0","stream_public_url":"http://127.0.0.1:9","http_public_url":"http://127.0.0.1:9"}}"#,
            data.display()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_navad"))
        .arg("--config")
        .arg(&cfg_path)
        .env("NAVA_ADMIN_USER", "admin")
        .env("NAVA_ADMIN_PASSWORD", "testpass1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = std::io::BufReader::new(stdout);
        loop {
            buf.clear();
            match std::io::BufRead::read_line(&mut reader, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(buf.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let err_log = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let err_copy = std::sync::Arc::clone(&err_log);
    thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = std::io::BufReader::new(stderr);
        loop {
            buf.clear();
            match std::io::BufRead::read_line(&mut reader, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => err_copy.lock().unwrap().push_str(&buf),
            }
        }
    });
    let mut stream = None;
    let mut control = None;
    let start = Instant::now();
    while stream.is_none() || control.is_none() {
        if start.elapsed() > Duration::from_secs(25) {
            let _ = child.kill();
            panic!("navad did not bind: {}", err_log.lock().unwrap());
        }
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(line) => {
                if let Some(rest) = line.strip_prefix("navad: stream ") {
                    stream = Some(rest.trim().to_string());
                }
                if let Some(rest) = line.strip_prefix("navad: control ") {
                    control = Some(rest.trim().trim_start_matches("http://").to_string());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let stream = stream.expect("stream address");
    let control = control.expect("control address");
    let login = http(&control, "POST", "/api/login", None, r#"{"username":"admin","password":"testpass1"}"#);
    let token = json_string(&login, "token");
    let station = http(&control, "POST", "/api/stations", Some(&token), r#"{"name":"Night","slug":"night","description":"","genre":"jazz"}"#);
    let station_id = json_string(&station, "id");
    let mount_body = format!(r#"{{"station_id":"{station_id}","path":"/live","source_password":"secret1"}}"#);
    let _mount = http(&control, "POST", "/api/mounts", Some(&token), &mount_body);
    let mut listener = TcpStream::connect(&stream).unwrap();
    listener.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    listener.write_all(b"GET /live HTTP/1.0\r\n\r\n").unwrap();
    let mut sniff = [0u8; 64];
    let _ = listener.read(&mut sniff);
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let killed = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if killed.elapsed() > Duration::from_secs(8) {
            let _ = child.kill();
            panic!("SIGINT did not stop navad within 8s\n{}", err_log.lock().unwrap());
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn http(addr: &str, method: &str, path: &str, token: Option<&str>, body: &str) -> String {
    let mut sock = TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Length: {}\r\n", body.len());
    if let Some(token) = token {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("\r\n");
    req.push_str(body);
    sock.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        match sock.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    assert!(text.contains("200 ") || text.contains("201 "), "{method} {path} -> {text}");
    text
}

fn json_string(body: &str, key: &str) -> String {
    let json = body.split("\r\n\r\n").nth(1).unwrap_or("");
    let needle = format!("\"{key}\":\"");
    let start = json.find(&needle).unwrap_or_else(|| panic!("missing {key} in {json}"));
    let rest = &json[start + needle.len()..];
    let end = rest.find('"').unwrap();
    rest[..end].to_string()
}
