//! Reviewing a game pack before it is shared (`game-bridge review`).
//!
//! A pack that parses is already held to the format's ceiling: it names no
//! program, no image and no console line, its launch arguments are a template
//! spawned without a shell, and its paths, ports and art are validated as data
//! (`pack.rs`, `launch.rs`, `content.rs`). **That ceiling is the safety
//! property, not this module.** Intent is not in the syntax, so no check here
//! can promise a pack is harmless, and nothing should say it does.
//!
//! What this adds is the next layer down: things the format allows, because a
//! real game needs them, that a person reviewing a contribution must look at,
//! and a few that no honest pack needs at all.
//!
//! - **Refuse** is for a pack that could only be meant to mislead or harm:
//!   a launch argument that runs console commands or sends a player elsewhere,
//!   a LAN port that is a remote-access service on every player's machine,
//!   invisible characters in a name, a name or wire `app_name` already taken
//!   by another game. CI fails on these, `game-bridge repo build` will not
//!   list such a pack, and neither a launcher nor a node installs one from a
//!   repository.
//! - **Review** is for what a person must read before it is merged: an
//!   archive download (its files run inside game servers, and nothing here
//!   sees into it), a Steam app no other pack uses, a link in the notes, a
//!   LAN room that admits every port, an unfamiliar launch argument.
//!
//! Every check finds its subject by property, never by a game's id.

use crate::content::{PackContent, SteamPlatform};
use crate::lan_filter;
use crate::pack::{GamePack, PackLanInbound};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A person must read this before the pack is merged or installed.
    Review,
    /// No honest pack needs this; refused.
    Refuse,
}

impl Severity {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Refuse => "refuse",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// The pack field it is about, as written in the TOML.
    pub field: String,
    pub message: String,
}

impl core::fmt::Display for Finding {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}: {}", self.severity.label(), self.field, self.message)
    }
}

/// Launch argument heads both engines read as a console command that changes
/// the player's own setup, runs a file, or talks to a server's admin
/// interface. No pack needs one to point a game at a server.
const REFUSED_COMMANDS: &[&str] = &[
    "+exec",
    "+alias",
    "+bind",
    "+unbind",
    "+unbindall",
    "+rcon",
    "+rcon_password",
    "+rcon_address",
    "+writecfg",
    "+host_writeconfig",
    "+cl_allowdownload",
    "+cl_allowupload",
    "+cl_downloadfilter",
    "+cl_dlmax",
    "+setinfo",
    "+con_logfile",
    "+condump",
    "+quit",
    "+exit",
    "+sv_cheats",
    "-condebug",
    "-insecure",
    "-dev",
    "-toconsole",
    "-userdir",
    "-basedir",
];

/// Launch argument heads a pack is expected to carry.
const FAMILIAR_ARGS: &[&str] = &[
    "+connect",
    "+password",
    "+name",
    "-game",
    "-novid",
    "-console",
    "-windowed",
    "-fullscreen",
    "-steam",
    "-nojoy",
    "-noipx",
];

/// Characters that make text read differently from what it is: bidirectional
/// overrides, zero-width joiners and spaces, the soft hyphen, a byte order
/// mark. A name carrying one can look like another game's.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
        | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}' | '\u{3164}' | '\u{FEFF}' | '\u{FFA0}')
}

/// Letters and digits only, lowercased: what a person sees when reading a name.
fn looks_like(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn has_link(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("://") || t.contains("www.")
}

/// Whether `token` reads as a fixed network address: an IPv4 address, or a
/// `host:port`. The only address a launch argument may hold is the
/// launcher's own `{address}`.
fn names_an_address(token: &str) -> bool {
    if token.contains('{') {
        return false;
    }
    let host = token.rsplit_once(':').map_or(token, |(h, p)| {
        if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            token
        }
    });
    let ipv4 = host.split('.').count() == 4
        && host
            .split('.')
            .all(|o| !o.is_empty() && o.len() <= 3 && o.chars().all(|c| c.is_ascii_digit()));
    ipv4 || (host != token && host.contains('.'))
}

