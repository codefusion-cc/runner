//! The Go manager's `labels_test.go`, case by case, then what the Rust version adds: size labels, repository
//! variables, hostile input.

use std::collections::BTreeMap;

use cf_workflows::{
    DOCKER_LABEL, LabelSet, MAX_LABEL_BYTES, MAX_SET_LABELS, POOL_LABEL, SizeError, TRUSTED_LABEL,
    is_host_label, is_trusted_label, self_hosted, self_hosted_with, size_gb,
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
        "jobs:\n  a:\n    runs-on: ${{ vars.A && vars.B }}\n  b:\n    runs-on: ${{ format('{1}', vars.CI_RUNNER) }}\n",
    )]);
    assert_eq!(
        keys(&self_hosted_with(
            &wf,
            &vars(&[("CI_RUNNER", "cf-8g"), ("A", "x"), ("B", "cf-8g")])
        )),
        [POOL_LABEL]
    );
}

// added: parentheses and format, as a job that takes its labels from one of two variables writes them; before, such
// a job stood for the pool's label whatever the variables said.
#[test]
fn a_label_made_by_format_inside_parentheses_is_read() {
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  heavy:\n    runs-on: ${{ fromJSON(vars.CI_RUNNER_HEAVY || format('\"{0}\"', vars.CI_RUNNER || 'ubuntu-latest')) }}\n  plain:\n    runs-on: ${{ (vars.CI_RUNNER_DOCKER) || format('{0}', 'ubuntu-latest') }}\n",
    )]);
    let with = |list: &[(&str, &str)]| keys(&self_hosted_with(&wf, &vars(list)));
    assert_eq!(with(&[("CI_RUNNER", "cf-2g")]), ["cf-2g", POOL_LABEL]);
    assert_eq!(
        with(&[
            ("CI_RUNNER_HEAVY", r#"["cf-8g","cf-macos"]"#),
            ("CI_RUNNER", "cf-2g")
        ]),
        ["cf-8g,cf-macos", POOL_LABEL]
    );
    assert_eq!(
        with(&[("CI_RUNNER_DOCKER", "cf-docker")]),
        ["cf-docker", POOL_LABEL]
    );
    assert_eq!(
        with(&[]),
        [POOL_LABEL],
        "nothing set: the pool's label, as before"
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
    // fromJSON inside toJSON is a string again, and so is toJSON inside fromJSON: it parses back the string toJSON
    // made.
    let wf = files(&[(
        "ci.yml",
        "jobs:\n  a:\n    runs-on: ${{ toJSON(fromJSON(vars.CI_RUNNER)) }}\n  b:\n    runs-on: ${{ fromJSON(toJSON(vars.CI_RUNNER)) }}\n",
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

// added: the labels a job asks for, as GitHub's jobs API names them, under the same rules as a workflow's `runs-on`.
#[test]
fn a_jobs_labels_are_served_under_the_rules_of_a_workflow() {
    let served = |labels: &[&str]| LabelSet::served(labels.iter().copied()).map(|set| set.key());
    assert_eq!(
        served(&["self-hosted", "cf-2g"]).as_deref(),
        Some("cf-2g,self-hosted")
    );
    assert_eq!(
        served(&["CF-8G", " cf-macos "]).as_deref(),
        Some("cf-8g,cf-macos"),
        "cased and spaced"
    );
    assert_eq!(served(&["cf-docker"]).as_deref(), Some("cf-docker"));
    assert_eq!(
        served(&["self-hosted", "linux", "arm64", "heyhubs", "ubuntu-24.04", "a_b"]).as_deref(),
        Some("a_b,arm64,heyhubs,linux,self-hosted,ubuntu-24.04")
    );
    // None of ours, another platform, an expression GitHub left unread, an empty label: not served.
    for labels in [
        &["ubuntu-latest"][..],
        &["cf-macos"],
        &["cf-8g-ish"],
        &[],
        &["self-hosted", "macos"],
        &["self-hosted", "Windows"],
        &["cf-2g", "x64"],
        &["self-hosted", "${{ matrix.os }}"],
        &["self-hosted", ""],
    ] {
        assert_eq!(served(labels), None, "{labels:?}");
    }
}

// added: what a stranger's pull request can put in `runs-on` is bounded: the number of labels, their length and
// their characters (letters and digits of any script, as the Go manager kept them, and `-`, `_`, `.`). A comma would let one label read as two in a set's key.
#[test]
fn a_set_of_too_many_too_long_or_odd_labels_is_not_served() {
    let served = |labels: Vec<String>| LabelSet::served(labels).map(|set| set.key());
    let numbered = |n: usize| -> Vec<String> {
        std::iter::once("self-hosted".to_string())
            .chain((1..n).map(|i| format!("l{i}")))
            .collect()
    };
    assert!(
        served(numbered(MAX_SET_LABELS)).is_some(),
        "{MAX_SET_LABELS} labels"
    );
    assert_eq!(served(numbered(MAX_SET_LABELS + 1)), None);
    assert_eq!(served(numbered(100_000)), None);
    let long = |n: usize| vec!["self-hosted".to_string(), "x".repeat(n)];
    assert!(served(long(MAX_LABEL_BYTES)).is_some());
    assert_eq!(served(long(MAX_LABEL_BYTES + 1)), None);
    assert_eq!(served(long(1 << 20)), None);
    assert_eq!(
        served(vec!["cf-2g".into(), "Za\u{17c}\u{f3}\u{142}\u{107}".into()]).as_deref(),
        Some("cf-2g,za\u{17c}\u{f3}\u{142}\u{107}"),
        "letters of any script"
    );
    for odd in [
        "self-hosted,cf-docker",
        "a b",
        "\u{1F600}",
        "a\u{200b}b",
        "\u{202e}cf-2g",
        "tab\there",
        "nul\u{0}",
        "new\nline",
        "a/b",
    ] {
        assert_eq!(served(vec!["cf-2g".into(), odd.into()]), None, "{odd:?}");
    }
    // Repeats and case count once, before the bound: GitHub compares labels without regard to case.
    let repeated: Vec<String> = std::iter::repeat_n("SELF-HOSTED".to_string(), 1000).collect();
    assert_eq!(served(repeated).as_deref(), Some("self-hosted"));
}

// added: the bounds hold for a workflow on the default branch too, through the same rule.
#[test]
fn a_workflows_set_of_odd_labels_is_not_served_either() {
    let wf = files(&[(
        "ci.yml",
        &format!(
            "jobs:\n  a:\n    runs-on: [self-hosted, \"{}\"]\n  b:\n    runs-on: [self-hosted, \"a,b\"]\n  c:\n    runs-on: [cf-2g]\n",
            "x".repeat(MAX_LABEL_BYTES + 1)
        ),
    )]);
    assert_eq!(keys(&self_hosted(&wf)), ["cf-2g"]);
}

proptest! {
    // added: whatever a job's labels are, a set that is served is one a workflow could have asked for: a label of
    // ours, no other platform, within the bounds.
    #[test]
    fn whatever_a_job_names_a_served_set_keeps_the_rules(labels in prop::collection::vec("\\PC{0,20}|self-hosted|cf-[0-9]g|cf-docker|macos|cf-macos", 0..24)) {
        if let Some(set) = LabelSet::served(labels.iter()) {
            prop_assert!(set.labels().len() <= MAX_SET_LABELS);
            prop_assert!(set.labels().iter().any(|l| l == "self-hosted" || l == "cf-docker" || ["cf-2g", "cf-4g", "cf-6g", "cf-8g"].contains(&l.as_str())));
            for label in set.labels() {
                prop_assert!(label.len() <= MAX_LABEL_BYTES && !label.is_empty());
                prop_assert!(label.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c)), "{}", label);
                prop_assert!(!["macos", "osx", "windows", "x64", "x86", "arm"].contains(&label.as_str()));
            }
        }
    }
}

// The trusted class: a pull request may write `runs-on: [trusted, cf-8g]` or set it through a variable; a shared
// host must never take such a job, for a trusted host's job carries production tokens.
#[test]
fn no_shared_host_serves_a_set_with_the_trusted_label() {
    for labels in [
        &[TRUSTED_LABEL][..],
        &["self-hosted", "trusted"],
        &["cf-8g", "Trusted"],
        &["cf-docker", " TRUSTED "],
        &["cf-2g", "cf-linux", "trusted"],
    ] {
        assert_eq!(LabelSet::served(labels.iter().copied()), None, "{labels:?}");
    }
    let wf = files(&[(
        "deploy.yml",
        "jobs:\n  a:\n    runs-on: [self-hosted, trusted]\n  b:\n    runs-on: ${{ vars.CI_RUNNER_TRUSTED }}\n  c:\n    runs-on: [cf-4g]\n",
    )]);
    assert_eq!(
        keys(&self_hosted_with(&wf, &vars(&[("CI_RUNNER_TRUSTED", "trusted")]))),
        ["cf-4g"]
    );
}

// GitHub matches labels without regard to case, and a capital of a letter outside ASCII can be an ASCII one: the
// long s `ſ` is `S` in capitals. A shared Actions runner registered as `truſted` could then be handed main's deploy,
// which asks for `trusted`, though lowercasing alone never turns it into the trusted label.
#[test]
fn no_shared_host_serves_a_set_whose_label_is_trusted_in_another_case() {
    for labels in [&["self-hosted", "tru\u{17f}ted"][..], &["cf-8g", "TRU\u{17f}TED"]] {
        assert_eq!(LabelSet::served(labels.iter().copied()), None, "{labels:?}");
    }
    // Another word with a long s is no trusted label.
    assert_eq!(
        LabelSet::served(["cf-2g", "tru\u{17f}\u{17f}"]).map(|set| set.key()),
        Some("cf-2g,tru\u{17f}\u{17f}".to_string())
    );
    assert!(is_trusted_label("tru\u{17f}ted"));
    assert!(is_trusted_label(" Trusted "));
    assert!(!is_trusted_label("trusted-x"));
    assert!(!is_trusted_label("tru\u{17f}\u{17f}"));
    let wf = files(&[(
        "deploy.yml",
        "jobs:\n  a:\n    runs-on: [self-hosted, \"tru\u{17f}ted\"]\n  b:\n    runs-on: ${{ fromJSON(vars.CI_RUNNER_TRUSTED) }}\n  c:\n    runs-on: [cf-4g]\n",
    )]);
    assert_eq!(
        keys(&self_hosted_with(
            &wf,
            &vars(&[("CI_RUNNER_TRUSTED", "[\"cf-8g\", \"TRU\u{17f}TED\"]")])
        )),
        ["cf-4g"]
    );
}

// added: GitHub's own images name a platform with a version (`macos-latest`, `windows-2022`); a job asking for one
// beside a label of ours is left to other Actions runners, as one asking for the bare platform is. A label that only
// starts with the same letters is not a platform.
#[test]
fn a_platform_with_a_version_is_another_platform() {
    for platform in [
        "macos-latest",
        "macos-14",
        "macos-13-xlarge",
        "MacOS-Latest",
        "windows-latest",
        "windows-2022",
        "osx-arm64",
        "x64-large",
        "arm-v7",
    ] {
        assert_eq!(
            LabelSet::served(["self-hosted", "cf-2g", platform]),
            None,
            "{platform}"
        );
    }
    for ours in ["macosx", "armada", "x86ish", "windowsill", "cf-macos", "os-x"] {
        assert!(
            LabelSet::served(["self-hosted", "cf-2g", ours]).is_some(),
            "{ours}"
        );
    }
}
