//! Accounts for the node's page: a username and password, and an optional
//! authenticator-app code (TOTP, RFC 6238), instead of pasting the API token.
//!
//! The API token stays what it was: every route accepts it, scripts and a
//! controller use it, and it is what creates the first account and the way
//! back in when a phone with the codes is lost. Accounts only add a way for a
//! person to sign in. With no token configured the node's API is loopback-only
//! and unauthenticated (`AgentConfig::validate`), and accounts are not used.
//!
//! Rules:
//! - **Passwords are Argon2id hashes**, never stored or logged in clear.
//! - **A wrong username and a wrong password read the same**, so asking does
//!   not tell anyone which accounts exist, and a password is checked against a
//!   hash even for a username that does not exist, so the time does not either.
//! - **Sign-in attempts are throttled** per username: after
//!   [`MAX_FAILURES`] in [`FAILURE_WINDOW`], that name is refused for
//!   [`LOCKOUT`], whatever is sent.
//! - **A code is good once.** The 30-second step of the last code accepted is
//!   kept, and a code for that step or an older one is refused.
//! - **Sessions live in memory** for [`SESSION_LIFETIME`] and end at sign-out
//!   or an agent restart. The file holds accounts only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

pub const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 3600);
pub const MAX_FAILURES: usize = 5;
pub const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
pub const LOCKOUT: Duration = Duration::from_secs(5 * 60);
pub const MIN_PASSWORD_LEN: usize = 10;
/// New accounts a node takes by sign-up in an hour, across everyone.
pub const SIGNUPS_PER_HOUR: usize = 30;
/// Accounts a node holds before sign-up is refused.
pub const MAX_ACCOUNTS: usize = 10_000;
const TOTP_STEP: u64 = 30;
const TOTP_DIGITS: u32 = 6;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Totp {
    /// The shared secret, Base32. Needed in clear to check a code, like the
    /// API token; the file is owner-only.
    secret: String,
    /// Off while being set up: a secret is only in force once a code from it
    /// has been typed back.
    enabled: bool,
    /// The step of the last code accepted, so it is not accepted again.
    #[serde(default)]
    last_step: u64,
}

/// What an account may do on the page. An admin manages accounts, game files,
/// interfaces and packs, and acts as an admin of the index through a
/// controller; a user runs servers. The API token counts as an admin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The default for an account written before roles existed: every account
    /// could do everything then, and taking that away on upgrade would lock
    /// an operator out of their own node.
    #[default]
    Admin,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    username: String,
    /// Argon2id, in the PHC string format.
    password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    totp: Option<Totp>,
    created_unix: u64,
    #[serde(default)]
    role: Role,
    /// A user asked to become an admin, at this time. Decided by an admin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    admin_requested_unix: Option<u64>,
    /// Random, made once per account: what makes this account's own identity
    /// at a controller its own (`controller::account_secret`). A name taken
    /// again after its account was removed gets a new one, so it never
    /// inherits the old account's servers. Written the first time it is asked
    /// for on an account from before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_salt: Option<String>,
    /// When the account signed itself up and accepted the node's terms, if it
    /// did. An account an admin made has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accepted_terms_unix: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    users: Vec<User>,
}

/// What the page is told about an account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountView {
    pub username: String,
    pub totp_enabled: bool,
    pub created_unix: u64,
    pub role: Role,
    /// Set while this user's request to become an admin waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_requested_unix: Option<u64>,
    /// When it signed itself up and accepted the node's terms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_terms_unix: Option<u64>,
}

/// A sign-in that did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginRefusal {
    /// Wrong username or password; the same words for both.
    Wrong,
    /// The password was right and this account asks for a code.
    NeedsCode,
    /// The code was wrong, or already used.
    WrongCode,
    /// Too many failures for this name lately.
    Throttled { retry_after: Duration },
}

impl std::fmt::Display for LoginRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wrong => write!(f, "wrong username or password"),
            Self::NeedsCode => write!(f, "enter the code from your authenticator app"),
            Self::WrongCode => write!(f, "that code is not right, or was already used"),
            Self::Throttled { retry_after } => {
                write!(f, "too many tries; wait {} seconds", retry_after.as_secs().max(1))
            }
        }
    }
}

struct Session {
    username: String,
    expires: Instant,
}

/// A username is data: short, lower case, digits and `._-`. Refused, never
/// repaired.
pub fn valid_username(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
}

