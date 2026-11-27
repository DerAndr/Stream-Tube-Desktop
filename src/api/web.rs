//! Browser sign-in: the YouTube website's own session cookies.
//!
//! The TV sign-in can read the account (feeds, history, likes), but YouTube no
//! longer lets it play videos, so nothing watched through it reaches the account.
//! With the website's cookies the app reports playback the way youtube.com does:
//! the video lands in the account's watch history and the position you stopped at
//! is what other devices resume from.
//!
//! Signing in happens in a real browser (Edge, Chrome, Brave or Chromium) started
//! with its own empty profile. The app reads that profile's youtube.com cookies over
//! a private DevTools pipe once sign-in is done, then closes the browser
//! and deletes the profile. Nothing else from the user's browsers is read.
use super::http;
use anyhow::{Context, Result, anyhow};
use futures::channel::oneshot;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

const WEB_VERSION: &str = "2.20251120.01.00";
const ORIGIN: &str = "https://www.youtube.com";
const SIGN_IN_URL: &str = "https://accounts.google.com/ServiceLogin?service=youtube&uilel=3&passive=true&continue=https%3A%2F%2Fwww.youtube.com%2Fsignin%3Faction_handle_signin%3Dtrue%26app%3Ddesktop%26next%3Dhttps%253A%252F%252Fwww.youtube.com%252F";

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct Session {
    cookies: BTreeMap<String, String>,
    account: Option<String>,
}

fn session() -> &'static Mutex<Option<Session>> {
    static S: OnceLock<Mutex<Option<Session>>> = OnceLock::new();
    S.get_or_init(|| {
        let s: Option<Session> = crate::store::kv_get("web_session").and_then(|s| serde_json::from_str(&s).ok());
        Mutex::new(s.filter(|s| has_login(&s.cookies)))
    })
}

#[cfg(target_os = "macos")]
static SAFARI_PLAYBACK_SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn playback_session() -> Option<Session> {
    #[cfg(target_os = "macos")]
    if let Some(safari) = SAFARI_PLAYBACK_SESSION.lock().as_ref() {
        return Some(safari.clone());
    }
    session().lock().clone()
}

fn playback_connected() -> bool {
    #[cfg(target_os = "macos")]
    if SAFARI_PLAYBACK_SESSION.lock().is_some() {
        return true;
    }
    connected()
}

fn save(s: &Option<Session>) {
    crate::store::kv_set("web_session", &s.as_ref().map(|s| serde_json::to_string(s).unwrap_or_default()).unwrap_or_default());
}

fn has_login(c: &BTreeMap<String, String>) -> bool {
    has_login_names(|name| c.contains_key(name))
}

fn has_login_names(has: impl Fn(&str) -> bool) -> bool {
    (has("SAPISID") || has("__Secure-3PAPISID")) && (has("SID") || has("__Secure-3PSID"))
}

/// Whether the browser sign-in is connected.
pub fn connected() -> bool {
    session().lock().is_some()
}

/// The account name the browser session belongs to.
pub fn account_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    if let Some(safari) = SAFARI_PLAYBACK_SESSION.lock().as_ref() {
        return safari.account.clone();
    }
    session().lock().as_ref().and_then(|s| s.account.clone())
}

pub fn disconnect() {
    #[cfg(target_os = "macos")]
    SAFARI_PLAYBACK_SESSION.lock().take();
    let mut s = session().lock();
    *s = None;
    save(&s);
}

/// Validate the candidate before replacing the current account session.
fn connect(cookies: BTreeMap<String, String>) -> Result<String> {
    let (candidate, name) = verify_session(cookies)?;
    #[cfg(target_os = "macos")]
    SAFARI_PLAYBACK_SESSION.lock().take();
    let mut current = session().lock();
    *current = Some(candidate);
    save(&current);
    Ok(name)
}

#[cfg(target_os = "macos")]
pub(super) fn connect_safari(cookies: &[Value], cancel: &AtomicBool) -> Result<String> {
    let (candidate, name) = verify_session(youtube_cookies(cookies))?;
    let mut current = SAFARI_PLAYBACK_SESSION.lock();
    if cancel.load(Ordering::Acquire) {
        return Err(anyhow!("Safari sign-in cancelled"));
    }
    // Keep the session in memory and separate from the signed-in browsing features.
    *current = Some(candidate);
    Ok(name)
}

fn verify_session(cookies: BTreeMap<String, String>) -> Result<(Session, String)> {
    if !has_login(&cookies) {
        return Err(anyhow!("These cookies aren't from a signed-in YouTube session."));
    }
    let mut candidate = Session { cookies, account: None };
    let name = fetch_account_name(&candidate).context("YouTube didn't accept the session")?;
    candidate.account = Some(name.clone());
    Ok((candidate, name))
}

fn cookie_header_for(s: &Session) -> String {
    s.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
}

