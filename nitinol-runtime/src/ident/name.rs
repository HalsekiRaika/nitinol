/// The name given to a process, used to find it with
/// [`ProcessSystem::lookup_by_name`](crate::ProcessSystem::lookup_by_name).
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct ProcessName(Box<str>);

impl ProcessName {
    /// Creates a name from `name`.
    pub fn new(name: impl AsRef<str>) -> Self {
        Self(name.as_ref().into())
    }
}

impl AsRef<str> for ProcessName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProcessName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
