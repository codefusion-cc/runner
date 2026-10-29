use cf_scaleset::{Error, JitConfig, MessageSession, ScaleSetClient};

use crate::traits::{Jobs, Registry};

impl Jobs for MessageSession {
    async fn acquire(&self, request_ids: &[i64]) -> Result<Vec<i64>, Error> {
        self.acquire_jobs(request_ids).await
    }
}

/// A repository's Actions runner registry: the client, and where it registers Actions runners.
#[derive(Debug, Clone)]
pub struct SetRegistry {
    client: ScaleSetClient,
    kind: Kind,
}

#[derive(Debug, Clone)]
enum Kind {
    /// With this scale set.
    Set(i64),
    /// Outside any scale set, each carrying exactly these labels: Actions runners of their own ([`crate::held`]).
    Own(Vec<String>),
}

impl SetRegistry {
    /// The registry of a scale set's Actions runners.
    pub fn of_set(client: ScaleSetClient, scale_set_id: i64) -> SetRegistry {
        SetRegistry {
            client,
            kind: Kind::Set(scale_set_id),
        }
    }

    /// The registry of Actions runners of their own, outside any scale set, each carrying exactly `labels`.
    pub fn own(client: ScaleSetClient, labels: Vec<String>) -> SetRegistry {
        SetRegistry {
            client,
            kind: Kind::Own(labels),
        }
    }
}

impl Registry for SetRegistry {
    async fn generate_jit(&self, name: &str) -> Result<JitConfig, Error> {
        match &self.kind {
            Kind::Set(scale_set_id) => self.client.generate_jit_config(*scale_set_id, name).await,
            Kind::Own(labels) => self.client.generate_plain_jit_config(name, labels).await,
        }
    }

    async fn remove_runner(&self, runner_id: i64) -> Result<bool, Error> {
        self.client.remove_runner(runner_id).await
    }

    async fn runner_busy(&self, runner_id: i64) -> Result<Option<bool>, Error> {
        self.client.runner_busy(runner_id).await
    }
}