/// Writes the browser sign-in's youtube.com cookies as a Netscape cookies.txt for
/// yt-dlp, in the private profile folder under a name of its own. The caller
/// deletes it when yt-dlp is done. None when the browser sign-in isn't connected.
pub fn write_cookies_file() -> Option<PathBuf> {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let text = {
        let s = session().lock();
        let s = s.as_ref()?;
        let mut t = String::from("# Netscape HTTP Cookie File\n");
        for (k, v) in &s.cookies {
            // domain, include subdomains, path, secure, expiry, name, value
            t.push_str(&format!(".youtube.com\tTRUE\t/\tTRUE\t2147483647\t{k}\t{v}\n"));
        }
        t
    };
    let path = crate::store::data_dir().join(format!("yt-dlp-cookies-{}-{}.txt", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(&path).ok()?.write_all(text.as_bytes()).ok()?;
    Some(path)
}

fn sha1_hex(s: &str) -> String {
    use sha1::{Digest, Sha1};
    Sha1::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The `Authorization` header youtube.com sends with its API calls.
fn sapisid_auth(s: &Session) -> Option<String> {
    let c = &s.cookies;
    let ts = chrono::Utc::now().timestamp();
    let parts: Vec<String> = [("SAPISIDHASH", "SAPISID"), ("SAPISID1PHASH", "__Secure-1PAPISID"), ("SAPISID3PHASH", "__Secure-3PAPISID")]
        .iter()
        .filter_map(|(scheme, name)| {
            let v = c.get(*name).or_else(|| if *name == "SAPISID" { c.get("__Secure-3PAPISID") } else { None })?;
            Some(format!("{scheme} {ts}_{}", sha1_hex(&format!("{ts} {v} {ORIGIN}"))))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// A call to the YouTube website's API as the signed-in user.
pub fn web_call(endpoint: &str, body: Value) -> Result<Value> {
    let signed_in = session().lock().clone().ok_or_else(|| anyhow!("browser sign-in not connected"))?;
    web_call_with(&signed_in, endpoint, body)
}

fn web_call_with(s: &Session, endpoint: &str, mut body: Value) -> Result<Value> {
    let cookie = cookie_header_for(s);
    let auth = sapisid_auth(s).ok_or_else(|| anyhow!("browser sign-in not connected"))?;
    let st = crate::store::settings_snapshot();
    body["context"] = json!({ "client": { "clientName": "WEB", "clientVersion": WEB_VERSION, "hl": st.language, "gl": st.region } });
    let (status, v) = http::post_json_status(
        &format!("https://www.youtube.com/youtubei/v1/{endpoint}?prettyPrint=false"),
        &body,
        &[
            ("Cookie", &cookie),
            ("Authorization", &auth),
            ("Origin", ORIGIN),
            ("X-Origin", ORIGIN),
            ("Referer", "https://www.youtube.com/"),
            ("X-Goog-AuthUser", "0"),
            ("X-Youtube-Bootstrap-Logged-In", "true"),
            ("X-Youtube-Client-Name", "1"),
            ("X-Youtube-Client-Version", WEB_VERSION),
        ],
    )?;
    if status == 401 || status == 403 {
        return Err(anyhow!("YouTube refused the browser session (HTTP {status}). Connect it again."));
    }
    if status >= 400 {
        return Err(anyhow!("HTTP {status}"));
    }
    Ok(v)
}

fn fetch_account_name(s: &Session) -> Result<String> {
    let r = web_call_with(s, "account/account_menu", json!({}))?;
    let header = crate::api::json::find(&r, "activeAccountHeaderRenderer").ok_or_else(|| anyhow!("not signed in"))?;
    let name = header.get("accountName").map(crate::api::json::text).filter(|s| !s.is_empty());
    let handle = header.get("channelHandle").map(crate::api::json::text).filter(|s| !s.is_empty());
    name.or(handle).ok_or_else(|| anyhow!("not signed in"))
}

/// Regular videos of a page, without the Shorts shelf (its items carry no channel or length).
fn long_videos(p: crate::api::types::Page) -> crate::api::types::Page {
    use crate::api::types::Item;
    let mut seen = std::collections::HashSet::new();
    crate::api::types::Page {
        items: p.items.into_iter().filter(|i| matches!(i, Item::Video(v) if !v.is_short && !v.channel_name.is_empty() && seen.insert(v.id.clone()))).collect(),
        continuation: p.continuation,
    }
}

/// The account's own home feed, the same one youtube.com shows.
pub fn home() -> Result<crate::api::types::Page> {
    let r = web_call("browse", json!({ "browseId": "FEwhat_to_watch" }))?;
    Ok(long_videos(crate::api::innertube::collect_items(&r)))
}

/// Next page of a feed loaded with the browser session.
pub fn browse_continue(token: &str) -> Result<crate::api::types::Page> {
    let r = web_call("browse", json!({ "continuation": token }))?;
    Ok(long_videos(crate::api::innertube::collect_items(&r)))
}

// ───────────── sign-in through a browser ─────────────

#[cfg(target_os = "macos")]
fn find_macos_browser(applications: &std::path::Path, user_applications: Option<&std::path::Path>) -> Option<(String, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;

    let browsers = [
        ("Google Chrome", "Google Chrome"),
        ("Microsoft Edge", "Microsoft Edge"),
        ("Brave", "Brave Browser"),
        ("Chromium", "Chromium"),
        ("Vivaldi", "Vivaldi"),
    ];
    browsers.into_iter().find_map(|(name, bundle)| {
        [Some(applications), user_applications].into_iter().flatten().find_map(|directory| {
            let executable = directory.join(format!("{bundle}.app")).join("Contents/MacOS").join(bundle);
            executable
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                .then(|| (name.to_string(), executable))
        })
    })
}

/// Chromium-based browsers the sign-in can use, most likely first.
pub(crate) fn find_browser() -> Option<(String, PathBuf)> {
    #[cfg(windows)]
    {
        let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
        let pf86 = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let candidates = [
            ("Google Chrome", format!(r"{pf}\Google\Chrome\Application\chrome.exe")),
            ("Google Chrome", format!(r"{pf86}\Google\Chrome\Application\chrome.exe")),
            ("Google Chrome", format!(r"{local}\Google\Chrome\Application\chrome.exe")),
            ("Microsoft Edge", format!(r"{pf86}\Microsoft\Edge\Application\msedge.exe")),
            ("Microsoft Edge", format!(r"{pf}\Microsoft\Edge\Application\msedge.exe")),
            ("Brave", format!(r"{pf}\BraveSoftware\Brave-Browser\Application\brave.exe")),
            ("Brave", format!(r"{local}\BraveSoftware\Brave-Browser\Application\brave.exe")),
            ("Chromium", format!(r"{local}\Chromium\Application\chrome.exe")),
        ];
        candidates.into_iter().map(|(n, p)| (n.to_string(), PathBuf::from(p))).find(|(_, p)| p.is_file())
    }
    #[cfg(not(windows))]
    {
        #[cfg(target_os = "macos")]
        {
            let user_applications = dirs::home_dir().map(|home| home.join("Applications"));
            if let Some(browser) = find_macos_browser(std::path::Path::new("/Applications"), user_applications.as_deref()) {
                return Some(browser);
            }
        }

        let names = [
            ("Google Chrome", "google-chrome"),
            ("Google Chrome", "google-chrome-stable"),
            ("Chromium", "chromium"),
            ("Chromium", "chromium-browser"),
            ("Brave", "brave-browser"),
            ("Brave", "brave"),
            ("Microsoft Edge", "microsoft-edge"),
            ("Microsoft Edge", "microsoft-edge-stable"),
            ("Vivaldi", "vivaldi"),
        ];
        let path = std::env::var_os("PATH")?;
        for (n, bin) in names {
            for dir in std::env::split_paths(&path) {
                let p = dir.join(bin);
                if p.is_file() {
                    return Some((n.to_string(), p));
                }
            }
        }
        None
    }
}

/// The sign-in browser's DevTools connection. It runs over a pair of pipes that
/// only this process holds (`--remote-debugging-pipe`), not a loopback port that
/// any local process could connect to and read the fresh Google cookies from.
struct Cdp {
    to_browser: std::io::PipeWriter,
    replies: mpsc::Receiver<Value>,
    next_id: i64,
}

impl Cdp {
    /// Adds the pipe flags to the browser's command line. Call `spawn` on the
    /// command, then `Cdp::started` with what this returns.
    fn attach(cmd: &mut std::process::Command) -> Result<(Self, ChildEnds)> {
        // the browser reads commands from the first pipe and writes replies to the second
        let (browser_in, to_browser) = std::io::pipe()?;
        let (from_browser, browser_out) = std::io::pipe()?;
        cmd.arg("--remote-debugging-pipe");
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::process::CommandExt;
            let (r, w) = (browser_in.as_raw_fd(), browser_out.as_raw_fd());
            unsafe extern "C" {
                fn dup(fd: i32) -> i32;
                fn dup2(from: i32, to: i32) -> i32;
                fn fcntl(fd: i32, cmd: i32, ...) -> i32;
            }
            const F_SETFD: i32 = 2;
            // SAFETY: only async-signal-safe calls between fork and exec. Chromium
            // expects its input on fd 3 and its output on fd 4.
            unsafe {
                cmd.pre_exec(move || {
                    let (mut r, mut w) = (r, w);
                    if w == 3 {
                        w = dup(w);
                    }
                    if r == 4 {
                        r = dup(r);
                    }
                    if r < 0 || w < 0 || dup2(r, 3) < 0 || dup2(w, 4) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // dup2 onto the same number keeps close-on-exec; clear it
                    fcntl(3, F_SETFD, 0);
                    fcntl(4, F_SETFD, 0);
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
            }
            const HANDLE_FLAG_INHERIT: u32 = 1;
            let (r, w) = (browser_in.as_raw_handle(), browser_out.as_raw_handle());
            // SAFETY: plain Win32 calls on handles this process owns. Only the
            // browser's ends become inheritable; ours stay private.
            unsafe {
                if SetHandleInformation(r, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0
                    || SetHandleInformation(w, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            cmd.arg(format!("--remote-debugging-io-pipes={},{}", r as usize, w as usize));
        }
        let (tx, replies) = mpsc::channel();
        std::thread::Builder::new().name("signin-devtools".into()).spawn(move || {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(from_browser);
            let mut buf = Vec::new();
            // messages are JSON, each ended by a NUL byte
            while reader.read_until(0, &mut buf).is_ok_and(|n| n > 0) {
                if buf.last() == Some(&0) {
                    buf.pop();
                }
                if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
                buf.clear();
            }
        })?;
        Ok((Self { to_browser, replies, next_id: 0 }, ChildEnds(browser_in, browser_out)))
    }

    fn call(&mut self, method: &str) -> Result<Value> {
        use std::io::Write;
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({ "id": id, "method": method }).to_string().into_bytes();
        msg.push(0);
        self.to_browser.write_all(&msg)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.checked_duration_since(Instant::now()).ok_or_else(|| anyhow!("devtools: no reply"))?;
            let v = self.replies.recv_timeout(left).map_err(|_| anyhow!("devtools: no reply"))?;
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return Ok(v);
            }
        }
    }

    /// Asks the browser for its cookies.
    fn cookies(&mut self) -> Result<Vec<Value>> {
        let v = self.call("Storage.getCookies")?;
        Ok(v.pointer("/result/cookies").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    fn close_browser(&mut self) {
        let _ = self.call("Browser.close");
    }
}

/// The browser's ends of the pipes. Dropped once it has started, so that only the
/// browser holds them.
struct ChildEnds(#[allow(dead_code)] std::io::PipeReader, #[allow(dead_code)] std::io::PipeWriter);

fn youtube_cookies(list: &[Value]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for c in list {
        let domain = c.get("domain").and_then(Value::as_str).unwrap_or("");
        if !(domain == "youtube.com" || domain.ends_with(".youtube.com")) {
            continue;
        }
        if let (Some(n), Some(v)) = (c.get("name").and_then(Value::as_str), c.get("value").and_then(Value::as_str)) {
            out.insert(n.to_string(), v.to_string());
        }
    }
    out
}

/// Opens the selected browser for YouTube sign-in and returns the account name.
/// Setting `cancel` stops waiting and closes the temporary browser window.
pub fn browser_sign_in(exe: PathBuf, cancel: Arc<AtomicBool>) -> Result<String> {
    let profile = crate::store::data_dir().join(format!("signin-browser-{:016x}", rand::random::<u64>()));
    let mut dir = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir.mode(0o700);
    }
    dir.create(&profile)?;
    let result = (|| {
        #[cfg(target_os = "macos")]
        sign_in_window(&exe, &profile, &cancel)?;
        let mut cmd = sign_in_command(&exe, &profile);
        #[cfg(target_os = "macos")]
        cmd.args(["--headless=new", "about:blank"]);
        #[cfg(not(target_os = "macos"))]
        cmd.arg(SIGN_IN_URL);
        let (mut cdp, browser_ends) = Cdp::attach(&mut cmd)?;
        let mut child = cmd.spawn().context("couldn't start the browser")?;
        drop(browser_ends);
        let started = Instant::now();
        let timeout = if cfg!(target_os = "macos") { 20 } else { 20 * 60 };
        let result = loop {
            std::thread::sleep(Duration::from_millis(1500));
            if cancel.load(Ordering::Relaxed) {
                break Err(anyhow!("cancelled"));
            }
            if let Ok(Some(_)) = child.try_wait() {
                break Err(anyhow!("The browser was closed before sign-in finished."));
            }
            if started.elapsed() > Duration::from_secs(timeout) {
                break Err(anyhow!("Sign-in timed out."));
            }
            let Ok(list) = cdp.cookies() else { continue };
            let cookies = youtube_cookies(&list);
            if has_login(&cookies) && cookies.contains_key("LOGIN_INFO") {
                break Ok(cookies);
            }
        };
        cdp.close_browser();
        wait_for_browser_exit(&mut child);
        result
    })();
    for _ in 0..8 {
        if std::fs::remove_dir_all(&profile).is_ok() || !profile.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(anyhow!("cancelled"));
    }
    connect(result?)
}

fn sign_in_command(exe: &std::path::Path, profile: &std::path::Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--disable-sync", "--new-window"]);
    if let Some(p) = crate::store::proxy_setting() {
        cmd.arg(format!("--proxy-server={p}"));
    }
    cmd
}

fn wait_for_browser_exit(child: &mut std::process::Child) {
    for _ in 0..20 {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "macos")]
fn sign_in_window(exe: &std::path::Path, profile: &std::path::Path, cancel: &AtomicBool) -> Result<()> {
    // Google rejects sign-in in a debug-controlled browser. Keep the human login
    // separate from the later cookie read, in the same app-owned temporary profile.
    let mut child = sign_in_command(exe, profile).arg(SIGN_IN_URL).spawn().context("couldn't start the browser")?;
    let started = Instant::now();
    let result = loop {
        std::thread::sleep(Duration::from_millis(1500));
        if cancel.load(Ordering::Relaxed) {
            break Err(anyhow!("cancelled"));
        }
        if profile_has_login(profile) {
            break Ok(());
        }
        if let Ok(Some(_)) = child.try_wait() {
            break Err(anyhow!("The browser was closed before sign-in finished."));
        }
        if started.elapsed() > Duration::from_secs(20 * 60) {
            break Err(anyhow!("Sign-in timed out."));
        }
    };
    if !matches!(child.try_wait(), Ok(Some(_))) {
        // Chrome flushes its cookie store on SIGTERM. Only signal our own child.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    }
    wait_for_browser_exit(&mut child);
    result
}

#[cfg(target_os = "macos")]
fn profile_has_login(profile: &std::path::Path) -> bool {
    ["Default/Cookies", "Default/Network/Cookies"].iter().any(|file| {
        let Ok(db) = rusqlite::Connection::open_with_flags(profile.join(file), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return false };
        let Ok(mut query) = db.prepare("SELECT name FROM cookies WHERE host_key IN ('.youtube.com', 'youtube.com')") else { return false };
        let Ok(rows) = query.query_map([], |r| r.get::<_, String>(0)) else { return false };
        let names: Vec<String> = rows.filter_map(Result::ok).collect();
        let has = |name: &str| names.iter().any(|n| n == name);
        has("LOGIN_INFO") && has_login_names(has)
    })
}

#[cfg(all(test, target_os = "macos"))]
mod sign_in_tests {
    use super::*;

    #[test]
    fn waits_for_youtube_login_in_the_temporary_profile() {
        let profile = std::env::temp_dir().join(format!("streamtube-signin-test-{:016x}", rand::random::<u64>()));
        assert!(!profile_has_login(&profile));
        for file in ["Default/Cookies", "Default/Network/Cookies"] {
            let path = profile.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute("CREATE TABLE cookies (host_key TEXT, name TEXT)", []).unwrap();
            for host in [".google.com", ".youtube.com.evil.example"] {
                for name in ["LOGIN_INFO", "SID", "SAPISID"] {
                    db.execute("INSERT INTO cookies VALUES (?1, ?2)", [host, name]).unwrap();
                }
            }
            assert!(!profile_has_login(&profile), "other origins are not a YouTube login");
            for name in ["LOGIN_INFO", "__Secure-3PSID"] {
                db.execute("INSERT INTO cookies VALUES ('.youtube.com', ?1)", [name]).unwrap();
            }
            assert!(!profile_has_login(&profile), "the session is incomplete");
            db.execute("INSERT INTO cookies VALUES ('.youtube.com', '__Secure-3PAPISID')", []).unwrap();
            assert!(profile_has_login(&profile));
            drop(db);
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir_all(profile).unwrap();
    }
}

/// Reads a Netscape-format cookies.txt (as saved by "Get cookies.txt" style
/// browser extensions) and connects with its youtube.com cookies.
pub fn import_cookies_txt(path: &std::path::Path) -> Result<String> {
    let text = std::fs::read_to_string(path)?;
    let mut cookies = BTreeMap::new();
    for line in text.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 7 {
            continue;
        }
        let domain = f[0].trim_start_matches('.');
        if domain == "youtube.com" || domain.ends_with(".youtube.com") {
            cookies.insert(f[5].to_string(), f[6].trim_end_matches('\r').to_string());
        }
    }
    if cookies.is_empty() {
        return Err(anyhow!("No youtube.com cookies in that file."));
    }
    connect(cookies)
}

// ───────────── reporting what is watched ─────────────

fn sts() -> Option<u64> {
    static STS: Mutex<Option<(u64, Instant)>> = Mutex::new(None);
    if let Some((v, at)) = *STS.lock() {
        if at.elapsed() < Duration::from_secs(6 * 3600) {
            return Some(v);
        }
    }
    let api = String::from_utf8(http::get_bytes("https://www.youtube.com/iframe_api", &[]).ok()?).ok()?;
    let id: String = api
        .split("/s/player/")
        .nth(1)
        .or_else(|| api.split(r"player\/").nth(1))?
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    let js = String::from_utf8(http::get_bytes(&format!("https://www.youtube.com/s/player/{id}/player_ias.vflset/en_US/base.js"), &[]).ok()?).ok()?;
    let v: u64 = ["signatureTimestamp:", "sts:"]
        .iter()
        .find_map(|k| js.split(k).nth(1).and_then(|t| t.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()))?;
    *STS.lock() = Some((v, Instant::now()));
    Some(v)
}

/// Use the same origin as the signed-in YouTube website and preserve its tracking query.
fn first_party_stats_url(base: &str, endpoint: &str) -> Result<String> {
    let suffix = base
        .strip_prefix("https://s.youtube.com/")
        .or_else(|| base.strip_prefix("https://www.youtube.com/"))
        .ok_or_else(|| anyhow!("unexpected playback stats origin"))?;
    let expected = format!("api/stats/{endpoint}");
    if suffix.split('?').next() != Some(expected.as_str()) || suffix.contains('#') {
        return Err(anyhow!("unexpected playback stats endpoint"));
    }
    Ok(format!("{ORIGIN}/{suffix}"))
}

fn stats_url(base: &str, fields: Vec<(&str, String)>) -> String {
    let (path, query) = base.split_once('?').unwrap_or((base, ""));
    let final_report = fields.iter().any(|(key, value)| *key == "final" && value == "1");
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| {
            let (key, value) = kv.split_once('=').unwrap_or((kv, ""));
            // Safari's final watch-time report has no scheduled follow-up.
            if final_report && matches!(key, "rtn" | "rti") {
                return None;
            }
            Some((key.to_string(), value.to_string()))
        })
        .collect();
    for (key, value) in fields {
        match pairs.iter_mut().find(|(current, _)| current == key) {
            Some(pair) => pair.1 = value,
            None => pairs.push((key.to_string(), value)),
        }
    }
    format!("{path}?{}", pairs.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join("&"))
}

fn watch_diagnostics_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("STREAMTUBE_WATCH_HISTORY_DIAGNOSTICS").is_some_and(|v| v == "1"))
}

// Log only the allowed fields in the request as sent, with their values validated.
fn watch_report_summary(url: &str, status: Option<u16>) -> String {
    use std::fmt::Write;

    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let phase = if path.ends_with("/watchtime") { "watchtime" } else { "playback" };
    let mut summary = match status {
        Some(code) => format!("{phase} http={code}"),
        None => format!("{phase} http=unavailable"),
    };
    for (key, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
        let allowed = match key {
            "cmt" | "st" | "et" | "len" | "rt" | "rtn" | "rti" => value.parse::<f64>().is_ok_and(f64::is_finite),
            "state" => matches!(value, "playing" | "paused"),
            "final" => value == "1",
            _ => false,
        };
        if allowed {
            let _ = write!(summary, " {key}={value}");
        }
    }
    summary
}

struct Watch {
    id: String,
    playback: String,
    watchtime: String,
    cpn: String,
    len: f64,
    started: Instant,
    last_ping: Instant,
    last_cmt: f64,
    pings: u32,
    playing: bool,
}

impl Watch {
    fn begin(id: &str, len: f64, start: f64) -> Result<Self> {
        let cpn: String = {
            use rand::Rng;
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut rng = rand::rng();
            (0..16).map(|_| A[rng.random_range(0..A.len())] as char).collect()
        };
        let mut body = json!({ "videoId": id, "cpn": cpn, "racyCheckOk": true, "contentCheckOk": true });
        if let Some(sts) = sts() {
            body["playbackContext"] = json!({ "contentPlaybackContext": { "signatureTimestamp": sts, "html5Preference": "HTML5_PREF_WANTS" } });
        }
        let session = playback_session().ok_or_else(|| anyhow!("watch history is disconnected"))?;
        let r = web_call_with(&session, "player", body)?;
        let tracking_url = |key: &str, endpoint: &str| {
            let url = r.pointer(&format!("/playbackTracking/{key}/baseUrl")).and_then(Value::as_str)
                .ok_or_else(|| anyhow!("no {endpoint} tracking"))?;
            first_party_stats_url(url, endpoint)
        };
        let playback = tracking_url("videostatsPlaybackUrl", "playback")?;
        let watchtime = tracking_url("videostatsWatchtimeUrl", "watchtime")?;
        let len = r
            .pointer("/videoDetails/lengthSeconds")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .filter(|l: &f64| *l > 0.0)
            .unwrap_or(len);
        let start = start.max(0.0);
        let w = Watch { id: id.to_string(), playback, watchtime, cpn, len, started: Instant::now(), last_ping: Instant::now(), last_cmt: start, pings: 0, playing: true };
        w.get(&w.playback, &[("cmt", format!("{start:.3}")), ("rtn", "0".into()), ("state", "playing".into())])?;
        Ok(w)
    }

    /// Sends a stats ping: the base URL from the player response with the fields
    /// youtube.com's own player adds. Fields already in the base URL are replaced,
    /// never repeated.
    fn get(&self, base: &str, fields: &[(&str, String)]) -> Result<()> {
        let rt = self.started.elapsed().as_secs_f64();
        let mut add: Vec<(&str, String)> = vec![
            ("ver", "2".into()),
            ("cpn", self.cpn.clone()),
            ("c", "WEB".into()),
            ("cver", WEB_VERSION.into()),
            ("cplayer", "UNIPLAYER".into()),
            ("cbr", "Chrome".into()),
            ("cbrver", "140.0.0.0".into()),
            ("cos", "Windows".into()),
            ("cosver", "10.0".into()),
            ("cplatform", "DESKTOP".into()),
            ("fs", "0".into()),
            ("volume", "100".into()),
            ("muted", "0".into()),
            ("lact", "1200".into()),
            ("len", format!("{:.3}", self.len)),
            ("rt", format!("{rt:.3}")),
        ];
        add.extend(fields.iter().cloned());
        let url = stats_url(base, add);
        let session = playback_session().ok_or_else(|| anyhow!("watch history is disconnected"))?;
        let cookie = cookie_header_for(&session);
        let referer = format!("https://www.youtube.com/watch?v={}", self.id);
        let status = http::get_status_without_redirects(
            &url,
            &[("Cookie", &cookie), ("Referer", &referer), ("Origin", ORIGIN), ("Cache-Control", "no-cache")],
        );
        if watch_diagnostics_enabled() {
            eprintln!("Watch history: {}", watch_report_summary(&url, status.as_ref().ok().copied()));
        }
        let status = status.map_err(|_| anyhow!("playback stats transport failed"))?;
        if !(200..300).contains(&status) {
            return Err(anyhow!("playback stats HTTP {status}"));
        }
        Ok(())
    }

    fn watchtime_fields(&self, cmt: f64, playing: bool, last: bool) -> Vec<(&'static str, String)> {
        let st = if cmt >= self.last_cmt && cmt - self.last_cmt < 120.0 { self.last_cmt } else { cmt };
        let mut fields = vec![
            ("cmt", format!("{cmt:.3}")),
            ("st", format!("{st:.3}")),
            ("et", format!("{cmt:.3}")),
            ("state", if playing { "playing" } else { "paused" }.to_string()),
            ("idpj", "-1".into()),
            ("ldpj", "-1".into()),
        ];
        if last {
            fields.push(("final", "1".into()));
        } else {
            let elapsed = format!("{:.0}", self.started.elapsed().as_secs_f64().max(1.0));
            fields.push(("rtn", elapsed.clone()));
            fields.push(("rti", elapsed));
        }
        fields
    }

    /// Reports the stretch watched since the last report, ending at `cmt`.
    fn report(&mut self, cmt: f64, playing: bool, last: bool) {
        let fields = self.watchtime_fields(cmt, playing, last);
        let result = self.get(&self.watchtime, &fields);
        self.last_ping = Instant::now();
        if result.is_ok() {
            self.last_cmt = cmt;
            self.pings += 1;
            self.playing = playing;
        }
    }

    /// youtube.com reports at 10, 20 and 30 seconds, then every 40 seconds.
    fn due(&self) -> bool {
        let every = if self.pings < 3 { 10 } else { 40 };
        self.last_ping.elapsed() >= Duration::from_secs(every)
    }
}

enum Event {
    Start { id: String, len: f64, start: f64 },
    Progress { id: String, t: f64, playing: bool },
    Stop { id: String, t: f64 },
    Flush(oneshot::Sender<()>),
}

static REPORTER: OnceLock<Mutex<mpsc::Sender<Event>>> = OnceLock::new();

fn reporter() -> &'static Mutex<mpsc::Sender<Event>> {
    REPORTER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Event>();
        std::thread::Builder::new()
            .name("watch-report".into())
            .spawn(move || {
                let mut cur: Option<Watch> = None;
                while let Ok(ev) = rx.recv() {
                    match ev {
                        Event::Start { id, len, start } => {
                            if let Some(mut w) = cur.take() {
                                let t = w.last_cmt;
                                w.report(t, false, true);
                            }
                            cur = Watch::begin(&id, len, start).ok();
                            if cur.is_none() && watch_diagnostics_enabled() {
                                eprintln!("Watch history: playback tracking did not start");
                            }
                        }
                        Event::Progress { id, t, playing } => {
                            if let Some(w) = cur.as_mut().filter(|w| w.id == id) {
                                if w.due() || playing != w.playing {
                                    w.report(t, playing, false);
                                }
                            }
                        }
                        Event::Stop { id, t } => {
                            if let Some(mut w) = cur.take() {
                                if w.id == id {
                                    w.report(t, false, true);
                                } else {
                                    cur = Some(w);
                                }
                            }
                        }
                        Event::Flush(flushed) => {
                            let _ = flushed.send(());
                        }
                    }
                }
            })
            .ok();
        Mutex::new(tx)
    })
}

/// A video started playing.
pub fn report_start(id: &str, len: f64, start: f64) {
    if playback_connected() && crate::store::settings_snapshot().history_enabled {
        let _ = reporter().lock().send(Event::Start { id: id.to_string(), len, start });
    }
}

/// Playback position update; cheap to call often.
pub fn report_progress(id: &str, t: f64, playing: bool) {
    send_position_report(|| Event::Progress { id: id.to_string(), t, playing });
}

/// The video was closed or replaced at position `t`.
pub fn report_stop(id: &str, t: f64) {
    send_position_report(|| Event::Stop { id: id.to_string(), t });
}

/// Wait for requests already queued, including a final report from a recently
/// closed player. There is nothing to flush until the worker has started.
pub(crate) fn flush_watch_reports() -> Option<oneshot::Receiver<()>> {
    let (flushed, done) = oneshot::channel();
    #[cfg(test)]
    if let Some(sender) = TEST_POSITION_REPORTS.with(|reports| reports.borrow().clone()) {
        sender.send(Event::Flush(flushed)).ok()?;
        return Some(done);
    }
    REPORTER.get()?.lock().send(Event::Flush(flushed)).ok()?;
    Some(done)
}

#[cfg(test)]
thread_local! {
    static TEST_POSITION_REPORTS: std::cell::RefCell<Option<mpsc::Sender<Event>>> = const { std::cell::RefCell::new(None) };
}

fn send_position_report(event: impl FnOnce() -> Event) {
    #[cfg(test)]
    if let Some(sender) = TEST_POSITION_REPORTS.with(|reports| reports.borrow().clone()) {
        let _ = sender.send(event());
        return;
    }
    if playback_connected() {
        let _ = reporter().lock().send(event());
    }
}

#[cfg(test)]
mod playback_tests {
    use super::{Event, REPORTER, TEST_POSITION_REPORTS, Watch, oneshot, stats_url, watch_report_summary};
    use crate::PlatformQuitRequested;
    use crate::api::types::VideoItem;
    use crate::player::controller;
    use crate::ui::{Pal, app::UiState, pip};
    use gpui::{Empty, TestAppContext, WindowHandle};
    use std::sync::mpsc::{Receiver, TryRecvError};
    use std::time::Duration;

    #[test]
    fn watch_report_summary_uses_only_allowlisted_fields_and_status() {
        let request = "https://www.youtube.com/api/stats/watchtime?docid=video&cpn=session&ei=token&cmt=82.125&st=80&et=82.125&rt=83.200&len=464&state=paused&final=1";
        assert_eq!(
            watch_report_summary(request, Some(204)),
            "watchtime http=204 cmt=82.125 st=80 et=82.125 rt=83.200 len=464 state=paused final=1",
        );
        let invalid = "https://www.youtube.com/api/stats/playback?cmt=%53ecret&len=NaN&rti=inf&state=unknown&final=token&rtn=10";
        assert_eq!(watch_report_summary(invalid, None), "playback http=unavailable rtn=10");
    }

    #[test]
    fn final_watchtime_url_keeps_the_position_and_omits_the_schedule() {
        let now = std::time::Instant::now();
        let watch = Watch {
            id: String::new(),
            playback: String::new(),
            watchtime: "https://www.youtube.com/api/stats/watchtime?docid=synthetic&rtn=9&rti=8".into(),
            cpn: String::new(),
            len: 464.0,
            started: now,
            last_ping: now,
            last_cmt: 80.0,
            pings: 1,
            playing: true,
        };
        for last in [false, true] {
            let request = stats_url(&watch.watchtime, watch.watchtime_fields(82.125, false, last));
            let (_, query) = request.split_once('?').unwrap();
            let fields: std::collections::BTreeMap<_, _> =
                query.split('&').map(|pair| pair.split_once('=').unwrap()).collect();
            assert_eq!(fields.get("cmt"), Some(&"82.125"));
            assert_eq!(fields.get("st"), Some(&"80.000"));
            assert_eq!(fields.get("et"), Some(&"82.125"));
            assert_eq!(fields.get("docid"), Some(&"synthetic"));
            assert_eq!(fields.get("state"), Some(&"paused"));
            assert_eq!(fields.get("final"), last.then_some(&"1"));
            assert_eq!(fields.contains_key("rtn"), !last);
            assert_eq!(fields.contains_key("rti"), !last);
        }
    }

    #[gpui::test]
    fn closing_player_only_enqueues_the_final_position(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-close";
        let (sender, reports) = std::sync::mpsc::channel();
        TEST_POSITION_REPORTS.with(|slot| slot.replace(Some(sender)));
        cx.update(|cx| {
            controller::init(cx);
            controller::player(cx).update(cx, |player, cx| {
                player.video = Some(VideoItem { id: video_id.into(), ..Default::default() });
                player.time = 82.125;
                player.duration = 464.0;
                player.set_paused(true);
                player.stop(cx);
            });
        });
        TEST_POSITION_REPORTS.with(|slot| slot.take());

        let events: Vec<_> = reports.try_iter().collect();
        assert!(matches!(events.as_slice(), [
            Event::Progress { id: paused_id, t: 82.125, playing: false },
            Event::Stop { id: closed_id, t: 82.125 },
        ] if paused_id == video_id && closed_id == video_id));
    }

    #[gpui::test]
    fn app_quit_enqueues_one_final_position_before_flushing(cx: &mut TestAppContext) {
        let (sender, reports) = std::sync::mpsc::channel();
        TEST_POSITION_REPORTS.with(|slot| slot.replace(Some(sender)));
        cx.update(|cx| {
            controller::init(cx);
            controller::player(cx).update(cx, |player, _| {
                player.video = Some(VideoItem { id: "test-watch-history-quit".into(), ..Default::default() });
                player.time = 84.625;
                player.duration = 464.0;
            });
            crate::request_quit(cx);
            crate::request_quit(cx);
        });
        TEST_POSITION_REPORTS.with(|slot| slot.take());

        assert!(matches!(reports.recv().unwrap(),
            Event::Stop { id, t: 84.625 } if id == "test-watch-history-quit"));
        let Event::Flush(flushed) = reports.recv().unwrap() else {
            panic!("quit must wait for the reporter after enqueuing the final position");
        };
        assert!(reports.try_recv().is_err());
        flushed.send(()).unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    fn closing_last_main_window_enqueues_one_final_position_before_flushing(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-last-main-window";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        let other = cx.add_window(|_, _| Empty);
        // GPUI's native close callback also goes through Window::remove_window.
        other.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        assert!(matches!(reports.try_recv(), Err(TryRecvError::Empty)),
            "closing a non-final window must leave reporting active");

        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        assert!(cx.windows().is_empty());
        assert_final_report_before_flush(cx, &reports, video_id);
    }

    #[gpui::test]
    fn closing_last_main_window_with_pip_open_flushes_the_final_position(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-last-main-window-with-pip";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        cx.update(pip::open);
        let panel = cx.read(pip::handle).expect("PiP window opened");
        assert!(cx.windows().contains(&panel));
        assert!(cx.read(|cx| cx.try_global::<crate::QuitRequested>().is_none()));

        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        assert!(main.root(cx).is_err(), "the main window must be closed before the flush");
        assert_final_report_before_flush(cx, &reports, video_id);
    }

    #[gpui::test]
    fn closing_pip_alone_keeps_the_main_window_and_playback_active(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-close-pip-only";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        cx.update(pip::open);
        let panel = cx.read(pip::handle).expect("PiP window opened");

        panel.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        assert_eq!(cx.windows(), [main.into()]);
        assert!(cx.read(pip::handle).is_none());
        cx.read(|cx| {
            assert!(cx.try_global::<crate::QuitRequested>().is_none());
            let player = controller::player(cx);
            let player = player.read(cx);
            assert_eq!(
                player.video.as_ref().map(|video| video.id.as_str()),
                Some(video_id)
            );
            assert!(!player.paused);
            assert_eq!(player.time, SHUTDOWN_TEST_POSITION);
        });
        assert!(
            reports.try_iter().all(|event| !matches!(event, Event::Stop { .. } | Event::Flush(_))),
            "closing PiP alone must leave reporting active"
        );
    }

    #[gpui::test]
    fn shutdown_waits_for_watch_history_flush_acknowledgement(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-shutdown-ack";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        let flushed = take_final_report_and_flush(&reports, video_id);
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()));

        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "platform exit must wait while the flush is pending");

        flushed.send(()).unwrap();
        cx.run_until_parked();
        assert!(cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "the flush acknowledgement must release platform exit before the deadline");
    }

    #[gpui::test]
    fn shutdown_requests_exit_when_watch_history_reporter_disconnects(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-shutdown-disconnect";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        let flushed = take_final_report_and_flush(&reports, video_id);
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "platform exit must wait while the flush is pending");

        drop(reports);
        drop(flushed);
        // Process the channel closure without advancing the shutdown deadline.
        cx.run_until_parked();
        assert!(cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "closing the flush acknowledgement channel must release platform exit immediately");
    }

    #[gpui::test]
    fn shutdown_requests_exit_at_three_seconds_if_watch_history_flush_stalls(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-shutdown-timeout";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        let flushed = take_final_report_and_flush(&reports, video_id);

        cx.executor().advance_clock(Duration::from_millis(2_999));
        cx.run_until_parked();
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "platform exit must wait until the full three seconds have elapsed");

        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "platform exit must proceed at the deadline with no acknowledgement");
        assert!(flushed.is_canceled(), "the timeout must stop waiting for the reporter");
    }

    #[gpui::test]
    fn closing_last_window_with_disconnected_reporter_requests_immediate_exit(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-queue-disconnected-before-close";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        drop(reports);
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()));

        main.update(cx, |_, window, _| window.remove_window()).unwrap();

        assert!(cx.windows().is_empty());
        assert!(cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "a failed flush enqueue must request platform exit synchronously");

        // Let any spawned work register its timers without advancing the test clock.
        cx.run_until_parked();
        assert!(!cx.dispatcher.scheduler().has_pending_tasks(),
            "a disconnected reporter queue must leave no shutdown timer pending");
    }

    #[gpui::test]
    fn closing_last_window_without_reporter_requests_immediate_exit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            controller::init(cx);
            crate::register_window_close_handler(cx);
        });
        let main = cx.add_window(|_, _| Empty);
        assert!(REPORTER.get().is_none());
        assert!(TEST_POSITION_REPORTS.with(|slot| slot.borrow().is_none()));
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()));

        main.update(cx, |_, window, _| window.remove_window()).unwrap();

        assert!(cx.windows().is_empty());
        assert!(cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "closing the last window without a reporter must request platform exit synchronously");

        // Let any spawned work register its timers without advancing the test clock.
        cx.run_until_parked();
        assert!(!cx.dispatcher.scheduler().has_pending_tasks(),
            "closing without a reporter must leave no shutdown timer pending");
        assert!(REPORTER.get().is_none(), "the reporter should remain uninitialised");
    }

    const SHUTDOWN_TEST_POSITION: f64 = 85.375;
    const SHUTDOWN_TEST_DURATION: f64 = 464.0;

    fn open_main_window_with_watch_history(
        cx: &mut TestAppContext,
        video_id: &str,
    ) -> (WindowHandle<Empty>, Receiver<Event>) {
        let (sender, reports) = std::sync::mpsc::channel();
        TEST_POSITION_REPORTS.with(|slot| slot.replace(Some(sender)));
        cx.on_quit(|| {
            TEST_POSITION_REPORTS.with(|slot| slot.take());
        });

        let video = VideoItem { id: video_id.into(), ..Default::default() };
        crate::store::history_add(&video);
        crate::store::history_progress(video_id, 80.0, SHUTDOWN_TEST_DURATION);
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Pal::new(true, 0x3ea6ff));
            cx.set_global(UiState::default());
            controller::init(cx);
            controller::player(cx).update(cx, |player, _| {
                player.video = Some(video);
                player.time = SHUTDOWN_TEST_POSITION;
                player.duration = SHUTDOWN_TEST_DURATION;
                player.paused = false;
            });
            crate::register_window_close_handler(cx);
        });
        (cx.add_window(|_, _| Empty), reports)
    }

    fn assert_final_report_before_flush(cx: &TestAppContext, reports: &Receiver<Event>, video_id: &str) {
        take_final_report_and_flush(reports, video_id).send(()).unwrap();
        cx.run_until_parked();
        assert!(matches!(reports.try_recv(), Err(TryRecvError::Empty)),
            "expected exactly one final report followed by one flush");
    }

    fn take_final_report_and_flush(reports: &Receiver<Event>, video_id: &str) -> oneshot::Sender<()> {
        let saved = crate::store::progress_map()[video_id];
        assert_eq!(saved, (SHUTDOWN_TEST_POSITION, SHUTDOWN_TEST_DURATION));
        assert!(matches!(reports.try_recv(),
            Ok(Event::Stop { id, t }) if id == video_id && t == saved.0));
        let Ok(Event::Flush(flushed)) = reports.try_recv() else {
            panic!("closing the last main window must flush after the final report");
        };
        flushed
    }
}

