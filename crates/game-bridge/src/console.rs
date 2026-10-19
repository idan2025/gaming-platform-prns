//! Telling a running game server to do something, and the one thing this build
//! knows how to tell it: change the map.
//!
//! # A pack names a protocol; this file owns the words
//!
//! Same rule as `content.rs`'s drivers and `launch.rs`'s `kind`: a pack selects
//! an enum variant **this build implements** and supplies typed parameters. It
//! never supplies the command text. A pack that could write the console line
//! would be a pack that could type anything at a dedicated server's console on
//! somebody else's node — `rcon_password`, `exec`, `quit` — which is naming
//! what runs by another route (`pack.rs`, module docs).
//!
//! So the pack says "this is a GoldSrc console" and gets `changelevel <map>`.
//!
//! # The map name is the only thing that crosses from a caller
//!
//! It is interpolated into a console line and into a container's environment,
//! so it is validated as data before either. [`validate_map_name`] is
//! deliberately a small allowlist rather than a search for bad characters:
//!
//! * **No newline** — a console reads one command per line, so a map name
//!   carrying `\n` is a second command the caller did not admit to sending.
//!   This is the whole reason the function exists.
//! * **No whitespace or quotes**, so the name cannot become two arguments.
//! * **No `..` and no leading `/`**, because the same string reaches a game
//!   that resolves it against its own content directory.
//!
//! There is no escaping path and no "sanitize by replacement": a name that does
//! not pass is refused, because a silently rewritten map name would load the
//! wrong map and look like a game bug.

use serde::{Deserialize, Serialize};

/// Longest map name accepted. Comfortably past every real one — Sven Co-op's
/// longest shipped map is 18 characters — and short enough that the console
/// line stays a console line.
pub const MAX_MAP_NAME_LEN: usize = 64;

/// A game console this build knows how to talk to.
///
/// The two variants issue the same command today. They are still two variants:
/// they are two engines, an operator reading a pack should see which one their
/// game is, and the day one of them needs different words this file changes and
/// no pack does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleProtocol {
    /// Half-Life 1 engine: Sven Co-op, Counter-Strike 1.6, Half-Life.
    Goldsrc,
    /// Source engine: Team Fortress 2 and its siblings.
    Source,
    /// Minecraft: Java Edition's server console (`MINECRAFT.md` §4). It reads
    /// commands from standard input, one per line, like the others.
    Minecraft,
    /// Minecraft: Bedrock Edition's dedicated server (`MINECRAFT.md` §16):
    /// the phone, console and Windows edition. Also stdin, with fewer
    /// commands: it has no ban.
    Bedrock,
}

impl ConsoleProtocol {
    /// The console line that changes the map, or why the name was refused.
    pub fn change_map(&self, map: &str) -> Result<String, MapNameError> {
        if !self.has_maps() {
            return Err(MapNameError::NoMaps);
        }
        validate_map_name(map)?;
        Ok(match self {
            // `changelevel`, not `map`: `map` restarts the server and drops
            // every player, which is the opposite of changing the map on a
            // live server. Both engines spell it the same way.
            Self::Goldsrc | Self::Source => format!("changelevel {map}"),
            Self::Minecraft | Self::Bedrock => unreachable!("refused above"),
        })
    }

    /// Whether this game has maps a console can change between. A Minecraft
    /// server has one world.
    pub fn has_maps(&self) -> bool {
        !matches!(self, Self::Minecraft | Self::Bedrock)
    }

