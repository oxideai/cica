use std::sync::Arc;

use crate::config::{Config, Paths};
use crate::sandbox::SandboxProvider;

pub struct Runtime {
    pub config: Arc<Config>,
    pub paths: Arc<Paths>,
    pub provider: Box<dyn SandboxProvider>,
}
