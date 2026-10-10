//! A scale set client against the fake GitHub, with real sockets and the real GitHub App client in between.
#![allow(dead_code)]

pub mod recorder;

pub use cf_fake_github::harness::*;
use cf_scaleset::{MessageSession, ScaleSet};

/// A scale set on `cf-2g` with an open session.
pub async fn with_session(h: &Harness) -> (ScaleSet, MessageSession) {
    let set = h
        .client
        .ensure_scale_set("air--cf-2g", &labels(&["self-hosted", "cf-2g"]))
        .await
        .expect("scale set");
    let session = h.client.open_session(set.id, "air").await.expect("session");
    (set, session)
}
