use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::config::Settings;
use crate::host_tool::{HostOutput, ScriptInput, Written};

mod bashkit;
#[cfg(target_os = "linux")]
mod bwrap;
#[cfg(target_os = "macos")]
mod seatbelt;

use self::bashkit::Bashkit;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    clap::ValueEnum,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Bashkit,

    #[cfg(target_os = "linux")]
    Bwrap,

    #[cfg(target_os = "macos")]
    Seatbelt,
}

#[async_trait]
pub trait Backend: Send + Sync {
    async fn exec_script(
        &self,
        script: &str,
        arg0: &str,
        args: Vec<String>,
        stdin: ScriptInput,
    ) -> anyhow::Result<HostOutput>;

    async fn read_text(&self, path: &Path) -> anyhow::Result<String>;

    async fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<Written>;
}

pub async fn new(
    settings: &Settings,
    session: Option<PathBuf>,
) -> anyhow::Result<Box<dyn Backend>> {
    anyhow::ensure!(
        session.is_none() || settings.backend() == Kind::Bashkit,
        "--session is only supported by the bashkit backend"
    );
    match settings.backend() {
        Kind::Bashkit => Ok(Box::new(Bashkit::new(settings, session).await?)),
        #[cfg(target_os = "linux")]
        Kind::Bwrap => Ok(Box::new(bwrap::Bwrap::new(settings).await?)),
        #[cfg(target_os = "macos")]
        Kind::Seatbelt => Ok(Box::new(seatbelt::Seatbelt::new(settings).await?)),
    }
}
