//! Shared test fixture: a small HTTPS server (rustls, self-signed cert) that
//! bridges every request to `git http-backend`, plus helpers to seed repos.
//!
//! The server is deliberately minimal (one request per connection, Connection:
//! close). It exists so the tests run real TLS and real smart-HTTP framing.

#![allow(dead_code)]

use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::thread;

use base64::{engine::general_purpose::STANDARD, Engine};
use git2_rustls_transport::Credentials;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

pub const USER: &str = "alice";
pub const PASSWORD: &str = "s3cret";
pub const UA_PREFIX: &str = "git2-rustls-transport/";

/// State the request handlers share with the tests.
struct Shared {
    root: PathBuf,
    user_agents: Mutex<Vec<String>>,
    unauthorized: AtomicUsize,
    seen: Mutex<Vec<Seen>>,
    port: u16,
    /// A second listener on the same host, for port-only origin changes.
    alt_port: u16,
}

/// What the server saw for one request.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub host: String,
    pub path: String,
    pub authorization: bool,
    pub content_length: bool,
    pub chunked: bool,
}

pub struct Fixture {
    origin: String,
    cert_pem: String,
    shared: Arc<Shared>,
    _root_dir: tempfile::TempDir,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();
static REGISTER: Once = Once::new();

/// The process-wide fixture; starts the HTTPS server on first use.
pub fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(|| {
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
        ])
        .expect("self-signed cert");
        let key_der =
            PrivateKeyDer::from(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        let server_config = Arc::new(
            ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key_der)
            .expect("server cert"),
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let alt_listener = TcpListener::bind("127.0.0.1:0").expect("bind alt");
        let alt_port = alt_listener.local_addr().expect("alt local addr").port();
        let root_dir = tempfile::tempdir().expect("temp root");
        let shared = Arc::new(Shared {
            root: root_dir.path().to_path_buf(),
            user_agents: Mutex::new(Vec::new()),
            unauthorized: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            port,
            alt_port,
        });

        for listener in [listener, alt_listener] {
            let accept_shared = Arc::clone(&shared);
            let server_config = Arc::clone(&server_config);
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let config = Arc::clone(&server_config);
                    let shared = Arc::clone(&accept_shared);
                    thread::spawn(move || serve_connection(stream, config, &shared));
                }
            });
        }

        Fixture {
            origin: format!("https://localhost:{port}"),
            cert_pem: cert.cert.pem(),
            shared,
            _root_dir: root_dir,
        }
    })
}

impl Fixture {
    pub fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.origin)
    }

    /// A client builder that trusts the self-signed certificate. It keeps
    /// reqwest's default (following) redirect policy, so the tests show that
    /// the transport overrides it.
    pub fn builder(&self) -> reqwest::blocking::ClientBuilder {
        reqwest::blocking::Client::builder().add_root_certificate(
            reqwest::Certificate::from_pem(self.cert_pem.as_bytes()).expect("cert pem"),
        )
    }

    pub fn root(&self) -> &Path {
        &self.shared.root
    }

    pub fn user_agents(&self) -> Vec<String> {
        self.shared.user_agents.lock().unwrap().clone()
    }

    pub fn unauthorized_count(&self) -> usize {
        self.shared.unauthorized.load(Ordering::SeqCst)
    }

    /// Requests whose path starts with `prefix`.
    pub fn seen(&self, prefix: &str) -> Vec<Seen> {
        self.shared
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.path.starts_with(prefix))
            .cloned()
            .collect()
    }
}

/// Registers the transport once per process. The callback supplies credentials
/// only for the test paths that need them.
pub fn register_once() {
    register_once_with_limits(git2_rustls_transport::Limits::default());
}

/// `register_once` with custom size caps. Registration is process-global, so
/// a test binary uses one set of limits for all its tests.
pub fn register_once_with_limits(limits: git2_rustls_transport::Limits) {
    REGISTER.call_once(|| {
        // SAFETY: `Once` serialises registration, and no test creates a
        // transport before `register_once` returns.
        unsafe { git2_rustls_transport::register(fixture().builder(), limits, callback) }
            .expect("register https transport");
    });
}

fn callback(url: &str) -> Option<Credentials> {
    let password = if ["/private/", "/xhost/", "/xport/", "/xpost/"]
        .iter()
        .any(|p| url.contains(p))
    {
        PASSWORD
    } else if url.contains("/badauth/") {
        "wrong"
    } else {
        return None;
    };
    Some(Credentials {
        username: USER.to_owned(),
        password: password.to_owned(),
    })
}

pub struct Seeded {
    pub bare: PathBuf,
    pub work: PathBuf,
}

