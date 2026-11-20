//! Channel text commands for a TeamSpeak output.
//!
//! The parser is pure. The gate limits how often one client may issue a command.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Play(String),
    Queue,
    Skip,
    Stop,
    Np,
    Vol(Vol),
    Radio,
    Help,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vol {
    Show,
    Set(u8),
    Invalid,
}

pub const COMMANDS: &[&str] = &["play", "queue", "skip", "stop", "np", "vol", "radio", "help"];

/// Parse a channel message with the default `!` prefix and every command enabled.
pub fn parse(raw: &str) -> Option<Command> {
    parse_with(raw, "!", COMMANDS)
}

/// Parse a channel message. `prefix` is the bot's command prefix. A command
/// that the bot has turned off returns `None`, the same as ordinary chat.
pub fn parse_with(raw: &str, prefix: &str, enabled: &[&str]) -> Option<Command> {
    let prefix = if prefix.is_empty() { "!" } else { prefix };
    let text = raw.trim();
    let rest = text.strip_prefix(prefix)?;
    if rest.is_empty() {
        return None;
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim().to_ascii_lowercase();
    let arg = parts.next().unwrap_or("").trim();
    if name.is_empty() || name.chars().any(|c| c.is_control()) {
        return None;
    }
    if !enabled.iter().any(|item| *item == name) {
        return None;
    }
    Some(match name.as_str() {
        "play" => Command::Play(clean_arg(arg)),
        "queue" => Command::Queue,
        "skip" => Command::Skip,
        "stop" => Command::Stop,
        "np" => Command::Np,
        "vol" => Command::Vol(parse_vol(arg)),
        "radio" => Command::Radio,
        "help" => Command::Help,
        _ => return None,
    })
}

fn clean_arg(arg: &str) -> String {
    arg.chars().filter(|c| !c.is_control()).take(180).collect()
}

fn parse_vol(arg: &str) -> Vol {
    if arg.is_empty() {
        return Vol::Show;
    }
    match arg.parse::<u16>() {
        Ok(n) if n <= 100 => Vol::Set(n as u8),
        _ => Vol::Invalid,
    }
}

/// Five commands in ten seconds per speaker. Extra commands are dropped.
pub struct Gate {
    hits: HashMap<String, VecDeque<Instant>>,
}

impl Gate {
    pub fn new() -> Self {
        Self { hits: HashMap::new() }
    }

    pub fn allow(&mut self, key: &str) -> bool {
        const MAX: usize = 5;
        const WINDOW: Duration = Duration::from_secs(10);
        if key.is_empty() {
            return false;
        }
        if self.hits.len() > 400 {
            self.hits.clear();
        }
        let now = Instant::now();
        let q = self.hits.entry(key.to_string()).or_default();
        while q.front().is_some_and(|t| now.saturating_duration_since(*t) > WINDOW) {
            q.pop_front();
        }
        if q.len() >= MAX {
            return false;
        }
        q.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_channel_commands() {
        assert_eq!(parse("!play"), Some(Command::Play(String::new())));
        assert_eq!(parse("  !PLAY  blue monday  "), Some(Command::Play("blue monday".into())));
        assert_eq!(parse("!queue"), Some(Command::Queue));
        assert_eq!(parse("!skip"), Some(Command::Skip));
        assert_eq!(parse("!stop"), Some(Command::Stop));
        assert_eq!(parse("!np"), Some(Command::Np));
        assert_eq!(parse("!vol"), Some(Command::Vol(Vol::Show)));
        assert_eq!(parse("!vol 0"), Some(Command::Vol(Vol::Set(0))));
        assert_eq!(parse("!vol 100"), Some(Command::Vol(Vol::Set(100))));
        assert_eq!(parse("!vol 101"), Some(Command::Vol(Vol::Invalid)));
        assert_eq!(parse("!vol loud"), Some(Command::Vol(Vol::Invalid)));
        assert_eq!(parse("!radio"), Some(Command::Radio));
        assert_eq!(parse("!help"), Some(Command::Help));
        assert_eq!(parse_with("?play blue", "?", COMMANDS), Some(Command::Play("blue".into())));
        assert_eq!(parse_with("!play blue", "?", COMMANDS), None);
        assert_eq!(parse_with("!play blue", "!", &["queue", "help"]), None);
        assert_eq!(parse_with("!help", "!", &["help"]), Some(Command::Help));
    }

    #[test]
    fn ignores_plain_chat_and_unknown_commands() {
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("play song"), None);
        assert_eq!(parse("hey !play song"), None);
        assert_eq!(parse("!"), None);
        assert_eq!(parse("!youtube cats"), None);
        assert_eq!(parse("!yt https://example.com"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn gate_drops_the_sixth_command_in_the_window() {
        let mut gate = Gate::new();
        for _ in 0..5 {
            assert!(gate.allow("speaker"));
        }
        assert!(!gate.allow("speaker"));
        assert!(gate.allow("someone-else"));
    }
}
