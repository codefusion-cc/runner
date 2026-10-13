//! The kernel against what this crate writes: tables applied in a fresh user and network namespace of their own and
//! listed back. Each test runs its body in a child of this test program that it starts in a new namespace, so no
//! rule ever reaches the host's own. Where unprivileged user namespaces are not allowed, a test says so and passes,
//! unless `CF_REQUIRE_NAMESPACES` is set (CI sets it, so a runner that cannot do this goes red, not quiet).
#![cfg(target_os = "linux")]

mod common;

use cf_nft::{
    ARP, Chain, Error, Family, Hook, IPV4, IPV6, Load, Match, TCP, Table, UDP, Verdict, apply, batch, list,
};
use common::{dest, device, rule};
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Set in the child that runs a test's body inside its namespace.
const INSIDE: &str = "CF_NFT_INSIDE_NAMESPACE";
/// The child's exit code when the kernel will not let it change rules in its own namespace.
const NOT_ALLOWED: i32 = 77;

fn write(path: &std::ffi::CStr, text: &[u8]) -> io::Result<()> {
    // SAFETY: open, write and close take a path and a buffer we own; they are safe between fork and exec.
    unsafe {
        let file = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if file < 0 {
            return Err(io::Error::last_os_error());
        }
        let written = libc::write(file, text.as_ptr().cast(), text.len());
        libc::close(file);
        if written != text.len() as isize {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Runs `body` in a child of this program in a new user and network namespace, as root of that user namespace
/// only. In the parent it returns once the child passed.
fn in_namespace(test: &str, body: impl FnOnce()) {
    if std::env::var_os(INSIDE).is_some() {
        allowed();
        body();
        return;
    }
    let required = std::env::var_os("CF_REQUIRE_NAMESPACES").is_some();
    let skip = |why: String| {
        assert!(!required, "{test}: {why}");
        eprintln!("SKIPPED {test}: {why}");
    };
    // SAFETY: getuid and getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let (uid_map, gid_map) = (format!("0 {uid} 1"), format!("0 {gid} 1"));
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(INSIDE, "1");
    // SAFETY: the closure only makes system calls (unshare, open, write, close) and allocates nothing.
    unsafe {
        command.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
                return Err(io::Error::last_os_error());
            }
            write(c"/proc/self/setgroups", b"deny")?;
            write(c"/proc/self/uid_map", uid_map.as_bytes())?;
            write(c"/proc/self/gid_map", gid_map.as_bytes())
        });
    }
    let output = match command.output() {
        Ok(output) => output,
        Err(error) => return skip(format!("no user and network namespace of our own: {error}")),
    };
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    match output.status.code() {
        Some(0) if said.contains("1 passed") => {}
        Some(NOT_ALLOWED) => skip(format!(
            "the kernel keeps rules from an unprivileged namespace: {said}"
        )),
        _ => panic!("{test} failed in its namespace ({}):\n{said}", output.status),
    }
}

/// Ends the child as not allowed when the kernel refuses it the rules of its own namespace, as a kernel that holds
/// unprivileged user namespaces to fewer capabilities does.
fn allowed() {
    match apply(&batch(&[], Load::Replace).unwrap()) {
        Err(Error::Netlink(cf_netlink::Error::Refused { error, .. }))
            if error.raw_os_error() == Some(libc::EPERM) =>
        {
            println!("refused for want of permission: {error}");
            std::process::exit(NOT_ALLOWED);
        }
        result => result.unwrap(),
    }
}

fn apply_tables(tables: &[Table], load: Load) {
    apply(&batch(tables, load).unwrap()).unwrap();
}

