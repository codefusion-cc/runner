//! The Go manager's `labels_test.go`, case by case, then what the Rust version adds: size labels, repository
//! variables, hostile input.

use std::collections::BTreeMap;

use cf_workflows::{
    DOCKER_LABEL, LabelSet, POOL_LABEL, SizeError, is_host_label, self_hosted, self_hosted_with, size_gb,
};
use proptest::prelude::*;

fn files(list: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    list.iter()
        .map(|(n, c)| (n.to_string(), c.as_bytes().to_vec()))
        .collect()
}

fn keys(sets: &[LabelSet]) -> Vec<String> {
    sets.iter().map(LabelSet::key).collect()
}

fn vars(list: &[(&str, &str)]) -> BTreeMap<String, String> {
    list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

// Go: TestSelfHosted, one case per row.
#[test]
fn self_hosted_cases_of_the_go_manager() {
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Vec<(&str, &str)>, Vec<&str>)> = vec![
        ("no workflows", vec![], vec![]),
        (
            "list form, as HeyHubs writes it",
            vec![(
                "ci.yml",
                r####"
jobs:
  test:
    runs-on: [self-hosted, heyhubs]
"####,
            )],
            vec!["heyhubs,self-hosted"],
        ),
        (
            "block list, mixed case and repeats",
            vec![(
                "ci.yml",
                r####"
jobs:
  test:
    runs-on:
      - Self-Hosted
      - DieselPaul
      - dieselpaul
"####,
            )],
            vec!["dieselpaul,self-hosted"],
        ),
        (
            "one set per distinct request, across files, deduplicated",
            vec![
                (
                    "ci.yml",
                    "jobs:\n  a:\n    runs-on: [self-hosted, dieselpaul]\n  b:\n    runs-on: [dieselpaul, self-hosted]\n",
                ),
                (
                    "backup.yml",
                    "jobs:\n  backup:\n    runs-on: [self-hosted, dieselpaul-backup]\n",
                ),
            ],
            vec!["dieselpaul,self-hosted", "dieselpaul-backup,self-hosted"],
        ),
        (
            "GitHub-hosted jobs are not ours",
            vec![(
                "ci.yml",
                r####"
jobs:
  hosted:
    runs-on: ubuntu-latest
  also-hosted:
    runs-on: [ubuntu-24.04-arm]
"####,
            )],
            vec![],
        ),
        (
            "a bare self-hosted string is a request",
            vec![("ci.yml", "jobs:\n  a:\n    runs-on: self-hosted\n")],
            vec!["self-hosted"],
        ),
        (
            "jobs that need another platform are left alone",
            vec![(
                "ci.yml",
                r####"
jobs:
  mac:
    runs-on: [self-hosted, macOS, heyhubs]
  intel:
    runs-on: [self-hosted, X64]
  win:
    runs-on: [self-hosted, windows]
  linux:
    runs-on: [self-hosted, linux, ARM64]
"####,
            )],
            vec!["arm64,linux,self-hosted"],
        ),
        (
            "expressions cannot be known ahead of a run",
            vec![(
                "ci.yml",
                r####"
jobs:
  matrix:
    runs-on: ${{ matrix.os }}
  partly:
    runs-on: [self-hosted, "${{ inputs.label }}"]
"####,
            )],
            vec![],
        ),
        (
            "a runner from a variable is the pool's: the Macs serve it under PoolLabel",
            vec![(
                "ci.yml",
                r####"
jobs:
  plain:
    runs-on: ${{ vars.CI_RUNNER || 'ubuntu-latest' }}
  heavy:
    runs-on: ${{ vars.CI_RUNNER_HEAVY || vars.CI_RUNNER || 'ubuntu-latest' }}
  light:
    runs-on: ${{ vars.CI_RUNNER_LIGHT || vars.CI_RUNNER || 'ubuntu-latest' }}
  small:
    runs-on: ${{ fromJSON(toJSON(vars.CI_RUNNER_SMALL || 'ubuntu-latest')) }}
  bare:
    runs-on: "${{vars.CI_RUNNER}}"
  indexed:
    runs-on: ${{ vars['CI_RUNNER'] }}
"####,
            )],
            vec![POOL_LABEL],
        ),
        (
            "a reusable workflow's runs-on input from a variable, as the automerge caller passes it",
            vec![(
                "automerge.yml",
                r####"
jobs:
  automerge:
    uses: codefusion-cc/codefusion/.github/workflows/automerge.yml@main
    with:
      runs-on: ${{ toJSON(vars.CI_RUNNER_SMALL || 'ubuntu-latest') }}
      client-id: ${{ vars.AUTOMERGE_CLIENT_ID }}
  fixed:
    uses: ./.github/workflows/automerge.yml
    with:
      runs-on: '"ubuntu-latest"'
  other-input:
    uses: ./.github/workflows/x.yml
    with:
      runner: ${{ vars.CI_RUNNER }}
"####,
            )],
            vec![POOL_LABEL],
        ),
        (
            "only one runs-on input counts, and only on a call",
            vec![(
                "ci.yml",
                r####"
jobs:
  not-a-call:
    runs-on: ubuntu-latest
    with:
      runs-on: ${{ vars.CI_RUNNER }}
  call-from-matrix:
    uses: ./.github/workflows/x.yml
    with:
      runs-on: ${{ toJSON(matrix.os) }}
"####,
            )],
            vec![],
        ),
        (
            "expressions that read anything but variables, or name another platform, stay unserved",
            vec![(
                "ci.yml",
                r####"
jobs:
  matrix:
    runs-on: ${{ matrix.os }}
  mixed-matrix:
    runs-on: ${{ vars.CI_RUNNER || matrix.os }}
  input:
    runs-on: ${{ inputs.runner || vars.CI_RUNNER }}
  github:
    runs-on: ${{ github.event_name == 'push' && vars.CI_RUNNER || 'ubuntu-latest' }}
  literal:
    runs-on: ${{ 'macos-latest' }}
  prefixed:
    runs-on: runner-${{ vars.CI_RUNNER }}
  two:
    runs-on: ${{ vars.A }}-${{ vars.B }}
  unclosed:
    runs-on: ${{ vars.CI_RUNNER || 'ubuntu-latest }}
  quoted-vars:
    runs-on: ${{ 'vars.CI_RUNNER' }}
  mac-list:
    runs-on: [self-hosted, macOS, "${{ vars.CI_RUNNER }}"]
"####,
            )],
            vec![],
        ),
        (
            "a variable in a list stands for the pool's label next to the plain ones",
            vec![(
                "ci.yml",
                r####"
jobs:
  with-repo-label:
    runs-on: ["${{ vars.CI_RUNNER }}", HeyHubs]
  with-self-hosted:
    runs-on: [self-hosted, "${{ vars.CI_RUNNER || 'ubuntu-latest' }}"]
  with-input:
    runs-on: [self-hosted, "${{ inputs.label }}", "${{ vars.CI_RUNNER }}"]
  plain:
    runs-on: [self-hosted, dieselpaul]
"####,
            )],
            vec!["dieselpaul,self-hosted", "heyhubs,self-hosted", "self-hosted"],
        ),
        (
            "labels mapping form is read; groups are not ours",
            vec![(
                "ci.yml",
                r####"
jobs:
  labels:
    runs-on:
      labels: [self-hosted, website]
  group:
    runs-on:
      group: big
      labels: [self-hosted, other]
"####,
            )],
            vec!["self-hosted,website"],
        ),
        (
            "a broken file does not hide the others",
            vec![
                ("broken.yml", "jobs: [unclosed\n  runs-on: [self-hosted, x"),
                ("ci.yml", "jobs:\n  a:\n    runs-on: [self-hosted, console]\n"),
            ],
            vec!["console,self-hosted"],
        ),
        (
            "reusable workflow calls and odd shapes are skipped",
            vec![(
                "ci.yml",
                r####"
on: push
jobs:
  call:
    uses: ./.github/workflows/other.yml
  nested:
    runs-on: [[self-hosted], x]
  empty:
    runs-on: [self-hosted, ""]
  number:
    runs-on: 5
"####,
            )],
            vec![],
        ),
        (
            "a file without jobs",
            vec![("action.yml", "name: x\nruns:\n  using: composite\n")],
            vec![],
        ),
        (
            "unicode labels survive as written, lowercased",
            vec![("ci.yml", "jobs:\n  a:\n    runs-on: [self-hosted, Zażółć]\n")],
            vec!["self-hosted,zażółć"],
        ),
    ];
    for (name, list, want) in cases {
        let got = keys(&self_hosted(&files(&list)));
        assert_eq!(got, want, "{name}");
    }
}

// Go: TestSelfHostedProperties. Every returned set is sorted, lowercase, free of repeats and has self-hosted,
// whatever the input; and splitting the jobs over files in any way gives the same answer.
proptest! {
    #[test]
    fn sets_are_normal_and_do_not_depend_on_the_split(picks in prop::collection::vec(prop::collection::vec(0usize..10, 1..5), 1..9)) {
        let pool = ["self-hosted", "Self-Hosted", "heyhubs", "HeyHubs", "linux", "ARM64", "macOS", "x64", "a", "b"];
        let jobs: Vec<String> = picks
            .iter()
            .enumerate()
            .map(|(j, labels)| {
                let labels: Vec<&str> = labels.iter().map(|&i| pool[i]).collect();
                format!("  j{j}:\n    runs-on: [{}]\n", labels.join(", "))
            })
            .collect();
        let one = files(&[("all.yml", &format!("jobs:\n{}", jobs.concat()))]);
        let split: BTreeMap<String, Vec<u8>> = jobs
            .iter()
            .enumerate()
            .map(|(i, job)| (format!("f{i}.yml"), format!("jobs:\n{job}").into_bytes()))
            .collect();
        let (a, b) = (self_hosted(&one), self_hosted(&split));
        prop_assert_eq!(keys(&a), keys(&b));
        let foreign = ["macos", "osx", "windows", "x64", "x86", "arm"];
        for set in &a {
            for (i, label) in set.labels().iter().enumerate() {
                prop_assert_eq!(label, &label.to_lowercase());
                prop_assert!(!foreign.contains(&label.as_str()));
                if i > 0 {
                    prop_assert!(set.labels()[i - 1] < *label);
                }
            }
            prop_assert!(set.contains("self-hosted"));
        }
    }

    // added: nothing a workflow file says can make it panic or hang, and whatever comes back is a normal set.
    #[test]
    fn arbitrary_bytes_are_never_a_panic(bytes in prop::collection::vec(any::<u8>(), 0..400)) {
        let sets = self_hosted(&BTreeMap::from([("x.yml".to_string(), bytes)]));
        for set in sets {
            prop_assert!(set.contains("self-hosted") || set.labels().iter().any(|l| l.starts_with("cf-")));
        }
    }

    // added: nor can an expression, in any shape, make the evaluator disagree with itself about what is served.
    #[test]
    fn arbitrary_expressions_are_never_a_panic(expression in "[ -~\u{0080}-\u{00ff}]{0,60}") {
        let yaml = format!("jobs:\n  a:\n    runs-on: [self-hosted, \"${{{{ {} }}}}\"]\n", expression.replace('"', "'").replace('\\', "/"));
        let _ = self_hosted_with(&files(&[("x.yml", &yaml)]), &vars(&[("CI_RUNNER", "cf-8g")]));
    }
}

// added: the size labels
#[test]
fn a_size_label_is_served_with_or_without_self_hosted() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  a:\n    runs-on: cf-8g\n  b:\n    runs-on: [self-hosted, cf-2g]\n  c:\n    runs-on: [cf-4g, linux]\n  d:\n    runs-on: ubuntu-latest\n",
    )]);
    assert_eq!(
        keys(&self_hosted(&wf)),
        ["cf-2g,self-hosted", "cf-4g,linux", "cf-8g"]
    );
}

