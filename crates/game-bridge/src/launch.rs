//! Launch profiles — how a pack says "start the game", without naming a
//! command (`PLAN.md` §13.1).
//!
//! `PLAN.md` §10 held open for a long time whether a pack may carry launch
//! arguments at all, because a downloadable TOML that can run something is a
//! remote-code-execution primitive. Decided 2026-08-31: **yes, on the player's
//! own machine only, as a constrained template, and never on a node.**
//!
//! # The asymmetry that makes this safe
//!
//! A launch profile affects the machine of the person who *chose to install
//! that pack*, running *their own* game. A node runs code on someone else's
//! hardware, for strangers. The two are not the same risk and this module must
//! never be reused for the second: `GameRuntime.image` stays operator config,
//! and a pack still cannot name what a node executes (`PLAN.md` §8 phase 3).
//!
//! # The four rules a later change could quietly break
//!
//! 1. **The pack never names an executable.** The program comes from the
//!    player's own installed game, located by the launcher and confirmed by the
//!    player. A pack that could name a binary is a pack that can run one, and
//!    every other rule here would then be decoration.
//! 2. **`args` is a template, not a string that is shelled.** [`build_args`]
//!    produces an argument **vector**, and the caller spawns it directly. No
//!    `sh -c`, ever — that is what makes `$(...)`, `;`, `|`, `&&` and backticks
//!    inert characters rather than syntax. Pinned by
//!    `shell_metacharacters_are_inert_text_not_syntax`.
//! 3. **The substituted values are the launcher's, not the pack's.**
//!    `{address}` is the local port the launcher just bound. A pack chooses
//!    *where* a value lands, never *what* it is.
//! 4. **An unknown placeholder is an error, not empty text.** A typo would
//!    otherwise silently launch a game with a missing connect address, and the
//!    player would see a game that "just does not work". Pinned by
//!    `an_unknown_placeholder_is_refused_at_parse`.
//!
//! Together: the worst an unreviewed `[launch]` block can do is start the
//! player's own game with odd flags. That ceiling is what makes a pack
//! marketplace tenable (`PLAN.md` §13.2) — **not** a scanner, because intent is
//! not in the syntax.

use serde::{Deserialize, Serialize};

/// The engine families this build knows how to start.
///
/// An enum the code implements, exactly like `QueryProtocol::A2s` and the
/// content drivers. A `kind` this build does not know fails to parse rather
/// than being half-understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchKind {
    /// Half-Life 1 engine: Sven Co-op, HLDM, CS 1.6.
    Goldsrc,
    /// Half-Life 2 engine: TF2, CS:S, Garry's Mod.
    Source,
}

/// How to point a player's own copy of a game at a server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchProfile {
    pub kind: LaunchKind,
    /// Steam application id of the **client**, so the launcher can find the
    /// game in a Steam library the player already has.
    ///
    /// A hint for locating an installation, never a path and never a program.
    /// A player with a non-Steam copy points the launcher at their own game
    /// once instead.
    #[serde(default)]
    pub steam_app_id: Option<u32>,
    /// Argument templates, in order. Each is one argument after substitution,
    /// or several if it contains spaces outside a placeholder — see
    /// [`build_args`].
    #[serde(default)]
    pub args: Vec<String>,
}

/// A value the launcher is willing to substitute into a template.
///
/// This list **is** the vocabulary. Adding one is a deliberate act; a pack
/// cannot invent a placeholder, and an unknown one is refused rather than
/// replaced with nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placeholder {
    /// `host:port` the launcher bound locally for this join.
    Address,
    /// Just the port half of the same thing.
    Port,
    /// The server's password, when the player supplied one.
    Password,
    /// The player's chosen display name.
    Name,
}

impl Placeholder {
    /// The token as it appears inside braces in a pack.
    pub fn token(&self) -> &'static str {
        match self {
            Self::Address => "address",
            Self::Port => "port",
            Self::Password => "password",
            Self::Name => "name",
        }
    }

    fn parse(token: &str) -> Option<Self> {
        match token {
            "address" => Some(Self::Address),
            "port" => Some(Self::Port),
            "password" => Some(Self::Password),
            "name" => Some(Self::Name),
            _ => None,
        }
    }
}

