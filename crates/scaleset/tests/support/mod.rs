//! A scale set client through the fake console to the fake GitHub, with real sockets and the real clients between.
#![allow(dead_code)]

pub mod recorder;

pub use cf_harness::*;
use cf_scaleset::{MessageSession, ScaleSet};

/// A scale set on `cf-2g` with an open session.
pub async fn with_session(h: &Harness) -> (ScaleSet, MessageSession) {
    let set = h
        .client
        .ensure_scale_set("mac-a--cf-2g", &labels(&["self-hosted", "cf-2g"]))
        .await
        .expect("scale set");
    let session = h.client.open_session(set.id).await.expect("session");
    (set, session)
}