pub struct Accounts {
    path: PathBuf,
    store: Mutex<Store>,
    sessions: Mutex<HashMap<String, Session>>,
    failures: Mutex<HashMap<String, Vec<Instant>>>,
    /// When the last sign-ups happened, for [`SIGNUPS_PER_HOUR`].
    signups: Mutex<Vec<Instant>>,
    /// A hash checked when the username does not exist, so a miss costs the
    /// same time as a wrong password.
    decoy: String,
}

impl Accounts {
    /// Read the accounts at `path` (none yet if it is not there). A file that
    /// does not parse is an error, never "no accounts": that would let the
    /// next visitor with the token create one and lock the owner out.
    pub fn load(path: &Path) -> Result<Self> {
        let store = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            path: path.to_path_buf(),
            store: Mutex::new(store),
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            signups: Mutex::new(Vec::new()),
            decoy: hash_password("not a real account, only spent time")?,
        })
    }

    fn save(&self, store: &Store) -> Result<()> {
        let partial = self.path.with_extension("json.partial");
        {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&partial).with_context(|| format!("writing {}", partial.display()))?;
            file.write_all(&serde_json::to_vec_pretty(store)?)?;
            file.sync_all()?;
        }
        std::fs::rename(&partial, &self.path).with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    fn lock_store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn any(&self) -> bool {
        !self.lock_store().users.is_empty()
    }

    pub fn list(&self) -> Vec<AccountView> {
        self.lock_store().users.iter().map(view).collect()
    }

    pub fn get(&self, username: &str) -> Option<AccountView> {
        self.lock_store().users.iter().find(|u| u.username == username).map(view)
    }

    /// Add an account. The caller decides who may (the first needs the API
    /// token; later ones an admin).
    pub fn create(&self, username: &str, password: &str, now: SystemTime) -> Result<AccountView> {
        self.create_with_role(username, password, Role::Admin, now)
    }

    pub fn create_with_role(&self, username: &str, password: &str, role: Role, now: SystemTime) -> Result<AccountView> {
        if !valid_username(username) {
            return Err(anyhow!("a username is 1 to 32 characters: lower case, digits, '.', '_' and '-'"));
        }
        check_password(password)?;
        let hashed = hash_password(password)?;
        let mut store = self.lock_store();
        if store.users.iter().any(|u| u.username == username) {
            return Err(anyhow!("that username is taken"));
        }
        store.users.push(User {
            username: username.into(),
            password: hashed,
            totp: None,
            created_unix: unix(now),
            role,
            admin_requested_unix: None,
            key_salt: Some(new_salt()?),
            accepted_terms_unix: None,
        });
        self.save(&store)?;
        Ok(view(store.users.last().expect("just pushed")))
    }

    /// Someone signing themselves up on a node that allows it: always a user,
    /// with the time they accepted the node's terms. Held to
    /// [`SIGNUPS_PER_HOUR`] across everyone and [`MAX_ACCOUNTS`] in all, since
    /// a sign-up needs nothing but a name: past either it is refused, never
    /// queued.
    pub fn sign_up(&self, username: &str, password: &str, now: SystemTime) -> Result<AccountView> {
        {
            let mut recent = self.signups.lock().unwrap_or_else(|e| e.into_inner());
            let hour_ago = Instant::now().checked_sub(Duration::from_secs(3600));
            recent.retain(|t| hour_ago.is_none_or(|h| *t > h));
            if recent.len() >= SIGNUPS_PER_HOUR {
                return Err(anyhow!("too many new accounts lately; try again in an hour"));
            }
            if self.lock_store().users.len() >= MAX_ACCOUNTS {
                return Err(anyhow!("this node takes no new accounts"));
            }
            recent.push(Instant::now());
        }
        self.create_with_role(username, password, Role::User, now)?;
        let mut store = self.lock_store();
        let user = store.users.iter_mut().find(|u| u.username == username).ok_or_else(|| anyhow!("no such account"))?;
        user.accepted_terms_unix = Some(unix(now));
        let out = view(user);
        self.save(&store)?;
        Ok(out)
    }

    /// This account's salt for its identity at a controller, made and written
    /// now if it has none yet.
    pub fn key_salt(&self, username: &str) -> Result<String> {
        let mut store = self.lock_store();
        let user = store.users.iter_mut().find(|u| u.username == username).ok_or_else(|| anyhow!("no such account"))?;
        if let Some(salt) = &user.key_salt {
            return Ok(salt.clone());
        }
        let salt = new_salt()?;
        user.key_salt = Some(salt.clone());
        self.save(&store)?;
        Ok(salt)
    }

    /// Whether `password` is this account's, for a step that needs it typed
    /// again (removing one's own account). Counts toward the same throttle as
    /// a sign-in.
    pub fn check_password_of(&self, username: &str, password: &str) -> bool {
        if self.throttled(username).is_some() {
            return false;
        }
        let hash = self.lock_store().users.iter().find(|u| u.username == username).map(|u| u.password.clone());
        let ok = verify_password(password, hash.as_deref().unwrap_or(&self.decoy)) && hash.is_some();
        if !ok {
            self.note_failure(username);
        }
        ok
    }

    /// Whether [`remove`](Self::remove) would remove this account, without
    /// removing it: for a caller with something to do first.
    pub fn check_removable(&self, username: &str) -> Result<()> {
        removable(&self.lock_store(), username)
    }

    /// Remove an account and end its sessions. The last account is kept, or
    /// nobody could sign in without the token.
    pub fn remove(&self, username: &str) -> Result<()> {
        let mut store = self.lock_store();
        removable(&store, username)?;
        store.users.retain(|u| u.username != username);
        self.save(&store)?;
        drop(store);
        self.end_sessions_of(username);
        Ok(())
    }

    /// The role of an account, if it exists.
    pub fn role(&self, username: &str) -> Option<Role> {
        self.lock_store().users.iter().find(|u| u.username == username).map(|u| u.role)
    }

    /// Make an account an admin or a user. The last admin is never made a user,
    /// or nobody could manage the node without the token. Settles any request
    /// to become an admin.
    pub fn set_role(&self, username: &str, role: Role) -> Result<AccountView> {
        let mut store = self.lock_store();
        if role == Role::User && is_last_admin(&store, username) {
            return Err(anyhow!("this is the last admin; make another account an admin first"));
        }
        let user = store.users.iter_mut().find(|u| u.username == username).ok_or_else(|| anyhow!("no such account"))?;
        user.role = role;
        user.admin_requested_unix = None;
        let out = view(user);
        self.save(&store)?;
        Ok(out)
    }

    /// A user asks to become an admin. An admin asking is refused (nothing to
    /// ask for); asking twice keeps the first time.
    pub fn request_admin(&self, username: &str, now: SystemTime) -> Result<AccountView> {
        let mut store = self.lock_store();
        let user = store.users.iter_mut().find(|u| u.username == username).ok_or_else(|| anyhow!("no such account"))?;
        if user.role == Role::Admin {
            return Err(anyhow!("this account is already an admin"));
        }
        user.admin_requested_unix.get_or_insert(unix(now));
        let out = view(user);
        self.save(&store)?;
        Ok(out)
    }

    /// An admin turns a request down: the user stays a user.
    pub fn deny_admin(&self, username: &str) -> Result<AccountView> {
        let mut store = self.lock_store();
        let user = store.users.iter_mut().find(|u| u.username == username).ok_or_else(|| anyhow!("no such account"))?;
        if user.admin_requested_unix.take().is_none() {
            return Err(anyhow!("that account has not asked to become an admin"));
        }
        let out = view(user);
        self.save(&store)?;
        Ok(out)
    }

    /// Check a sign-in and, if it holds, start a session. Returns the session
    /// token.
    pub fn login(&self, username: &str, password: &str, code: Option<&str>, now: SystemTime) -> Result<String, LoginRefusal> {
        let key = username.to_ascii_lowercase();
        if let Some(retry_after) = self.throttled(&key) {
            return Err(LoginRefusal::Throttled { retry_after });
        }
        let found = self.lock_store().users.iter().find(|u| u.username == key).cloned();
        let hash = found.as_ref().map_or(self.decoy.as_str(), |u| u.password.as_str());
        let password_ok = verify_password(password, hash) && found.is_some();
        let Some(user) = found.filter(|_| password_ok) else {
            self.note_failure(&key);
            return Err(LoginRefusal::Wrong);
        };
        if let Some(totp) = user.totp.as_ref().filter(|t| t.enabled) {
            let Some(code) = code.map(str::trim).filter(|c| !c.is_empty()) else {
                return Err(LoginRefusal::NeedsCode);
            };
            match check_code(&totp.secret, code, unix(now), totp.last_step) {
                Some(step) => self.set_last_step(&user.username, step),
                None => {
                    self.note_failure(&key);
                    return Err(LoginRefusal::WrongCode);
                }
            }
        }
        self.failures.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        Ok(self.start_session(&user.username))
    }

    fn throttled(&self, key: &str) -> Option<Duration> {
        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let list = failures.entry(key.to_string()).or_default();
        list.retain(|t| t.elapsed() < FAILURE_WINDOW);
        if list.len() >= MAX_FAILURES {
            let latest = *list.iter().max().expect("non-empty");
            let since = latest.elapsed();
            if since < LOCKOUT {
                return Some(LOCKOUT - since);
            }
        }
        None
    }

    fn note_failure(&self, key: &str) {
        self.failures.lock().unwrap_or_else(|e| e.into_inner()).entry(key.to_string()).or_default().push(Instant::now());
    }

    fn set_last_step(&self, username: &str, step: u64) {
        let mut store = self.lock_store();
        if let Some(t) = store.users.iter_mut().find(|u| u.username == username).and_then(|u| u.totp.as_mut()) {
            t.last_step = step;
        }
        if let Err(e) = self.save(&store) {
            tracing::warn!(error = %format!("{e:#}"), "could not record a used sign-in code");
        }
    }

    fn start_session(&self, username: &str) -> String {
        let mut bytes = [0u8; 32];
        // getrandom failing is a broken host; a session must never be guessable.
        getrandom::getrandom(&mut bytes).expect("no entropy for a session token");
        let token = format!("s.{}", hex::encode(bytes));
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.retain(|_, s| s.expires > Instant::now());
        sessions.insert(token.clone(), Session { username: username.into(), expires: Instant::now() + SESSION_LIFETIME });
        token
    }

    /// The account a session token belongs to, if it is live.
    pub fn session(&self, token: &str) -> Option<String> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get(token).filter(|s| s.expires > Instant::now()).map(|s| s.username.clone())
    }

    pub fn end_session(&self, token: &str) {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(token);
    }

    fn end_sessions_of(&self, username: &str) {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, s| s.username != username);
    }

    /// Change a password, after checking the current one. Every other session
    /// of the account ends, so a stolen session does not outlive the change.
    pub fn change_password(&self, username: &str, current: &str, new: &str, keep: Option<&str>) -> Result<()> {
        check_password(new)?;
        let hashed = hash_password(new)?;
        let mut store = self.lock_store();
        let user = store.users.iter_mut().find(|u| u.username == username).ok_or_else(|| anyhow!("no such account"))?;
        if !verify_password(current, &user.password) {
            return Err(anyhow!("the current password is not right"));
        }
        user.password = hashed;
        self.save(&store)?;
        drop(store);
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|token, s| s.username != username || Some(token.as_str()) == keep);
        Ok(())
    }

    /// Start setting up codes: a fresh secret, not yet in force. Returns it in
    /// Base32 with the `otpauth://` address and a QR code of it as SVG.
    pub fn begin_totp(&self, username: &str, issuer: &str) -> Result<(String, String, String)> {
        let mut store = self.lock_store();
        let user = store.users.iter_mut().find(|u| u.username == username).ok_or_else(|| anyhow!("no such account"))?;
        if user.totp.as_ref().is_some_and(|t| t.enabled) {
            return Err(anyhow!("codes are already on; turn them off first"));
        }
        let mut secret = [0u8; 20];
        getrandom::getrandom(&mut secret).map_err(|e| anyhow!("no entropy: {e}"))?;
        let secret = base32_encode(&secret);
        user.totp = Some(Totp { secret: secret.clone(), enabled: false, last_step: 0 });
        self.save(&store)?;
        let label = format!("{issuer}:{username}");
        let uri = format!(
            "otpauth://totp/{}?secret={secret}&issuer={}&algorithm=SHA1&digits={TOTP_DIGITS}&period={TOTP_STEP}",
            url_escape(&label),
            url_escape(issuer)
        );
        let svg = qrcode::QrCode::new(uri.as_bytes())
            .map_err(|e| anyhow!("making the QR code: {e}"))?
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(200, 200)
            .build();
        Ok((secret, uri, svg))
    }

    /// Turn codes on, once a code from the new secret has been typed back.
    pub fn enable_totp(&self, username: &str, code: &str, now: SystemTime) -> Result<()> {
        let mut store = self.lock_store();
        let user = store.users.iter_mut().find(|u| u.username == username).ok_or_else(|| anyhow!("no such account"))?;
        let totp = user.totp.as_mut().ok_or_else(|| anyhow!("start setting up codes first"))?;
        if totp.enabled {
            return Err(anyhow!("codes are already on"));
        }
        let step = check_code(&totp.secret, code, unix(now), 0).ok_or_else(|| anyhow!("that code is not right"))?;
        totp.enabled = true;
        totp.last_step = step;
        self.save(&store)
    }

    /// Turn codes off, with a current code.
    pub fn disable_totp(&self, username: &str, code: &str, now: SystemTime) -> Result<()> {
        let mut store = self.lock_store();
        let user = store.users.iter_mut().find(|u| u.username == username).ok_or_else(|| anyhow!("no such account"))?;
        let totp = user.totp.as_ref().filter(|t| t.enabled).ok_or_else(|| anyhow!("codes are not on"))?;
        check_code(&totp.secret, code, unix(now), totp.last_step).ok_or_else(|| anyhow!("that code is not right"))?;
        user.totp = None;
        self.save(&store)
    }
}

