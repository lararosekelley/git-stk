//! The local server a tour page talks to while it is open: loopback only,
//! every path behind a per-run token, and gone once the last tab is.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// How long to wait for the first tab to open the page.
const FIRST_TAB_WAIT: Duration = Duration::from_secs(300);
/// How long a tab may go without its heartbeat before it counts as gone, as
/// when the browser quits without saying so. Browsers slow the timers of a
/// tab hidden for a while to about once a minute, so this outlasts that.
const TAB_TIMEOUT: Duration = Duration::from_secs(180);
/// How long to stay up after the last tab closes, so a reload reconnects.
const LAST_TAB_GRACE: Duration = Duration::from_secs(4);
const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub struct Request {
    pub method: String,
    /// The path after the token, such as `api/conversation`.
    pub path: String,
    pub query: String,
    pub body: Vec<u8>,
}

impl Request {
    /// The value of `key` in the query string.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    }
}

pub struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    pub fn html(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into_bytes(),
        }
    }

    pub fn json(value: &serde_json::Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: value.to_string().into_bytes(),
        }
    }

    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::json!({ "error": message })
                .to_string()
                .into_bytes(),
        }
    }
}

pub struct Server {
    listener: TcpListener,
    port: u16,
    token: String,
}

/// Ports tried after the preferred one before taking any free port.
const NEARBY_PORTS: u16 = 10;

impl Server {
    /// Listen on `preferred`, or a port just past it when that is taken, so
    /// the page keeps one origin, and with it what the browser stored for it,
    /// from one tour to the next; any free port otherwise.
    pub fn bind(preferred: u16) -> Result<Self> {
        let listener = (0..NEARBY_PORTS)
            .filter_map(|offset| preferred.checked_add(offset))
            .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
            .map_or_else(|| TcpListener::bind("127.0.0.1:0"), Ok)
            .context("failed to start the tour server")?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            port,
            token: token(),
        })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}/", self.port, self.token)
    }

    /// Answer requests with `handler` until no tab has the page open.
    pub fn run(self, handler: &(dyn Fn(Request) -> Response + Sync)) -> Result<()> {
        let tabs = Tabs::new();
        thread::scope(|scope| {
            loop {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        let tabs = &tabs;
                        let server = &self;
                        scope.spawn(move || server.answer(stream, tabs, handler));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if tabs.all_gone() {
                            return Ok(());
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                    // One connection going wrong - a client resetting it while
                    // queued, or a signal - is no reason to stop serving.
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(error).context("the tour server stopped"),
                }
            }
        })
    }

    fn answer(
        &self,
        stream: TcpStream,
        tabs: &Tabs,
        handler: &(dyn Fn(Request) -> Response + Sync),
    ) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let response = match read_request(&stream) {
            Ok(head) => match self.route(head) {
                Ok(request) => match request.path.as_str() {
                    "api/alive" => {
                        tabs.alive(&String::from_utf8_lossy(&request.body));
                        Response::json(&serde_json::json!({}))
                    }
                    "api/closed" => {
                        tabs.closed(&String::from_utf8_lossy(&request.body));
                        Response::json(&serde_json::json!({}))
                    }
                    _ => handler(request),
                },
                Err(response) => response,
            },
            Err(_) => Response::error(400, "bad request"),
        };
        let _ = write_response(stream, &response);
    }

    /// The request, once it has shown it comes from this tour's page.
    fn route(&self, raw: RawRequest) -> Result<Request, Response> {
        // A name other than the loopback address is another site resolving
        // to this machine.
        let hosts = [
            format!("127.0.0.1:{}", self.port),
            format!("localhost:{}", self.port),
        ];
        if !raw
            .header("host")
            .is_some_and(|host| hosts.iter().any(|allowed| allowed == host))
        {
            return Err(Response::error(403, "wrong host"));
        }
        if raw.method == "POST" {
            let same_origin = raw
                .header("origin")
                .is_none_or(|origin| hosts.iter().any(|host| origin == format!("http://{host}")));
            let json = raw
                .header("content-type")
                .is_some_and(|kind| kind.starts_with("application/json"));
            if !same_origin || !json {
                return Err(Response::error(403, "cross-origin request"));
            }
        }
        let (path, query) = raw.target.split_once('?').unwrap_or((&raw.target, ""));
        let Some(path) = path
            .strip_prefix('/')
            .and_then(|path| path.strip_prefix(self.token.as_str()))
            .and_then(|path| path.strip_prefix('/').or(path.is_empty().then_some("")))
        else {
            return Err(Response::error(404, "not found"));
        };
        Ok(Request {
            method: raw.method,
            path: path.to_owned(),
            query: query.to_owned(),
            body: raw.body,
        })
    }
}

/// The open tabs, each by the id it sends with its heartbeat.
struct Tabs {
    started: Instant,
    seen: Mutex<HashMap<String, Instant>>,
    /// When the last tab closed, while none has reopened.
    emptied: Mutex<Option<Instant>>,
}

impl Tabs {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            seen: Mutex::new(HashMap::new()),
            emptied: Mutex::new(None),
        }
    }

    fn alive(&self, tab: &str) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(tab.to_owned(), Instant::now());
        }
        if let Ok(mut emptied) = self.emptied.lock() {
            *emptied = None;
        }
    }

    fn closed(&self, tab: &str) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        seen.remove(tab);
        if seen.is_empty()
            && let Ok(mut emptied) = self.emptied.lock()
        {
            *emptied = Some(Instant::now());
        }
    }

    fn all_gone(&self) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return true;
        };
        let now = Instant::now();
        let had_tabs = !seen.is_empty();
        seen.retain(|_, last| now.duration_since(*last) < TAB_TIMEOUT);
        if !seen.is_empty() {
            return false;
        }
        let emptied = self.emptied.lock().ok().and_then(|emptied| *emptied);
        match emptied {
            Some(when) => now.duration_since(when) >= LAST_TAB_GRACE,
            None if had_tabs => true,
            None => now.duration_since(self.started) >= FIRST_TAB_WAIT,
        }
    }
}

