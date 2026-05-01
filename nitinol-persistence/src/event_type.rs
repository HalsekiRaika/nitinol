#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventType(&'static str);

impl EventType {
    pub const fn from_str(s: &'static str) -> Self {
        Self(s)
    }

    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}
