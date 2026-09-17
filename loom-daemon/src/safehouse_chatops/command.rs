//! The **closed** ChatOps command enum and its strict tokenizing parser
//! (Issue #7893, Phase 3a of #4196).
//!
//! # Why this layer is deliberately boring
//!
//! The operator boundary ruling for #4196 splits inbound steering into two
//! layers, and this file is the lower one:
//!
//! 1. **`loom-daemon` keeps a closed, typed command enum** — the daemon never
//!    interprets natural language. A message addressed to the daemon either
//!    tokenizes into exactly one [`Command`] variant or it is refused. There is
//!    no fallback, no fuzzy match, no "did you mean", and above all **no
//!    free-text-to-shell path**: nothing parsed here is ever interpolated into a
//!    command line. Every variant maps onto an existing typed
//!    [`crate::types::Request`] (see [`super::runtime::command_to_request`]).
//! 2. **Natural language lives in a separate operator-agent persona** (Phase
//!    3b), which reads human intent and *uses* this typed surface. That agent is
//!    out of scope here and gets its own issue.
//!
//! Keeping the grammar this small is the security property, not an accident of
//! effort: the entire accepted language is six verbs, each taking at most one
//! charset-validated argument, and the parser is a pure function over a `&str`
//! with no I/O, no regex, and no allocation of anything it did not validate.

use std::fmt;

/// Longest accepted free-form token (a sweep id). Sweep ids are
/// `sweep-issue-<N>-<unix-secs>` shaped, so this is generous by an order of
/// magnitude while still bounding what can be echoed back into a room or a log
/// line.
const MAX_TOKEN_LEN: usize = 128;

/// Longest accepted confirmation nonce. [`super::nonce`] issues 12 hex
/// characters; the cap only exists so a malformed confirm cannot carry an
/// unbounded string into the refusal reply.
const MAX_NONCE_LEN: usize = 64;

/// How much of an unrecognized token is echoed back in a refusal. Long enough
/// for an operator to see their own typo, short enough that the room reply can
/// never become an amplification channel.
const ECHO_LEN: usize = 32;

/// The human-facing summary of the closed command set, appended to every
/// refusal reply so discovery needs no external documentation.
pub const USAGE: &str = "accepted: `status` | `dispatch <issue>` | `cancel <sweep-id>` \
     | `unblock <issue>` | `watch <issue>` | `confirm <nonce>`";

/// Every inbound steering command `loom-daemon` accepts.
///
/// Closed by construction: [`Command::parse`] is the only constructor reachable
/// from an inbound room message, and it returns [`ParseError`] for everything
/// that is not exactly one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `status` — the daemon's operability snapshot.
    Status,
    /// `dispatch <issue>` — dispatch a sweep for an issue.
    Dispatch { issue: u32 },
    /// `cancel <sweep-id>` — cancel a tracked sweep. **Destructive**: requires a
    /// confirm-nonce round-trip (see [`Command::requires_confirmation`]).
    Cancel { sweep: String },
    /// `unblock <issue>` — clear the daemon's insta-crash quarantine for an
    /// issue, the operator-reachable release path that makes it dispatchable
    /// again.
    Unblock { issue: u32 },
    /// `watch <issue>` — register a durable watch on an issue.
    Watch { number: u32 },
    /// `confirm <nonce>` — redeem a nonce issued for a pending destructive
    /// command. Never executed directly; it resolves to the *stored* command.
    Confirm { nonce: String },
}

impl Command {
    /// The canonical verb word, for logs and event payloads.
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Dispatch { .. } => "dispatch",
            Self::Cancel { .. } => "cancel",
            Self::Unblock { .. } => "unblock",
            Self::Watch { .. } => "watch",
            Self::Confirm { .. } => "confirm",
        }
    }

    /// Whether this command may only execute after a confirm-nonce round-trip.
    ///
    /// **`cancel` only.** The issue's rule is "`cancel`; anything force-scoped",
    /// and none of the other five verbs is force-scoped:
    ///
    /// - `status` / `watch` are explicitly named non-destructive by the
    ///   acceptance criteria — they read, or register a read.
    /// - `dispatch` is additive and recoverable (the thing it starts can itself
    ///   be `cancel`led, which *is* gated).
    /// - `unblock` clears a daemon-local quarantine flag; re-quarantining is
    ///   automatic if the underlying condition recurs.
    ///
    /// `cancel` is the one verb that destroys in-flight work that cannot be
    /// recovered by re-running it. Adding a second gated verb later is a single
    /// arm here — deliberately the only place destructiveness is decided.
    #[must_use]
    pub const fn requires_confirmation(&self) -> bool {
        matches!(self, Self::Cancel { .. })
    }

    /// A short, log/event-safe rendering.
    ///
    /// [`Command::Confirm`]'s nonce is **redacted**: a nonce is a single-use
    /// capability, and a log line or event payload is a far wider audience than
    /// the room reply that issued it.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Status => "status".to_owned(),
            Self::Dispatch { issue } => format!("dispatch {issue}"),
            Self::Cancel { sweep } => format!("cancel {sweep}"),
            Self::Unblock { issue } => format!("unblock {issue}"),
            Self::Watch { number } => format!("watch {number}"),
            Self::Confirm { .. } => "confirm <redacted>".to_owned(),
        }
    }

    /// Parse one inbound message body into a command.
    ///
    /// Pure: no I/O, no env, no clock. Whitespace-tokenized, verb
    /// case-insensitive, at most one argument — anything else is a
    /// [`ParseError`], never a guess.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut tokens = text.split_whitespace();
        let Some(raw_verb) = tokens.next() else {
            return Err(ParseError::Empty);
        };
        let Some(verb) = Verb::from_word(raw_verb) else {
            return Err(ParseError::UnknownVerb {
                verb: echo(raw_verb),
            });
        };
        let arg = tokens.next();
        if tokens.next().is_some() {
            return Err(ParseError::ExtraArguments {
                verb: verb.as_str(),
            });
        }
        let name = verb.as_str();
        match verb {
            Verb::Status => {
                if arg.is_some() {
                    return Err(ParseError::ExtraArguments { verb: name });
                }
                Ok(Self::Status)
            }
            Verb::Dispatch => Ok(Self::Dispatch {
                issue: parse_number(name, arg)?,
            }),
            Verb::Unblock => Ok(Self::Unblock {
                issue: parse_number(name, arg)?,
            }),
            Verb::Watch => Ok(Self::Watch {
                number: parse_number(name, arg)?,
            }),
            Verb::Cancel => Ok(Self::Cancel {
                sweep: parse_token(name, "a sweep id", arg, MAX_TOKEN_LEN)?,
            }),
            Verb::Confirm => Ok(Self::Confirm {
                nonce: parse_token(name, "a confirmation nonce", arg, MAX_NONCE_LEN)?,
            }),
        }
    }
}