    /// Lines to send before copying a running server's files, so what is on
    /// disk is whole (`MINECRAFT.md` §7). Empty for a game with no world that
    /// is written while it runs.
    pub fn before_backup(&self) -> &'static [&'static str] {
        match self {
            // Saving off stops the server writing chunks under the copy;
            // `flush` makes the save it does now finish before it answers.
            Self::Minecraft => &["save-off", "save-all flush"],
            // Asynchronous: the server prepares, and says so to `save query`
            // ([`backup_poll`](Self::backup_poll)).
            Self::Bedrock => &["save hold"],
            Self::Goldsrc | Self::Source => &[],
        }
    }

    /// Lines to send after the copy, whether it succeeded or not.
    pub fn after_backup(&self) -> &'static [&'static str] {
        match self {
            Self::Minecraft => &["save-on"],
            Self::Bedrock => &["save resume"],
            Self::Goldsrc | Self::Source => &[],
        }
    }

    /// A line to send again while waiting for the server to be ready to be
    /// copied, if the game needs one asked, and the log text that says it is
    /// ready.
    pub fn backup_ready(&self) -> Option<(Option<&'static str>, &'static str)> {
        match self {
            Self::Minecraft => Some((None, "Saved the game")),
            Self::Bedrock => Some((Some("save query"), "Data saved")),
            Self::Goldsrc | Self::Source => None,
        }
    }

    /// The console line that shows `message` to everyone on the server. Its
    /// text is held to the same rule as a server name, the free text that
    /// already reaches a console: no `;`, `"`, `+` or control character can
    /// make it a second command.
    pub fn say(&self, message: &str) -> Result<String, ServerNameError> {
        validate_server_name(message)?;
        Ok(match self {
            // A server console's `say` reaches every player's chat on every
            // game here.
            Self::Goldsrc | Self::Source | Self::Minecraft | Self::Bedrock => format!("say {message}"),
        })
    }

    /// The console line that makes the server print how many players are on
    /// it, for a game whose status cannot be asked over the network. Bedrock
    /// answers no status query (NetherNet), so its count comes from here;
    /// the answer is a log line, read by [`parse_player_count`].
    pub fn player_count_line(&self) -> Option<&'static str> {
        match self {
            Self::Bedrock => Some("list"),
            Self::Goldsrc | Self::Source | Self::Minecraft => None,
        }
    }
}

/// `(online, max)` from a server log line that answers a player-count line,
/// such as Bedrock's `There are 1/10 players online:`. Any other line is
/// `None`, never a guess.
pub fn parse_player_count(log_line: &str) -> Option<(u32, u32)> {
    let rest = log_line.split("There are ").nth(1)?;
    let (online, rest) = rest.split_once('/')?;
    let (max, rest) = rest.split_once(' ')?;
    if !rest.starts_with("players online") {
        return None;
    }
    Some((online.parse().ok()?, max.parse().ok()?))
}

/// Most bots a caller may ask for. A GoldSrc server holds 32 slots including
/// humans, so anything past this is a typo rather than a request.
pub const MAX_BOTS: u8 = 32;

/// A bot implementation this build knows how to drive over a game's console.
///
/// Same seam as [`ConsoleProtocol`]: a pack names the variant, this file owns
/// the words. There is exactly one today, and it is the one Valve ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BotProtocol {
    /// Valve's Z-Bot, as shipped with Condition Zero. Present in the
    /// Counter-Strike server library too, and inert there: the code is gated on
    /// an `isCZero` flag, so `bot_add` on a `cstrike` server does nothing at
    /// all — no bot, no error. A pack for a game whose mod is not `czero` must
    /// not declare it.
    Zbot,
    /// YaPB, a third-party bot for Counter-Strike 1.6 and Condition Zero.
    ///
    /// **A pack can never name this one** (`pack.rs`'s `PackBots` has no
    /// variant for it, deliberately). Z-Bot is part of a game: a node that has
    /// Condition Zero has it. YaPB is a separate binary an operator installs
    /// into their own content copy, so only that operator can say it is there —
    /// it is `[games.<id>].bots` in the node's config, next to the image, for
    /// exactly the reason the image lives there.
    Yapb,
}

impl BotProtocol {
    /// The console lines that leave a server running `count` bots.
    ///
    /// Two lines, and the first is not optional. `bot_join_after_player`
    /// defaults to 1, which holds every bot out of the game until a human
    /// arrives — so a node that set only `bot_quota` would report success on a
    /// server that stays visibly empty, which is exactly the bug an operator
    /// cannot diagnose from the outside.
    ///
    /// `bot_quota` rather than repeated `bot_add`, because a quota is
    /// idempotent: asking twice for four bots leaves four, and asking for zero
    /// removes them. Every value is a number this function formats, so nothing
    /// a caller sends can become a second command.
    pub fn quota_lines(&self, count: u8) -> Result<Vec<String>, BotCountError> {
        if count > MAX_BOTS {
            return Err(BotCountError::TooMany { asked: count, limit: MAX_BOTS });
        }
        Ok(match self {
            Self::Zbot => {
                vec!["bot_join_after_player 0".to_string(), format!("bot_quota {count}")]
            }
            // `yb_autovacate 0` is this bot's version of the same trap: it
            // defaults to 1, which keeps a slot free for a human by quietly
            // running one bot fewer than asked. Off, so a request for four is
            // four. `yb_quota` itself needs no companion — YaPB's bots join an
            // empty server on their own, which Z-Bot's do not.
            Self::Yapb => {
                vec!["yb_autovacate 0".to_string(), format!("yb_quota {count}")]
            }
        })
    }
}

/// Why a bot count was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotCountError {
    TooMany { asked: u8, limit: u8 },
}