/// Creates `<root>/<name>` as a bare repo with one commit on `main`, plus a
/// working copy whose `origin` is that bare repo.
pub fn seed(name: &str) -> Seeded {
    let root = fixture().root().to_path_buf();
    let bare = root.join(name);
    let work = root.join(format!("{name}.work"));
    git(&root, &["init", "-q", "--bare", path_str(&bare)]);
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&root, &["init", "-q", path_str(&work)]);
    git(&work, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&work, &["remote", "add", "origin", path_str(&bare)]);
    commit_file(&work, "README.md", "hello\n", "init");
    git(&work, &["push", "-q", "origin", "main"]);
    Seeded { bare, work }
}

pub fn commit_file(dir: &Path, file: &str, content: &str, message: &str) {
    std::fs::write(dir.join(file), content).expect("write file");
    git(dir, &["add", file]);
    git(
        dir,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

pub fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1 server over rustls.
// ---------------------------------------------------------------------------

struct Req {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }

    fn with(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

fn serve_connection(tcp: TcpStream, config: Arc<ServerConfig>, shared: &Shared) {
    let Ok(conn) = ServerConnection::new(config) else {
        return;
    };
    let mut tls = StreamOwned::new(conn, tcp);
    let request = match read_request(&mut BufReader::new(&mut tls)) {
        Ok(Some(request)) => request,
        // Handshake failures and aborted clients are expected in some tests.
        _ => return,
    };
    let reply = route(&request, shared);
    let _ = write_reply(&mut tls, &reply);
    tls.conn.send_close_notify();
    let _ = tls.flush();
}

fn read_request(reader: &mut impl BufRead) -> io::Result<Option<Req>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.trim_end().splitn(3, ' ');
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("/").to_owned();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (target, String::new()),
    };

    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }

    let mut request = Req {
        method,
        path,
        query,
        headers,
        body: Vec::new(),
    };
    let chunked = request
        .header("Transfer-Encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    if chunked {
        request.body = read_chunked(reader)?;
    } else if let Some(len) = request.header("Content-Length") {
        let len: usize = len
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        request.body = vec![0; len];
        reader.read_exact(&mut request.body)?;
    }
    Ok(Some(request))
}

fn read_chunked(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let size_text = line.trim().split(';').next().unwrap_or("0");
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if size == 0 {
            // Skip trailers up to the blank line.
            loop {
                line.clear();
                if reader.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
                    break;
                }
            }
            return Ok(body);
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        line.clear();
        reader.read_line(&mut line)?; // CRLF after each chunk
    }
}

fn write_reply(out: &mut impl Write, reply: &Reply) -> io::Result<()> {
    write!(out, "HTTP/1.1 {} status\r\n", reply.status)?;
    for (name, value) in &reply.headers {
        write!(out, "{name}: {value}\r\n")?;
    }
    write!(
        out,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.body.len()
    )?;
    out.write_all(&reply.body)?;
    out.flush()
}

fn expected_auth() -> String {
    format!("Basic {}", STANDARD.encode(format!("{USER}:{PASSWORD}")))
}

fn redirect(status: u16, location: &str) -> Reply {
    Reply::new(status, Vec::new()).with("Location", location)
}

