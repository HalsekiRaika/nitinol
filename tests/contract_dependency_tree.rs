//! Acceptance scenario "a contract layer that does not depend on tokio": the
//! normal dependency tree of
//! `nitinol = { features = ["contract"] }` must not contain `tokio` or
//! `nitinol-runtime`.
//!
//! `-e normal` is mandatory: `cargo tree` defaults to `normal,build,dev`, and
//! both the umbrella and `nitinol-persistence` carry `tokio` as a
//! dev-dependency, which is outside this contract.
//!
//! The check extracts one package name per tree line instead of searching the
//! raw output for a substring: the printed manifest paths are part of that
//! output, so a substring search reports a package that is not in the tree.
//!
//! The conformance kit is checked the same way and for a stronger reason: a kit
//! that claims to verify *any* interpreter cannot itself reach for one. Depending
//! on `nitinol-eventsource` — or on the runtime beneath it — would let the kit
//! measure a particular interpreter's machinery rather than the laws every
//! interpreter owes, and would put it out of reach of anyone writing their own.
//!
//! The contract crate itself is checked on its own tree as well, for the
//! reason the identity contract lives in it: the domain promises the machinery
//! its event identities there, so the promise cannot depend on the machinery
//! that receives it. `nitinol-persistence` depends on `nitinol-contract`, never
//! the other way round, and the only package the contract reaches is `bytes`,
//! the currency its codec signature is stated in. The umbrella's `contract`
//! feature still carries `nitinol-persistence`, because the derive places its
//! entry in the registry that crate owns — which is why the check above cannot
//! stand in for this one.

use std::process::Command;

/// The two packages the acceptance criteria name as absent from the tree.
const FORBIDDEN_PACKAGES: [&str; 2] = ["tokio", "nitinol-runtime"];

/// Presence anchors: without them a tree that resolved the wrong package or an
/// empty feature set would pass the negative check vacuously.
const REQUIRED_PACKAGES: [&str; 2] = ["nitinol-contract", "nitinol-persistence"];

/// What the conformance kit must not be able to reach: no async runtime, and no
/// interpreter of its own.
const KIT_FORBIDDEN_PACKAGES: [&str; 3] = ["tokio", "nitinol-runtime", "nitinol-eventsource"];

/// Presence anchors for the kit's tree, for the same reason as above.
const KIT_REQUIRED_PACKAGES: [&str; 2] = ["nitinol-contract", "nitinol-persistence"];

/// What the contract crate must not reach: the machinery its identities are
/// promised to, the runtime that drives it, and the packages that machinery is
/// built from (link-time collection and the store's timestamps).
const CONTRACT_CRATE_FORBIDDEN_PACKAGES: [&str; 5] = [
    "nitinol-persistence",
    "tokio",
    "nitinol-runtime",
    "linkme",
    "jiff",
];

/// Presence anchors for the contract crate's tree, for the same reason as
/// above: the crate itself, and the one package it depends on.
const CONTRACT_CRATE_REQUIRED_PACKAGES: [&str; 2] = ["nitinol-contract", "bytes"];

/// The contract crate's direct dependencies, exactly.
const CONTRACT_CRATE_DIRECT_DEPENDENCIES: [&str; 1] = ["bytes"];

/// One package name per rendered tree line, with the tree-drawing prefix and
/// the trailing `v<version> (<path>) (*)` decoration removed.
fn package_names(tree: &str) -> Vec<&str> {
    tree.lines()
        .filter_map(|line| {
            line.trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, '│' | '├' | '└' | '─' | '|' | '+' | '\\' | '-')
            })
            .split_whitespace()
            .next()
        })
        .collect()
}

#[test]
fn contract_feature_tree_has_no_tokio_and_no_runtime() {
    let cargo = std::env::var("CARGO").expect("CARGO is set by cargo for test processes");

    let output = Command::new(cargo)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "tree",
            "-p",
            "nitinol",
            "-e",
            "normal",
            "--no-default-features",
            "--features",
            "contract",
            "--color",
            "never",
        ])
        .output()
        .expect("`cargo tree` must be spawnable");

    let stdout = String::from_utf8(output.stdout).expect("`cargo tree` writes UTF-8 to stdout");
    let stderr = String::from_utf8(output.stderr).expect("`cargo tree` writes UTF-8 to stderr");

    assert!(
        output.status.success(),
        "`cargo tree -p nitinol -e normal --no-default-features --features contract` \
         exited with {status}:\n{stderr}",
        status = output.status,
    );

    let packages = package_names(&stdout);

    for required in REQUIRED_PACKAGES {
        assert!(
            packages.contains(&required),
            "`{required}` is missing from the `contract` dependency tree, \
             so the absence of {FORBIDDEN_PACKAGES:?} proves nothing:\n{stdout}",
        );
    }

    for forbidden in FORBIDDEN_PACKAGES {
        assert!(
            !packages.contains(&forbidden),
            "`{forbidden}` is reachable from `nitinol` with only the `contract` \
             feature enabled:\n{stdout}",
        );
    }
}

