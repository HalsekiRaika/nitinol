pub struct Context {
    pub(crate) sequence: i64,
}

impl Context {
    pub fn sequence(&self) -> i64 {
        self.sequence
    }
}