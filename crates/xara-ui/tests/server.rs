use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Instant;
use xara_host::response::{LinkWidth, Response};
use xara_testkit::json::Value;
use xara_ui::messages::Info;
use xara_ui::server::{Access, MAX_PAGES, MAX_STREAMS, TICK, TOKEN_FILE, UiServer, WS_IDLE, stored_token, valid_token};
use xara_ui::telemetry::{Control, Event, Telemetry, channel};
use xara_ui::ws;

mod common {
    pub mod ws;
}
use common::ws::{eventually, head_of, masked, message, open, open_raw, replayed, server_frame, until, upgrade};

const INFO: Info = Info { version: "test", mode: "hf", span_hz: 3000 };
const TOKEN: &str = "s3cret-token";

fn controlled() -> Access {
    Access { control: true, token: Some(TOKEN.into()), ..Access::default() }
}

fn cookie(addr: SocketAddr, token: &str) -> String {
    format!("\r\nCookie: theme=dark; xara_token_{}={token}", addr.port())
}

fn start() -> (Telemetry, SocketAddr) {
    start_with(Access::default())
}

fn start_with(access: Access) -> (Telemetry, SocketAddr) {
    let (t, feed) = channel(64);
    let ui = UiServer::bind("127.0.0.1", 0, feed, INFO, access).unwrap();
    (t, ui.addr)
}

fn get(addr: SocketAddr, head: &str) -> (String, String) {
    let mut s = open(addr, head);
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let (h, body) = out.split_once("\r\n\r\n").unwrap();
    (h.lines().next().unwrap().to_string(), body.to_string())
}

fn wait_controls(t: &Telemetry, n: usize) -> Vec<(u64, Control)> {
    let mut got = Vec::new();
    eventually("controls", || {
        got.extend(t.controls());
        got.len() >= n
    });
    got
}

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn kinds(c: &[(u64, Control)]) -> Vec<Control> {
    c.iter().map(|c| c.1).collect()
}

#[test]
fn static_files_and_errors() {
    let (_t, addr) = start();
    let (st, body) = get(addr, "GET / HTTP/1.1");
    assert_eq!(st, "HTTP/1.1 200 OK");
    assert!(body.contains("<title>xara</title>") && body.contains("app.js"));
    assert_eq!(get(addr, "GET /app.js HTTP/1.1").0, "HTTP/1.1 200 OK");
    assert_eq!(get(addr, "GET /style.css?v=2 HTTP/1.1").0, "HTTP/1.1 200 OK");
    assert_eq!(get(addr, "GET /etc/passwd HTTP/1.1").0, "HTTP/1.1 404 Not Found");
    assert_eq!(get(addr, "POST / HTTP/1.1").0, "HTTP/1.1 405 Method Not Allowed");
    assert_eq!(get(addr, "GET /ws HTTP/1.1").0, "HTTP/1.1 400 Bad Request");
}