impl core::fmt::Display for BotCountError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooMany { asked, limit } => {
                write!(f, "{asked} bots is over this build's limit of {limit}")
            }
        }
    }
}

impl std::error::Error for BotCountError {}

/// Why a map name was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapNameError {
    Empty,
    TooLong(usize),
    /// A byte outside the allowlist. Carries the offending character so the
    /// message can name it.
    NotAllowed(char),
    /// `..` anywhere, or a name that starts with `/` or `.`.
    Traversal,
    /// The game has no maps to change between.
    NoMaps,
}

impl core::fmt::Display for MapNameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => write!(f, "map name is empty"),
            Self::TooLong(n) => {
                write!(f, "map name is {n} bytes, over the {MAX_MAP_NAME_LEN}-byte limit")
            }
            Self::NotAllowed(c) => write!(
                f,
                "map name contains {c:?}; only letters, digits, '_', '-', '.' and '/' are allowed"
            ),
            Self::Traversal => write!(
                f,
                "map name must not begin with '/' or '.', or contain '..': it is resolved \
                 against the game's own content directory"
            ),
            Self::NoMaps => write!(f, "this game has one world and no map to change to"),
        }
    }
}

impl std::error::Error for MapNameError {}

/// Judge a map name as data. See the module docs for why this is an allowlist.
pub fn validate_map_name(map: &str) -> Result<(), MapNameError> {
    if map.is_empty() {
        return Err(MapNameError::Empty);
    }
    if map.len() > MAX_MAP_NAME_LEN {
        return Err(MapNameError::TooLong(map.len()));
    }
    if let Some(c) =
        map.chars().find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')))
    {
        return Err(MapNameError::NotAllowed(c));
    }
    if map.starts_with('/') || map.starts_with('.') || map.contains("..") {
        return Err(MapNameError::Traversal);
    }
    Ok(())
}

/// Longest console line an operator may send. A real command is a few words;
/// this keeps a paste accident from becoming a wall of input to a game.
pub const MAX_CONSOLE_LINE_LEN: usize = 256;

/// Why an operator's console line was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleLineError {
    Empty,
    TooLong(usize),
    /// A control character, a newline above all: a console reads one command
    /// per line, so a second line is a second command nobody saw typed.
    Control(char),
}

impl core::fmt::Display for ConsoleLineError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => write!(f, "the console line is empty"),
            Self::TooLong(n) => {
                write!(f, "the console line is {n} bytes, over the {MAX_CONSOLE_LINE_LEN}-byte limit")
            }
            Self::Control(c) => write!(f, "the console line contains {c:?}; send one line, with no control characters"),
        }
    }
}

impl std::error::Error for ConsoleLineError {}

/// Judge a line the **operator** types at their own server's console
/// (`ROADMAP.md` item 7). Free text is allowed — `;` included, since chaining
/// commands on your own server is your business — but it is exactly one line:
/// a newline, a carriage return or any other control character is refused,
/// never stripped. Only the node's own API accepts one; a pack names a
/// protocol and never a command, and an index can never send one.
pub fn validate_console_line(line: &str) -> Result<(), ConsoleLineError> {
    if line.trim().is_empty() {
        return Err(ConsoleLineError::Empty);
    }
    if line.len() > MAX_CONSOLE_LINE_LEN {
        return Err(ConsoleLineError::TooLong(line.len()));
    }
    if let Some(c) = line.chars().find(|c| c.is_control()) {
        return Err(ConsoleLineError::Control(c));
    }
    Ok(())
}

/// Something a server's owner does to a player (`MINECRAFT.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayerAction {
    Kick,
    Ban,
    Unban,
    /// Make a player a server operator. Minecraft only.
    Op,
    /// Take operator away. Minecraft only.
    Deop,
}

impl PlayerAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kick => "kick",
            Self::Ban => "ban",
            Self::Unban => "unban",
            Self::Op => "op",
            Self::Deop => "deop",
        }
    }
}

/// What names the player an action is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    /// A Minecraft account name.
    MinecraftName,
    /// An Xbox gamertag, the name a Bedrock player has. It may hold spaces.
    Gamertag,
    /// The name a player shows in a GoldSrc or Source game.
    PlayerName,
    /// A Steam ID: `STEAM_0:1:123` or `[U:1:246]`.
    SteamId,
}

/// Where a game's ban list lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BanList {
    /// The game keeps it in its own server directory (Minecraft's
    /// `banned-players.json`), and the node reads it.
    GameFile,
    /// The game would write it into the shared, read-only copy, so the node
    /// keeps it and sends it again after every start.
    NodeRecord,
    /// The game's server has no ban at all (Bedrock). Its allowlist is the
    /// way to keep someone out.
    None,
}