#[cfg(test)]
mod host_tests {
    use super::first_party_stats_url;

    #[test]
    fn stats_requests_keep_the_tracking_query_on_youtubes_first_party_origin() {
        assert_eq!(
            first_party_stats_url("https://s.youtube.com/api/stats/playback?ei=signed%2Fvalue&len=200", "playback").unwrap(),
            "https://www.youtube.com/api/stats/playback?ei=signed%2Fvalue&len=200",
        );
        assert_eq!(
            first_party_stats_url("https://www.youtube.com/api/stats/watchtime?ei=test", "watchtime").unwrap(),
            "https://www.youtube.com/api/stats/watchtime?ei=test",
        );
    }

    #[test]
    fn only_expected_youtube_stats_endpoints_are_allowed() {
        for url in [
            "http://s.youtube.com/api/stats/watchtime",
            "https://s.youtube.com.evil.example/api/stats/watchtime",
            "https://www.youtube.com@evil.example/api/stats/watchtime",
            "https://www.youtube.com/api/stats/playback",
            "https://www.youtube.com/api/stats/watchtime#fragment",
            "https://www.youtube.com/api/stats/../stats/watchtime",
        ] {
            assert!(first_party_stats_url(url, "watchtime").is_err(), "{url}");
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod browser_tests {
    use super::find_macos_browser;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn finds_executable_browser_in_macos_app_bundle() {
        let applications = std::env::temp_dir().join(format!("streamtube-browser-test-{:x}", rand::random::<u64>()));
        let safari = applications.join("Safari.app/Contents/MacOS/Safari");
        let brave = applications.join("Brave Browser.app/Contents/MacOS/Brave Browser");
        for executable in [&safari, &brave] {
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(executable, b"").unwrap();
        }
        fs::set_permissions(&safari, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&brave, fs::Permissions::from_mode(0o600)).unwrap();
        let before = find_macos_browser(&applications, None);
        fs::set_permissions(&brave, fs::Permissions::from_mode(0o700)).unwrap();
        let after = find_macos_browser(&applications, None);
        fs::remove_dir_all(&applications).unwrap();

        assert!(before.is_none());
        assert_eq!(after, Some(("Brave".into(), brave)));
    }
}

#[cfg(test)]
mod devtools_pipe_tests {
    /// Starts the real browser (headless) and talks to it over the private pipes.
    #[test]
    #[ignore = "needs Chrome, Edge, Brave or Chromium installed"]
    fn talks_to_the_browser_over_pipes() {
        let (_, exe) = super::find_browser().expect("a browser");
        let profile = std::env::temp_dir().join(format!("st-cdp-test-{}", std::process::id()));
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(format!("--user-data-dir={}", profile.display()))
            .args(["--headless=new", "--no-first-run", "--no-default-browser-check", "about:blank"]);
        let (mut cdp, ends) = super::Cdp::attach(&mut cmd).unwrap();
        let mut child = cmd.spawn().unwrap();
        drop(ends);
        let mut ok = None;
        for _ in 0..20 {
            if let Ok(c) = cdp.cookies() {
                ok = Some(c);
                break;
            }
        }
        cdp.close_browser();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&profile);
        assert!(ok.is_some(), "no reply over the DevTools pipe");
    }
}