fn route(request: &Req, shared: &Shared) -> Reply {
    shared
        .user_agents
        .lock()
        .unwrap()
        .push(request.header("User-Agent").unwrap_or_default().to_owned());

    shared.seen.lock().unwrap().push(Seen {
        method: request.method.clone(),
        host: request.header("Host").unwrap_or_default().to_owned(),
        path: request.path.clone(),
        authorization: request.header("Authorization").is_some(),
        content_length: request.header("Content-Length").is_some(),
        chunked: request.header("Transfer-Encoding").is_some(),
    });

    let path = request.path.as_str();
    let query = request.query.as_str();
    let is_post = request.method == "POST";
    let with_query = |rest: &str| {
        if query.is_empty() {
            rest.to_owned()
        } else {
            format!("{rest}?{query}")
        }
    };

    // /xhost/<repo>: needs auth, then redirects info/refs to another host
    // (127.0.0.1 instead of localhost) under /leak, which needs no auth.
    if let Some(rest) = path.strip_prefix("/xhost") {
        if request.header("Authorization") != Some(expected_auth().as_str()) {
            shared.unauthorized.fetch_add(1, Ordering::SeqCst);
            return Reply::new(401, Vec::new()).with("WWW-Authenticate", "Basic realm=\"git\"");
        }
        let location = format!("https://127.0.0.1:{}/leak{}", shared.port, with_query(rest));
        return redirect(301, &location);
    }
    // /xport/<repo>: needs auth, then redirects info/refs to the same host on
    // another port, under /leak.
    if let Some(rest) = path.strip_prefix("/xport") {
        if request.header("Authorization") != Some(expected_auth().as_str()) {
            shared.unauthorized.fetch_add(1, Ordering::SeqCst);
            return Reply::new(401, Vec::new()).with("WWW-Authenticate", "Basic realm=\"git\"");
        }
        let location = format!(
            "https://localhost:{}/leak{}",
            shared.alt_port,
            with_query(rest)
        );
        return redirect(301, &location);
    }
    // /xpost/<repo>: GETs need auth and are served; POSTs get a 307 to another
    // host under /leak (the body must never get there).
    if let Some(rest) = path.strip_prefix("/xpost") {
        if is_post {
            let location = format!("https://127.0.0.1:{}/leak{}", shared.port, with_query(rest));
            return redirect(307, &location);
        }
        if request.header("Authorization") != Some(expected_auth().as_str()) {
            shared.unauthorized.fetch_add(1, Ordering::SeqCst);
            return Reply::new(401, Vec::new()).with("WWW-Authenticate", "Basic realm=\"git\"");
        }
        let content_type = request.header("Content-Type");
        return run_cgi(
            &shared.root,
            rest,
            &request.method,
            query,
            content_type,
            Some(USER),
            &request.body,
        );
    }
    // /xauth/<repo>: redirects info/refs, with no auth, to /private on another
    // host. The 401 there must not reach the credential callback.
    if let Some(rest) = path.strip_prefix("/xauth") {
        let location = format!(
            "https://127.0.0.1:{}/private{}",
            shared.port,
            with_query(rest)
        );
        return redirect(301, &location);
    }
    // /uiloc/<repo>: redirects info/refs to the same origin under /leak, with
    // userinfo in the Location. It must not become an Authorization header.
    if let Some(rest) = path.strip_prefix("/uiloc") {
        let location = format!(
            "https://evil:stolen@localhost:{}/leak{}",
            shared.port,
            with_query(rest)
        );
        return redirect(301, &location);
    }
    // /tohttp/<repo>: redirects info/refs to plain http.
    if let Some(rest) = path.strip_prefix("/tohttp") {
        let location = format!("http://localhost:{}{}", shared.port, with_query(rest));
        return redirect(301, &location);
    }

    // /old/<repo>: permanent redirect to the real repo (info/refs is followed).
    if let Some(rest) = path.strip_prefix("/old") {
        let location = if query.is_empty() {
            rest.to_owned()
        } else {
            format!("{rest}?{query}")
        };
        return redirect(301, &location);
    }

    let (path_info, user) = if let Some(rest) = path.strip_prefix("/leak") {
        (rest.to_owned(), None)
    } else if let Some(rest) = path.strip_prefix("/redir") {
        // POSTs are redirected (the client must refuse them); GETs are served.
        if is_post {
            return redirect(307, rest);
        }
        (rest.to_owned(), None)
    } else if let Some(rest) = ["/private", "/badauth", "/nocreds"]
        .iter()
        .find_map(|prefix| path.strip_prefix(*prefix))
    {
        if request.header("Authorization") != Some(expected_auth().as_str()) {
            shared.unauthorized.fetch_add(1, Ordering::SeqCst);
            return Reply::new(401, Vec::new()).with("WWW-Authenticate", "Basic realm=\"git\"");
        }
        (rest.to_owned(), Some(USER))
    } else {
        (path.to_owned(), None)
    };

    run_cgi(
        &shared.root,
        &path_info,
        &request.method,
        query,
        request.header("Content-Type"),
        user,
        &request.body,
    )
}

fn run_cgi(
    root: &Path,
    path_info: &str,
    method: &str,
    query: &str,
    content_type: Option<&str>,
    user: Option<&str>,
    body: &[u8],
) -> Reply {
    let mut cmd = Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.receivepack")
        .env("GIT_CONFIG_VALUE_0", "true")
        .env("REQUEST_METHOD", method)
        .env("PATH_INFO", path_info)
        .env("QUERY_STRING", query)
        .env("CONTENT_LENGTH", body.len().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(ct) = content_type {
        cmd.env("CONTENT_TYPE", ct);
    }
    if let Some(u) = user {
        cmd.env("REMOTE_USER", u);
    }
    let mut child = cmd.spawn().expect("spawn git http-backend");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(body)
        .expect("write cgi body");
    let output = child.wait_with_output().expect("wait for http-backend");
    parse_cgi(&output.stdout)
}

/// Turns CGI output (headers, blank line, body) into an HTTP reply.
fn parse_cgi(raw: &[u8]) -> Reply {
    let (head, body): (&[u8], &[u8]) = if let Some(i) = find(raw, b"\r\n\r\n") {
        (&raw[..i], &raw[i + 4..])
    } else if let Some(i) = find(raw, b"\n\n") {
        (&raw[..i], &raw[i + 2..])
    } else {
        (raw, &[])
    };
    let mut status = 200u16;
    let mut content_type = "application/octet-stream".to_owned();
    for line in String::from_utf8_lossy(head).lines() {
        if let Some(value) = line.strip_prefix("Status: ") {
            status = value.get(..3).and_then(|s| s.parse().ok()).unwrap_or(500);
        } else if let Some(value) = line.strip_prefix("Content-Type: ") {
            content_type = value.trim().to_owned();
        }
    }
    Reply::new(status, body.to_vec()).with("Content-Type", &content_type)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
