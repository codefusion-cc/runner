//! The Linux monitor: cloud-hypervisor's static aarch64 build, pinned in `crates/vmm-ch/src/pins.rs`.
//!
//! `cargo xtask cloud-hypervisor` fetches it into `target/vmm-programs`, where the build script of `cf-vmm-ch`
//! embeds it. A file already there with the pinned bytes is kept. It is read from our copy first, a release of this
//! repository tagged `cloud-hypervisor-<version>`, and from the project's release only when the copy is missing
//! (`pinned.rs`).
//!
//! `cargo xtask cloud-hypervisor check`, run weekly by `.github/workflows/pins.yml`, goes red when our copy stops
//! answering with the pinned bytes, and says when the project has a newer release.
//!
//! To move the pin: take `cloud-hypervisor-static-aarch64` of the new release, check that its SHA-256 is the
//! `digest` GitHub lists for the asset (`gh api repos/cloud-hypervisor/cloud-hypervisor/releases/tags/<version>`),
//! publish our copy (`gh release create cloud-hypervisor-<version> cloud-hypervisor-static-aarch64 --prerelease
//! --latest=false`, by the owner), then change the three numbers in `pins.rs`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::pinned::{self, Checked, Curl, GITHUB_RELEASE_HOSTS, MAX_LISTING_BYTES, Pin, Scratch, Source};

mod pins {
    include!("../../crates/vmm-ch/src/pins.rs");
}

pub use pins::CLOUD_HYPERVISOR_VERSION as VERSION;

/// What the build calls it.
const WHAT: &str = "cloud-hypervisor";
/// The asset's name in a release.
const ASSET: &str = "cloud-hypervisor-static-aarch64";
/// Where `cf-vmm-ch`'s build script looks for it, below the workspace.
const DIR: &str = "target/vmm-programs";

/// The pinned file.
pub const PIN: Pin<'static> = Pin {
    size: pins::CLOUD_HYPERVISOR_SIZE,
    sha256: pins::CLOUD_HYPERVISOR_SHA256,
};

/// Where a build reads it, in order: our copy, then the project's release.
pub fn sources() -> [Source; 2] {
    [
        Source {
            name: "our copy",
            url: format!(
                "https://github.com/codefusion-cc/runner/releases/download/cloud-hypervisor-{VERSION}/{ASSET}"
            ),
            hosts: GITHUB_RELEASE_HOSTS,
        },
        Source {
            name: "the project's release",
            url: format!(
                "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/{VERSION}/{ASSET}"
            ),
            hosts: GITHUB_RELEASE_HOSTS,
        },
    ]
}

/// The project's newest release, as GitHub's API says it.
fn latest_release() -> Source {
    Source {
        name: "the project's latest release",
        url: "https://api.github.com/repos/cloud-hypervisor/cloud-hypervisor/releases/latest".into(),
        hosts: &["api.github.com"],
    }
}

/// `cargo xtask cloud-hypervisor [check]`.
pub fn run(root: &Path, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("check") => check(root, &args[1..]),
        _ => crate::no_options(args).and_then(|()| fetch(root).map(|_| ())),
    }
}

/// Puts the pinned file into `target/vmm-programs` and returns its path.
pub fn fetch(root: &Path) -> Result<PathBuf> {
    let dest = root.join(DIR).join("cloud-hypervisor");
    pinned::ensure(&Curl::https(), &sources(), PIN, &dest, WHAT)?;
    println!("{} is cloud-hypervisor {VERSION}", dest.display());
    Ok(dest)
}

/// Fails only when our copy does not give the pinned bytes. The project slow, down or newer is a warning.
fn check(root: &Path, args: &[String]) -> Result<()> {
    crate::no_options(args)?;
    let dir = Scratch::new(root.join("target/cloud-hypervisor-check"))?;
    let checked = check_sources(&Curl::https(), &sources(), &latest_release(), dir.path());
    pinned::conclude(checked, WHAT, "xtask/src/cloud_hypervisor.rs")
}

fn check_sources(curl: &Curl, sources: &[Source], latest: &Source, dir: &Path) -> Checked {
    let mut checked = pinned::check_sources(curl, sources, PIN, dir, WHAT);
    let file = dir.join("latest.json");
    let newest = pinned::get(curl, latest, MAX_LISTING_BYTES, curl.listing, &file)
        .map_err(|e| e.to_string())
        .and_then(|()| fs::read(&file).map_err(|e| e.to_string()))
        .and_then(|bytes| tag_of(&bytes));
    match newest {
        Ok(newest) if newest == VERSION => println!("cloud-hypervisor has no release newer than {VERSION}"),
        Ok(newest) => checked.notices.push(format!(
            "cloud-hypervisor has {newest}, the pin is {VERSION}: move the pin (xtask/src/cloud_hypervisor.rs)"
        )),
        Err(error) => checked
            .notices
            .push(format!("could not read cloud-hypervisor's latest release: {error}")),
    }
    checked
}