#[test]
fn websocket_session() {
    let (t, addr) = start();
    let (mut s, h) = upgrade(addr, &format!("\r\nOrigin: http://{addr}"));
    assert!(h.starts_with("HTTP/1.1 101 "), "{h}");
    assert!(h.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
    let hello = message(&mut s);
    assert_eq!(
        (hello["t"].as_str(), hello["mode"].as_str(), hello["replay"].as_u64()),
        (Some("hello"), Some("hf"), Some(0))
    );
    assert_eq!(
        (
            hello["meter_floor"].as_f64(),
            hello["drive_min"].as_f64(),
            hello["tune_limit"].as_f64(),
            hello["span"].as_f64()
        ),
        (Some(-120.0), Some(-30.0), Some(20.0), Some(3000.0))
    );

    t.update(|s| {
        s.now = 96_000;
        s.ptt = true;
        s.link.state = "Sending".into();
        s.rx.feed(&[0.25; 480]);
    });
    let c = Response::Connected { caller: "A".into(), answerer: "B".into(), vias: vec![], width: LinkWidth::Hz(2300) };
    t.event(96_000, Event::Host(c));
    let m = until(&mut s, |m| m["t"] == "host");
    assert_eq!(
        (m["kind"].as_str(), m["line"].as_str(), m["ts"].as_f64()),
        (Some("connected"), Some("CONNECTED A B 2300"), Some(2.0))
    );
    let st = until(&mut s, |m| m["t"] == "status");
    assert_eq!(
        (st["ptt"].as_bool(), st["state"].as_str(), st["ts"].as_f64()),
        (Some(true), Some("Sending"), Some(2.0))
    );
    assert!((st["rx"]["rms"].as_f64().unwrap() + 12.04).abs() < 0.01);
    let next = until(&mut s, |m| m["t"] == "status");
    assert_eq!(next["rx"]["rms"].as_f64(), Some(-120.0), "meters reset after each tick");

    s.write_all(&masked(ws::PING, b"hi")).unwrap();
    let pong = loop {
        let f = server_frame(&mut s);
        if f.0 != ws::TEXT {
            break f;
        }
    };
    assert_eq!(pong, (ws::PONG, b"hi".to_vec()));
    s.write_all(&masked(ws::CLOSE, &[3, 232])).unwrap();
    let close = loop {
        let f = server_frame(&mut s);
        if f.0 != ws::TEXT {
            break f;
        }
    };
    assert_eq!(close, (ws::CLOSE, vec![3, 232]));
}

#[test]
fn cross_origin_websocket_refused() {
    let (_t, addr) = start();
    let (_, h) = upgrade(addr, "\r\nOrigin: http://evil.example");
    assert!(h.starts_with("HTTP/1.1 403 "), "{h}");
}

#[test]
fn history_replayed_to_late_clients() {
    let (t, addr) = start();
    for i in 0..3 {
        t.event(i * 480, Event::Trace(format!("line {i}")));
    }
    let (mut s, _) = replayed(addr, 3);
    for i in 0..3 {
        let m = message(&mut s);
        assert_eq!((m["t"].as_str(), m["text"].as_str()), (Some("trace"), Some(format!("line {i}").as_str())));
    }
}

#[test]
fn ndjson_stream_survives_departed_clients() {
    let (t, addr) = start();
    drop(open(addr, "GET /stream HTTP/1.1"));
    let mut s = open(addr, "GET /stream HTTP/1.1");
    assert!(head_of(&mut s).contains("Content-Type: application/x-ndjson"));
    let mut r = BufReader::new(s);
    let mut line = String::new();
    let mut next = || {
        line.clear();
        r.read_line(&mut line).unwrap();
        xara_testkit::json::from_str(&line).unwrap()
    };
    assert_eq!(next()["t"], "hello");
    t.event(0, Event::UnableToConnect);
    let mut kinds = Vec::new();
    while !kinds.contains(&"unable_to_connect".to_string()) {
        let m = next();
        if m["t"] == "notice" {
            kinds.push(m["kind"].as_str().unwrap().to_string());
        }
        assert!(kinds.len() < 5);
    }
}

#[test]
fn waterfall_subscription() {
    let (t, addr) = start();
    let (mut s, _) = upgrade(addr, "");
    message(&mut s);
    assert!(!t.wants_audio());
    s.write_all(&masked(ws::TEXT, b"WATERFALL ON")).unwrap();
    let head = until(&mut s, |m| m["t"] == "waterfall");
    assert_eq!(
        (head["row"].as_u64(), head["span"].as_f64(), head["bins"].as_u64(), head["floor"].as_f64()),
        (Some(1), Some(3000.0), Some(512), Some(-130.0))
    );
    eventually("audio request", || t.wants_audio());
    let tone: Vec<f64> =
        (0..480).map(|i| 0.5 * (2.0 * std::f64::consts::PI * 1500.0 * i as f64 / 48_000.0).sin()).collect();
    for _ in 0..20 {
        t.audio(&tone);
    }
    let row = loop {
        let (op, p) = server_frame(&mut s);
        if op == ws::BINARY {
            break p;
        }
    };
    assert_eq!((row[0], u16::from_le_bytes([row[2], row[3]]), row.len()), (1, 512, 516));
    let peak = row[4..].iter().enumerate().max_by_key(|(_, v)| **v).unwrap().0;
    assert_eq!(peak, 256);
    s.write_all(&masked(ws::TEXT, b"WATERFALL OFF")).unwrap();
    eventually("audio release after WATERFALL OFF", || !t.wants_audio());
}

#[test]
fn departed_subscriber_stops_audio() {
    let (t, addr) = start();
    let (mut s, _) = upgrade(addr, "");
    message(&mut s);
    s.write_all(&masked(ws::TEXT, b"WATERFALL ON")).unwrap();
    until(&mut s, |m| m["t"] == "waterfall");
    drop(s);
    eventually("audio release after the subscriber left", || !t.wants_audio());
}

#[test]
fn foreign_host_names_refused() {
    let (_t, addr) = start();
    let (st, body) = get(addr, "GET / HTTP/1.1\r\nHost: rebound.evil.example:8080");
    assert_eq!(st, "HTTP/1.1 403 Forbidden");
    assert!(body.contains("--ui-host"));
    let mut s = open_raw(addr, "GET / HTTP/1.1");
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 403 "));
    assert_eq!(get(addr, "GET / HTTP/1.1\r\nHost: localhost:8080").0, "HTTP/1.1 200 OK");
    let (_t, named) = start_with(Access { hosts: vec!["shack.lan".into()], ..Access::default() });
    assert_eq!(get(named, "GET / HTTP/1.1\r\nHost: shack.lan:8080").0, "HTTP/1.1 200 OK");
}

fn control_reply(s: &mut TcpStream) -> Value {
    until(s, |m| m["t"] == "control")
}

#[test]
fn controls_refused_unless_enabled() {
    let (t, addr) = start();
    let (mut s, _) = upgrade(addr, "");
    assert_eq!(message(&mut s)["control"].as_bool(), Some(false));
    s.write_all(&masked(ws::TEXT, b"TUNE ON")).unwrap();
    let r = control_reply(&mut s);
    assert_eq!((r["ok"].as_bool(), r["what"].as_str()), (Some(false), Some("tune on")));
    assert!(t.controls().is_empty());
}

#[test]
fn controls_reach_the_modem_when_enabled() {
    let (t, addr) = start_with(controlled());
    let (mut s, _) = upgrade(addr, &cookie(addr, TOKEN));
    assert_eq!(message(&mut s)["control"].as_bool(), Some(true));
    for c in ["TUNE ON", "DRIVE -12", "TUNE OFF", "ABORT", "MYCALL N0CALL"] {
        s.write_all(&masked(ws::TEXT, c.as_bytes())).unwrap();
    }
    let got = wait_controls(&t, 4);
    assert_eq!(kinds(&got), [Control::Tune(true), Control::Drive(-12.0), Control::Tune(false), Control::Abort]);
    assert!(got.iter().all(|c| c.0 == got[0].0));
}

#[test]
fn token_link_sets_a_cookie_and_redirects_without_the_token_or_refuses() {
    let (_t, addr) = start_with(controlled());
    let mut s = open(addr, &format!("GET /?token={TOKEN} HTTP/1.1"));
    let mut r = String::new();
    s.read_to_string(&mut r).unwrap();
    assert!(r.starts_with("HTTP/1.1 303 See Other\r\n") && r.contains("\r\nLocation: /\r\n"), "{r}");
    let set = format!("\r\nSet-Cookie: xara_token_{}={TOKEN}; Path=/; HttpOnly; SameSite=Strict\r\n", addr.port());
    assert!(r.contains(&set), "{r}");
    let mut s = open(addr, "GET /index.html?token=wrong-token1 HTTP/1.1");
    let mut r = String::new();
    s.read_to_string(&mut r).unwrap();
    assert!(r.starts_with("HTTP/1.1 403 Forbidden\r\n") && r.ends_with("wrong token\n"), "{r}");
    assert!(!r.contains("Set-Cookie") && !r.contains("Location"), "{r}");
    let (_t, off) = start();
    let mut s = open(off, &format!("GET /?token={TOKEN} HTTP/1.1"));
    let mut r = String::new();
    s.read_to_string(&mut r).unwrap();
    assert!(r.starts_with("HTTP/1.1 403 Forbidden\r\n") && r.ends_with("not enabled\n"), "{r}");
    assert!(!r.contains("Set-Cookie"), "{r}");
    assert_eq!(get(addr, "GET / HTTP/1.1").0, "HTTP/1.1 200 OK");
}

#[test]
fn controls_need_the_cookie_even_on_loopback() {
    let (t, addr) = start_with(controlled());
    let other_port = format!("\r\nCookie: xara_token_{}={TOKEN}", addr.port() ^ 1);
    for (path, host, extra) in [
        ("/ws", addr.to_string(), String::new()),
        ("/ws", addr.to_string(), format!("\r\nOrigin: http://{addr}")),
        (&*format!("/ws?token={TOKEN}"), addr.to_string(), String::new()),
        ("/ws", addr.to_string(), cookie(addr, "wrong-token1")),
        ("/ws", addr.to_string(), other_port),
        ("/ws", "rebound.local".to_string(), String::new()),
        ("/ws", format!("rebound.local:{}", addr.port()), format!("\r\nOrigin: http://rebound.local:{}", addr.port())),
    ] {
        let mut s = open_raw(
            addr,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13{extra}"
            ),
        );
        assert!(head_of(&mut s).starts_with("HTTP/1.1 101"), "{host} {extra}");
        let hello = message(&mut s);
        assert_eq!(
            (hello["control"].as_bool(), hello["locked"].as_bool()),
            (Some(false), Some(true)),
            "{host} {extra}"
        );
        s.write_all(&masked(ws::TEXT, b"TUNE ON")).unwrap();
        assert_eq!(control_reply(&mut s)["ok"].as_bool(), Some(false));
        s.write_all(&masked(ws::TEXT, b"SETTINGS ?")).unwrap();
        assert_eq!(until(&mut s, |m| m["t"] == "settings_result")["ok"].as_bool(), Some(false));
    }
    assert!(t.controls().is_empty());
    let (mut s, _) = upgrade(addr, &cookie(addr, TOKEN));
    let hello = message(&mut s);
    assert_eq!((hello["control"].as_bool(), hello["locked"].as_bool()), (Some(true), Some(false)));
    let (_t, viewer) = start();
    let (mut v, _) = upgrade(viewer, "");
    assert_eq!(message(&mut v)["locked"].as_bool(), Some(false));
}