fn view(u: &User) -> AccountView {
    AccountView {
        username: u.username.clone(),
        totp_enabled: u.totp.as_ref().is_some_and(|t| t.enabled),
        created_unix: u.created_unix,
        role: u.role,
        admin_requested_unix: u.admin_requested_unix,
        accepted_terms_unix: u.accepted_terms_unix,
    }
}

fn new_salt() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("no entropy: {e}"))?;
    Ok(hex::encode(bytes))
}

fn removable(store: &Store, username: &str) -> Result<()> {
    if !store.users.iter().any(|u| u.username == username) {
        return Err(anyhow!("no such account"));
    }
    if store.users.len() == 1 {
        return Err(anyhow!("this is the last account; add another before removing it"));
    }
    if is_last_admin(store, username) {
        return Err(anyhow!("this is the last admin; make another account an admin first"));
    }
    Ok(())
}

fn is_last_admin(store: &Store, username: &str) -> bool {
    store.users.iter().any(|u| u.username == username && u.role == Role::Admin)
        && store.users.iter().filter(|u| u.role == Role::Admin).count() == 1
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn check_password(p: &str) -> Result<()> {
    if p.chars().count() < MIN_PASSWORD_LEN {
        return Err(anyhow!("a password is at least {MIN_PASSWORD_LEN} characters"));
    }
    if p.len() > 1024 {
        return Err(anyhow!("that password is too long"));
    }
    Ok(())
}

fn hash_password(p: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(p.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("hashing a password: {e}"))
}

fn verify_password(p: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| Argon2::default().verify_password(p.as_bytes(), &parsed).is_ok())
}