/// The tag of a release as GitHub's API describes it.
fn tag_of(json: &[u8]) -> std::result::Result<String, String> {
    let release: serde_json::Value = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    release["tag_name"]
        .as_str()
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "the release has no tag".into())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::pinned::host_of;
    use crate::pinned::testing::*;

    fn release(tag: &str) -> Vec<u8> {
        ok(format!(r#"{{"tag_name":"{tag}","name":"Release","assets":[]}}"#).as_bytes())
    }

    fn latest(port: u16) -> Source {
        Source {
            name: "latest",
            ..source(port, "/latest")
        }
    }

    /// `check_sources` with a stand-in pin: the real one names megabytes the tests do not serve.
    fn checked(curl: &Curl, sources: &[Source], latest: &Source) -> Checked {
        let dir = tempfile::tempdir().unwrap();
        let hash = sha(BODY);
        let mut checked = pinned::check_sources(curl, sources, pin(&hash), dir.path(), WHAT);
        let newest = check_sources(curl, &[], latest, dir.path());
        checked.notices.extend(newest.notices);
        checked
    }

    #[test]
    fn the_pin_is_current_and_says_nothing() {
        let port = serve(vec![("/copy", ok(BODY)), ("/latest", release(VERSION))]);
        let checked = checked(&local(port), &[source(port, "/copy")], &latest(port));
        assert_eq!(checked.copy, Ok(()));
        assert!(checked.notices.is_empty(), "{:?}", checked.notices);
    }

    #[test]
    fn a_newer_release_is_a_warning_naming_both() {
        let port = serve(vec![("/copy", ok(BODY)), ("/latest", release("v99.0"))]);
        let checked = checked(&local(port), &[source(port, "/copy")], &latest(port));
        assert_eq!(checked.copy, Ok(()));
        assert_eq!(
            checked.notices,
            [format!(
                "cloud-hypervisor has v99.0, the pin is {VERSION}: move the pin (xtask/src/cloud_hypervisor.rs)"
            )]
        );
    }

    #[test]
    fn an_api_that_is_down_slow_or_says_nonsense_is_a_warning_not_red() {
        let stalled = stall();
        let port = serve(vec![
            ("/copy", ok(BODY)),
            ("/html", ok(b"<html>rate limited</html>")),
            ("/untagged", ok(br#"{"tag_name":""}"#)),
            ("/other", ok(br#"{"message":"API rate limit exceeded"}"#)),
        ]);
        for (latest, why) in [
            (source(port, "/gone"), "HTTP 404"),
            (source(stalled, "/latest"), "curl failed"),
            (source(port, "/html"), "expected value"),
            (source(port, "/untagged"), "no tag"),
            (source(port, "/other"), "no tag"),
        ] {
            let started = Instant::now();
            let checked = checked(&local(port), &[source(port, "/copy")], &latest);
            assert_eq!(checked.copy, Ok(()));
            assert_eq!(checked.notices.len(), 1, "{:?}", checked.notices);
            assert!(
                checked.notices[0].starts_with("could not read cloud-hypervisor's latest release")
                    && checked.notices[0].contains(why),
                "{:?}",
                checked.notices
            );
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "took {:?}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn our_copy_missing_is_red_even_when_the_project_answers() {
        let port = serve(vec![("/project", ok(BODY)), ("/latest", release(VERSION))]);
        let checked = checked(
            &local(port),
            &[source(port, "/copy"), source(port, "/project")],
            &latest(port),
        );
        assert!(
            checked.copy.as_ref().unwrap_err().contains("HTTP 404"),
            "{:?}",
            checked.copy
        );
        assert!(checked.notices.is_empty(), "{:?}", checked.notices);
    }

    #[test]
    fn the_sources_are_https_release_assets_of_the_pinned_version() {
        let [ours, project] = sources();
        assert_eq!(
            ours.url,
            format!(
                "https://github.com/codefusion-cc/runner/releases/download/cloud-hypervisor-{VERSION}/{ASSET}"
            )
        );
        assert_eq!(
            project.url,
            format!(
                "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/{VERSION}/{ASSET}"
            )
        );
        for source in [ours, project, latest_release()] {
            assert!(source.url.starts_with("https://"), "{}", source.url);
            assert!(source.hosts.contains(&host_of(&source.url).unwrap().as_str()));
        }
        assert_eq!(PIN.sha256.len(), 64);
        assert!(
            PIN.sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }
}
