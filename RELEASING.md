# Releasing

## What a release is

A release is a publish to [crates.io](https://crates.io). No git tag is created
and no GitHub Release is made: nitinol is a library, so there is no binary to
distribute anywhere else.

Releases are cut by hand with [cargo-release](https://github.com/crate-ci/cargo-release),
configured by `release.toml` at the workspace root and by the
`[package.metadata.release]` table of the umbrella crate `nitinol`. It bumps the
version, raises the requirement every dependent crate states on it, rewrites
`CHANGELOG.md`, commits, pushes to `master`, and publishes the crates in
dependency order.

## Branches

- `dev` is where work lands. Every change is merged there first.
- `master` holds what has been released. It only ever moves by fast-forwarding
  to `dev` and by the release commit itself, so it always builds and passes CI.
  `release.toml` refuses to run from any other branch.

## Versioning

`0.6.0` is a provisional starting point: every crate was set to the same number
so that the first release begins from one place. The number is written in each
crate's own `[package]`, not shared through `[workspace.package]`, so raising
one crate never moves another. From then on, each crate is versioned on its own
under [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While a crate is at `0.x`:

- a change that breaks its public API raises the **minor** version;
- an addition or a fix raises the **patch** version.

A crate is released when it has changed itself, or when it starts using a new
API of a crate it depends on.

### The umbrella crate

`nitinol` re-exports its members, so whenever any member is published,
`nitinol` is published with it. That way `nitinol = "x.y.z"` always names a
consistent set of crates.

`CHANGELOG.md` follows that version: each version heading is the umbrella's
version and the release date, and each entry starts with the name of the crate
it concerns, as the entries do today — for example `` - (`nitinol-persistence`): … ``.

## Publish order

1. `nitinol-contract`
2. `nitinol-persistence`
3. `nitinol-runtime`
4. `nitinol-macros`
5. `nitinol-conformance`
6. `nitinol-store-conformance`
7. `nitinol-eventsource`
8. `nitinol-saga`
9. `nitinol-sqlx`
10. `nitinol`

cargo-release works this order out from the dependency graph by itself, and
after publishing each crate it waits until the crate appears in the index
before publishing the next.

## Steps

Install cargo-release:

```sh
cargo install cargo-release
```

Every `cargo release` command below is a dry run unless `--execute` is given.

A release starts by bringing `master` up to `dev`, and ends by bringing the
release commit back to `dev`:

```sh
git checkout master
git merge --ff-only dev
cargo release … --execute     # one of the commands below
git checkout dev
git merge --ff-only master
```

If `dev` has moved on while the release was being cut, the last step is an
ordinary merge instead of a fast-forward.

The date written into `CHANGELOG.md` is the UTC date of the release.

The first release publishes every crate at the version already in the
manifests, so no level is given:

```sh
cargo release --workspace --execute
```

The first release has ten new crates, so the limit on new packages is set to
10 in `release.toml`.

After that, release the crates that changed together with the umbrella crate:

```sh
cargo release <patch|minor> -p <crate> -p nitinol --execute
```

## Checking before a release

`cargo publish --dry-run` does not succeed for a crate whose internal
dependencies are not yet on crates.io. Packaging a single crate with `-p <crate>`
fails for the same reason, because its internal dependencies cannot be resolved
from the index, and `cargo package --workspace` fails because it also takes in
the examples, which are not published and whose internal dependencies carry no
version. So name all ten publishable crates in one call, which resolves the
internal dependencies among them, and let cargo-release rehearse the release:

```sh
cargo package --no-verify \
  -p nitinol-contract -p nitinol-persistence -p nitinol-runtime \
  -p nitinol-macros -p nitinol-conformance -p nitinol-store-conformance \
  -p nitinol-eventsource -p nitinol-saga -p nitinol-sqlx -p nitinol
cargo release --workspace
```
