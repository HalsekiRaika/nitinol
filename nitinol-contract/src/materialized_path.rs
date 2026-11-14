use std::fmt;
use std::str::FromStr;

use crate::event_type::EventType;

/// The canonical `.`-separated path an [`EventType`] is written down as.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MaterializedPath {
    pub(crate) segments: Vec<String>,
}

impl MaterializedPath {
    /// Returns `true` when `self` equals `ancestor` or is one of its
    /// descendants. Matching is on `.`-delimited segment boundaries, so
    /// `nitinol.sagax` is not within `nitinol.saga`.
    pub fn is_within(&self, ancestor: &MaterializedPath) -> bool {
        if ancestor.segments.len() > self.segments.len() {
            return false;
        }
        self.segments[..ancestor.segments.len()] == ancestor.segments[..]
    }
}

/// Error raised when a string cannot be parsed as a [`MaterializedPath`].
///
/// `Display` and `Error` are written out by hand so that the identity contract
/// takes on no dependency beyond `bytes`.
#[derive(Debug)]
pub enum MaterializedPathParseError {
    /// The input was empty.
    Empty,
    /// The input contained an empty segment.
    EmptySegment {
        /// The string that failed to parse.
        input: String,
        /// The zero-based position of the empty segment.
        index: usize,
    },
}

impl fmt::Display for MaterializedPathParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a materialized path must contain at least one segment"),
            Self::EmptySegment { input, index } => write!(
                f,
                "materialized path `{input}` has an empty segment at position {index}"
            ),
        }
    }
}

impl std::error::Error for MaterializedPathParseError {}

impl FromStr for MaterializedPath {
    type Err = MaterializedPathParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(MaterializedPathParseError::Empty);
        }
        let mut segments = Vec::new();
        for (index, segment) in s.split('.').enumerate() {
            if segment.is_empty() {
                return Err(MaterializedPathParseError::EmptySegment {
                    input: s.to_owned(),
                    index,
                });
            }
            segments.push(segment.to_owned());
        }
        Ok(Self { segments })
    }
}

impl fmt::Display for MaterializedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.segments.join("."))
    }
}

impl From<&EventType> for MaterializedPath {
    fn from(event_type: &EventType) -> Self {
        let mut segments = Vec::new();
        let family = event_type.family().as_str();
        if !family.is_empty() {
            segments.extend(family.split('.').map(str::to_owned));
        }
        segments.push(event_type.type_name().as_str().to_owned());
        Self { segments }
    }
}