#[test]
fn a_closed_or_silent_control_page_is_reported_gone() {
    let (t, addr) = start_with(controlled());
    let (mut s, _) = upgrade(addr, &cookie(addr, TOKEN));
    message(&mut s);
    s.write_all(&masked(ws::TEXT, b"TUNE ON")).unwrap();
    drop(s);
    let got = wait_controls(&t, 2);
    assert_eq!(kinds(&got), [Control::Tune(true), Control::Left]);
    assert_eq!(got[0].0, got[1].0);
    let (mut v, _) = upgrade(addr, "");
    message(&mut v);
    drop(v);
    let (mut s, _) = upgrade(addr, &cookie(addr, TOKEN));
    message(&mut s);
    s.write_all(&masked(ws::CLOSE, &[3, 232])).unwrap();
    assert_eq!(kinds(&wait_controls(&t, 1)), [Control::Left]);
    let (mut silent, _) = upgrade(addr, &cookie(addr, TOKEN));
    let t0 = Instant::now();
    let mut pings = 0;
    let mut b = [0u8; 2];
    while silent.read_exact(&mut b).is_ok() {
        pings += usize::from(b[0] & 0x0F == ws::PING);
        let mut p = vec![0u8; usize::from(b[1] & 0x7F)];
        if b[1] & 0x7F >= 126 {
            let mut l = vec![0u8; if b[1] & 0x7F == 126 { 2 } else { 8 }];
            silent.read_exact(&mut l).unwrap();
            p = vec![0u8; l.iter().fold(0usize, |n, &x| n << 8 | usize::from(x))];
        }
        if silent.read_exact(&mut p).is_err() {
            break;
        }
    }
    let gone = t0.elapsed();
    assert!(pings >= 2 && gone >= WS_IDLE - TICK && gone < WS_IDLE * 2, "{pings} pings, {gone:?}");
    assert_eq!(kinds(&wait_controls(&t, 1)), [Control::Left]);
}

