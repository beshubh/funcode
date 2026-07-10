use anyhow::{Context, Result as AnyResult, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fmt, fs,
    io::Write as _,
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_ADDRESS: &str = "127.0.0.1:1455";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthEvent {
    BrowserOpened {
        authorization_url: String,
        browser_opened: bool,
    },
    Succeeded {
        account_id: Option<String>,
    },
    Failed {
        message: String,
    },
    Cancelled,
}

#[derive(Debug)]
enum AuthCommand {
    Authenticate,
    Cancel,
    Shutdown,
}

pub struct AuthTaskRunner {
    commands: Sender<AuthCommand>,
    events: Receiver<AuthEvent>,
    thread: Option<JoinHandle<()>>,
}

impl AuthTaskRunner {
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = thread::spawn(move || run_auth_coordinator(command_rx, event_tx));
        Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
        }
    }

    pub fn authenticate(&self) -> AnyResult<()> {
        self.commands
            .send(AuthCommand::Authenticate)
            .context("the authentication runner is unavailable")
    }

    pub fn cancel(&self) -> AnyResult<()> {
        self.commands
            .send(AuthCommand::Cancel)
            .context("the authentication runner is unavailable")
    }

    pub fn try_event(&self) -> Option<AuthEvent> {
        self.events.try_recv().ok()
    }

    pub fn shutdown(&mut self) {
        let _ = self.commands.send(AuthCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for AuthTaskRunner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug, Clone)]
struct ChatGptOAuth {
    verifier: String,
    challenge: String,
    state: String,
}

impl ChatGptOAuth {
    fn new() -> Self {
        let mut verifier_bytes = [0_u8; 32];
        rand::rng().fill(&mut verifier_bytes);
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

        let mut state_bytes = [0_u8; 32];
        rand::rng().fill(&mut state_bytes);
        let state = URL_SAFE_NO_PAD.encode(state_bytes);

        Self {
            verifier,
            challenge,
            state,
        }
    }

    fn authorization_url(&self) -> Result<Url, url::ParseError> {
        let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize"))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", "openid profile email offline_access")
            .append_pair("code_challenge", &self.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("state", &self.state)
            .append_pair("originator", "funcode");
        Ok(url)
    }

    fn callback_code(&self, url: &Url) -> AnyResult<String> {
        if url.path() != "/auth/callback" {
            bail!("unexpected OAuth callback path");
        }
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        if let Some(error) = query.get("error") {
            let description = query
                .get("error_description")
                .map(String::as_str)
                .unwrap_or(error);
            bail!("ChatGPT sign-in failed: {description}");
        }
        if query.get("state").map(String::as_str) != Some(self.state.as_str()) {
            bail!("OAuth callback state did not match; sign-in was rejected");
        }
        query
            .get("code")
            .filter(|code| !code.is_empty())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("OAuth callback did not include an authorization code"))
    }
}

impl Default for ChatGptOAuth {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub account_id: Option<String>,
}

impl fmt::Debug for OAuthCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthCredentials")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .finish()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct AuthFile {
    version: u8,
    openai: OAuthCredentials,
}