/// HMAC-SHA1 (RFC 2104), which TOTP is defined over.
fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha1::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha1::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// The code for one 30-second step (RFC 6238, SHA-1, 6 digits).
fn totp_at(secret: &[u8], step: u64) -> u32 {
    let mac = hmac_sha1(secret, &step.to_be_bytes());
    let offset = (mac[19] & 0x0f) as usize;
    let bin = u32::from_be_bytes([mac[offset] & 0x7f, mac[offset + 1], mac[offset + 2], mac[offset + 3]]);
    bin % 10u32.pow(TOTP_DIGITS)
}

/// The step a code matches, one step either side of now for a clock that has
/// drifted, and only a step after `after` (a code is good once).
fn check_code(secret_b32: &str, code: &str, now_unix: u64, after: u64) -> Option<u64> {
    let code: u32 = code.trim().replace(' ', "").parse().ok()?;
    let secret = base32_decode(secret_b32)?;
    let now = now_unix / TOTP_STEP;
    [now.saturating_sub(1), now, now + 1]
        .into_iter()
        .filter(|&step| step > after)
        .find(|&step| totp_at(&secret, step) == code)
}

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &b in data {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buffer >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text.bytes().filter(|c| *c != b'=') {
        let v = B32.iter().position(|&x| x == c.to_ascii_uppercase())? as u32;
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buffer >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B, SHA-1 rows (8 digits there; the last 6 here).
    #[test]
    fn codes_match_the_rfc_vectors() {
        let secret = b"12345678901234567890";
        for (time, eight) in [(59u64, 94287082u32), (1111111109, 7081804), (1111111111, 14050471), (1234567890, 89005924), (2000000000, 69279037)] {
            assert_eq!(totp_at(secret, time / 30), eight % 1_000_000, "t={time}");
        }
        // RFC 2202 test case 2.
        assert_eq!(hex::encode(hmac_sha1(b"Jefe", b"what do ya want for nothing?")), "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
    }

    #[test]
    fn base32_round_trips_and_matches_rfc4648() {
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        assert_eq!(base32_decode("mzxw6ytboi======").unwrap(), b"foobar");
        assert!(base32_decode("not!base32").is_none());
    }

    fn accounts() -> (tempfile::TempDir, Accounts) {
        let dir = tempfile::tempdir().unwrap();
        let a = Accounts::load(&dir.path().join("web-accounts.json")).unwrap();
        (dir, a)
    }

    /// A wrong name and a wrong password are the same answer; a right one
    /// starts a session that sign-out ends; the file is owner-only and holds
    /// no password in clear.
    #[test]
    fn a_sign_in_starts_a_session_and_a_wrong_name_reads_like_a_wrong_password() {
        let (dir, a) = accounts();
        let now = SystemTime::now();
        assert!(a.create("op", "short", now).is_err(), "too short");
        assert!(a.create("Op Name", "long enough pass", now).is_err(), "not a username");
        a.create("op", "correct horse battery", now).unwrap();
        assert!(a.create("op", "another long password", now).is_err(), "taken");

        assert_eq!(a.login("op", "wrong password!!", None, now), Err(LoginRefusal::Wrong));
        assert_eq!(a.login("nobody", "correct horse battery", None, now), Err(LoginRefusal::Wrong));
        let token = a.login("op", "correct horse battery", None, now).unwrap();
        assert_eq!(a.session(&token).as_deref(), Some("op"));
        a.end_session(&token);
        assert_eq!(a.session(&token), None);

        let path = dir.path().join("web-accounts.json");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("correct horse"), "a password was stored in clear");
        assert!(text.contains("$argon2id$"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let again = Accounts::load(&path).unwrap();
        assert!(again.login("op", "correct horse battery", None, now).is_ok(), "kept across a restart");
    }

    #[test]
    fn too_many_wrong_tries_lock_the_name_out_even_with_the_right_password() {
        let (_dir, a) = accounts();
        let now = SystemTime::now();
        a.create("op", "correct horse battery", now).unwrap();
        for _ in 0..MAX_FAILURES {
            assert_eq!(a.login("op", "nope nope nope", None, now), Err(LoginRefusal::Wrong));
        }
        assert!(matches!(a.login("op", "correct horse battery", None, now), Err(LoginRefusal::Throttled { .. })));
    }

    /// Codes are on only once one has been typed back; then a sign-in needs a
    /// code, a code is good once, and turning them off needs a current one.
    #[test]
    fn a_code_is_needed_once_codes_are_on_and_each_code_is_good_once() {
        let (_dir, a) = accounts();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        a.create("op", "correct horse battery", t0).unwrap();
        let (secret, uri, svg) = a.begin_totp("op", "Lanthorn Host").unwrap();
        assert!(uri.starts_with("otpauth://totp/Lanthorn%20Host%3Aop?secret="), "{uri}");
        assert!(svg.contains("<svg"));
        assert!(a.login("op", "correct horse battery", None, t0).is_ok(), "not in force until confirmed");

        let raw = base32_decode(&secret).unwrap();
        let code = |t: SystemTime| format!("{:06}", totp_at(&raw, unix(t) / 30));
        assert!(a.enable_totp("op", "000000", t0).is_err() || code(t0) == "000000");
        a.enable_totp("op", &code(t0), t0).unwrap();
        assert!(a.get("op").unwrap().totp_enabled);

        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(a.login("op", "correct horse battery", None, t1), Err(LoginRefusal::NeedsCode));
        assert!(a.login("op", "correct horse battery", Some(&code(t1)), t1).is_ok());
        assert_eq!(
            a.login("op", "correct horse battery", Some(&code(t1)), t1),
            Err(LoginRefusal::WrongCode),
            "the same code twice"
        );

        let t2 = t1 + Duration::from_secs(60);
        assert!(a.disable_totp("op", "123", t2).is_err());
        a.disable_totp("op", &code(t2), t2).unwrap();
        assert!(a.login("op", "correct horse battery", None, t2).is_ok());
    }

    /// A sign-up is always a user, with the time the terms were accepted, and
    /// sign-ups stop at the hourly limit across everyone. Each account's salt
    /// is made once and kept; one from before salts gets one when asked; a
    /// name taken again after removal gets a new one.
    #[test]
    fn a_sign_up_is_a_user_and_each_account_keeps_a_salt_of_its_own() {
        let (dir, a) = accounts();
        let now = SystemTime::now();
        a.create("admin", "correct horse battery", now).unwrap();
        let made = a.sign_up("player", "correct horse battery", now).unwrap();
        assert_eq!(made.role, Role::User);
        assert!(made.accepted_terms_unix.is_some());
        for i in 1..SIGNUPS_PER_HOUR {
            a.sign_up(&format!("p{i}"), "correct horse battery", now).unwrap();
        }
        assert!(a.sign_up("one-too-many", "correct horse battery", now).is_err());

        let salt = a.key_salt("player").unwrap();
        let again = Accounts::load(&dir.path().join("web-accounts.json")).unwrap();
        assert_eq!(again.key_salt("player").unwrap(), salt, "a salt changed across a restart");
        again.remove("player").unwrap();
        again.create_with_role("player", "correct horse battery", Role::User, now).unwrap();
        assert_ne!(again.key_salt("player").unwrap(), salt, "a name taken again kept the old account's salt");

        assert!(again.check_password_of("admin", "correct horse battery"));
        assert!(!again.check_password_of("admin", "wrong wrong wrong"));
        assert!(!again.check_password_of("nobody", "correct horse battery"));
    }

    #[test]
    fn a_password_change_ends_the_other_sessions_and_the_last_account_stays() {
        let (_dir, a) = accounts();
        let now = SystemTime::now();
        a.create("op", "correct horse battery", now).unwrap();
        let mine = a.login("op", "correct horse battery", None, now).unwrap();
        let other = a.login("op", "correct horse battery", None, now).unwrap();
        assert!(a.change_password("op", "wrong current pw", "a new long password", Some(&mine)).is_err());
        a.change_password("op", "correct horse battery", "a new long password", Some(&mine)).unwrap();
        assert!(a.session(&mine).is_some(), "the session that changed it stays");
        assert!(a.session(&other).is_none(), "another session ended");
        assert!(a.remove("op").is_err(), "the last account");
        a.create("second", "another long password", now).unwrap();
        a.remove("op").unwrap();
        assert!(a.session(&mine).is_none(), "a removed account's sessions end");
    }

    /// A user asks to become an admin and an admin decides; the last admin is
    /// never made a user or removed; an account written before roles existed
    /// reads as an admin, so an upgrade locks nobody out.
    #[test]
    fn a_user_becomes_an_admin_only_by_an_admin_and_the_last_admin_stays() {
        let (dir, a) = accounts();
        let now = SystemTime::now();
        a.create("op", "correct horse battery", now).unwrap();
        a.create_with_role("guest", "another long password", Role::User, now).unwrap();
        assert_eq!(a.role("guest"), Some(Role::User));
        assert!(a.request_admin("op", now).is_err(), "an admin has nothing to ask for");
        let asked = a.request_admin("guest", now).unwrap();
        assert!(asked.admin_requested_unix.is_some());
        a.deny_admin("guest").unwrap();
        assert!(a.get("guest").unwrap().admin_requested_unix.is_none());
        assert!(a.deny_admin("guest").is_err(), "nothing left to deny");

        assert!(a.set_role("op", Role::User).is_err(), "the last admin");
        a.request_admin("guest", now).unwrap();
        let promoted = a.set_role("guest", Role::Admin).unwrap();
        assert_eq!(promoted.role, Role::Admin);
        assert!(promoted.admin_requested_unix.is_none(), "promotion settles the request");
        a.set_role("op", Role::User).unwrap();
        assert!(a.remove("guest").is_err(), "now guest is the last admin");

        // An account file from before roles: every account was an admin then.
        let path = dir.path().join("old.json");
        std::fs::write(&path, br#"{"users":[{"username":"old","password":"$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaA","created_unix":1}]}"#).unwrap();
        assert_eq!(Accounts::load(&path).unwrap().role("old"), Some(Role::Admin));
    }

    #[test]
    fn an_accounts_file_that_does_not_parse_is_an_error_not_no_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-accounts.json");
        std::fs::write(&path, b"{oops").unwrap();
        assert!(Accounts::load(&path).is_err());
    }
}