#[test]
fn streams_and_viewers_cannot_take_the_control_slot() {
    let (_t, addr) = start_with(controlled());
    let stream = || {
        let s = open(addr, "GET /stream HTTP/1.1");
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 {
            if line.contains(r#""t":"hello""#) {
                return Some(s);
            }
            line.clear();
        }
        None
    };
    let streams: Vec<_> = (0..MAX_STREAMS).map(|_| stream().expect("stream refused")).collect();
    assert!(stream().is_none());
    let viewers: Vec<_> = (0..MAX_PAGES - 1)
        .map(|_| {
            let (mut s, _) = upgrade(addr, "");
            assert_eq!(message(&mut s)["t"].as_str(), Some("hello"));
            s
        })
        .collect();
    let (_, head) = upgrade(addr, "");
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    let (mut c, _) = upgrade(addr, &cookie(addr, TOKEN));
    assert_eq!(message(&mut c)["control"].as_bool(), Some(true));
    drop((streams, viewers));
}

#[cfg(unix)] // asserts POSIX token modes, which Windows has no equivalent of
#[test]
fn stored_token_is_private_and_stable() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = Scratch::new("token");
    let dir = &tmp.0;
    let t = stored_token(dir).unwrap();
    let path = dir.join(TOKEN_FILE);
    let mode = || std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert!(valid_token(&t) && mode() == 0o600);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(stored_token(dir).unwrap(), t);
    assert_eq!(mode(), 0o600);
    std::fs::write(&path, "bad\n").unwrap();
    assert_ne!(stored_token(dir).unwrap(), t);
}

