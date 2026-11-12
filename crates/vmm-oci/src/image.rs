//! The job image in the engine. A container-image release's root file system is a gzip tar of the files a job's
//! machine boots from; the image store verifies it and hands it here ([`cf_image::store::Target::Engine`]), which
//! imports it as an image with no name, names it `codefusion-runner-guest:<version>` ([`crate::image_tag`]) once its
//! hash is the signed one, and removes it when it is not.

use crate::http::{Body, Engine, escape};
use crate::{CALL, IMAGE_REPOSITORY};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

/// How long the engine may take to unpack what it was sent before its answer.
const IMPORT: Duration = Duration::from_secs(30 * 60);

/// The images of the engine at a socket.
pub struct Images {
    engine: Engine,
}

impl Images {
    /// The engine at `socket`.
    pub fn new(socket: &Path) -> Self {
        Self {
            engine: Engine::new(socket),
        }
    }
}

/// The id of the image an import made, from the engine's answer: one JSON object after another, the last naming
/// the image (Docker: `{"status":"sha256:…"}`; Podman: `{"id":"…"}`), or one saying why there is none.
pub fn imported_id(answer: &[u8]) -> Result<String, String> {
    let mut id = None;
    for message in serde_json::Deserializer::from_slice(answer).into_iter::<serde_json::Value>() {
        let message = message.map_err(|error| format!("the engine's answer to the import: {error}"))?;
        if let Some(error) = message
            .get("error")
            .or_else(|| message.get("errorDetail")?.get("message"))
        {
            return Err(format!(
                "the import was refused: {}",
                error.as_str().unwrap_or("for no reason given")
            ));
        }
        let named = message.get("id").and_then(serde_json::Value::as_str).or_else(|| {
            message
                .get("status")?
                .as_str()
                .filter(|status| status.starts_with("sha256:"))
        });
        if let Some(named) = named {
            id = Some(named.to_string());
        }
    }
    let id = id.ok_or("the engine named no image for the import")?;
    let hex = id.strip_prefix("sha256:").unwrap_or(&id);
    if hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(id)
    } else {
        Err(format!(
            "the engine named the import {id:?}, which is no image id"
        ))
    }
}

impl cf_image::store::Engine for Images {
    fn import(&mut self, tar: &mut dyn Read, size: u64) -> Result<String, String> {
        let answer = self
            .engine
            .upload(
                "/images/create?fromSrc=-",
                "application/x-tar",
                tar,
                size,
                Some(IMPORT),
            )
            .map_err(|error| format!("the import: {error}"))?;
        if !answer.ok() {
            return Err(format!(
                "the import was refused ({}): {}",
                answer.status,
                answer.message()
            ));
        }
        imported_id(&answer.body)
    }

    fn name(&mut self, id: &str, version: &str) -> Result<(), String> {
        let path = format!(
            "/images/{}/tag?repo={}&tag={}",
            escape(id),
            escape(IMAGE_REPOSITORY),
            escape(version)
        );
        let answer = self
            .engine
            .request("POST", &path, Body::Empty, Some(CALL))
            .map_err(|error| format!("naming the image: {error}"))?;
        if answer.ok() {
            Ok(())
        } else {
            Err(format!(
                "the image was not named ({}): {}",
                answer.status,
                answer.message()
            ))
        }
    }

    fn remove(&mut self, id: &str) {
        let _ = self.engine.request(
            "DELETE",
            &format!("/images/{}?force=1", escape(id)),
            Body::Empty,
            Some(CALL),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn the_import_names_its_image_as_docker_and_podman_say_it() {
        let docker = format!("{{\"status\":\"{ID}\"}}\r\n");
        assert_eq!(imported_id(docker.as_bytes()), Ok(ID.into()));
        let podman = format!(
            "{{\"status\":\"Download complete\",\"progressDetail\":{{}},\"id\":\"{}\"}}\n",
            &ID[7..]
        );
        assert_eq!(imported_id(podman.as_bytes()), Ok(ID[7..].into()));
        let progress = format!("{{\"status\":\"Importing\",\"progressDetail\":{{\"current\":5}}}}\n{docker}");
        assert_eq!(imported_id(progress.as_bytes()), Ok(ID.into()));
    }

    #[test]
    fn an_import_that_names_no_image_is_refused_with_what_the_engine_said() {
        let refused =
            imported_id(b"{\"errorDetail\":{\"message\":\"archive/tar: invalid tar header\"}}").unwrap_err();
        assert!(refused.contains("invalid tar header"), "{refused}");
        assert!(
            imported_id(b"{\"error\":\"no space left on device\"}")
                .unwrap_err()
                .contains("no space")
        );
        for nothing in [
            &b""[..],
            b"{\"status\":\"Importing\"}",
            b"{\"id\":\"../../etc\"}",
            b"not json",
        ] {
            assert!(
                imported_id(nothing).is_err(),
                "{:?}",
                String::from_utf8_lossy(nothing)
            );
        }
    }
}
