use cf_scaleset::{Error, JitConfig, MessageSession, ScaleSetClient};

use crate::traits::{Jobs, Registry};

impl Jobs for MessageSession {
    async fn acquire(&self, request_ids: &[i64]) -> Result<Vec<i64>, Error> {
        self.acquire_jobs(request_ids).await
    }
}

/// A scale set's Actions runner registry: the client and the scale set it registers Actions runners with.
#[derive(Debug, Clone)]
pub struct SetRegistry {
    /// The scale set client.
    pub client: ScaleSetClient,
    /// The scale set.
    pub scale_set_id: i64,
}

impl Registry for SetRegistry {
    async fn generate_jit(&self, name: &str) -> Result<JitConfig, Error> {
        self.client
            .generate_jit_config(self.scale_set_id, name, "_work")
            .await
    }

    async fn remove_runner(&self, runner_id: i64) -> Result<(), Error> {
        self.client.remove_runner(runner_id).await
    }

    async fn runner_busy(&self, runner_id: i64) -> Result<Option<bool>, Error> {
        self.client.runner_busy(runner_id).await
    }
}
