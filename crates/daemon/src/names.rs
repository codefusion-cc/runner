use cf_ghapp::Repo;
use cf_workflows::{LabelSet, POOL_LABEL};
use sha2::{Digest, Sha256};

/// Lowercases `text`, joins its runs of letters and digits with single dashes, and cuts it to at most `max` bytes
/// (0: no limit) without a dash at either end. Machine names and scale set names are made of slugs.
pub fn slug(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    if max > 0 && out.len() > max {
        out.truncate(max);
        out.truncate(out.trim_end_matches('-').len());
    }
    out
}

/// Turns a host name like `Build-Mac-2.local` into a short lowercase name like `build-mac-2`.
pub fn machine_name(hostname: &str) -> String {
    let name = slug(hostname.strip_suffix(".local").unwrap_or(hostname), 24);
    if name.is_empty() { "mac".into() } else { name }
}

/// Names an Actions runner after the machine, with a random part that never is all digits: some workflows read a
/// number from the end of the Actions runner name.
pub fn runner_name(machine: &str, random: u32) -> String {
    format!("{machine}-j{:07x}", random & 0x0fff_ffff)
}

/// The name an Actions runner's machine has on this Mac: the random part of the Actions runner's name, which is
/// what a vm-host allows (32 plain characters at most) whatever the Mac is called.
pub fn job_name(runner: &str) -> &str {
    runner.rsplit('-').next().unwrap_or(runner)
}

/// Names a machine's scale set for one repository's label set: the machine, a double dash (which machine names
/// never contain, so `mac-a` never claims the sets of `mac-a-2`), the labels for a person to read, and a hash of the
/// repository and the whole set, so that two repositories that ask for the same labels, and two sets whose labels
/// read the same, never share a name.
pub fn scale_set_name(machine: &str, repo: &str, set: &LabelSet) -> String {
    let parts: Vec<String> = set
        .labels()
        .iter()
        .filter(|l| !matches!(l.as_str(), POOL_LABEL | "linux" | "arm64"))
        .map(|l| slug(l, 0))
        .filter(|s| !s.is_empty())
        .collect();
    let mut name = format!("{machine}-");
    if !parts.is_empty() {
        name.push('-');
        name.push_str(&parts.join("-"));
    }
    if name.len() > 46 {
        name.truncate(46);
        name.truncate(name.trim_end_matches('-').len());
    }
    let mut hash = Sha256::new();
    hash.update(repo.to_lowercase().as_bytes());
    hash.update([0]);
    hash.update(set.key().as_bytes());
    let sum = hash.finalize();
    format!("{name}-{:02x}{:02x}{:02x}{:02x}", sum[0], sum[1], sum[2], sum[3])
}

/// Which repositories the Runner takes jobs from.
#[derive(Debug, Clone)]
pub struct ServePolicy {
    /// Serve only these `owner/name` repositories; empty serves every private repository the App can see.
    pub repos: Vec<String>,
    /// Public repositories to serve on purpose: their jobs can be a stranger's code.
    pub public_repos: Vec<String>,
    /// The repositories whose `cf-docker` jobs get a Docker daemon: private ones only, whatever else is listed.
    pub docker_repos: Vec<String>,
    /// The most Docker jobs at once ([`crate::machines::MachineConfig::docker_max_jobs`]).
    pub docker_max_jobs: usize,
    /// The host labels this host has ([`cf_workflows::is_host_label`]): a set that asks for others is left to
    /// the hosts that have them.
    pub host_labels: Vec<String>,
}

impl Default for ServePolicy {
    fn default() -> Self {
        ServePolicy {
            repos: Vec::new(),
            public_repos: Vec::new(),
            docker_repos: Vec::new(),
            docker_max_jobs: 1,
            host_labels: Vec::new(),
        }
    }
}