/// Longest kick or ban reason. A sentence, not a paragraph.
pub const MAX_REASON_LEN: usize = 120;
/// Longest in-game name accepted as a kick target on a Steam engine.
pub const MAX_PLAYER_NAME_LEN: usize = 64;

/// Why a moderation request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModerationError {
    /// This game has no such action (operators on a Steam engine).
    NotSupported(PlayerAction),
    /// The target is not a valid name or Steam ID for this action.
    BadTarget { kind: TargetKind, target: String },
    BadReason(String),
}

impl core::fmt::Display for ModerationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotSupported(a) => write!(f, "this game has no {} command", a.as_str()),
            Self::BadTarget { kind, target } => match kind {
                TargetKind::MinecraftName => write!(
                    f,
                    "{target:?} is not a Minecraft name: 1 to 16 letters, digits or '_'"
                ),
                TargetKind::Gamertag => write!(
                    f,
                    "{target:?} is not a gamertag: 1 to 16 letters, digits or spaces, not starting or ending with a space"
                ),
                TargetKind::PlayerName => write!(
                    f,
                    "{target:?} cannot be used as a player name here: no ';', '\"', '+' or control \
                     characters, at most {MAX_PLAYER_NAME_LEN} characters"
                ),
                TargetKind::SteamId => write!(
                    f,
                    "{target:?} is not a Steam ID; write it as STEAM_0:1:12345 or [U:1:24691]"
                ),
            },
            Self::BadReason(why) => write!(f, "the reason cannot be sent: {why}"),
        }
    }
}

impl std::error::Error for ModerationError {}

/// A Minecraft account name: 1 to 16 ASCII letters, digits or `_`.
pub fn is_minecraft_name(s: &str) -> bool {
    (1..=16).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// An Xbox gamertag: 1 to 16 ASCII letters, digits or single spaces, with no
/// space at either end. It reaches a console line in quotes, which it cannot
/// close.
pub fn is_gamertag(s: &str) -> bool {
    (1..=16).contains(&s.len())
        && !s.starts_with(' ')
        && !s.ends_with(' ')
        && !s.contains("  ")
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b' ')
}

/// A Steam ID in the text form a GoldSrc or Source console takes:
/// `STEAM_X:Y:Z` (X 0 to 5, Y 0 or 1, Z digits) or `[U:1:Z]`.
pub fn is_steam_id(s: &str) -> bool {
    let digits = |p: &str| !p.is_empty() && p.len() <= 10 && p.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = s.strip_prefix("STEAM_") {
        let parts: Vec<&str> = rest.split(':').collect();
        return parts.len() == 3
            && matches!(parts[0], "0" | "1" | "2" | "3" | "4" | "5")
            && matches!(parts[1], "0" | "1")
            && digits(parts[2]);
    }
    s.strip_prefix("[U:1:").and_then(|r| r.strip_suffix(']')).is_some_and(digits)
}

impl ConsoleProtocol {
    /// Whether this game's console can do `action`.
    pub fn supports(&self, action: PlayerAction) -> bool {
        match self {
            Self::Minecraft => true,
            Self::Bedrock => matches!(action, PlayerAction::Kick | PlayerAction::Op | PlayerAction::Deop),
            Self::Goldsrc | Self::Source => !matches!(action, PlayerAction::Op | PlayerAction::Deop),
        }
    }

    /// What names the player for `action` on this game. On a Steam engine a
    /// kick takes the in-game name; a ban takes the Steam ID, because a name
    /// can be anything and a ban is the action a wrong match makes permanent.
    pub fn target_kind(&self, action: PlayerAction) -> TargetKind {
        match (self, action) {
            (Self::Minecraft, _) => TargetKind::MinecraftName,
            (Self::Bedrock, _) => TargetKind::Gamertag,
            (_, PlayerAction::Kick) => TargetKind::PlayerName,
            _ => TargetKind::SteamId,
        }
    }

    pub fn ban_list(&self) -> BanList {
        match self {
            Self::Minecraft => BanList::GameFile,
            Self::Bedrock => BanList::None,
            Self::Goldsrc | Self::Source => BanList::NodeRecord,
        }
    }

