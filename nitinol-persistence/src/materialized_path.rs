use std::fmt;
use std::str::FromStr;

use crate::event_type::EventType;

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
#[derive(Debug, thiserror::Error)]
pub enum MaterializedPathParseError {
    #[error("a materialized path must contain at least one segment")]
    Empty,
    #[error("materialized path `{input}` has an empty segment at position {index}")]
    EmptySegment { input: String, index: usize },
}

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
        if let Some(variant) = event_type.variant() {
            segments.push(variant.as_str().to_owned());
        }
        Self { segments }
    }
}
