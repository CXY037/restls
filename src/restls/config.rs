use std::collections::HashSet;
use std::net::IpAddr;
use tokio::sync::Mutex;
use lazy_static::lazy_static;

lazy_static! {
    pub static ref LOGGED_IPS: Mutex<HashSet<IpAddr>> = Mutex::new(HashSet::new());
}