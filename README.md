# cargo-goggles

![Crates.io Version](https://img.shields.io/crates/v/cargo-goggles)
![Crates.io License](https://img.shields.io/crates/l/cargo-goggles)
[![CI](https://github.com/M4SS-Code/cargo-goggles/workflows/CI/badge.svg)](https://github.com/M4SS-Code/cargo-goggles/actions)
[![dependency status](https://deps.rs/crate/cargo-goggles/0.0.1/status.svg)](https://deps.rs/crate/cargo-goggles/0.0.1)

Verify that registry crates in your Cargo.lock are reproducible from the git repository.

This cargo subcommand analyzes the following properties for crates in your Cargo.lock:

1. `Cargo.toml` contains a `repository` field pointing at a valid git repository
2. For each of the releases you are using, a valid git tag is present on the release commit
3. The tagged commit matches the value in `.cargo_vcs_info.json`, if present
4. The contents of the crates.io release are reproducible from the files inside the repo

## How to use it

```shell
# Install
cargo install --locked cargo-goggles

# Run it inside your project (must already contain a Cargo.lock file)
cargo goggles
```

## Roadmap

* Cleanup most of the code
* Make it into a proper library and CLI
* Support registries other than crates.io
* Fix some flaws
* Make it pull previously cloned repositories when changes are available
* Stop relying on the `git` CLI
* Make it faster
* Make it easy to see differences between the contents of the git repository and the registry

## License

Licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