#[derive(Debug, Clone)]
pub struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    pub fn standard() -> AnyResult<Self> {
        let root = std::env::var_os("FUNCODE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".funcode")))
            .or_else(|| {
                std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".funcode"))
            })
            .context("could not determine a home directory for funcode credentials")?;
        Ok(Self::at(root.join("auth.json")))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn save(&self, credentials: &OAuthCredentials) -> AnyResult<()> {
        let parent = self
            .path
            .parent()
            .context("the auth file path has no parent directory")?;
        create_private_directory(parent)?;

        let auth = AuthFile {
            version: 1,
            openai: credentials.clone(),
        };
        let temporary_path = parent.join(format!(
            ".auth.json.{}-{}.tmp",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary_path)
            .with_context(|| format!("failed to open {}", temporary_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let result = (|| -> AnyResult<()> {
            serde_json::to_writer_pretty(&mut file, &auth)
                .context("failed to serialize auth data")?;
            file.write_all(b"\n")?;
            file.sync_all().context("failed to flush auth data")?;
            drop(file);
            replace_auth_file(&temporary_path, &self.path)?;
            #[cfg(unix)]
            {
                let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result
    }

    pub fn load(&self) -> AnyResult<Option<OAuthCredentials>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("failed to read auth data"),
        };
        let auth: AuthFile = serde_json::from_slice(&bytes).context("failed to parse auth data")?;
        Ok(Some(auth.openai))
    }
}

fn create_private_directory(path: &std::path::Path) -> AnyResult<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    access_token: String,
    refresh_token: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_expires_in() -> u64 {
    3600
}

#[derive(Debug, Deserialize)]
struct JwtClaims {
    chatgpt_account_id: Option<String>,
    organizations: Option<Vec<OrganizationClaim>>,
    #[serde(rename = "https://api.openai.com/auth")]
    openai_auth: Option<OpenAiAuthClaim>,
}

#[derive(Debug, Deserialize)]
struct OrganizationClaim {
    id: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiAuthClaim {
    chatgpt_account_id: Option<String>,
}

enum FlowControl {
    Continue,
    Shutdown,
}

fn replace_auth_file(
    temporary_path: &std::path::Path,
    auth_path: &std::path::Path,
) -> AnyResult<()> {
    fs::rename(temporary_path, auth_path).context("failed to install new auth data")
}

fn run_auth_coordinator(commands: Receiver<AuthCommand>, events: Sender<AuthEvent>) {
    while let Ok(command) = commands.recv() {
        match command {
            AuthCommand::Authenticate => {
                if matches!(run_browser_flow(&commands, &events), FlowControl::Shutdown) {
                    return;
                }
            }
            AuthCommand::Cancel => {
                let _ = events.send(AuthEvent::Cancelled);
            }
            AuthCommand::Shutdown => return,
        }
    }
}

fn run_browser_flow(commands: &Receiver<AuthCommand>, events: &Sender<AuthEvent>) -> FlowControl {
    let listener = match TcpListener::bind(CALLBACK_ADDRESS) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = events.send(AuthEvent::Failed {
                message: format!(
                    "could not start the local sign-in callback on port 1455: {error}"
                ),
            });
            return FlowControl::Continue;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        let _ = events.send(AuthEvent::Failed {
            message: format!("could not configure the local sign-in callback: {error}"),
        });
        return FlowControl::Continue;
    }

    let oauth = ChatGptOAuth::new();
    let authorization_url = match oauth.authorization_url() {
        Ok(url) => url,
        Err(error) => {
            let _ = events.send(AuthEvent::Failed {
                message: format!("could not construct the ChatGPT sign-in URL: {error}"),
            });
            return FlowControl::Continue;
        }
    };
    let browser_opened = open_browser(authorization_url.as_str());
    if events
        .send(AuthEvent::BrowserOpened {
            authorization_url: authorization_url.to_string(),
            browser_opened,
        })
        .is_err()
    {
        return FlowControl::Shutdown;
    }

    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    loop {
        match commands.try_recv() {
            Ok(AuthCommand::Cancel) => {
                let _ = events.send(AuthEvent::Cancelled);
                return FlowControl::Continue;
            }
            Ok(AuthCommand::Shutdown) | Err(TryRecvError::Disconnected) => {
                return FlowControl::Shutdown;
            }
            Ok(AuthCommand::Authenticate) | Err(TryRecvError::Empty) => {}
        }

        match listener.accept() {
            Ok((mut stream, _)) => match receive_callback(&mut stream, &oauth) {
                Ok(Some(code)) => {
                    match exchange_code(&code, &oauth.verifier)
                        .and_then(credentials_from_tokens)
                        .and_then(|credentials| {
                            AuthStore::standard()?.save(&credentials)?;
                            Ok(credentials)
                        }) {
                        Ok(credentials) => {
                            let _ = write_browser_response(&mut stream, 200, success_page());
                            let _ = events.send(AuthEvent::Succeeded {
                                account_id: credentials.account_id,
                            });
                        }
                        Err(error) => {
                            let message = error.to_string();
                            let _ = write_browser_response(&mut stream, 500, &error_page(&message));
                            let _ = events.send(AuthEvent::Failed { message });
                        }
                    }
                    return FlowControl::Continue;
                }
                Ok(None) => {}
                Err(error) => {
                    let message = error.to_string();
                    let _ = write_browser_response(&mut stream, 400, &error_page(&message));
                    let _ = events.send(AuthEvent::Failed { message });
                    return FlowControl::Continue;
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                let _ = events.send(AuthEvent::Failed {
                    message: format!("the local sign-in callback failed: {error}"),
                });
                return FlowControl::Continue;
            }
        }

        if Instant::now() >= deadline {
            let _ = events.send(AuthEvent::Failed {
                message: "ChatGPT sign-in timed out after five minutes".into(),
            });
            return FlowControl::Continue;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn receive_callback(stream: &mut TcpStream, oauth: &ChatGptOAuth) -> AnyResult<Option<String>> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request_bytes = read_http_headers(stream)?;
    if request_bytes.is_empty() {
        return Ok(None);
    }
    let request = std::str::from_utf8(&request_bytes).context("invalid callback request")?;
    let Some(request_line) = request.lines().next() else {
        return Ok(None);
    };
    let mut parts = request_line.split_whitespace();
    if parts.next() != Some("GET") {
        write_browser_response(stream, 405, "Method not allowed")?;
        return Ok(None);
    }
    let target = parts
        .next()
        .context("callback request did not include a target")?;
    let url = Url::parse(&format!("http://localhost:1455{target}"))?;
    if url.path() != "/auth/callback" {
        write_browser_response(stream, 404, "Not found")?;
        return Ok(None);
    }
    oauth.callback_code(&url).map(Some)
}

fn read_http_headers(reader: &mut impl std::io::Read) -> AnyResult<Vec<u8>> {
    let mut request_bytes = Vec::with_capacity(1024);
    loop {
        if request_bytes.len() >= 16 * 1024 {
            bail!("OAuth callback request headers were too large");
        }
        let mut chunk = [0_u8; 1024];
        let bytes_read = reader.read(&mut chunk)?;
        if bytes_read == 0 {
            if request_bytes.is_empty() {
                return Ok(request_bytes);
            }
            break;
        }
        request_bytes.extend_from_slice(&chunk[..bytes_read]);
        if request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    Ok(request_bytes)
}

fn exchange_code(code: &str, verifier: &str) -> AnyResult<TokenResponse> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("funcode/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to create the ChatGPT sign-in client")?;
    let response = client
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .context("could not reach the ChatGPT token endpoint")?;
    let status = response.status();
    if !status.is_success() {
        bail!("ChatGPT token exchange failed with status {status}");
    }
    response
        .json()
        .context("ChatGPT returned an invalid token response")
}

fn credentials_from_tokens(tokens: TokenResponse) -> AnyResult<OAuthCredentials> {
    let account_id = tokens
        .id_token
        .as_deref()
        .and_then(parse_account_id)
        .or_else(|| parse_account_id(&tokens.access_token));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    Ok(OAuthCredentials {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: now
            .as_millis()
            .saturating_add(u128::from(tokens.expires_in) * 1000)
            .min(u128::from(u64::MAX)) as u64,
        account_id,
    })
}

fn parse_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: JwtClaims = serde_json::from_slice(&decoded).ok()?;
    claims
        .chatgpt_account_id
        .or_else(|| claims.openai_auth.and_then(|auth| auth.chatgpt_account_id))
        .or_else(|| {
            claims
                .organizations
                .and_then(|organizations| organizations.into_iter().next())
                .map(|organization| organization.id)
        })
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open").arg(url).spawn();
    #[cfg(not(any(unix, target_os = "windows")))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "browser opening is not supported on this platform",
    ));
    result.is_ok()
}

fn write_browser_response(stream: &mut TcpStream, status: u16, body: &str) -> AnyResult<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Response",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(())
}

fn success_page() -> &'static str {
    "<!doctype html><meta charset=\"utf-8\"><title>funcode authenticated</title><h1>Authenticated</h1><p>You can return to funcode.</p>"
}

fn error_page(message: &str) -> String {
    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>funcode sign-in failed</title><h1>Sign-in failed</h1><p>{escaped}</p>"
    )
}