/// The conformance kit must be usable by an interpreter that does not exist
/// yet, so it may depend on the contract and the store abstractions and on
/// nothing that already interprets them.
#[test]
fn conformance_kit_tree_carries_no_interpreter_and_no_runtime() {
    let cargo = std::env::var("CARGO").expect("CARGO is set by cargo for test processes");

    let output = Command::new(cargo)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "tree",
            "-p",
            "nitinol-conformance",
            "-e",
            "normal",
            "--color",
            "never",
        ])
        .output()
        .expect("`cargo tree` must be spawnable");

    let stdout = String::from_utf8(output.stdout).expect("`cargo tree` writes UTF-8 to stdout");
    let stderr = String::from_utf8(output.stderr).expect("`cargo tree` writes UTF-8 to stderr");

    assert!(
        output.status.success(),
        "`cargo tree -p nitinol-conformance -e normal` exited with {status}:\n{stderr}",
        status = output.status,
    );

    let packages = package_names(&stdout);

    for required in KIT_REQUIRED_PACKAGES {
        assert!(
            packages.contains(&required),
            "`{required}` is missing from the conformance kit's dependency tree, \
             so the absence of {KIT_FORBIDDEN_PACKAGES:?} proves nothing:\n{stdout}",
        );
    }

    for forbidden in KIT_FORBIDDEN_PACKAGES {
        assert!(
            !packages.contains(&forbidden),
            "`{forbidden}` is reachable from `nitinol-conformance`, so the kit is tied to one \
             interpreter's machinery instead of the laws every interpreter owes:\n{stdout}",
        );
    }
}

/// Runs `cargo tree` from the workspace root with `args`, and returns its
/// stdout after asserting it succeeded.
fn cargo_tree(args: &[&str]) -> String {
    let cargo = std::env::var("CARGO").expect("CARGO is set by cargo for test processes");

    let output = Command::new(cargo)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .arg("tree")
        .args(args)
        .output()
        .expect("`cargo tree` must be spawnable");

    let stdout = String::from_utf8(output.stdout).expect("`cargo tree` writes UTF-8 to stdout");
    let stderr = String::from_utf8(output.stderr).expect("`cargo tree` writes UTF-8 to stderr");

    assert!(
        output.status.success(),
        "`cargo tree {args}` exited with {status}:\n{stderr}",
        args = args.join(" "),
        status = output.status,
    );

    stdout
}

/// Given the contract crate's normal dependency tree, When its packages are
/// read, Then nothing of the machinery is in it — not `nitinol-persistence`,
/// not a runtime, not `linkme` or `jiff`.
#[test]
fn contract_crate_tree_reaches_none_of_the_machinery() {
    let stdout = cargo_tree(&["-p", "nitinol-contract", "-e", "normal", "--color", "never"]);
    let packages = package_names(&stdout);

    for required in CONTRACT_CRATE_REQUIRED_PACKAGES {
        assert!(
            packages.contains(&required),
            "`{required}` is missing from the contract crate's dependency tree, \
             so the absence of {CONTRACT_CRATE_FORBIDDEN_PACKAGES:?} proves nothing:\n{stdout}",
        );
    }

    for forbidden in CONTRACT_CRATE_FORBIDDEN_PACKAGES {
        assert!(
            !packages.contains(&forbidden),
            "`{forbidden}` is reachable from `nitinol-contract`, so the identity a domain \
             promises depends on the machinery it is promised to:\n{stdout}",
        );
    }
}

/// Given the contract crate's direct normal dependencies, When they are read,
/// Then they are `bytes` and nothing else — a derive helper or an error crate
/// added for convenience would be a dependency the contract does not need.
#[test]
fn contract_crate_depends_directly_on_bytes_alone() {
    let stdout = cargo_tree(&[
        "-p",
        "nitinol-contract",
        "-e",
        "normal",
        "--depth",
        "1",
        "--color",
        "never",
    ]);
    let mut direct: Vec<&str> = package_names(&stdout)
        .into_iter()
        .filter(|package| *package != "nitinol-contract")
        .collect();
    direct.sort_unstable();
    direct.dedup();

    assert_eq!(
        direct, CONTRACT_CRATE_DIRECT_DEPENDENCIES,
        "`nitinol-contract` depends directly on exactly `bytes`:\n{stdout}",
    );
}