struct RawRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn read_request(stream: &TcpStream) -> Result<RawRequest> {
    let mut reader = BufReader::new(stream);
    let mut head = Vec::new();
    loop {
        let read = reader.read_until(b'\n', &mut head)?;
        if read == 0 || head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
        if head.len() > MAX_HEAD_BYTES {
            bail!("request head too large");
        }
    }
    let mut request = parse_head(&String::from_utf8_lossy(&head))?;
    let length: usize = request
        .header("content-length")
        .map(str::parse)
        .transpose()?
        .unwrap_or_default();
    if length > MAX_BODY_BYTES {
        bail!("request body too large");
    }
    request.body = vec![0; length];
    reader.read_exact(&mut request.body)?;
    Ok(request)
}

fn parse_head(head: &str) -> Result<RawRequest> {
    let mut lines = head.lines();
    let mut start = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target)) = (start.next(), start.next()) else {
        bail!("no request line");
    };
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Ok(RawRequest {
        method: method.to_owned(),
        target: target.to_owned(),
        headers,
        body: Vec::new(),
    })
}

fn write_response(mut stream: TcpStream, response: &Response) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    )?;
    stream.write_all(&response.body)?;
    stream.flush()
}

/// 128 bits from the operating system's randomness, which seeds each
/// `RandomState`, as 32 hex digits.
fn token() -> String {
    (0..2)
        .map(|_| format!("{:016x}", RandomState::new().build_hasher().finish()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tour_takes_its_preferred_port_when_it_is_free() {
        let first = Server::bind(0).unwrap();
        let preferred = first.port;
        drop(first);
        let held = Server::bind(preferred).unwrap();
        assert_eq!(held.port, preferred);
        // Taken, so the next tour of this repository moves along.
        let next = Server::bind(preferred).unwrap();
        assert_ne!(next.port, preferred);
    }

    fn server() -> Server {
        Server {
            listener: TcpListener::bind("127.0.0.1:0").unwrap(),
            port: 4000,
            token: "secret".into(),
        }
    }

    fn raw(head: &str) -> RawRequest {
        parse_head(head).unwrap()
    }

    #[test]
    fn requests_need_the_token_and_the_loopback_host() {
        let server = server();
        let ok = server
            .route(raw(
                "GET /secret/api/conversation?chapter=2 HTTP/1.1\r\nHost: 127.0.0.1:4000\r\n\r\n",
            ))
            .ok()
            .unwrap();
        assert_eq!(
            (ok.path.as_str(), ok.param("chapter")),
            ("api/conversation", Some("2"))
        );
        let page = server
            .route(raw("GET /secret/ HTTP/1.1\r\nHost: localhost:4000\r\n\r\n"))
            .ok()
            .unwrap();
        assert_eq!(page.path, "");

        let refused = |head: &str| {
            server
                .route(raw(head))
                .err()
                .map(|response| response.status)
        };
        assert_eq!(
            refused("GET /other/ HTTP/1.1\r\nHost: 127.0.0.1:4000\r\n\r\n"),
            Some(404)
        );
        assert_eq!(
            refused("GET /secretx/ HTTP/1.1\r\nHost: 127.0.0.1:4000\r\n\r\n"),
            Some(404)
        );
        assert_eq!(
            refused("GET /secret/ HTTP/1.1\r\nHost: evil.test:4000\r\n\r\n"),
            Some(403)
        );
        assert_eq!(refused("GET /secret/ HTTP/1.1\r\n\r\n"), Some(403));
    }

    #[test]
    fn posts_must_be_same_origin_json() {
        let server = server();
        let post = |headers: &str| {
            server
                .route(raw(&format!(
                    "POST /secret/api/alive HTTP/1.1\r\nHost: 127.0.0.1:4000\r\n{headers}\r\n"
                )))
                .err()
                .map(|response| response.status)
        };
        assert_eq!(post("Content-Type: application/json\r\n"), None);
        assert_eq!(
            post("Content-Type: application/json\r\nOrigin: http://127.0.0.1:4000\r\n"),
            None
        );
        assert_eq!(post("Content-Type: text/plain\r\n"), Some(403));
        assert_eq!(
            post("Content-Type: application/json\r\nOrigin: https://evil.test\r\n"),
            Some(403)
        );
    }

    #[test]
    fn the_server_lives_while_a_tab_does() {
        let tabs = Tabs::new();
        assert!(!tabs.all_gone(), "waits for the first tab");
        tabs.alive("a");
        tabs.alive("b");
        tabs.closed("a");
        assert!(!tabs.all_gone(), "one tab is still open");
        tabs.closed("b");
        assert!(!tabs.all_gone(), "a reload gets a moment to reconnect");
        *tabs.emptied.lock().unwrap() = Some(Instant::now() - LAST_TAB_GRACE);
        assert!(tabs.all_gone());
        // A reload that reconnects keeps it up.
        tabs.alive("c");
        assert!(!tabs.all_gone());
        // A tab that stops beating counts as gone.
        tabs.seen
            .lock()
            .unwrap()
            .insert("c".into(), Instant::now() - TAB_TIMEOUT);
        assert!(tabs.all_gone());
    }

    #[test]
    fn tokens_are_long_and_differ() {
        let (a, b) = (token(), token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