#[test]
fn a_label_that_only_looks_like_a_size_is_not_ours() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  a:\n    runs-on: cf-3g\n  b:\n    runs-on: CF-16G\n  c:\n    runs-on: cf-g\n  d:\n    runs-on: [x, cf-4gb]\n  e:\n    runs-on: cf-04g\n  f:\n    runs-on: [cf-008g]\n",
    )]);
    assert!(self_hosted(&wf).is_empty());
}

#[test]
fn sizes_are_cf_labels_and_self_hosted_alone_is_four_gb() {
    let size = |labels: &[&str]| size_gb(&LabelSet::new(labels));
    assert_eq!(size(&["self-hosted"]), Ok(4));
    assert_eq!(size(&["self-hosted", "heyhubs"]), Ok(4));
    for n in [2, 4, 6, 8] {
        assert_eq!(size(&["self-hosted", &format!("CF-{n}G")]), Ok(n));
        assert_eq!(size(&[&format!("cf-{n}g")]), Ok(n));
    }
    assert_eq!(
        size(&["cf-8g", "cf-8g", "Cf-8g"]),
        Ok(8),
        "one size spelled twice is one size"
    );
    assert_eq!(
        size(&["self-hosted", "cf-3g"]),
        Err(SizeError::Unknown("cf-3g".into()))
    );
    assert_eq!(
        size(&["self-hosted", "cf-0g"]),
        Err(SizeError::Unknown("cf-0g".into()))
    );
    assert_eq!(
        size(&["self-hosted", "cf-99999999999999999999g"]),
        Err(SizeError::Unknown("cf-99999999999999999999g".into()))
    );
    assert_eq!(
        size(&["cf-2g", "cf-8g"]),
        Err(SizeError::Conflict("cf-2g".into(), "cf-8g".into()))
    );
    assert_eq!(
        size(&["self-hosted", "cf-04g"]),
        Err(SizeError::Unknown("cf-04g".into())),
        "a size has one spelling"
    );
    assert_eq!(
        size(&["self-hosted", "cf-4gb"]),
        Ok(4),
        "cf-4gb is no size label, just a label"
    );
}

