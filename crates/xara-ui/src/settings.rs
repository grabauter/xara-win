use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const FILE: &str = "settings";
const DRIVE: &str = "drive-db";
pub const KEYS: [&str; 8] = ["call", "hf", "fm", DRIVE, "capture", "playback", "ptt-lead-ms", "ptt-tail-ms"];
const MAX_VALUE: usize = 256;

pub type Pairs = Vec<(String, String)>;
pub type Validate = Box<dyn Fn(&Pairs) -> Result<(), String> + Send + Sync>;
pub type List = Box<dyn Fn() -> io::Result<Devices> + Send + Sync>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub desc: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Devices {
    pub backend: &'static str,
    pub capture: Vec<Device>,
    pub playback: Vec<Device>,
}

pub struct Settings {
    pub dir: Option<PathBuf>,
    pub values: Pairs,
    pub error: Option<String>,
    saved: Mutex<Pairs>,
    validate: Validate,
    list: List,
}

pub struct Init {
    pub dir: Option<PathBuf>,
    pub values: Pairs,
    pub saved: Pairs,
    pub error: Option<String>,
    pub validate: Validate,
    pub list: List,
}

impl Settings {
    pub fn new(i: Init) -> Self {
        Self {
            dir: i.dir,
            values: i.values,
            error: i.error,
            saved: Mutex::new(i.saved),
            validate: i.validate,
            list: i.list,
        }
    }

    pub fn saved(&self) -> Pairs {
        self.saved.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn devices(&self) -> io::Result<Devices> {
        (self.list)()
    }

    pub fn save(&self, changes: &Pairs) -> Result<Pairs, String> {
        let dir = self.dir.as_ref().ok_or("no state directory: start xara with --state-dir or as a systemd service")?;
        let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
        let next = merge(&saved, changes);
        (self.validate)(&next)?;
        store(dir, &next).map_err(|e| format!("{}: {e}", dir.join(FILE).display()))?;
        *saved = next.clone();
        Ok(next)
    }

    pub fn reset(&self) -> Result<(), String> {
        let dir = self.dir.as_ref().ok_or("no state directory")?;
        let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
        let keep: Pairs = saved.iter().filter(|(k, _)| k == DRIVE).cloned().collect();
        store(dir, &keep).map_err(|e| e.to_string())?;
        *saved = keep;
        Ok(())
    }
}

pub fn merge(base: &Pairs, changes: &Pairs) -> Pairs {
    let mut out: Pairs = base.iter().filter(|(k, _)| !changes.iter().any(|(c, _)| c == k)).cloned().collect();
    out.extend(changes.iter().filter(|(_, v)| !v.is_empty()).cloned());
    out.sort_by_key(|(k, _)| KEYS.iter().position(|x| x == k));
    out
}

pub struct Entry {
    pub line: usize,
    pub key: String,
    pub value: Option<String>,
}

pub fn entries(text: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for (i, l) in text.lines().enumerate() {
        let (line, l) = (i + 1, l.trim());
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let (key, value) = match l.split_once('=') {
            Some((k, v)) => (k.trim_end(), Some(v.trim_start().to_string())),
            None => (l, None),
        };
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Err(format!("line {line}: expected key = value"));
        }
        out.push(Entry { line, key: key.to_string(), value });
    }
    Ok(out)
}

pub(crate) fn parse(text: &str) -> Result<Pairs, String> {
    let mut out = Pairs::new();
    for Entry { line, key, value } in entries(text)? {
        add(&mut out, key, value.unwrap_or_default()).map_err(|e| format!("line {line}: {e}"))?;
    }
    Ok(out)
}

pub(crate) fn parse_save(body: &str) -> Result<Pairs, String> {
    let mut out = Pairs::new();
    for l in body.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (k, v) = l.split_once(char::is_whitespace).map_or((l, ""), |(k, v)| (k, v.trim()));
        add(&mut out, k.to_string(), v.to_string())?;
    }
    Ok(out)
}

fn add(out: &mut Pairs, k: String, v: String) -> Result<(), String> {
    if !KEYS.contains(&k.as_str()) {
        return Err(format!("unknown setting {k:?}"));
    }
    if out.iter().any(|(o, _)| *o == k) {
        return Err(format!("{k} given twice"));
    }
    if v.len() > MAX_VALUE || v.chars().any(char::is_control) {
        return Err(format!("bad value for {k}"));
    }
    if (k == "capture" || k == "playback") && !device_ok(&v) {
        return Err(format!("{k} {v:?}: not a device name (letters, digits and _.,:=@+- only; no file or tee plugin)"));
    }
    out.push((k, v));
    Ok(())
}

fn device_ok(v: &str) -> bool {
    v.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.,:=@+-".contains(&b))
        && !v.split([':', ',', '=']).any(|t| t == "file" || t == "tee")
}

pub(crate) fn render(kv: &Pairs) -> String {
    kv.iter().map(|(k, v)| format!("{k} = {v}\n")).collect()
}

