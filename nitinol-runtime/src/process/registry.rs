use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::ident::{Pid, ProcessName};
use crate::process::AnyProxy;

pub struct ProcessRegistry {
    processes: RwLock<HashMap<Pid, AnyProxy>>,
    aliases: RwLock<HashMap<ProcessName, Pid>>
}