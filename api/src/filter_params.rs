use std::time::Duration;

use super::selector::Selector;

#[derive(Default, Clone, Debug)]
pub struct FilterParams {
    pub fields: Option<Selector>,
    pub labels: Option<Selector>,
    pub timeout: Option<Duration>,
}

impl FilterParams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_fields(mut self, fields: impl Into<Selector>) -> Self {
        self.fields = Some(fields.into());
        self
    }

    pub fn with_labels(mut self, labels: impl Into<Selector>) -> Self {
        self.labels = Some(labels.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

#[cfg(feature = "client")]
impl From<&FilterParams> for kube::api::ListParams {
    fn from(params: &FilterParams) -> Self {
        let mut list_params = Self::default();
        if let Some(fields) = params.fields.as_ref() {
            list_params = list_params.fields(&fields.to_string());
        }
        if let Some(labels) = params.labels.as_ref() {
            list_params = list_params.labels(&labels.to_string());
        }
        if let Some(timeout) = params.timeout {
            list_params = list_params.timeout(timeout.as_secs() as u32);
        }
        list_params
    }
}

#[cfg(feature = "client")]
impl From<&FilterParams> for kube::api::WatchParams {
    fn from(params: &FilterParams) -> Self {
        let mut watch_params = Self::default();
        if let Some(fields) = params.fields.as_ref() {
            watch_params = watch_params.fields(&fields.to_string());
        }
        if let Some(labels) = params.labels.as_ref() {
            watch_params = watch_params.labels(&labels.to_string());
        }
        if let Some(timeout) = params.timeout {
            watch_params = watch_params.timeout(timeout.as_secs() as u32);
        }
        watch_params
    }
}

#[cfg(feature = "runtime")]
impl From<&FilterParams> for kube::runtime::watcher::Config {
    fn from(params: &FilterParams) -> Self {
        let mut watch_config = Self::default();
        if let Some(fields) = params.fields.as_ref() {
            watch_config = watch_config.fields(&fields.to_string());
        }
        if let Some(labels) = params.labels.as_ref() {
            watch_config = watch_config.labels(&labels.to_string());
        }
        if let Some(timeout) = params.timeout {
            watch_config = watch_config.timeout(timeout.as_secs() as u32);
        }
        watch_config
    }
}