// added: repository variables
#[test]
fn a_variable_naming_a_size_decides_the_scale_set() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  heavy:\n    runs-on: ${{ vars.CI_RUNNER_HEAVY || vars.CI_RUNNER || 'ubuntu-latest' }}\n  small:\n    runs-on: ${{ vars.CI_RUNNER_SMALL || 'ubuntu-latest' }}\n",
    )]);
    let none = self_hosted_with(&wf, &vars(&[]));
    assert_eq!(
        keys(&none),
        [POOL_LABEL],
        "no variables: both stand for the pool's label, as in Go"
    );

    let set = self_hosted_with(
        &wf,
        &vars(&[("CI_RUNNER_HEAVY", "cf-8g"), ("CI_RUNNER", "cf-4g")]),
    );
    assert_eq!(
        keys(&set),
        ["cf-8g", "self-hosted"],
        "heavy reads CI_RUNNER_HEAVY, small is still hosted and so the pool's"
    );

    // The first variable that is not empty wins, names are case-insensitive, JSON strings are unquoted.
    let set = self_hosted_with(
        &wf,
        &vars(&[
            ("CI_RUNNER_HEAVY", ""),
            ("CI_RUNNER", "\"CF-6G\""),
            ("CI_RUNNER_SMALL", "ubuntu-latest"),
        ]),
    );
    assert_eq!(keys(&set), ["cf-6g", "self-hosted"]);
    let set = self_hosted_with(&wf, &vars(&[("ci_runner_heavy", "cf-2g")]));
    assert_eq!(
        keys(&set),
        ["cf-2g", "self-hosted"],
        "variable names are case-insensitive"
    );
}