fn settings_server(dir: &std::path::Path) -> (Telemetry, SocketAddr) {
    use xara_ui::settings::{Device, Devices, Init, Settings};
    let s = Settings::new(Init {
        dir: Some(dir.to_path_buf()),
        values: vec![("call".into(), "N0CALL".into()), ("capture".into(), "hw:0".into())],
        saved: vec![("drive-db".into(), "-9".into())],
        error: Some("saved settings failed: test".into()),
        validate: Box::new(|kv| match kv.iter().find(|(k, _)| k == "call") {
            Some((_, v)) if v == "BAD" => Err("bad call BAD".into()),
            _ => Ok(()),
        }),
        list: Box::new(|| {
            let d = |n: &str| Device { name: n.into(), desc: format!("{n} desc") };
            Ok(Devices { backend: "alsa", capture: vec![d("hw:0")], playback: vec![d("hw:0"), d("null")] })
        }),
    });
    start_with(Access { settings: Some(std::sync::Arc::new(s)), ..controlled() })
}

#[test]
fn settings_over_the_websocket() {
    let tmp = Scratch::new("settings");
    let dir = &tmp.0;
    let (t, addr) = settings_server(dir);
    let (mut s, _) = upgrade(addr, &cookie(addr, TOKEN));
    message(&mut s);
    s.write_all(&masked(ws::TEXT, b"SETTINGS ?")).unwrap();
    let m = until(&mut s, |m| m["t"] == "settings");
    assert_eq!(
        (m["available"].as_bool(), m["values"]["call"].as_str(), m["saved"]["drive-db"].as_str()),
        (Some(true), Some("N0CALL"), Some("-9"))
    );
    assert_eq!(m["error"].as_str(), Some("saved settings failed: test"));
    s.write_all(&masked(ws::TEXT, b"DEVICES ?")).unwrap();
    let d = until(&mut s, |m| m["t"] == "devices");
    assert_eq!((d["backend"].as_str(), d["playback"][1]["name"].as_str()), (Some("alsa"), Some("null")));

    s.write_all(&masked(ws::TEXT, b"SETTINGS SAVE\ncall BAD")).unwrap();
    let r = until(&mut s, |m| m["t"] == "settings_result");
    assert_eq!((r["ok"].as_bool(), r["text"].as_str()), (Some(false), Some("bad call BAD")));
    s.write_all(&masked(ws::TEXT, b"SETTINGS SAVE\nradio yes")).unwrap();
    assert_eq!(until(&mut s, |m| m["t"] == "settings_result")["ok"].as_bool(), Some(false));
    s.write_all(&masked(ws::TEXT, b"SETTINGS SAVE\ncall N1CALL\ncapture plughw:CARD=X,DEV=0")).unwrap();
    assert_eq!(until(&mut s, |m| m["t"] == "settings_result")["ok"].as_bool(), Some(true));
    let m = until(&mut s, |m| m["t"] == "settings");
    assert_eq!((m["saved"]["call"].as_str(), m["saved"]["drive-db"].as_str()), (Some("N1CALL"), Some("-9")));
    let file = std::fs::read_to_string(dir.join("settings")).unwrap();
    assert!(
        file.contains("call = N1CALL\n")
            && file.contains("drive-db = -9\n")
            && file.contains("capture = plughw:CARD=X,DEV=0\n")
    );

    s.write_all(&masked(ws::TEXT, b"SETTINGS RESET")).unwrap();
    assert_eq!(until(&mut s, |m| m["t"] == "settings_result")["ok"].as_bool(), Some(true));
    let file = std::fs::read_to_string(dir.join("settings")).unwrap();
    assert!(!file.contains("call") && file.contains("drive-db = -9"));

    s.write_all(&masked(ws::TEXT, b"RESTART")).unwrap();
    eventually("restart reaching the modem", || kinds(&t.controls()) == [Control::Restart]);
}