/// Everything a reviewer should know about `pack`, most severe first.
/// `others` are the packs it would sit beside (a repository's, or the shipped
/// set); one with the same id is the version it replaces.
pub fn review(pack: &GamePack, others: &[GamePack]) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut add = |severity, field: &str, message: String| {
        out.push(Finding { severity, field: field.to_string(), message })
    };
    use Severity::{Refuse, Review};

    // Text a person reads.
    let mut texts: Vec<(&str, &str)> = vec![("id", &pack.id), ("display_name", &pack.display_name)];
    if let Some(n) = &pack.notes {
        texts.push(("notes", n));
    }
    if let PackContent::Manual { note: Some(n) } = &pack.content {
        texts.push(("content.note", n));
    }
    for p in &pack.extra_ports {
        texts.push(("extra_ports.name", &p.name));
    }
    for (field, text) in &texts {
        if text.chars().any(is_invisible) {
            add(Refuse, field, "holds invisible or text-direction characters, which make it read as something it is not".into());
        }
        let multiline = matches!(*field, "notes" | "content.note");
        if text.chars().any(|c| c.is_control() && !(multiline && matches!(c, '\n' | '\t' | '\r'))) {
            add(Refuse, field, "holds control characters".into());
        }
        if multiline && has_link(text) {
            add(
                Review,
                field,
                "contains a link; check where it goes before players are sent there".into(),
            );
        }
    }
    if !pack.display_name.is_ascii() {
        add(
            Review,
            "display_name",
            "uses non-ASCII letters; check it is not imitating another game's name".into(),
        );
    }

    // Other games' names.
    for other in others {
        if other.id == pack.id {
            add(
                Review,
                "id",
                format!("replaces the existing pack {:?}; read the whole change", other.id),
            );
            continue;
        }
        if looks_like(&other.display_name) == looks_like(&pack.display_name) {
            add(
                Refuse,
                "display_name",
                format!("reads as {:?}, another pack's game ({:?})", other.display_name, other.id),
            );
        }
        if other.app_name == pack.app_name {
            add(
                Refuse,
                "app_name",
                format!(
                    "is {:?}'s app_name; servers of the two games would be mistaken for each other",
                    other.id
                ),
            );
        }
    }

    // Launch arguments: the player's own machine.
    if let Some(launch) = &pack.launch {
        for template in &launch.args {
            let tokens: Vec<&str> = template.split_whitespace().collect();
            let head = tokens.first().copied().unwrap_or_default();
            let head_lc = head.to_ascii_lowercase();
            let field = "launch.args";
            if template.contains([';', '"', '\'', '\n']) {
                add(Refuse, field, format!("{template:?} could chain a console command (; or a quote); an argument is one value"));
            } else if REFUSED_COMMANDS.contains(&head_lc.as_str()) {
                add(Refuse, field, format!("{template:?} runs {head} on the player's game; nothing needs it to join a server"));
            } else if tokens.iter().any(|t| t.contains("://") || names_an_address(t)) {
                add(Refuse, field, format!("{template:?} names a fixed address; a game is only ever pointed at the launcher's {{address}}"));
            } else if head_lc == "+connect" && template.trim() != "+connect {address}" {
                add(
                    Refuse,
                    field,
                    format!("{template:?}: +connect takes {{address}} and nothing else"),
                );
            } else if template.contains("{password}") && template.trim() != "+password {password}" {
                add(
                    Refuse,
                    field,
                    format!("{template:?} puts the server password somewhere other than +password"),
                );
            } else if !FAMILIAR_ARGS.contains(&head_lc.as_str()) {
                add(Review, field, format!("{template:?} is not an argument packs usually carry; check what {head} does in this game"));
            }
        }
    }

    // Where a node gets the game's files.
    let known_apps: Vec<u32> = others
        .iter()
        .filter(|o| o.id != pack.id)
        .filter_map(|o| match o.content {
            PackContent::Steamcmd { app_id, .. } => Some(app_id),
            _ => None,
        })
        .collect();
    match &pack.content {
        PackContent::Archive { url, .. } => {
            let host = url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url);
            add(
                Review,
                "content.url",
                format!(
                    "downloads an archive from {host}. Its files run inside game servers on every node that installs this game, \
                     and nothing here can see into it: download it, check its sha256 and read what it holds"
                ),
            );
            if url.starts_with("http://") {
                add(Review, "content.url", "is plain http; the digest still decides, but https keeps the download private".into());
            }
        }
        PackContent::Steamcmd { app_id, .. } if !known_apps.contains(app_id) => add(
            Review,
            "content.app_id",
            format!("Steam app {app_id} is used by no other pack; check it is this game's dedicated server (steamdb.info/app/{app_id})"),
        ),
        _ => {}
    }
    if let PackContent::Steamcmd { platform: SteamPlatform::Windows, .. } = &pack.content {
        add(
            Review,
            "content.platform",
            "fetches the Windows server, which runs under Wine on a node that chose a Wine image: check the Linux build really does not work, and that it was run under Wine".into(),
        );
    }

    // Ports.
    if let Some(lan) = &pack.lan {
        for p in &lan.ports {
            if !lan_filter::is_allowable(p.port) {
                add(
                    Refuse,
                    "lan.ports",
                    format!("{} is a system or remote-access port; a room would let every member reach it on every player's machine", p.port),
                );
            }
        }
        if lan.inbound == PackLanInbound::Any {
            add(Review, "lan.inbound", "\"any\" lets room members reach every port on each other's machines; only for a game whose ports cannot be listed".into());
        }
    }
    for (field, port) in std::iter::once(("default_port", pack.default_port))
        .chain(pack.extra_ports.iter().map(|p| ("extra_ports.port", p.port)))
    {
        if port < lan_filter::LOWEST_ALLOWABLE_PORT {
            add(
                Review,
                field,
                format!("{port} is a system port; no game server is expected there"),
            );
        }
    }

    // Writable paths are empty directories mounted over the shared content.
    for w in &pack.writable_paths {
        let w = w.trim_end_matches('/');
        let hides_maps = pack.maps_dir.as_deref().is_some_and(|m| {
            let m = m.trim_end_matches('/');
            m == w || m.starts_with(&format!("{w}/"))
        });
        if hides_maps || w.ends_with("/cfg") || w == "cfg" {
            add(Review, "writable_paths", format!("{w:?} is mounted empty over the game's own files there (maps or config), which hides them"));
        }
    }

    out.sort_by(|a, b| b.severity.cmp(&a.severity));
    out
}

