# Contributing

Thanks for your interest. Issues and pull requests are welcome.

## Before you open a pull request

```sh
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
cargo deny check
```

- The minimum supported Rust version is 1.89 (`rust-version` in `Cargo.toml`).
  CI checks it.
- Add a test for every bug fix. A regression test should fail without the fix.
- Keep `unsafe` out of the library. If a module truly needs it, isolate it
  and put a `// SAFETY:` comment on every block.
- Do not `unwrap`, `expect` or `panic!` on data that comes from the network.
- Keep dependencies to the necessary minimum.
- Update `CHANGELOG.md` under "Unreleased".
- `crates/thirtysftp/README.md` is a copy of the root `README.md` (a test checks
  it): after editing the root one, `cp README.md crates/thirtysftp/README.md`.

## Interoperability tests

Tests that need OpenSSH programs are `#[ignore]`d and run in CI on Linux. To
run them locally, see [docs/INTEROP.md](docs/INTEROP.md):

```sh
cargo test -p thirtysftp --test openssh -- --ignored
```

## Fuzzing

`fuzz/` is a separate package with cargo-fuzz targets (it needs a nightly
toolchain):

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run decode
cargo +nightly fuzz run frames
```

## Benchmarks

Benchmarks are in the `thirtysftp-bench` crate so that their dependencies stay
out of the library. See [docs/PERFORMANCE.md](docs/PERFORMANCE.md) for how to run
them. Measure before changing a default, and put the numbers in that file.

## License

By contributing you agree that your contribution is licensed under
Apache-2.0.

## Releasing

Maintainers publish a release by tagging the version that `Cargo.toml`
declares:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The `Release` workflow tests the crate, publishes it to crates.io and creates
the GitHub release. Update `CHANGELOG.md` and the version in `Cargo.toml`
before tagging. A published version cannot be replaced.
