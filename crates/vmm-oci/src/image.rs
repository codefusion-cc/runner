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

/// What [`Images::prune`] did: the names it removed, and those the engine kept because a container uses them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pruned {
    /// `codefusion-runner-guest:<version>` names removed, oldest first.
    pub removed: Vec<String>,
    /// Names the engine did not remove, as a container still uses them: left for the next install.
    pub kept: Vec<String>,
}

impl Images {
    /// Removes the engine's older job images once `version` is installed: every `codefusion-runner-guest` name but
    /// `version`'s and the newest before it, which a Runner whose settings still name it runs until they move on.
    /// The engine refuses to remove an image a container uses (a job's, or one not yet removed), which stays.
    pub fn prune(&self, version: &str) -> Result<Pruned, String> {
        let answer = self
            .engine
            .request("GET", "/images/json", Body::Empty, Some(CALL))
            .map_err(|error| format!("the engine's images: {error}"))?;
        if !answer.ok() {
            return Err(format!(
                "the engine's images ({}): {}",
                answer.status,
                answer.message()
            ));
        }
        let listed = answer
            .json()
            .map_err(|error| format!("the engine's images: {error}"))?;
        // Each name of ours with its image's age; Podman names an import `localhost/<repository>:<tag>`.
        let mut ours: Vec<(i64, String, String)> = listed
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|image| {
                let created = image
                    .get("Created")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                image
                    .get("RepoTags")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .filter_map(move |name| {
                        let (repository, tag) = name.rsplit_once(':')?;
                        let repository = repository.strip_prefix("localhost/").unwrap_or(repository);
                        (repository == IMAGE_REPOSITORY).then(|| (created, tag.to_string(), name.to_string()))
                    })
            })
            .collect();
        ours.sort();
        // The newest other than the new one stays: a Runner may still name it.
        let before = ours
            .iter()
            .rev()
            .find(|(_, tag, _)| tag != version)
            .map(|(_, tag, _)| tag.clone());
        let mut pruned = Pruned::default();
        for (_, tag, name) in ours {
            if tag == version || Some(&tag) == before.as_ref() {
                continue;
            }
            // Never forced: the engine keeps an image a container uses.
            let removed = self.engine.request(
                "DELETE",
                &format!("/images/{}", escape(&name)),
                Body::Empty,
                Some(CALL),
            );
            match removed {
                Ok(answer) if answer.ok() || answer.status == 404 => pruned.removed.push(name),
                _ => pruned.kept.push(name),
            }
        }
        Ok(pruned)
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
    use crate::fake::FakeEngine;

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

    /// Each release adds a whole root file system to the engine: once a new one is in, the older ones go, but for
    /// the one just before it, and one a container uses stays.
    #[test]
    fn an_install_leaves_the_new_image_and_the_one_before_and_removes_the_older_ones() {
        let listed = serde_json::json!([
            { "Id": "sha256:a", "RepoTags": ["localhost/codefusion-runner-guest:image-20261001-1"], "Created": 100 },
            { "Id": "sha256:b", "RepoTags": ["codefusion-runner-guest:image-20261002-1"], "Created": 200 },
            { "Id": "sha256:c", "RepoTags": ["codefusion-runner-guest:image-20261003-1"], "Created": 300 },
            { "Id": "sha256:d", "RepoTags": ["codefusion-runner-guest:image-20261004-1"], "Created": 400 },
            { "Id": "sha256:e", "RepoTags": ["localhost/codefusion-runner-guest:image-20261105-2"], "Created": 500 },
            { "Id": "sha256:f", "RepoTags": ["busybox:latest", "codefusion-runner-guestx:1"], "Created": 50 },
            { "Id": "sha256:g", "RepoTags": ["<none>:<none>"], "Created": 60 },
            { "Id": "sha256:h", "RepoTags": null, "Created": 70 },
        ])
        .to_string();
        let in_use = format!("/images/{}", escape("codefusion-runner-guest:image-20261003-1"));
        let engine = FakeEngine::new(move |asked| match asked.method.as_str() {
            "GET" if asked.path.starts_with("/images/json") => (200, listed.clone()),
            "DELETE" if asked.path == in_use => {
                (409, "{\"message\":\"image is in use by a container\"}".into())
            }
            "DELETE" => (200, "[]".into()),
            _ => (404, "{}".into()),
        });
        let pruned = Images::new(&engine.socket).prune("image-20261105-2").unwrap();
        assert_eq!(
            pruned,
            Pruned {
                removed: vec![
                    "localhost/codefusion-runner-guest:image-20261001-1".into(),
                    "codefusion-runner-guest:image-20261002-1".into(),
                ],
                kept: vec!["codefusion-runner-guest:image-20261003-1".into()],
            }
        );
        let deleted = engine.lines("DELETE");
        assert_eq!(deleted.len(), 3, "{deleted:?}");
        assert!(
            deleted.iter().all(|line| !line.contains("force")),
            "never forced off a container: {deleted:?}"
        );
    }

    #[test]
    fn an_engine_that_lists_nothing_or_fails_removes_nothing() {
        for (status, body) in [(200, "[]"), (500, "{\"message\":\"down\"}"), (200, "not json")] {
            let engine = FakeEngine::new(move |_| (status, body.into()));
            let pruned = Images::new(&engine.socket).prune("image-20261105-2");
            assert!(engine.lines("DELETE").is_empty());
            if status == 200 && body == "[]" {
                assert_eq!(pruned, Ok(Pruned::default()));
            } else {
                assert!(pruned.is_err(), "{pruned:?}");
            }
        }
    }
}