/// The verb half of the grammar, separated so the `match` in
/// [`Command::parse`] is exhaustive over a closed type rather than over string
/// literals with an unreachable fallback arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Status,
    Dispatch,
    Cancel,
    Unblock,
    Watch,
    Confirm,
}

impl Verb {
    /// ASCII-case-insensitive exact match. Nothing else — no prefixes, no
    /// aliases, no leading `/` or `!`. An operator who types a chat-bot-style
    /// `/status` gets the refusal reply, which names the accepted forms.
    fn from_word(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "status" => Some(Self::Status),
            "dispatch" => Some(Self::Dispatch),
            "cancel" => Some(Self::Cancel),
            "unblock" => Some(Self::Unblock),
            "watch" => Some(Self::Watch),
            "confirm" => Some(Self::Confirm),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Dispatch => "dispatch",
            Self::Cancel => "cancel",
            Self::Unblock => "unblock",
            Self::Watch => "watch",
            Self::Confirm => "confirm",
        }
    }
}

/// Why an inbound message was not a command. Every variant is refused — there
/// is deliberately no "close enough" outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The message addressed to the daemon carried no words at all.
    Empty,
    /// The first word is not one of the six verbs.
    UnknownVerb { verb: String },
    /// A verb that needs an argument got none.
    MissingArgument {
        verb: &'static str,
        expected: &'static str,
    },
    /// A verb's argument was present but the wrong shape.
    BadArgument {
        verb: &'static str,
        expected: &'static str,
        got: String,
    },
    /// More tokens than the verb accepts (this is what a sentence of natural
    /// language beginning with a valid verb resolves to — refused, not
    /// interpreted).
    ExtraArguments { verb: &'static str },
}

impl ParseError {
    /// Stable machine-readable code for the event-bus payload. Kept separate
    /// from [`fmt::Display`] so prose can be reworded without breaking a
    /// subscriber that filters on the reason.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::UnknownVerb { .. } => "unknown-verb",
            Self::MissingArgument { .. } => "missing-argument",
            Self::BadArgument { .. } => "bad-argument",
            Self::ExtraArguments { .. } => "extra-arguments",
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty command"),
            Self::UnknownVerb { verb } => write!(f, "unknown command `{verb}`"),
            Self::MissingArgument { verb, expected } => {
                write!(f, "`{verb}` needs {expected}")
            }
            Self::BadArgument {
                verb,
                expected,
                got,
            } => write!(f, "`{verb}` expected {expected}, got `{got}`"),
            Self::ExtraArguments { verb } => {
                write!(f, "`{verb}` takes no extra arguments")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a positive forge number, tolerating one leading `#` (the forge's own
/// convention — `dispatch #7893` is what an operator will type).
fn parse_number(verb: &'static str, arg: Option<&str>) -> Result<u32, ParseError> {
    const EXPECTED: &str = "a positive issue number";
    let Some(arg) = arg else {
        return Err(ParseError::MissingArgument {
            verb,
            expected: EXPECTED,
        });
    };
    let digits = arg.strip_prefix('#').unwrap_or(arg);
    match digits.parse::<u32>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(ParseError::BadArgument {
            verb,
            expected: EXPECTED,
            got: echo(arg),
        }),
    }
}

/// Parse an opaque identifier token, charset- and length-validated.
///
/// The charset (`[A-Za-z0-9._-]`) is an allowlist, not a denylist: it is
/// narrower than anything a shell, a JSON pointer, or Matrix formatting could
/// act on, so a validated token is inert wherever it is later echoed. Nothing
/// downstream interpolates it into a command line regardless — this is the
/// belt to that braces.
fn parse_token(
    verb: &'static str,
    expected: &'static str,
    arg: Option<&str>,
    max_len: usize,
) -> Result<String, ParseError> {
    let Some(arg) = arg else {
        return Err(ParseError::MissingArgument { verb, expected });
    };
    let ok = !arg.is_empty()
        && arg.len() <= max_len
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(arg.to_owned())
    } else {
        Err(ParseError::BadArgument {
            verb,
            expected,
            got: echo(arg),
        })
    }
}

/// Render an untrusted token for inclusion in a refusal reply or a log line:
/// printable ASCII only, truncated to [`ECHO_LEN`]. Room text is untrusted
/// input (`defaults/docs/untrusted-external-content.md`); echoing it back
/// unfiltered would let a sender place arbitrary bytes into the daemon's own
/// log and into a room line attributed to the daemon persona.
fn echo(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .filter(|c| c.is_ascii_graphic())
        .take(ECHO_LEN)
        .collect();
    if out.is_empty() {
        out.push_str("<unprintable>");
    }
    out
}
