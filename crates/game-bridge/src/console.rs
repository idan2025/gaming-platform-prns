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
}

impl ConsoleProtocol {
    /// The console line that changes the map, or why the name was refused.
    pub fn change_map(&self, map: &str) -> Result<String, MapNameError> {
        validate_map_name(map)?;
        Ok(match self {
            // `changelevel`, not `map`: `map` restarts the server and drops
            // every player, which is the opposite of changing the map on a
            // live server. Both engines spell it the same way.
            Self::Goldsrc | Self::Source => format!("changelevel {map}"),
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        for n in 0..=MAX_BOTS {
            for line in BotProtocol::Zbot.quota_lines(n).unwrap() {
                assert!(!line.contains('\n'), "{line:?}");
                assert!(!line.contains(';'), "{line:?}");
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