/// What the launcher knows, to fill a template with.
///
/// Every field is the launcher's own: the address it bound, the name the player
/// typed. A pack contributes none of them.
#[derive(Debug, Clone, Default)]
pub struct LaunchValues {
    pub address: String,
    pub port: u16,
    pub password: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// A `{...}` naming something outside [`Placeholder`].
    UnknownPlaceholder(String),
    /// A `{` with no `}`, which would otherwise be passed through as text and
    /// look to a player like the game ignoring an argument.
    UnclosedPlaceholder(String),
    /// Too many arguments for anything a real game needs.
    TooManyArgs(usize),
    /// One template longer than any real argument.
    ArgTooLong(usize),
}

impl core::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownPlaceholder(t) => write!(
                f,
                "{{{t}}} is not a value the launcher provides. It knows: \
                 address, port, password, name"
            ),
            Self::UnclosedPlaceholder(a) => write!(f, "argument {a:?} opens {{ and never closes it"),
            Self::TooManyArgs(n) => write!(f, "{n} launch arguments is past the limit of {MAX_ARGS}"),
            Self::ArgTooLong(n) => {
                write!(f, "a launch argument is {n} bytes, over the {MAX_ARG_LEN}-byte limit")
            }
        }
    }
}

impl std::error::Error for LaunchError {}

/// More arguments than any real game needs. A bound, so a pack cannot make a
/// command line the operating system refuses in a confusing way.
pub const MAX_ARGS: usize = 32;

/// Longest single argument template.
pub const MAX_ARG_LEN: usize = 512;

impl LaunchProfile {
    /// Check the templates without running anything.
    ///
    /// Called at pack-parse time, so a bad `[launch]` block is a bad pack —
    /// reported with the file in hand rather than at the moment a player
    /// presses Join.
    pub fn validate(&self) -> Result<(), LaunchError> {
        if self.args.len() > MAX_ARGS {
            return Err(LaunchError::TooManyArgs(self.args.len()));
        }
        for arg in &self.args {
            if arg.len() > MAX_ARG_LEN {
                return Err(LaunchError::ArgTooLong(arg.len()));
            }
            // Substituting against empty values proves the *shape* is sound;
            // the values themselves cannot make it unsound, because they are
            // never re-parsed.
            expand(arg, &LaunchValues::default())?;
        }
        Ok(())
    }

    /// The argument vector to spawn the player's game with.
    ///
    /// **The caller must spawn this directly, never through a shell.** That is
    /// rule 2 in this module's docs and it is the whole safety property: passed
    /// to `Command::args`, a `;` is one byte of one argument.
    ///
    /// An argument whose value is absent — no password on this server — is
    /// dropped whole rather than passed empty, because `+password ""` and no
    /// `+password` mean different things to a game.
    pub fn build_args(&self, values: &LaunchValues) -> Result<Vec<String>, LaunchError> {
        let mut out = Vec::new();
        for template in &self.args {
            // A template may hold several arguments — `"+connect {address}"` is
            // the shape both engines want. The split is ours and happens
            // *before* substitution, so a value containing a space lands inside
            // one argument instead of becoming two.
            out.extend(split_template(template, values)?);
        }
        Ok(out)
    }
}

/// Substitute, returning `None` when a placeholder's value is absent.
fn expand(template: &str, values: &LaunchValues) -> Result<Option<String>, LaunchError> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| LaunchError::UnclosedPlaceholder(template.to_string()))?;
        let token = &after[..close];
        let placeholder = Placeholder::parse(token)
            .ok_or_else(|| LaunchError::UnknownPlaceholder(token.to_string()))?;
        match value_of(placeholder, values) {
            Some(v) => out.push_str(&v),
            // The whole argument goes, not just the hole in it.
            None => return Ok(None),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(Some(out))
}

/// Split a template into arguments **before** substituting, so a value
/// containing a space lands inside one argument instead of becoming two.
fn split_template(
    template: &str,
    values: &LaunchValues,
) -> Result<Vec<String>, LaunchError> {
    let mut out = Vec::new();
    for piece in template.split_whitespace() {
        match expand(piece, values)? {
            Some(v) => out.push(v),
            // One absent value drops the whole template, flag included: a bare
            // `+password` with nothing after it is worse than no flag at all.
            None => return Ok(Vec::new()),
        }
    }
    Ok(out)
}

