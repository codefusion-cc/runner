//! The environment a job's workload starts with.

/// The image is a root filesystem without the environment its Dockerfile declared, so what the Actions runner and
/// the tools in the image expect is set here. Entries of the job itself come after and win.
pub fn workload_environment(home: &str, user: &str, extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = [
        (
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        ),
        ("HOME", home),
        ("USER", user),
        ("LOGNAME", user),
        ("LANG", "C.UTF-8"),
        ("DEBIAN_FRONTEND", "noninteractive"),
        ("RUNNER_TOOL_CACHE", "/opt/hostedtoolcache"),
        ("AGENT_TOOLSDIRECTORY", "/opt/hostedtoolcache"),
        // run.sh forwards a stop to the Actions runner, which then cancels its job and uploads the log.
        ("RUNNER_MANUALLY_TRAP_SIG", "1"),
        (
            "ACTIONS_RUNNER_HOOK_JOB_STARTED",
            "/run/codefusion/job-started.sh",
        ),
        (
            "ACTIONS_RUNNER_HOOK_JOB_COMPLETED",
            "/run/codefusion/job-completed.sh",
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect();
    for (name, value) in extra {
        // A name with `=` or NUL, or an empty one, cannot be an environment variable.
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            continue;
        }
        all.retain(|(existing, _)| existing != name);
        all.push((name.clone(), value.clone()));
    }
    all
}

/// The user a workload runs as, from the image's `/etc/passwd`: `(uid, gid, home)`. `None` when the file does not
/// name the user.
pub fn find_user(passwd: &str, user: &str) -> Option<(u32, u32, String)> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != user {
            return None;
        }
        let _password = fields.next()?;
        let uid = fields.next()?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        let _comment = fields.next()?;
        let home = fields.next()?;
        home.starts_with('/').then(|| (uid, gid, home.to_string()))
    })
}

/// The groups a user is in, from the image's `/etc/group`: the primary one first, then every group that lists the
/// user. `sudo` asks for a group, so a workload without them cannot become root in its container.
pub fn groups_of(group: &str, user: &str, primary: u32) -> Vec<u32> {
    let mut groups = vec![primary];
    for line in group.lines() {
        let mut fields = line.split(':');
        let (Some(_name), Some(_password), Some(id), Some(members)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if let Ok(id) = id.parse::<u32>()
            && members.split(',').any(|member| member.trim() == user)
            && !groups.contains(&id)
        {
            groups.push(id);
        }
    }
    groups
}
