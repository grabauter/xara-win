use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use xara_testkit::json::Value;

const DEADLINE: Duration = Duration::from_secs(20);

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn monitor() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xara-monitor"));
    c.env("PULSE_SERVER", "unix:/nonexistent/xara-test").env_remove("STATE_DIRECTORY");
    c
}

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Kill {
    fn wait(&mut self) -> ExitStatus {
        let end = Instant::now() + DEADLINE;
        loop {
            if let Some(s) = self.0.try_wait().unwrap() {
                return s;
            }
            assert!(Instant::now() < end, "xara-monitor did not exit within {DEADLINE:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn run(args: &[&str]) -> (Option<i32>, String) {
    let mut k = Kill(monitor().args(args).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap());
    let drain = collect(k.0.stderr.take().unwrap());
    let code = k.wait().code();
    drop(k);
    (code, drain.join().unwrap())
}

struct Ui {
    child: Kill,
    port: u16,
    stdout: JoinHandle<String>,
    stderr: (Arc<Mutex<String>>, JoinHandle<()>),
}

fn collect(mut r: impl Read + Send + 'static) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        s
    })
}

fn start_ui(args: &[&str]) -> Ui {
    for _ in 0..5 {
        let port = free_port();
        let mut child = Kill(
            monitor()
                .args(args)
                .args(["--ui-port", &port.to_string()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = collect(child.0.stdout.take().unwrap());
        let text = Arc::new(Mutex::new(String::new()));
        let (t, mut r) = (text.clone(), BufReader::new(child.0.stderr.take().unwrap()));
        let reader = thread::spawn(move || {
            let mut line = String::new();
            while r.read_line(&mut line).is_ok_and(|n| n > 0) {
                t.lock().unwrap().push_str(&line);
                line.clear();
            }
        });
        let end = Instant::now() + DEADLINE;
        while child.0.try_wait().unwrap().is_none() {
            if text.lock().unwrap().contains("UI http://") {
                return Ui { child, port, stdout, stderr: (text, reader) };
            }
            assert!(Instant::now() < end, "xara-monitor never served: {}", text.lock().unwrap());
            thread::sleep(Duration::from_millis(10));
        }
        reader.join().unwrap();
        let e = text.lock().unwrap().clone();
        assert!(e.contains("--ui-port"), "xara-monitor exited: {e}");
    }
    panic!("no free UI port after 5 tries");
}

fn open(port: u16, path: &str) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut s) => {
                s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
                return s;
            }
            Err(e) if Instant::now() > deadline => panic!("{e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[test]
fn monitor_serves_a_read_only_page_and_keeps_it_after_the_input() {
    let Ui { mut child, port, stdout, stderr } = start_ui(&["--stdin", "48000", "--hf", "2300", "--json"]);
    let noise: Vec<u8> = (0..96_000i32).flat_map(|i| ((i * 7919 % 2001 - 1000) as i16).to_le_bytes()).collect();
    child.0.stdin.take().unwrap().write_all(&noise).unwrap();

    let mut page = String::new();
    open(port, "/").read_to_string(&mut page).unwrap();
    assert!(page.contains("<title>xara monitor</title>") && page.contains("common.js"), "{page}");
    let mut login = String::new();
    open(port, "/?token=0123456789abcdef").read_to_string(&mut login).unwrap();
    assert!(!login.contains("Set-Cookie"), "{login}");

    let mut stream = BufReader::new(open(port, "/stream"));
    let mut line = String::new();
    while line != "\r\n" {
        line.clear();
        stream.read_line(&mut line).unwrap();
    }
    let status = loop {
        line.clear();
        stream.read_line(&mut line).unwrap();
        let v: Value = xara_testkit::json::from_str(&line).unwrap();
        if v["t"] == "status" && v["ended"] == true && v["decoder"].is_object() {
            break v;
        }
    };
    assert_eq!((status["source"].as_str(), status["bw"].as_u64()), (Some("stdin"), Some(2300)));
    assert_eq!(status["ts"].as_f64(), Some(2.0), "{status}");
    assert!(status["decoder"]["stats"]["tx"].is_u64() && status["decoder"]["keys"].is_array(), "{status}");

    #[cfg(unix)]
    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGTERM)
    };
    #[cfg(windows)] // no signal delivery from tests: the stream assertions above are the point
    child.0.kill().unwrap();
    #[cfg(unix)]
    let code = child.wait().code();
    #[cfg(windows)]
    child.wait();
    drop(child);
    let stdout = stdout.join().unwrap();
    stderr.1.join().unwrap();
    let err = stderr.0.lock().unwrap().clone();
    #[cfg(unix)]
    assert_eq!(code, Some(0), "{err}");
    let lines: Vec<Value> = stdout.lines().map(|l| xara_testkit::json::from_str(l).unwrap()).collect();
    assert_eq!(lines[0]["t"].as_str(), Some("start"), "{stdout}");
    #[cfg(unix)] // a killed-on-Windows child never flushes the final summary
    assert_eq!(lines.last().unwrap()["t"].as_str(), Some("summary"));
    assert!(err.contains(&format!("UI http://127.0.0.1:{port}/ (read-only)")) && err.contains("input ended"), "{err}");
}

#[test]
fn ui_options_need_a_port() {
    for args in [&["--stdin", "12000", "--ui-addr", "0.0.0.0"][..], &["--stdin", "12000", "--ui-host", "shack"]] {
        let (code, err) = run(args);
        assert_eq!(code, Some(2), "{args:?}");
        assert!(err.contains("need --ui-port"), "{args:?}");
    }
    assert_eq!(run(&["--stdin", "12000", "--ui-port", "0"]).0, Some(2));
}