fn value_of(p: Placeholder, values: &LaunchValues) -> Option<String> {
    match p {
        Placeholder::Address => Some(values.address.clone()),
        Placeholder::Port => Some(values.port.to_string()),
        Placeholder::Password => values.password.clone(),
        Placeholder::Name => values.name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(args: &[&str]) -> LaunchProfile {
        LaunchProfile {
            kind: LaunchKind::Goldsrc,
            steam_app_id: Some(225840),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn values() -> LaunchValues {
        LaunchValues {
            address: "127.0.0.1:27015".into(),
            port: 27015,
            password: None,
            name: Some("idan".into()),
        }
    }

    #[test]
    fn a_template_becomes_the_arguments_a_game_expects() {
        let p = profile(&["+connect {address}", "+name {name}"]);
        assert_eq!(
            p.build_args(&values()).unwrap(),
            vec!["+connect", "127.0.0.1:27015", "+name", "idan"]
        );
    }

    /// **The load-bearing test.** A pack is a file a stranger wrote. Shell
    /// syntax in it must be text, and it is text because the result is spawned
    /// as a vector and never handed to a shell.
    #[test]
    fn shell_metacharacters_are_inert_text_not_syntax() {
        let p = profile(&["+connect{address};rm", "$(curl|sh)", "`id`", "&&whoami"]);
        let args = p.build_args(&values()).unwrap();
        assert_eq!(
            args,
            vec!["+connect127.0.0.1:27015;rm", "$(curl|sh)", "`id`", "&&whoami"]
        );
        // Each stays exactly one argument. Nothing was split on `;` or `|`,
        // because nothing interpreted them.
        assert_eq!(args.len(), 4);
    }

    /// A pack cannot invent a value. A typo is refused rather than silently
    /// becoming empty, which would launch a game with no connect address and
    /// look to a player like the platform simply not working.
    #[test]
    fn an_unknown_placeholder_is_refused_at_parse() {
        let p = profile(&["+exec {config_file}"]);
        assert_eq!(
            p.validate(),
            Err(LaunchError::UnknownPlaceholder("config_file".into()))
        );
        assert!(profile(&["+connect {address"]).validate().is_err());
    }

    /// An absent value drops its whole argument, flag and all: `+password ""`
    /// and no `+password` mean different things to a game.
    #[test]
    fn an_absent_value_drops_its_flag_rather_than_passing_it_empty() {
        let p = profile(&["+connect {address}", "+password {password}"]);
        let args = p.build_args(&values()).unwrap();
        assert_eq!(args, vec!["+connect", "127.0.0.1:27015"]);
        assert!(!args.iter().any(|a| a == "+password"));
    }

    /// A value containing a space stays one argument. The split happens before
    /// substitution, so a player's name cannot add an argument to their own
    /// command line — nor could a server's, if one ever landed here.
    #[test]
    fn a_value_with_spaces_stays_a_single_argument() {
        let mut v = values();
        v.name = Some("two words".into());
        let args = profile(&["+name {name}"]).build_args(&v).unwrap();
        assert_eq!(args, vec!["+name", "two words"]);
    }

    #[test]
    fn a_profile_within_the_limits_validates() {
        assert!(profile(&["+connect {address}"]).validate().is_ok());
        let many: Vec<String> = (0..MAX_ARGS + 1).map(|_| "-x".to_string()).collect();
        let p = LaunchProfile { kind: LaunchKind::Source, steam_app_id: None, args: many };
        assert_eq!(p.validate(), Err(LaunchError::TooManyArgs(MAX_ARGS + 1)));
    }

    /// A `kind` this build cannot start is refused, not guessed at — the same
    /// rule as an unimplemented content driver.
    #[test]
    fn an_unimplemented_engine_is_refused_not_ignored() {
        let src = r#"kind = "unreal"
args = []
"#;
        assert!(toml::from_str::<LaunchProfile>(src).is_err());
    }

    #[test]
    fn a_launch_block_round_trips_through_toml() {
        let src = r#"kind = "source"
steam_app_id = 440
args = ["+connect {address}", "+password {password}"]
"#;
        let p: LaunchProfile = toml::from_str(src).unwrap();
        assert_eq!(p.kind, LaunchKind::Source);
        assert_eq!(p.steam_app_id, Some(440));
        p.validate().unwrap();
    }
}
