//! Which jobs go first when several wait for room: jobs of the main branch and deploys.
//!
//! What a job message says about its job before it runs is all there is to go by: its repository, its name, its
//! workflow file at its ref (`jobWorkflowRef`, `owner/repo/.github/workflows/ci.yml@refs/heads/main`), its run and
//! the run's event. It names no head branch and no run attempt. The ref is read from the workflow's ref only when the
//! workflow is the repository's own: a reusable workflow called from a pull request names its own repository and
//! ref. Without a ref, the run's event stands in for it.

use cf_budget::Turn;
use cf_scaleset::JobBase;

/// Words in a job's or a workflow file's name that make it a deploy.
const DEPLOY_WORDS: [&str; 3] = ["deploy", "wdrożeni", "wdrozeni"];

/// Events of a run that, when its ref is not known, count as the main branch's: what a merge or a person starts.
const MAIN_EVENTS: [&str; 4] = ["push", "workflow_dispatch", "release", "deployment"];

/// The job's place in the line of jobs that wait: urgent when it deploys or runs for the main branch.
pub fn turn_of(job: &JobBase) -> Turn {
    Turn {
        urgent: deploys(job) || on_main(job),
        repo: job.repository(),
    }
}

/// Whether the job's name or its workflow file's name says it deploys.
fn deploys(job: &JobBase) -> bool {
    let file = workflow_path(job).rsplit('/').next().unwrap_or_default();
    [job.job_display_name.as_str(), file].iter().any(|name| {
        let name = name.to_lowercase();
        DEPLOY_WORDS.iter().any(|word| name.contains(word))
    })
}

/// Whether the job runs for the main branch: by its workflow's ref, or by the run's event when the ref is not the
/// repository's own or not given.
fn on_main(job: &JobBase) -> bool {
    match own_ref(job) {
        Some(git_ref) => git_ref == "refs/heads/main",
        None => MAIN_EVENTS.contains(&job.event_name.as_str()),
    }
}

/// The workflow file's path, `owner/repo/.github/workflows/<file>`.
fn workflow_path(job: &JobBase) -> &str {
    job.job_workflow_ref
        .rsplit_once('@')
        .map_or(job.job_workflow_ref.as_str(), |(path, _)| path)
}

/// The ref the workflow runs at, when the workflow is the job's repository's own.
fn own_ref(job: &JobBase) -> Option<&str> {
    let (path, git_ref) = job.job_workflow_ref.rsplit_once('@')?;
    let repo = job.repository();
    let own = path.len() > repo.len()
        && path.is_char_boundary(repo.len())
        && path[..repo.len()].eq_ignore_ascii_case(&repo)
        && path[repo.len()..].starts_with('/');
    (own && !repo.is_empty() && !git_ref.is_empty()).then_some(git_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(workflow_ref: &str, name: &str, event: &str) -> JobBase {
        JobBase {
            owner_name: "acme".into(),
            repository_name: "shop".into(),
            job_workflow_ref: workflow_ref.into(),
            job_display_name: name.into(),
            event_name: event.into(),
            ..JobBase::default()
        }
    }

    const CI: &str = "acme/shop/.github/workflows/ci.yml";

    #[test]
    fn the_ref_decides_when_the_workflow_is_the_repositorys_own() {
        let main = job(&format!("{CI}@refs/heads/main"), "check", "pull_request");
        assert!(turn_of(&main).urgent, "the ref wins over the event");
        let pull = job(&format!("{CI}@refs/pull/12/merge"), "check", "push");
        assert!(!turn_of(&pull).urgent);
        let branch = job(&format!("{CI}@refs/heads/mainline"), "check", "push");
        assert!(
            !turn_of(&branch).urgent,
            "a branch whose name starts with main is not main"
        );
        let upper = job("ACME/Shop/.github/workflows/ci.yml@refs/heads/main", "c", "");
        assert!(turn_of(&upper).urgent, "GitHub's names ignore case");
        assert_eq!(turn_of(&main).repo, "acme/shop");
    }

    #[test]
    fn a_reusable_workflow_of_another_repository_at_main_does_not_make_a_pull_request_urgent() {
        let called = job(
            "acme/shared/.github/workflows/check.yml@refs/heads/main",
            "check",
            "pull_request",
        );
        assert!(!turn_of(&called).urgent);
        // A repository whose name begins with this one's is another repository.
        let longer = job(
            "acme/shop-old/.github/workflows/ci.yml@refs/heads/main",
            "check",
            "pull_request",
        );
        assert!(!turn_of(&longer).urgent);
        let pushed = job(
            "acme/shared/.github/workflows/check.yml@refs/heads/main",
            "check",
            "push",
        );
        assert!(turn_of(&pushed).urgent, "then the event says it");
    }

    #[test]
    fn without_a_ref_the_event_stands_in() {
        for (event, urgent) in [
            ("push", true),
            ("workflow_dispatch", true),
            ("pull_request", false),
            ("pull_request_target", false),
            ("schedule", false),
            ("", false),
        ] {
            assert_eq!(turn_of(&job("", "check", event)).urgent, urgent, "{event}");
        }
        assert!(!turn_of(&job("no-at-sign", "check", "pull_request")).urgent);
        assert!(!turn_of(&job(&format!("{CI}@"), "check", "pull_request")).urgent);
    }

    #[test]
    fn a_deploy_goes_first_on_any_ref() {
        let preview = job(
            &format!("{CI}@refs/pull/3/merge"),
            "wdrożenie sklepu",
            "pull_request",
        );
        assert!(turn_of(&preview).urgent);
        assert!(turn_of(&job("", "Deploy to production", "pull_request")).urgent);
        assert!(turn_of(&job("", "WDROŻENIE", "")).urgent, "in capitals too");
        assert!(
            turn_of(&job("", "wdrozenie", "")).urgent,
            "and without the diacritic"
        );
        let file = job(
            "acme/shop/.github/workflows/deploy.yml@refs/pull/3/merge",
            "build",
            "pull_request",
        );
        assert!(turn_of(&file).urgent, "by the workflow file's name");
        let folder = job(
            "acme/deployer/.github/workflows/ci.yml@refs/pull/3/merge",
            "build",
            "pull_request",
        );
        assert!(!turn_of(&folder).urgent, "a repository's name is not the job's");
    }

    #[test]
    fn a_job_that_says_nothing_waits_its_turn() {
        let unknown = JobBase::default();
        assert_eq!(turn_of(&unknown), Turn::default());
        let odd = job("@@@", "\u{202e}ʎolqəp", "pull_request");
        assert!(!turn_of(&odd).urgent);
        let long = job(&"a".repeat(100_000), &"b".repeat(100_000), "pull_request");
        assert!(!turn_of(&long).urgent);
    }
}