#[test]
fn settings_need_controls() {
    let (_t, addr) = start();
    let (mut s, _) = upgrade(addr, "");
    message(&mut s);
    s.write_all(&masked(ws::TEXT, b"SETTINGS ?")).unwrap();
    let r = until(&mut s, |m| m["t"] == "settings_result");
    assert_eq!((r["ok"].as_bool(), r["what"].as_str()), (Some(false), Some("settings")));
    let (_t, addr) = start_with(controlled());
    let (mut s, _) = upgrade(addr, &cookie(addr, TOKEN));
    message(&mut s);
    s.write_all(&masked(ws::TEXT, b"DEVICES ?")).unwrap();
    assert_eq!(until(&mut s, |m| m["t"] == "settings_result")["text"].as_str(), Some("settings are not available"));
}

#[test]
fn fragmented_commands_and_protocol_errors() {
    let (_t, addr) = start();
    let (mut s, _) = upgrade(addr, "");
    message(&mut s);
    let mut first = masked(ws::TEXT, b"SETTINGS");
    first[0] &= 0x7F;
    s.write_all(&[first, masked(ws::PING, b"p"), masked(0, b" ?")].concat()).unwrap();
    let (mut pong, mut answer) = (false, false);
    while !(pong && answer) {
        let (op, p) = server_frame(&mut s);
        pong |= (op, &p[..]) == (ws::PONG, b"p");
        answer |= op == ws::TEXT && xara_testkit::json::from_slice(&p).unwrap()["t"] == "settings_result";
    }
    s.write_all(&masked(0x3, b"")).unwrap();
    let close = loop {
        let f = server_frame(&mut s);
        if f.0 != ws::TEXT {
            break f;
        }
    };
    assert_eq!(close, (ws::CLOSE, 1002u16.to_be_bytes().to_vec()));
}