#[test]
fn a_variable_that_names_something_else_leaves_the_pool_label() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  a:\n    runs-on: ${{ vars.CI_RUNNER || 'ubuntu-latest' }}\n",
    )]);
    for value in [
        "ubuntu-latest",
        "macos-14",
        "cf-3g",
        "[\"self-hosted\"]",
        "self-hosted, cf-8g",
        " ",
    ] {
        assert_eq!(
            keys(&self_hosted_with(&wf, &vars(&[("CI_RUNNER", value)]))),
            [POOL_LABEL],
            "{value:?}"
        );
    }
    assert_eq!(
        keys(&self_hosted_with(&wf, &vars(&[("CI_RUNNER", "self-hosted")]))),
        [POOL_LABEL]
    );
}

#[test]
fn a_reusable_workflow_input_and_a_list_item_read_variables_too() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  call:\n    uses: o/r/.github/workflows/x.yml@main\n    with:\n      runs-on: ${{ toJSON(vars.CI_RUNNER_SMALL || 'ubuntu-latest') }}\n  list:\n    runs-on: [\"${{ vars.CI_RUNNER }}\", heyhubs]\n",
    )]);
    let set = self_hosted_with(
        &wf,
        &vars(&[("CI_RUNNER_SMALL", "cf-2g"), ("CI_RUNNER", "cf-4g")]),
    );
    assert_eq!(keys(&set), ["cf-2g", "cf-4g,heyhubs"]);
}

