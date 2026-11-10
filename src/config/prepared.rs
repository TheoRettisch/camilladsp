//! The only configuration boundary accepted by the controller and guard.

use super::Configuration;
use crate::{Res, protection};
use serde::Serialize;

/// An admitted policy and identity bound to the exact normalized graph.
///
/// Construction consumes the editable configuration. Access is read-only;
/// editing a clone or consuming this wrapper requires validation again before
/// the result can enter processing. Serialization exposes only the graph, and
/// deserialization must go through `Configuration` and `validate_config`.
///
/// ```compile_fail
/// use camillalib::config::PreparedConfig;
/// fn edit(prepared: &mut PreparedConfig) {
///     prepared.config().pipeline = None;
/// }
/// ```
///
/// ```compile_fail
/// use camillalib::config::PreparedConfig;
/// let bypass: PreparedConfig = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct PreparedConfig {
    config: Configuration,
    #[serde(skip)]
    guard: protection::AdmittedGuard,
}

impl PreparedConfig {
    pub fn config(&self) -> &Configuration {
        &self.config
    }

    pub fn into_config(self) -> Configuration {
        self.config
    }

    pub fn identity(&self) -> protection::identity::Identity {
        self.guard.identity()
    }

    pub(crate) fn guard(&self) -> &protection::AdmittedGuard {
        &self.guard
    }
}

/// Normalize and validate before compiling admission, policy and identity once.
pub fn validate_config(mut config: Configuration, filename: Option<&str>) -> Res<PreparedConfig> {
    super::utils::normalize_and_validate(&mut config, filename)?;
    let guard = protection::AdmittedGuard::new(&config)?;
    Ok(PreparedConfig { config, guard })
}