    /// The console lines for one moderation action, or why it was refused.
    /// Every part that came from a caller is validated as data first; nothing
    /// is rewritten. A reason is sent only where the game's command takes one.
    pub fn moderation_lines(
        &self,
        action: PlayerAction,
        target: &str,
        reason: Option<&str>,
    ) -> Result<Vec<String>, ModerationError> {
        if !self.supports(action) {
            return Err(ModerationError::NotSupported(action));
        }
        let kind = self.target_kind(action);
        let valid = match kind {
            TargetKind::MinecraftName => is_minecraft_name(target),
            TargetKind::Gamertag => is_gamertag(target),
            TargetKind::SteamId => is_steam_id(target),
            TargetKind::PlayerName => {
                !target.trim().is_empty()
                    && target.len() <= MAX_PLAYER_NAME_LEN
                    && validate_server_name(target).is_ok()
            }
        };
        if !valid {
            return Err(ModerationError::BadTarget { kind, target: target.to_string() });
        }
        let reason = match reason.map(str::trim).filter(|r| !r.is_empty()) {
            Some(r) => {
                if r.len() > MAX_REASON_LEN {
                    return Err(ModerationError::BadReason(format!("it is over {MAX_REASON_LEN} characters")));
                }
                validate_server_name(r).map_err(|e| ModerationError::BadReason(e.to_string()))?;
                Some(r)
            }
            None => None,
        };
        let with_reason = |cmd: &str| match reason {
            Some(r) => format!("{cmd} {target} {r}"),
            None => format!("{cmd} {target}"),
        };
        Ok(match (self, action) {
            (Self::Minecraft, PlayerAction::Kick) => vec![with_reason("kick")],
            (Self::Minecraft, PlayerAction::Ban) => vec![with_reason("ban")],
            (Self::Minecraft, PlayerAction::Unban) => vec![format!("pardon {target}")],
            (Self::Minecraft, PlayerAction::Op) => vec![format!("op {target}")],
            (Self::Minecraft, PlayerAction::Deop) => vec![format!("deop {target}")],
            // Quoted: a gamertag may hold spaces, and cannot hold a quote.
            (Self::Bedrock, PlayerAction::Kick) => vec![match reason {
                Some(r) => format!("kick \"{target}\" {r}"),
                None => format!("kick \"{target}\""),
            }],
            (Self::Bedrock, PlayerAction::Op) => vec![format!("op \"{target}\"")],
            (Self::Bedrock, PlayerAction::Deop) => vec![format!("deop \"{target}\"")],
            (Self::Bedrock, PlayerAction::Ban | PlayerAction::Unban) => unreachable!("refused by supports()"),
            // The name is quoted because a name may hold spaces; it can hold
            // no quote (`validate_server_name`), so it cannot close one.
            (_, PlayerAction::Kick) => vec![format!("kick \"{target}\"")],
            // `0` is a permanent ban; `kick` removes them now if they are on.
            (_, PlayerAction::Ban) => vec![format!("banid 0 {target} kick")],
            (_, PlayerAction::Unban) => vec![format!("removeid {target}")],
            (_, PlayerAction::Op | PlayerAction::Deop) => unreachable!("refused by supports()"),
        })
    }

    /// The lines that put a node-kept ban list back after a start. Entries
    /// that are not Steam IDs are skipped: the record is the node's own file,
    /// but a hand-edited line must still not become a console command.
    pub fn reapply_bans(&self, steam_ids: &[String]) -> Vec<String> {
        match self.ban_list() {
            BanList::GameFile | BanList::None => Vec::new(),
            BanList::NodeRecord => {
                steam_ids.iter().filter(|id| is_steam_id(id)).map(|id| format!("banid 0 {id}")).collect()
            }
        }
    }
}

/// Why a server name was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerNameError {
    /// A character that starts or ends a console command: `;`, `"`, `+`, or
    /// a control character such as a newline.
    NotAllowed(char),
    /// A word beginning with `-`, which the engine reads as an option.
    Option(String),
}

impl core::fmt::Display for ServerNameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAllowed(c) => write!(
                f,
                "server name contains {c:?}; ';', '\"', '+' and control characters are \
                 not allowed, because the name reaches the game's own console"
            ),
            Self::Option(word) => write!(
                f,
                "server name contains the word {word:?}; a word may not begin with '-', \
                 because the game reads it as a start option"
            ),
        }
    }
}

impl std::error::Error for ServerNameError {}