#[test]
fn an_expression_the_evaluator_cannot_read_still_stands_for_the_pool_label() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  a:\n    runs-on: ${{ format('{0}', vars.CI_RUNNER) }}\n  b:\n    runs-on: ${{ vars.A && vars.B }}\n",
    )]);
    assert_eq!(
        keys(&self_hosted_with(
            &wf,
            &vars(&[("CI_RUNNER", "cf-8g"), ("A", "x"), ("B", "cf-8g")])
        )),
        [POOL_LABEL]
    );
}

// added: hostile or odd files
#[test]
fn a_huge_file_is_skipped_and_the_others_still_count() {
    let big = format!(
        "jobs:\n  a:\n    runs-on: [self-hosted, big]\n# {}\n",
        "x".repeat(600 * 1024)
    );
    let wf = BTreeMap::from([
        ("big.yml".to_string(), big.into_bytes()),
        (
            "ok.yml".to_string(),
            b"jobs:\n  a:\n    runs-on: [self-hosted, ok]\n".to_vec(),
        ),
    ]);
    assert_eq!(keys(&self_hosted(&wf)), ["ok,self-hosted"]);
}

#[test]
fn yaml_that_expands_on_itself_does_not_hang_or_panic() {
    let bomb = "a: &a [x, x, x, x, x, x, x, x, x]\nb: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a]\nc: &c [*b, *b, *b, *b, *b, *b, *b, *b, *b]\nd: &d [*c, *c, *c, *c, *c, *c, *c, *c, *c]\ne: &e [*d, *d, *d, *d, *d, *d, *d, *d, *d]\nf: &f [*e, *e, *e, *e, *e, *e, *e, *e, *e]\njobs:\n  j:\n    runs-on: [self-hosted, bomb]\n";
    let started = std::time::Instant::now();
    let sets = self_hosted(&files(&[("bomb.yml", bomb)]));
    assert!(started.elapsed().as_secs() < 5);
    assert!(sets.len() <= 1);
    let deep = format!(
        "jobs:\n  a:\n    runs-on: {}[self-hosted]{}\n",
        "[".repeat(20_000),
        "]".repeat(20_000)
    );
    let _ = self_hosted(&files(&[("deep.yml", &deep)]));
}

#[test]
fn jobs_of_odd_shapes_are_skipped_one_by_one() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  not-a-map: 5\n  list-job: [1, 2]\n  with-string:\n    uses: x\n    with: oops\n  fine:\n    runs-on: [self-hosted, fine]\n  null-job:\n  numbers:\n    runs-on: [self-hosted, 5, true]\n",
    )]);
    assert_eq!(
        keys(&self_hosted(&wf)),
        ["5,self-hosted,true", "fine,self-hosted"]
    );
}

#[test]
fn a_file_whose_jobs_is_not_a_mapping_is_skipped() {
    for text in [
        "jobs: 5\n",
        "jobs:\n",
        "jobs: [a]\n",
        "- jobs\n",
        "",
        "\u{feff}jobs:\n  a:\n    runs-on: [self-hosted]\n",
    ] {
        let _ = self_hosted(&files(&[("x.yml", text)]));
    }
}

#[test]
fn the_result_does_not_depend_on_the_order_of_files() {
    let a = ("a.yml", "jobs:\n  x:\n    runs-on: [self-hosted, zeta]\n");
    let b = ("b.yml", "jobs:\n  x:\n    runs-on: [self-hosted, alpha]\n");
    assert_eq!(
        keys(&self_hosted(&files(&[a, b]))),
        keys(&self_hosted(&files(&[b, a])))
    );
    assert_eq!(
        keys(&self_hosted(&files(&[a, b]))),
        ["alpha,self-hosted", "self-hosted,zeta"]
    );
}

// added: the literals true, false and null are part of a variable expression; any other bare word is a context.
#[test]
fn boolean_and_null_literals_are_allowed_in_a_variable_expression() {
    let served = |expr: &str| {
        keys(&self_hosted(&files(&[(
            "ci.yml",
            &format!("jobs:\n  a:\n    runs-on: ${{{{ {expr} }}}}\n"),
        )])))
    };
    assert_eq!(served("vars.CI_RUNNER || null"), [POOL_LABEL]);
    assert_eq!(served("vars.CI_RUNNER || FALSE"), [POOL_LABEL]);
    assert!(served("vars.CI_RUNNER || maybe").is_empty());
    assert!(served("vars.CI_RUNNER || env.X").is_empty());
}