pub fn load(dir: &Path) -> Result<Pairs, String> {
    match std::fs::read_to_string(dir.join(FILE)) {
        Ok(t) => parse(&t).map_err(|e| format!("{}: {e}", dir.join(FILE).display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Pairs::new()),
        Err(e) => Err(format!("{}: {e}", dir.join(FILE).display())),
    }
}

pub fn store(dir: &Path, kv: &Pairs) -> io::Result<()> {
    let tmp = dir.join(format!(".{FILE}.tmp"));
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o644);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(b"# written by xara; keys are xara(1) options without the leading --\n")?;
    f.write_all(render(kv).as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(p: &[(&str, &str)]) -> Pairs {
        p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn parse_and_render() {
        let p = parse("# c\n call = N0CALL \n\ncapture=plughw:CARD=Device,DEV=0\nplayback\n").unwrap();
        assert_eq!(p, kv(&[("call", "N0CALL"), ("capture", "plughw:CARD=Device,DEV=0"), ("playback", "")]));
        assert_eq!(parse(&render(&p)).unwrap(), p);
        for bad in
            ["radio = yes", "ptt = cm108:/dev/hidraw0", "call = A\ncall = B", "call = \u{7}", "call N0CALL", "= x"]
        {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse_save("call N0CALL\n capture  hw:1,0 \nplayback\n").unwrap(),
            parse(&render(&kv(&[("call", "N0CALL"), ("capture", "hw:1,0"), ("playback", "")]))).unwrap()
        );
        for bad in ["radio yes", "call A\ncall B", "capture file:x"] {
            assert!(parse_save(bad).is_err(), "{bad:?}");
        }
        for dev in [
            "plughw:CARD=Device,DEV=0",
            "hw:1,0",
            "null",
            "alsa_input.usb-Generic_USB_Audio-00.HiFi__Line__source",
            "xara_rx.monitor",
            "front:CARD=PCH,DEV=0",
        ] {
            assert!(parse(&format!("capture = {dev}\nplayback = {dev}")).is_ok(), "{dev}");
        }
        for dev in [
            "file:'|touch x'",
            "file:'\\174touch x'",
            "file:FILE=x",
            "file",
            "tee:null,x",
            "plug:tee:null,x",
            "plug:\"hw:1,0\"",
            "hw:1,0 x",
            "{type file}",
            "../x",
            "$HOME",
        ] {
            for k in ["capture", "playback"] {
                assert!(parse(&format!("{k} = {dev}")).is_err(), "{k} {dev}");
            }
        }
    }

    #[test]
    fn merge_replaces_removes_and_orders() {
        let base = kv(&[("drive-db", "-10"), ("call", "N0CALL"), ("capture", "a")]);
        let m = merge(&base, &kv(&[("capture", ""), ("playback", "b"), ("call", "N1CALL")]));
        assert_eq!(m, kv(&[("call", "N1CALL"), ("drive-db", "-10"), ("playback", "b")]));
    }

    #[test]
    fn store_load_save_reset() {
        let dir = std::env::temp_dir().join(format!("xara-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir).unwrap(), Pairs::new());
        let s = Settings::new(Init {
            dir: Some(dir.clone()),
            values: Pairs::new(),
            saved: kv(&[("drive-db", "-12")]),
            error: None,
            validate: Box::new(|p| if p.iter().any(|(_, v)| v == "bad") { Err("nope".into()) } else { Ok(()) }),
            list: Box::new(|| Ok(Devices::default())),
        });
        assert_eq!(s.save(&kv(&[("call", "bad")])), Err("nope".into()));
        assert_eq!(load(&dir).unwrap(), Pairs::new());
        s.save(&kv(&[("call", "N1CALL")])).unwrap();
        assert_eq!(load(&dir).unwrap(), kv(&[("call", "N1CALL"), ("drive-db", "-12")]));
        s.reset().unwrap();
        assert_eq!(load(&dir).unwrap(), kv(&[("drive-db", "-12")]));
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, dir.join(format!(".{FILE}.tmp"))).unwrap();
        // hard links on Windows reproduce the same trap without symlink privileges
        #[cfg(windows)]
        std::fs::hard_link(&victim, dir.join(format!(".{FILE}.tmp"))).unwrap();
        s.save(&kv(&[("call", "N2CALL")])).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
        assert!(!std::fs::symlink_metadata(dir.join(FILE)).unwrap().file_type().is_symlink());
        std::fs::write(dir.join(FILE), "call = N1CALL\nradio = yes\n").unwrap();
        assert!(load(&dir).unwrap_err().ends_with("line 2: unknown setting \"radio\""));
        std::fs::remove_dir_all(&dir).unwrap();
        let none = Settings::new(Init {
            dir: None,
            values: Pairs::new(),
            saved: Pairs::new(),
            error: None,
            validate: Box::new(|_| Ok(())),
            list: Box::new(|| Ok(Devices::default())),
        });
        assert!(none.save(&Pairs::new()).unwrap_err().contains("--state-dir"));
    }
}