/// Judge a server's display name before it reaches a game.
///
/// A name is free text, so unlike [`validate_map_name`] this is a denylist of
/// exactly what the engines treat as syntax. Every image passes the name as
/// `+hostname "<name>"` (or writes `hostname "<name>"` into a config), and
/// the engine rebuilds its command line and splits it again. Measured
/// 2026-09-30 on GoldSrc and on Source (TF2) alike:
///
/// * **`;`** ends a console command, so `x; sv_password y` sets a password.
/// * **`"`** closes the quoted value, and the rest of the line is commands.
/// * **`+`** starts a command — on GoldSrc even mid-word: `a+sv_password`
///   ran `sv_password`.
/// * **A word starting with `-`** is an engine option: ` -insecure` turned
///   VAC off on both engines. A `-` inside a word, or alone, is harmless and
///   allowed, so "Bob's - CS 24/7" is a fine name.
/// * **Control characters**, a newline above all: a console reads one command
///   per line.
///
/// Refused, never rewritten — the same rule as a map name.
pub fn validate_server_name(name: &str) -> Result<(), ServerNameError> {
    if let Some(c) = name.chars().find(|c| matches!(c, ';' | '"' | '+') || c.is_control()) {
        return Err(ServerNameError::NotAllowed(c));
    }
    if let Some(word) = name.split_whitespace().find(|w| w.starts_with('-') && w.len() > 1) {
        return Err(ServerNameError::Option(word.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minecraft_console_moderates_by_account_name() {
        let mc = ConsoleProtocol::Minecraft;
        assert_eq!(mc.moderation_lines(PlayerAction::Kick, "Steve_1", Some("spam")).unwrap(), ["kick Steve_1 spam"]);
        assert_eq!(mc.moderation_lines(PlayerAction::Ban, "Steve_1", None).unwrap(), ["ban Steve_1"]);
        assert_eq!(mc.moderation_lines(PlayerAction::Unban, "Steve_1", None).unwrap(), ["pardon Steve_1"]);
        assert_eq!(mc.moderation_lines(PlayerAction::Op, "Alex", None).unwrap(), ["op Alex"]);
        assert_eq!(mc.moderation_lines(PlayerAction::Deop, "Alex", None).unwrap(), ["deop Alex"]);
        for bad in ["", "a b", "x\nstop", "toolongname_12345", "@a", "Steve;op", "Steve-1"] {
            assert!(mc.moderation_lines(PlayerAction::Op, bad, None).is_err(), "{bad:?}");
        }
        assert_eq!(mc.ban_list(), BanList::GameFile);
        assert!(!mc.has_maps() && mc.change_map("world").is_err());
        assert_eq!(mc.before_backup(), ["save-off", "save-all flush"]);
    }

    #[test]
    fn bedrock_counts_players_from_its_log_and_other_lines_are_not_a_count() {
        assert_eq!(ConsoleProtocol::Bedrock.player_count_line(), Some("list"));
        assert_eq!(ConsoleProtocol::Minecraft.player_count_line(), None);
        assert_eq!(
            parse_player_count("[2026-10-19 16:36:51:201 INFO] There are 1/10 players online:"),
            Some((1, 10))
        );
        assert_eq!(parse_player_count("[INFO] There are 0/10 players online:"), Some((0, 10)));
        // A line that merely mentions a count is not an answer to `list`.
        assert_eq!(parse_player_count("There are 3 bots online"), None);
        assert_eq!(parse_player_count("There are x/10 players online:"), None);
        assert_eq!(parse_player_count("Player connected: idan2025"), None);
    }

    #[test]
    fn a_bedrock_console_kicks_and_ops_by_gamertag_and_has_no_ban() {
        let b = ConsoleProtocol::Bedrock;
        assert_eq!(b.moderation_lines(PlayerAction::Kick, "Cool Gamer 42", Some("spam")).unwrap(), ["kick \"Cool Gamer 42\" spam"]);
        assert_eq!(b.moderation_lines(PlayerAction::Op, "Steve", None).unwrap(), ["op \"Steve\""]);
        assert_eq!(b.moderation_lines(PlayerAction::Ban, "Steve", None), Err(ModerationError::NotSupported(PlayerAction::Ban)));
        for bad in ["", " lead", "trail ", "two  spaces", "quo\"te", "semi;colon", "x\nstop", "a_b", &"x".repeat(17)] {
            assert!(b.moderation_lines(PlayerAction::Kick, bad, None).is_err(), "{bad:?}");
        }
        assert_eq!(b.ban_list(), BanList::None);
        assert!(b.reapply_bans(&["STEAM_0:1:1".into()]).is_empty());
        assert_eq!(b.before_backup(), ["save hold"]);
        assert_eq!(b.backup_ready(), Some((Some("save query"), "Data saved")));
        assert!(b.change_map("x").is_err());
    }

    #[test]
    fn a_reason_or_target_can_never_carry_a_second_command() {
        for engine in [ConsoleProtocol::Minecraft, ConsoleProtocol::Bedrock, ConsoleProtocol::Goldsrc, ConsoleProtocol::Source] {
            for reason in ["bye\nstop", "bye; quit", "bye\" ; quit", "+quit", "a\rb"] {
                assert!(
                    engine.moderation_lines(PlayerAction::Kick, "Bob", Some(reason)).is_err(),
                    "{engine:?} sent reason {reason:?}"
                );
            }
        }
        let gs = ConsoleProtocol::Goldsrc;
        for name in ["Bob\" ; quit", "Bob\nquit", "a;b", "+x", ""] {
            assert!(gs.moderation_lines(PlayerAction::Kick, name, None).is_err(), "{name:?}");
        }
        assert!(gs.moderation_lines(PlayerAction::Kick, &"x".repeat(MAX_PLAYER_NAME_LEN + 1), None).is_err());
        let long = "x".repeat(MAX_REASON_LEN + 1);
        assert!(ConsoleProtocol::Minecraft.moderation_lines(PlayerAction::Ban, "Bob", Some(&long)).is_err());
    }

    #[test]
    fn a_steam_engine_kicks_by_name_bans_by_steam_id_and_has_no_operators() {
        for engine in [ConsoleProtocol::Goldsrc, ConsoleProtocol::Source] {
            assert_eq!(engine.moderation_lines(PlayerAction::Kick, "Big Bob", None).unwrap(), ["kick \"Big Bob\""]);
            assert_eq!(
                engine.moderation_lines(PlayerAction::Ban, "STEAM_0:1:12345", None).unwrap(),
                ["banid 0 STEAM_0:1:12345 kick"]
            );
            assert_eq!(engine.moderation_lines(PlayerAction::Unban, "[U:1:24691]", None).unwrap(), ["removeid [U:1:24691]"]);
            assert!(engine.moderation_lines(PlayerAction::Ban, "Big Bob", None).is_err(), "a ban needs the Steam ID");
            for action in [PlayerAction::Op, PlayerAction::Deop] {
                assert_eq!(
                    engine.moderation_lines(action, "STEAM_0:1:1", None),
                    Err(ModerationError::NotSupported(action))
                );
            }
            assert_eq!(engine.ban_list(), BanList::NodeRecord);
            assert_eq!(
                engine.reapply_bans(&["STEAM_0:1:1".into(), "x; quit".into()]),
                ["banid 0 STEAM_0:1:1"],
                "a line in the record that is not a Steam ID never reaches the console"
            );
        }
        for bad in ["STEAM_6:1:1", "STEAM_0:2:1", "STEAM_0:1:", "STEAM_0:1:1a", "[U:1:]", "[U:2:5]", "STEAM_0:1:1 kick"] {
            assert!(!is_steam_id(bad), "{bad}");
        }
    }

    #[test]
    fn an_operators_console_line_is_one_line_and_nothing_more() {
        assert!(validate_console_line("status").is_ok());
        assert!(validate_console_line("mp_timelimit 30; sv_restart 1").is_ok(), "chaining is the operator's call");
        for bad in ["status\nquit", "status\rquit", "a\u{0}b", "", "   "] {
            assert!(validate_console_line(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(validate_console_line(&"x".repeat(MAX_CONSOLE_LINE_LEN + 1)).is_err());
    }

    #[test]
    fn a_message_to_players_can_never_become_a_second_command() {
        let c = ConsoleProtocol::Goldsrc;
        assert_eq!(c.say("Restarting in 60 seconds").unwrap(), "say Restarting in 60 seconds");
        for bad in ["hi; quit", "hi\nquit", "a\"b", "a+quit"] {
            assert!(c.say(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// Each of these was measured running a console command or an engine
    /// option on a real GoldSrc or Source server when passed as its name.
    #[test]
    fn a_server_name_can_never_chain_a_console_command() {
        for bad in [
            "probe; sv_password injected",
            "x\" ; rcon_password y",
            "probe +sv_password plusinjected",
            "probe+sv_password+nospace",
            "probe -insecure",
            "-insecure",
            "line\nquit",
            "tab\there",
        ] {
            assert!(validate_server_name(bad).is_err(), "{bad:?} should have been refused");
        }
    }

    /// A name is free text for people. What the engines do not treat as
    /// syntax stays allowed, including a lone dash and one inside a word.
    #[test]
    fn an_ordinary_server_name_is_accepted() {
        for good in [
            "Bob's - CS 24/7",
            "verify-gearbox",
            "Friday night [EU] #3",
            "Ünïcode ok",
            "50% off / no rules",
        ] {
            assert_eq!(validate_server_name(good), Ok(()), "{good:?}");
        }
    }

    /// The quota is what a caller asks for, and the join cvar is what makes it
    /// visible. A build that sent only the quota would leave an operator
    /// looking at an empty server with no error anywhere.
    #[test]
    fn asking_for_bots_also_lets_them_join_an_empty_server() {
        let lines = BotProtocol::Zbot.quota_lines(4).unwrap();
        assert_eq!(lines, ["bot_join_after_player 0", "bot_quota 4"]);
    }

    /// Zero is a real request — it is how a server is emptied — and not an
    /// error.
    #[test]
    fn zero_bots_is_a_request_not_a_refusal() {
        assert_eq!(BotProtocol::Zbot.quota_lines(0).unwrap()[1], "bot_quota 0");
    }

    /// Both implementations answer the same request, in their own words. This
    /// is the test that fails if a variant is added and the match arm is
    /// forgotten.
    #[test]
    fn every_bot_protocol_has_words_for_a_quota() {
        for proto in [BotProtocol::Zbot, BotProtocol::Yapb] {
            let lines = proto.quota_lines(3).unwrap();
            assert!(lines.iter().any(|l| l.ends_with(" 3")), "{proto:?}: {lines:?}");
        }
    }

    #[test]
    fn a_count_past_the_ceiling_is_refused() {
        assert!(matches!(
            BotProtocol::Zbot.quota_lines(200),
            Err(BotCountError::TooMany { asked: 200, limit: MAX_BOTS })
        ));
    }

    /// The count is formatted, never interpolated from text, so there is no
    /// path by which a caller's value becomes a second console command. This
    /// asserts the property directly rather than trusting the type.
    #[test]
    fn no_bot_line_can_carry_a_second_command() {
        for proto in [BotProtocol::Zbot, BotProtocol::Yapb] {
        for n in 0..=MAX_BOTS {
            for line in proto.quota_lines(n).unwrap() {
                assert!(!line.contains('\n'), "{line:?}");
                assert!(!line.contains(';'), "{line:?}");
            }
        }
        }
    }

    #[test]
    fn ordinary_map_names_are_accepted() {
        for good in ["svencoop1", "de_dust2", "cp_dustbowl", "crossfire", "a1", "workshop/foo-2.1"]
        {
            validate_map_name(good).unwrap_or_else(|e| panic!("{good:?} was refused: {e}"));
        }
    }

    /// The one this module exists for. A console reads a line at a time, so a
    /// newline in a map name is a second command — `rcon_password hunter2`,
    /// `quit` — issued by whoever supplied the name.
    #[test]
    fn a_newline_can_never_reach_a_console_line() {
        let err = ConsoleProtocol::Goldsrc.change_map("de_dust2\nquit").unwrap_err();
        assert_eq!(err, MapNameError::NotAllowed('\n'));
        assert_eq!(
            ConsoleProtocol::Goldsrc.change_map("a\rquit").unwrap_err(),
            MapNameError::NotAllowed('\r')
        );
    }

    /// Anything that would split the line into more than one argument, or quote
    /// its way out of it.
    #[test]
    fn a_map_name_can_never_become_two_arguments() {
        for bad in ["de dust2", "a;b", "a\"b", "a'b", "a$b", "a`b", "a|b", "a\tb"] {
            assert!(
                ConsoleProtocol::Source.change_map(bad).is_err(),
                "{bad:?} should have been refused"
            );
        }
    }

    /// The same string reaches a game that resolves it against its content
    /// directory, which is a read-only mount holding every instance's copy.
    #[test]
    fn a_map_name_cannot_climb_out_of_the_content_directory() {
        for bad in ["../etc/passwd", "/etc/passwd", "maps/../../x", ".hidden"] {
            assert_eq!(
                validate_map_name(bad),
                Err(MapNameError::Traversal),
                "{bad:?} should have been refused as traversal"
            );
        }
    }

    #[test]
    fn an_empty_or_oversized_name_is_refused() {
        assert_eq!(validate_map_name(""), Err(MapNameError::Empty));
        assert_eq!(validate_map_name(&"a".repeat(65)), Err(MapNameError::TooLong(65)));
    }

    /// `changelevel`, not `map`: `map` restarts the server and drops everyone,
    /// which is not what "change the map on a live server" means.
    #[test]
    fn the_command_keeps_players_connected() {
        assert_eq!(
            ConsoleProtocol::Goldsrc.change_map("svencoop1").unwrap(),
            "changelevel svencoop1"
        );
        assert_eq!(
            ConsoleProtocol::Source.change_map("cp_badlands").unwrap(),
            "changelevel cp_badlands"
        );
    }
}
