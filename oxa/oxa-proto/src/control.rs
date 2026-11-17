//! Strict, borrowed control lines (#330). Parsing grants no sender authority or action policy.
use std::fmt;

pub const MAX_CONTROL_LINE_BYTES: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlVerb {
    Ack,
    Request,
    Question,
    Answer,
    Propose,
    VoteFor,
    VoteAgainst,
    VoteNeutral,
    Decided,
    Blessed,
    Rethink,
    Approved,
    Merged,
    Hold,
    Blocked,
    Done,
}

impl ControlVerb {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ack => "ACK",
            Self::Request => "REQ",
            Self::Question => "Q",
            Self::Answer => "ANS",
            Self::Propose => "PROP",
            Self::VoteFor => "VOTE(+1)",
            Self::VoteAgainst => "VOTE(-1)",
            Self::VoteNeutral => "VOTE(0)",
            Self::Decided => "DECIDED",
            Self::Blessed => "BLESSED",
            Self::Rethink => "RETHINK",
            Self::Approved => "APPROVED",
            Self::Merged => "MERGED",
            Self::Hold => "HOLD",
            Self::Blocked => "BLOCKED",
            Self::Done => "DONE",
        }
    }

    fn parse(word: &str) -> Result<Self, ControlParseError> {
        Ok(match word {
            "ACK" => Self::Ack,
            "REQ" => Self::Request,
            "Q" => Self::Question,
            "ANS" => Self::Answer,
            "PROP" => Self::Propose,
            "VOTE(+1)" => Self::VoteFor,
            "VOTE(-1)" => Self::VoteAgainst,
            "VOTE(0)" => Self::VoteNeutral,
            "DECIDED" => Self::Decided,
            "BLESSED" => Self::Blessed,
            "RETHINK" => Self::Rethink,
            "APPROVED" => Self::Approved,
            "MERGED" => Self::Merged,
            "HOLD" => Self::Hold,
            "BLOCKED" => Self::Blocked,
            "DONE" => Self::Done,
            _ => return Err(ControlParseError("unknown control verb")),
        })
    }

    fn needs_reason(self) -> bool {
        matches!(
            self,
            Self::Hold | Self::Blocked | Self::Rethink | Self::VoteAgainst
        )
    }

    fn needs_commit(self) -> bool {
        matches!(
            self,
            Self::VoteFor
                | Self::VoteAgainst
                | Self::VoteNeutral
                | Self::Decided
                | Self::Blessed
                | Self::Rethink
                | Self::Approved
                | Self::Merged
                | Self::Hold
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlSubject<'a> {
    Code { number: u64, commit: &'a str },
    Topic(&'a str),
}

impl fmt::Display for ControlSubject<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code { number, commit } => write!(f, "ox#{number}@{commit}"),
            Self::Topic(topic) => write!(f, "~{topic}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlLine<'a> {
    sender: &'a str,
    subject: ControlSubject<'a>,
    verb: ControlVerb,
    id: &'a str,
    reply_to: Option<&'a str>,
    text: &'a str,
}

impl<'a> ControlLine<'a> {
    /// One canonical line, excluding its transport's newline. No normalization or guessing.
    pub fn parse(line: &'a str) -> Result<Self, ControlParseError> {
        let bad = |why| ControlParseError(why);
        if line.is_empty() || line.len() > MAX_CONTROL_LINE_BYTES {
            return Err(bad("control line size is out of bounds"));
        }
        if line.trim() != line
            || line
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        {
            return Err(bad(
                "control line contains noncanonical whitespace or control characters",
            ));
        }
        let mut words = line.splitn(5, ' ');
        let sender = words
            .next()
            .and_then(|s| s.strip_suffix('>'))
            .ok_or(bad("missing sender marker"))?;
        if !atom(sender, 32) || !sender.as_bytes()[0].is_ascii_alphabetic() {
            return Err(bad("invalid sender"));
        }
        let subject = parse_subject(words.next().ok_or(bad("missing subject"))?)?;
        let verb = ControlVerb::parse(words.next().ok_or(bad("missing verb"))?)?;
        let ids = words.next().ok_or(bad("missing message id"))?;
        let (id, reply_to) = ids
            .split_once('/')
            .map_or((ids, None), |(id, reply)| (id, Some(reply)));
        if !atom(id, 64) || reply_to.is_some_and(|r| !atom(r, 64) || r == id) {
            return Err(bad("invalid message or reply id"));
        }
        if matches!(verb, ControlVerb::Ack | ControlVerb::Answer) && reply_to.is_none() {
            return Err(bad("ACK and ANS require a reply id"));
        }
        let text = words.next().unwrap_or("");
        if text.trim() != text {
            return Err(bad("noncanonical text spacing"));
        }
        if verb.needs_reason() && text.is_empty() {
            return Err(bad("this control verb requires a reason"));
        }
        if verb.needs_commit() && !matches!(subject, ControlSubject::Code { .. }) {
            return Err(bad(
                "verdicts, decisions and holds require an issue/PR and commit",
            ));
        }
        Ok(Self {
            sender,
            subject,
            verb,
            id,
            reply_to,
            text,
        })
    }

    pub fn sender(&self) -> &'a str {
        self.sender
    }
    pub fn subject(&self) -> ControlSubject<'a> {
        self.subject
    }
    pub fn verb(&self) -> ControlVerb {
        self.verb
    }
    pub fn id(&self) -> &'a str {
        self.id
    }
    pub fn reply_to(&self) -> Option<&'a str> {
        self.reply_to
    }
    pub fn text(&self) -> &'a str {
        self.text
    }
}

impl fmt::Display for ControlLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}> {} {} {}",
            self.sender,
            self.subject,
            self.verb.as_str(),
            self.id
        )?;
        if let Some(reply) = self.reply_to {
            write!(f, "/{reply}")?;
        }
        if !self.text.is_empty() {
            write!(f, " {}", self.text)?;
        }
        Ok(())
    }
}

fn atom(word: &str, max: usize) -> bool {
    !word.is_empty()
        && word.len() <= max
        && word.as_bytes()[0].is_ascii_alphanumeric()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn parse_subject(word: &str) -> Result<ControlSubject<'_>, ControlParseError> {
    if let Some(topic) = word.strip_prefix('~').filter(|t| atom(t, 64)) {
        return Ok(ControlSubject::Topic(topic));
    }
    let (number, commit) = word
        .strip_prefix("ox#")
        .and_then(|w| w.split_once('@'))
        .ok_or(ControlParseError("malformed control subject"))?;
    let parsed = number.parse::<u64>().ok().filter(|n| *n > 0);
    if number.starts_with('0')
        || !number.bytes().all(|b| b.is_ascii_digit())
        || parsed.is_none()
        || !(7..=40).contains(&commit.len())
        || !commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ControlParseError("invalid issue/PR number or commit"));
    }
    Ok(ControlSubject::Code {
        number: parsed.unwrap(),
        commit,
    })
}

/// Diagnostics contain no copy of the rejected message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlParseError(pub &'static str);

impl fmt::Display for ControlParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ControlParseError {}