#[cfg(test)]
mod tests {
    use super::{AuthStore, ChatGptOAuth, OAuthCredentials, REDIRECT_URI};
    use base64::Engine as _;
    use std::io::Cursor;
    use url::Url;

    #[test]
    fn authorization_uses_pkce_state_and_the_local_callback() {
        let oauth = ChatGptOAuth::new();
        let url = oauth.authorization_url().unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(
            url.as_str().split('?').next(),
            Some("https://auth.openai.com/oauth/authorize")
        );
        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert!(
            query
                .get("code_challenge")
                .is_some_and(|value| !value.is_empty())
        );
        assert!(query.get("state").is_some_and(|value| !value.is_empty()));
        assert!(
            query
                .get("scope")
                .is_some_and(|value| value.contains("offline_access"))
        );
    }

    #[test]
    fn callback_rejects_oauth_errors_and_mismatched_state() {
        let oauth = ChatGptOAuth::new();
        let valid = Url::parse(&format!(
            "{REDIRECT_URI}?code=secret-code&state={}",
            oauth.state
        ))
        .unwrap();
        assert_eq!(oauth.callback_code(&valid).unwrap(), "secret-code");

        let wrong_state =
            Url::parse(&format!("{REDIRECT_URI}?code=secret-code&state=wrong")).unwrap();
        assert!(
            oauth
                .callback_code(&wrong_state)
                .unwrap_err()
                .to_string()
                .contains("state")
        );

        let denied = Url::parse(&format!(
            "{REDIRECT_URI}?error=access_denied&error_description=Nope&state={}",
            oauth.state
        ))
        .unwrap();
        assert!(
            oauth
                .callback_code(&denied)
                .unwrap_err()
                .to_string()
                .contains("Nope")
        );
    }

