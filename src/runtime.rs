use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::config::{Config, Paths};
use crate::cron::{CronService, SystemClock};
use crate::pairing::PairingStore;
use crate::sandbox::SandboxProvider;

pub struct Runtime {
    pub config: Arc<Config>,
    pub paths: Arc<Paths>,
    pub provider: Arc<dyn SandboxProvider>,
    pub pairing: Mutex<PairingStore>,
    pub cron: CronService<SystemClock>,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