/// A bridge firewall in front of a tap, as the host puts it there.
fn bridge() -> Table {
    let tap = Match::InInterface(device("tap0"));
    let mut rules = vec![
        rule(&[tap, Match::EtherType(IPV6)], Verdict::Drop),
        rule(&[tap, Match::EtherType(ARP)], Verdict::Accept),
        rule(
            &[
                tap,
                dest([10, 0, 2, 3], 32),
                Match::Protocol(UDP),
                Match::DestPort(53),
            ],
            Verdict::Accept,
        ),
        rule(&[tap, Match::Protocol(UDP), Match::DestPort(67)], Verdict::Accept),
    ];
    for (network, prefix) in [
        ([10, 0, 0, 0], 8),
        ([172, 16, 0, 0], 12),
        ([192, 168, 0, 0], 16),
        ([169, 254, 0, 0], 16),
        ([100, 64, 0, 0], 10),
        ([127, 0, 0, 0], 8),
        ([0, 0, 0, 0], 8),
        ([224, 0, 0, 0], 4),
        ([203, 0, 113, 7], 32),
    ] {
        rules.push(rule(&[tap, dest(network, prefix)], Verdict::Drop));
    }
    rules.push(rule(&[Match::Protocol(TCP), Match::Established], Verdict::Accept));
    Table {
        family: Family::Bridge,
        name: "codefusion".into(),
        chains: vec![Chain {
            name: "forward".into(),
            hook: Hook::Forward,
            policy: Verdict::Accept,
            rules,
        }],
    }
}

/// An allowlist on the ingress of the namespace's loopback device, which every namespace has.
fn netdev() -> Table {
    Table {
        family: Family::Netdev,
        name: "allow".into(),
        chains: vec![Chain {
            name: "ingress".into(),
            hook: Hook::Ingress(device("lo")),
            policy: Verdict::Drop,
            rules: vec![
                rule(&[Match::EtherType(ARP)], Verdict::Accept),
                rule(
                    &[
                        dest([127, 0, 0, 0], 8),
                        Match::Protocol(TCP),
                        Match::DestPort(443),
                    ],
                    Verdict::Accept,
                ),
                rule(
                    &[Match::EtherType(IPV4), Match::InInterface(device("lo"))],
                    Verdict::Accept,
                ),
            ],
        }],
    }
}

/// The guest's kind of table.
fn inet() -> Table {
    Table {
        family: Family::Inet,
        name: "guest".into(),
        chains: vec![
            Chain {
                name: "output".into(),
                hook: Hook::Output,
                policy: Verdict::Accept,
                rules: vec![
                    rule(&[Match::OutLoopback], Verdict::Accept),
                    rule(&[Match::Ipv6], Verdict::Drop),
                ],
            },
            Chain {
                name: "input".into(),
                hook: Hook::Input,
                policy: Verdict::Drop,
                rules: vec![
                    rule(&[Match::InLoopback], Verdict::Accept),
                    rule(&[Match::Established], Verdict::Accept),
                ],
            },
        ],
    }
}

#[test]
fn a_bridge_firewall_is_what_the_kernel_holds_and_a_second_apply_replaces_it() {
    in_namespace(
        "a_bridge_firewall_is_what_the_kernel_holds_and_a_second_apply_replaces_it",
        || {
            assert_eq!(list().unwrap(), []);
            apply_tables(&[bridge()], Load::Replace);
            assert_eq!(list().unwrap(), [bridge()]);
            apply_tables(&[bridge()], Load::Replace);
            assert_eq!(
                list().unwrap(),
                [bridge()],
                "the second apply took the place of the first"
            );
        },
    );
}

#[test]
fn every_family_is_taken_and_a_replace_leaves_only_its_own_tables() {
    in_namespace(
        "every_family_is_taken_and_a_replace_leaves_only_its_own_tables",
        || {
            apply_tables(&[inet(), netdev()], Load::Add);
            assert_eq!(list().unwrap(), [inet(), netdev()]);
            apply_tables(&[bridge()], Load::Replace);
            assert_eq!(list().unwrap(), [bridge()]);
        },
    );
}

#[test]
fn a_batch_the_kernel_refuses_changes_nothing() {
    in_namespace("a_batch_the_kernel_refuses_changes_nothing", || {
        apply_tables(&[inet()], Load::Replace);
        // The bridge table's chain on a hook no family has (9 for 2, forward): the kernel refuses the chain, the
        // fourth message after the removal and the table.
        let mut refused = batch(&[bridge()], Load::Replace).unwrap();
        let hook = [8, 0, 1, 0, 0, 0, 0, 2];
        let at = refused
            .bytes
            .windows(hook.len())
            .position(|window| window == hook)
            .unwrap();
        refused.bytes[at + 7] = 9;
        match apply(&refused) {
            Err(Error::Netlink(cf_netlink::Error::Refused { message, .. })) => assert_eq!(message, 4),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            list().unwrap(),
            [inet()],
            "the removal in the refused batch did not happen"
        );
    });
}