/// The refusals in `findings`, as one sentence for an install that stops.
pub fn refusal(findings: &[Finding]) -> Option<String> {
    let refused: Vec<String> = findings
        .iter()
        .filter(|f| f.severity == Severity::Refuse)
        .map(|f| format!("{}: {}", f.field, f.message))
        .collect();
    (!refused.is_empty()).then(|| format!("this pack is refused: {}", refused.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped() -> Vec<GamePack> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../packs");
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().and_then(|e| e.to_str()) == Some("toml") {
                out.push(GamePack::parse(&std::fs::read_to_string(&p).unwrap()).unwrap());
            }
        }
        assert!(out.len() > 3);
        out
    }

    /// A pack with a launch block, found by property, to change one thing in.
    fn base() -> (String, GamePack) {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../packs");
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap();
            let pack = GamePack::parse(&src).unwrap();
            if pack.launch.as_ref().is_some_and(|l| !l.args.is_empty()) && pack.lan.is_none() {
                return (src, pack);
            }
        }
        panic!("no shipped pack has launch arguments");
    }

    fn refused(pack: &GamePack, others: &[GamePack]) -> Vec<Finding> {
        review(pack, others).into_iter().filter(|f| f.severity == Severity::Refuse).collect()
    }

    /// The shipped packs are the honest baseline: none is refused against the
    /// rest. A check that refuses one of them is a check that is wrong.
    #[test]
    fn no_shipped_pack_is_refused() {
        let all = shipped();
        for pack in &all {
            let others: Vec<GamePack> = all.iter().filter(|o| o.id != pack.id).cloned().collect();
            let r = refused(pack, &others);
            assert!(r.is_empty(), "{}: {r:?}", pack.id);
        }
    }

    /// What a hostile launch block would try: run a config file, chain a
    /// command, send the player to someone's own server, or leak the password.
    #[test]
    fn a_launch_argument_that_does_more_than_join_is_refused() {
        let (_, pack) = base();
        for arg in [
            "+exec autoexec.cfg",
            "+connect {address};+exec evil",
            "+connect 203.0.113.5:27015",
            "+connect evil.example:27015",
            "+connect {address} +password hunter2",
            "+say {password}",
            "+BIND w quit",
            "-insecure",
            "-game http://evil.example/x",
        ] {
            let mut p = pack.clone();
            p.launch.as_mut().unwrap().args.push(arg.into());
            assert!(!refused(&p, &[]).is_empty(), "{arg} was not refused");
        }
        let mut p = pack.clone();
        p.launch.as_mut().unwrap().args.push("-high".into());
        let r = review(&p, &[]);
        assert!(
            r.iter().any(|f| f.severity == Severity::Review && f.field == "launch.args"),
            "{r:?}"
        );
        assert!(refused(&p, &[]).is_empty());
    }

    /// A pack cannot pass itself off as another game, by name or on the wire,
    /// and a name cannot hide characters that change how it reads.
    #[test]
    fn a_pack_imitating_another_game_is_refused() {
        let all = shipped();
        let (_, pack) = base();
        let victim = all.iter().find(|o| o.id != pack.id).unwrap();
        let mut p = pack.clone();
        p.display_name = format!(" {} ", victim.display_name.to_uppercase());
        assert!(refused(&p, &all).iter().any(|f| f.field == "display_name"));
        let mut p = pack.clone();
        p.app_name = victim.app_name.clone();
        assert!(refused(&p, &all).iter().any(|f| f.field == "app_name"));
        let mut p = pack.clone();
        p.display_name = "Half\u{202E}efiL".into();
        assert!(refused(&p, &[]).iter().any(|f| f.field == "display_name"));
    }

    /// A room that would open a remote-access service is refused; one that
    /// opens every port, an archive and an unfamiliar Steam app are left to a
    /// person, said plainly.
    #[test]
    fn what_a_reviewer_must_read_is_named_and_a_remote_service_port_is_refused() {
        let all = shipped();
        let lan = all.iter().find(|p| p.lan.is_some()).unwrap();
        let mut p = lan.clone();
        p.lan.as_mut().unwrap().ports[0].port = 3389;
        assert!(refused(&p, &[]).iter().any(|f| f.field == "lan.ports"));

        let (_, mut p) = base();
        p.content = PackContent::Archive {
            url: "https://example.com/a.tar.gz".into(),
            sha256: "a".repeat(64),
            strip_components: 0,
        };
        p.notes = Some("get the map pack at https://example.com".into());
        let r = review(&p, &all);
        assert!(
            r.iter().any(|f| f.field == "content.url" && f.message.contains("example.com")),
            "{r:?}"
        );
        assert!(r.iter().any(|f| f.field == "notes"), "{r:?}");
        assert!(refused(&p, &[]).is_empty());

        let (_, mut p) = base();
        p.content = PackContent::Steamcmd { app_id: 4_000_000, mod_dir: None, platform: Default::default() };
        assert!(review(&p, &all).iter().any(|f| f.field == "content.app_id"));
    }
}