    #[test]
    fn credentials_round_trip_through_a_private_auth_file() {
        let root = std::env::temp_dir().join(format!(
            "funcode-auth-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path = root.join("auth.json");
        let store = AuthStore::at(path.clone());
        let credentials = OAuthCredentials {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 42,
            account_id: Some("account".into()),
        };

        store.save(&credentials).unwrap();
        assert_eq!(store.load().unwrap(), Some(credentials));

        let replacement = OAuthCredentials {
            access_token: "new-access".into(),
            refresh_token: "new-refresh".into(),
            expires_at: 84,
            account_id: Some("new-account".into()),
        };
        store.save(&replacement).unwrap();
        assert_eq!(store.load().unwrap(), Some(replacement));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_callback_accepts_fragmented_http_headers() {
        struct ChunkedReader {
            inner: Cursor<Vec<u8>>,
        }

        impl std::io::Read for ChunkedReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let length = buffer.len().min(7);
                self.inner.read(&mut buffer[..length])
            }
        }

        let request = b"GET /auth/callback?code=secret HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let mut reader = ChunkedReader {
            inner: Cursor::new(request.to_vec()),
        };

        assert_eq!(super::read_http_headers(&mut reader).unwrap(), request);
    }

    #[test]
    fn chatgpt_account_id_is_read_from_the_token_claims() {
        let payload = super::URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "workspace-123"
                }
            })
            .to_string(),
        );
        let token = format!("header.{payload}.signature");

        assert_eq!(
            super::parse_account_id(&token).as_deref(),
            Some("workspace-123")
        );
    }
}