impl ServePolicy {
    /// Whether the Runner takes jobs from a repository: an active one, private or listed as public, and in the
    /// list when there is one.
    pub fn serves(&self, repo: &Repo) -> bool {
        if repo.archived || repo.disabled {
            return false;
        }
        if !repo.private && !listed(&self.public_repos, repo) {
            return false;
        }
        self.repos.is_empty() || listed(&self.repos, repo) || listed(&self.public_repos, repo)
    }

    /// Whether a repository's `cf-docker` jobs are served: one the Runner serves, private, and in
    /// [`ServePolicy::docker_repos`]. A public repository never is, even listed: anyone can open a pull request
    /// there, and a Docker job is the most a job can do on a machine.
    pub fn serves_docker(&self, repo: &Repo) -> bool {
        repo.private && self.serves(repo) && listed(&self.docker_repos, repo)
    }

    /// Whether this host serves a label set: it has every host label the set asks for.
    pub fn serves_labels(&self, set: &LabelSet) -> bool {
        set.host_labels().all(|wanted| {
            self.host_labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case(wanted))
        })
    }
}

/// Whether a list of `owner/name` repositories names this one; GitHub's names ignore case.
fn listed(list: &[String], repo: &Repo) -> bool {
    list.iter().any(|name| name.eq_ignore_ascii_case(&repo.full_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn repo(name: &str, private: bool) -> Repo {
        Repo {
            full_name: name.into(),
            pushed_at: None,
            html_url: String::new(),
            private,
            archived: false,
            disabled: false,
        }
    }

    proptest! {
        #[test]
        fn a_runner_name_never_ends_in_a_number(random in any::<u32>()) {
            let name = runner_name("build-mac-2", random);
            let last = name.rsplit('-').next().unwrap();
            prop_assert!(name.starts_with("build-mac-2-j"));
            prop_assert_eq!(last.len(), 8);
            prop_assert!(last.chars().any(|c| !c.is_ascii_digit()));
            prop_assert!(name.len() <= 64);
        }

        #[test]
        fn a_slug_has_only_lowercase_letters_digits_and_inner_dashes(text in ".{0,80}", max in 0usize..40) {
            let s = slug(&text, max);
            prop_assert!(s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
            prop_assert!(!s.starts_with('-') && !s.ends_with('-') && !s.contains("--"));
            if max > 0 { prop_assert!(s.len() <= max); }
        }
    }

    #[test]
    fn runner_names_at_the_edges() {
        assert_eq!(runner_name("m", 0), "m-j0000000");
        assert_eq!(runner_name("m", u32::MAX), "m-jfffffff");
    }

    #[test]
    fn machine_names_come_from_host_names() {
        assert_eq!(machine_name("Build-Mac-2.local"), "build-mac-2");
        assert_eq!(machine_name("Ünïcode Host"), "n-code-host");
        assert_eq!(machine_name("...local"), "mac");
        assert_eq!(machine_name(""), "mac");
        assert_eq!(machine_name(&"a".repeat(100)).len(), 24);
    }

    #[test]
    fn scale_set_names_tell_sets_and_repositories_apart_and_machines_never_claim_each_others() {
        let plain = LabelSet::new(["self-hosted", "cf-2g"]);
        let other = LabelSet::new(["self-hosted", "cf-4g"]);
        let a = scale_set_name("mac-a", "org/app", &plain);
        assert!(a.starts_with("mac-a--cf-2g-"), "{a}");
        assert_ne!(a, scale_set_name("mac-a", "org/app", &other));
        assert_ne!(
            a,
            scale_set_name("mac-a", "org/shop", &plain),
            "two repositories that ask for the same labels"
        );
        assert_ne!(
            scale_set_name("mac-a", "org/a", &LabelSet::new(["self-hosted", "b"])),
            scale_set_name("mac-a", "org/a-b", &LabelSet::new(["self-hosted"])),
        );
        assert_ne!(a, scale_set_name("mac-a-2", "org/app", &plain));
        assert!(
            !scale_set_name("mac-a-2", "org/app", &plain).starts_with("mac-a--"),
            "mac-a must not claim the sets of mac-a-2"
        );
        assert_eq!(
            a,
            scale_set_name("mac-a", "Org/App", &LabelSet::new(["CF-2G", "Self-Hosted"])),
            "case and order do not matter"
        );
        let long = scale_set_name(
            &"m".repeat(24),
            "org/app",
            &LabelSet::new(["self-hosted", &"x".repeat(80)]),
        );
        assert!(long.len() <= 55, "{long}");
        assert!(long.starts_with(&format!("{}--x", "m".repeat(24))), "{long}");
    }

    #[test]
    fn only_private_repositories_in_the_list_are_served() {
        let open = ServePolicy::default();
        assert!(open.serves(&repo("org/app", true)));
        assert!(
            !open.serves(&repo("org/public", false)),
            "a public repository's jobs can be a stranger's code"
        );
        let mut archived = repo("org/old", true);
        archived.archived = true;
        assert!(!open.serves(&archived));
        let mut disabled = repo("org/off", true);
        disabled.disabled = true;
        assert!(!open.serves(&disabled));
        let listed = ServePolicy {
            repos: vec!["Org/App".into()],
            ..ServePolicy::default()
        };
        assert!(listed.serves(&repo("org/app", true)));
        assert!(!listed.serves(&repo("org/other", true)));
        let on_purpose = ServePolicy {
            public_repos: vec!["org/public".into()],
            ..ServePolicy::default()
        };
        assert!(on_purpose.serves(&repo("org/public", false)));
        assert!(on_purpose.serves(&repo("org/app", true)));
    }

    #[test]
    fn docker_is_for_listed_private_repositories_only() {
        let policy = ServePolicy {
            public_repos: vec!["org/public".into()],
            docker_repos: vec!["Org/Shop".into(), "org/public".into(), "org/gone".into()],
            ..ServePolicy::default()
        };
        assert!(policy.serves_docker(&repo("org/shop", true)), "listed, any case");
        assert!(
            !policy.serves_docker(&repo("org/app", true)),
            "served, not listed"
        );
        assert!(
            !policy.serves_docker(&repo("org/public", false)),
            "a public repository never gets Docker, even listed twice"
        );
        let mut archived = repo("org/gone", true);
        archived.archived = true;
        assert!(!policy.serves_docker(&archived), "listed but not served");
        let narrow = ServePolicy {
            repos: vec!["org/app".into()],
            docker_repos: vec!["org/shop".into()],
            ..ServePolicy::default()
        };
        assert!(
            !narrow.serves_docker(&repo("org/shop", true)),
            "listed for Docker, not served at all"
        );
        assert!(
            !ServePolicy::default().serves_docker(&repo("org/shop", true)),
            "no list, no Docker"
        );
        assert_eq!(ServePolicy::default().docker_max_jobs, 1);
    }

    #[test]
    fn a_set_with_host_labels_is_served_only_by_a_host_that_has_them_all() {
        let mac = ServePolicy {
            host_labels: vec!["cf-macos".into(), "cf-studio".into()],
            ..ServePolicy::default()
        };
        let set = |labels: &[&str]| LabelSet::new(labels);
        for labels in [
            &["cf-8g"][..],
            &["cf-8g", "cf-macos"],
            &["CF-8G", "CF-MACOS"],
            &["self-hosted", "cf-macos", "cf-studio"],
            &["cf-8g", "heyhubs"],
            &["cf-docker", "cf-macos"],
        ] {
            assert!(mac.serves_labels(&set(labels)), "{labels:?}");
        }
        for labels in [
            &["cf-8g", "cf-linux"][..],
            &["cf-8g", "cf-macos", "cf-linux"],
            &["cf-8g", "cf-mini"],
        ] {
            assert!(!mac.serves_labels(&set(labels)), "{labels:?}");
        }
        // A host with no labels serves only the sets that ask for none.
        assert!(ServePolicy::default().serves_labels(&set(&["cf-8g"])));
        assert!(!ServePolicy::default().serves_labels(&set(&["cf-8g", "cf-macos"])));
    }
}