#[test]
fn nesting_deep_enough_to_exhaust_a_stack_is_skipped_and_the_others_still_count() {
    let deep = format!(
        "jobs:\n  a:\n    runs-on: {}{}\n",
        "[".repeat(100_000),
        "]".repeat(100_000)
    );
    let got = self_hosted(&files(&[
        ("deep.yml", deep.as_str()),
        ("ok.yml", "jobs:\n  a:\n    runs-on: [self-hosted, cf-8g]\n"),
    ]));
    assert_eq!(keys(&got), vec!["cf-8g,self-hosted"]);
}

#[test]
fn an_anchor_and_its_alias_name_the_same_labels() {
    let got = self_hosted(&files(&[(
        "ci.yml",
        "jobs:\n  a:\n    runs-on: &mac [self-hosted, cf-4g]\n  b:\n    runs-on: *mac\n    needs: a\n",
    )]));
    assert_eq!(keys(&got), vec!["cf-4g,self-hosted"]);
}

#[test]
fn keys_that_are_numbers_or_booleans_leave_the_rest_of_the_file_readable() {
    let got = self_hosted(&files(&[(
        "ci.yml",
        "on:\n  push:\n    branches: [main]\nenv:\n  1: one\n  true: yes\n  ? [a, b]\n  : complex\njobs:\n  7:\n    runs-on: [self-hosted, 16]\n",
    )]));
    assert_eq!(keys(&got), vec!["16,self-hosted"]);
}

// added: Docker
#[test]
fn the_docker_label_is_served_alone_beside_self_hosted_or_with_a_size() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  a:\n    runs-on: cf-docker\n  b:\n    runs-on: [self-hosted, CF-Docker]\n  c:\n    runs-on: [cf-docker, cf-6g]\n  d:\n    runs-on: [cf-docker, macos]\n  e:\n    runs-on: [docker]\n  f:\n    runs-on: cf-dockers\n",
    )]);
    let sets = self_hosted(&wf);
    assert_eq!(
        keys(&sets),
        ["cf-6g,cf-docker", "cf-docker", "cf-docker,self-hosted"]
    );
    assert!(sets.iter().all(LabelSet::docker));
    assert!(!LabelSet::new(["self-hosted", "cf-dockers"]).docker());
}

#[test]
fn a_docker_job_is_four_gb_unless_a_size_says_otherwise() {
    let size = |labels: &[&str]| size_gb(&LabelSet::new(labels));
    assert_eq!(size(&["cf-docker"]), Ok(4));
    assert_eq!(size(&["cf-docker", "self-hosted"]), Ok(4));
    assert_eq!(size(&["cf-docker", "cf-8g"]), Ok(8));
}

#[test]
fn a_variable_naming_the_docker_label_decides_the_scale_set() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  stack:\n    runs-on: ${{ vars.CI_RUNNER_DOCKER || 'ubuntu-latest' }}\n",
    )]);
    for value in ["cf-docker", "CF-DOCKER", "\"cf-docker\""] {
        assert_eq!(
            keys(&self_hosted_with(&wf, &vars(&[("CI_RUNNER_DOCKER", value)]))),
            [DOCKER_LABEL],
            "{value:?}"
        );
    }
    for value in ["cf-dockers", "docker", "ubuntu-latest"] {
        assert_eq!(
            keys(&self_hosted_with(&wf, &vars(&[("CI_RUNNER_DOCKER", value)]))),
            [POOL_LABEL],
            "{value:?}"
        );
    }
}

// added: host labels, which only the hosts that list them in their config serve
#[test]
fn a_host_label_is_a_cf_label_of_a_name_that_is_no_size_and_not_docker() {
    for label in ["cf-macos", "cf-linux", "cf-mac-studio", "cf-m4", "cf-a"] {
        assert!(is_host_label(label), "{label}");
    }
    let longest = format!("cf-{}", "a".repeat(37));
    assert!(is_host_label(&longest), "40 characters");
    for label in [
        "cf-8g",
        "cf-2g",
        "cf-docker",
        "cf-3g",
        "cf-4gb",
        "cf-",
        "cf--macos",
        "cf-macos-",
        "cf-mac--os",
        "cf-mac_os",
        "cf-mac os",
        "cf-macOS",
        "cf-mäc",
        "macos",
        "self-hosted",
        "xcf-macos",
        "",
    ] {
        assert!(!is_host_label(label), "{label:?}");
    }
    assert!(!is_host_label(&format!("{longest}a")), "41 characters");
}

