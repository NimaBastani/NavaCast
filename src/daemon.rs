use crate::api::{self, ApiState};
use crate::config::{self, Config};
use crate::encoder::{self, AudioFactory, RouterFactory};
use crate::engine::{Engine, EngineOpts};
use crate::error::Error;
use crate::library::Library;
use crate::store::Store;
use crate::teamspeak::TeamSpeak;
use crate::{ignore_sigpipe, stream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

pub fn run() -> Result<(), Error> {
    ignore_sigpipe();
    let mut args = std::env::args().skip(1);
    let mut config_path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                config_path = Some(PathBuf::from(args.next().ok_or_else(|| Error::Invalid("missing --config path".into()))?));
            }
            "--help" | "-h" => {
                println!("navad [--config navad.json]\n\nNavaCast. Control API defaults to 127.0.0.1:8088. Stream port defaults to :8000 and does not serve admin routes.");
                return Ok(());
            }
            other => return Err(Error::Invalid(format!("unknown argument {other}"))),
        }
    }
    let cfg = Config::load(config_path.as_deref())?;
    let data = PathBuf::from(&cfg.data_dir);
    std::fs::create_dir_all(data.join("media"))?;
    std::fs::create_dir_all(data.join("recordings"))?;
    let store = Arc::new(Store::open(&data.join("navad.db"))?);
    let (admin_user, admin_pass) = config::admin_from_env();
    if let Some((user, pass, token)) = api::bootstrap(&store, admin_user, admin_pass)? {
        println!("navad: created the first admin account");
        println!("  username: {user}");
        println!("  password: {pass}");
        println!("  token:    {token}");
        println!("Store the token now. It is not shown again.");
    }
    let kind = encoder::default_encoder();
    println!("navad: default encoder {kind}; each mount uses ffmpeg, native mp3, or built-in opus");
    let factory: Arc<dyn AudioFactory> = Arc::new(RouterFactory);
    let library = Arc::new(Library::new(Arc::clone(&store), data.join("media"), cfg.tz_offset_min));
    let engine = Arc::new(Engine::new(
        factory,
        library,
        EngineOpts::production(data.join("media"), cfg.meta_int, cfg.max_conns_per_ip, Duration::from_millis(cfg.fade_ms)),
    ));
    let trusted = crate::limit::Trusted::parse(&cfg.trusted_proxies);
    engine.set_stream_public(cfg.stream_public_url.clone());
    engine.set_trusted(trusted.clone());
    engine.set_recordings_root(data.join("recordings"));
    engine.set_recording_cap(cfg.max_recording_bytes);
    let teamspeak = TeamSpeak::open(Arc::clone(&store), Arc::clone(&engine), data.join("media"));
    let admin_state = Arc::new(ApiState {
        store: Arc::clone(&store),
        engine: Arc::clone(&engine),
        media_root: data.join("media"),
        max_upload: cfg.max_upload_bytes,
        stream_public_url: cfg.stream_public_url.clone(),
        http_public_url: cfg.http_public_url.clone(),
        allow_admin: true,
        trusted: trusted.clone(),
        fails: crate::limit::FailWindow::new(8, Duration::from_secs(60)),
        recordings_root: data.join("recordings"),
        teamspeak: Arc::clone(&teamspeak),
    });
    let stream_addr = Config::normalize_listen(&cfg.stream_listen);
    let http_addr = Config::normalize_listen(&cfg.http_listen);
    let mut stream_handle = stream::spawn(&stream_addr, Arc::clone(&engine), Duration::from_secs(cfg.source_timeout_sec))?;
    let mut http_handle = api::spawn(&http_addr, Arc::clone(&admin_state))?;
    let mut public_handle = if cfg.public_listen.is_empty() {
        None
    } else {
        let public_state = Arc::new(ApiState {
            store: Arc::clone(&store),
            engine: Arc::clone(&engine),
            media_root: data.join("media"),
            max_upload: cfg.max_upload_bytes,
            stream_public_url: cfg.stream_public_url.clone(),
            http_public_url: cfg.http_public_url.clone(),
            allow_admin: false,
            trusted,
            fails: crate::limit::FailWindow::new(8, Duration::from_secs(60)),
            recordings_root: data.join("recordings"),
            teamspeak: Arc::clone(&teamspeak),
        });
        Some(api::spawn(&Config::normalize_listen(&cfg.public_listen), public_state)?)
    };
    let mut deny = vec![http_handle.addr];
    if let Some(p) = &public_handle {
        deny.push(p.addr);
    }
    engine.set_relay_policy(cfg.allow_private_relay, deny);
    api::watch_recordings(&admin_state);
    api::sync_all(&admin_state)?;
    println!("navad: stream {}", stream_handle.addr);
    println!("navad: control http://{}", http_handle.addr);
    println!("navad: public stream url {}", cfg.stream_public_url);
    if let Some(p) = &public_handle {
        println!("navad: public http://{} (no admin routes)", p.addr);
    }
    let (tx, rx) = mpsc::channel();
    let installed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&installed);
    let engine_stop = Arc::clone(&engine);
    let ts_stop = Arc::clone(&teamspeak);
    if ctrlc::set_handler(move || {
        if !flag.swap(true, Ordering::SeqCst) {
            engine_stop.begin_shutdown();
            ts_stop.begin_shutdown();
            let _ = tx.send(());
        }
    }).is_err() {
        println!("navad: signal handler unavailable, running until the process is killed");
        let _ = rx.recv();
    } else {
        let _ = rx.recv();
        println!("navad: shutting down");
    }
    engine.shutdown();
    teamspeak.shutdown();
    stream_handle.shutdown();
    http_handle.shutdown();
    if let Some(mut p) = public_handle.take() {
        p.shutdown();
    }
    Ok(())
}