#[test]
fn a_set_names_its_host_labels() {
    let set = LabelSet::new(["CF-8G", "cf-macos", "heyhubs", "cf-docker", "cf-linux"]);
    assert_eq!(set.host_labels().collect::<Vec<_>>(), ["cf-linux", "cf-macos"]);
    assert_eq!(LabelSet::new(["cf-8g", "self-hosted"]).host_labels().count(), 0);
}

#[test]
fn a_variable_read_through_from_json_may_hold_a_list_of_labels() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  heavy:\n    runs-on: ${{ fromJSON(vars.CI_RUNNER_HEAVY || '\"ubuntu-latest\"') }}\n",
    )]);
    let with = |value: &str| keys(&self_hosted_with(&wf, &vars(&[("CI_RUNNER_HEAVY", value)])));
    assert_eq!(
        keys(&self_hosted_with(&wf, &vars(&[]))),
        [POOL_LABEL],
        "no variable: the pool's label"
    );
    assert_eq!(with(r#"["cf-8g","cf-macos"]"#), ["cf-8g,cf-macos"]);
    assert_eq!(
        with(r#" [ "CF-8G" , "cf-macos", "cf-8g" ] "#),
        ["cf-8g,cf-macos"],
        "spaced, cased, repeated"
    );
    assert_eq!(with(r#"["self-hosted","cf-macos"]"#), ["cf-macos,self-hosted"]);
    assert_eq!(with(r#"["cf-6g"]"#), ["cf-6g"], "a list of one");
    assert_eq!(
        with(r#""cf-8g""#),
        ["cf-8g"],
        "a JSON string is one label, as before"
    );
    assert_eq!(with("cf-8g"), ["cf-8g"]);
    // A list must name a label of these Actions runners besides the host's, else it is the pool's label, as any
    // value that names none.
    for value in [
        r#"["cf-macos"]"#,
        r#"["ubuntu-latest","cf-8g-ish"]"#,
        "[]",
        "[1,2]",
        r#"["cf-8g",2]"#,
        r#"["cf-8g""#,
        r#"{"labels":["cf-8g"]}"#,
        r#"[["cf-8g"]]"#,
        "ubuntu-latest",
    ] {
        assert_eq!(with(value), [POOL_LABEL], "{value:?}");
    }
    // A list that also names another platform asks for that platform: these Actions runners leave it.
    assert_eq!(with(r#"["cf-8g","macos"]"#), Vec::<String>::new());
    assert_eq!(with(r#"["cf-8g",""]"#), ["cf-8g"], "an empty label is dropped");
}

#[test]
fn only_from_json_turns_a_list_into_labels_as_github_does() {
    // GitHub takes a variable's text as one label unless fromJSON parses it: such a job waits for a runner with a
    // label that looks like a list, which none has; it stands for the pool's label, as before.
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  a:\n    runs-on: ${{ vars.CI_RUNNER || 'ubuntu-latest' }}\n  b:\n    runs-on: ${{ toJSON(vars.CI_RUNNER) }}\n",
    )]);
    assert_eq!(
        keys(&self_hosted_with(
            &wf,
            &vars(&[("CI_RUNNER", r#"["cf-8g","cf-macos"]"#)])
        )),
        [POOL_LABEL]
    );
    // fromJSON inside toJSON is a string again.
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  a:\n    runs-on: ${{ toJSON(fromJSON(vars.CI_RUNNER)) }}\n",
    )]);
    assert_eq!(
        keys(&self_hosted_with(
            &wf,
            &vars(&[("CI_RUNNER", r#"["cf-8g","cf-macos"]"#)])
        )),
        [POOL_LABEL]
    );
}

#[test]
fn a_list_from_a_variable_joins_the_labels_beside_it_and_a_reusable_workflows_input() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  list:\n    runs-on: [\"${{ fromJSON(vars.CI_RUNNER_HEAVY) }}\", heyhubs]\n  call:\n    uses: o/r/.github/workflows/x.yml@main\n    with:\n      runs-on: ${{ fromJSON(vars.CI_RUNNER_HEAVY || '\"ubuntu-latest\"') }}\n",
    )]);
    assert_eq!(
        keys(&self_hosted_with(
            &wf,
            &vars(&[("CI_RUNNER_HEAVY", r#"["cf-8g","cf-macos"]"#)])
        )),
        ["cf-8g,cf-macos", "cf-8g,cf-macos,heyhubs"]
    );
}
