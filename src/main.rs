use std::{
    env, fs,
    io::{self as std_io, Read},
    str,
};

use anyhow::{ensure, Context, Result};
use cargo_lock::{package::SourceKind, Checksum, Lockfile};
use cargo_toml::Manifest;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::package::{PackageComparison, PackageContents};

use self::git::GitRepository;
use self::registry::RegistryCrate;

mod git;
mod io;
mod package;
mod registry;
mod rustup;

const USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (+",
    env!("CARGO_PKG_REPOSITORY"),
    ")"
);

#[derive(Debug, Deserialize)]
struct CargoVcsInfo {
    git: CargoGitVcsInfo,
    // path_in_vcs: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CargoGitVcsInfo {
    sha1: String,
}

fn main() -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()?;

    let default_toolchain = self::rustup::default_toolchain();

    let crates_io_index = "https://github.com/rust-lang/crates.io-index".parse::<Url>()?;

    let current_dir = env::current_dir()?;
    let temp_dir = env::temp_dir().join(env!("CARGO_PKG_NAME"));
    let crates_dir = temp_dir.join("crates");
    let repos_dir = temp_dir.join("repositories");
    fs::create_dir_all(&crates_dir)?;
    fs::create_dir_all(&repos_dir)?;

    let lock = current_dir.join("Cargo.lock");
    ensure!(
        lock.try_exists()?,
        "Cargo.lock not found in current working directory"
    );

    let lock = Lockfile::load(lock).context("decode Cargo.lock")?;

    for package in lock.packages {
        //
        // Check that it's the official crates.io registry
        //

        let Some(source) = package.source else {
            println!("package {} doesn't have a source", package.name);
            continue;
        };

        if source.kind() != &SourceKind::Registry {
            continue;
        }
        if source.url() != &crates_io_index {
            println!(
                "package {} isn't part of the official crates.io registry",
                package.name
            );
            continue;
        };

        //
        // Download the package
        //

        let registry_crate = match RegistryCrate::obtain(
            &client,
            &crates_dir,
            package.name.as_str(),
            &package.version,
        ) {
            Ok(registry_crate) => registry_crate,
            Err(err) => {
                println!(
                    "Couldn't obtain package {} v{} err={:?}",
                    package.name, package.version, err
                );
                continue;
            }
        };
        let registry_crate_package = registry_crate.package();

        //
        // Verify the package checksum
        //

        match package.checksum {
            Some(Checksum::Sha256(expected_sha256_hash)) => {
                let mut sha256 = Sha256::new();
                std_io::copy(&mut registry_crate_package.raw_reader()?, &mut sha256)?;
                let sha256 = sha256.finalize();

                ensure!(
                    <[u8; 32]>::from(sha256) == expected_sha256_hash,
                    "package {} digest doesn't match",
                    package.name
                );
            }
            None => {
                println!("package {} doesn't have a checksum", package.name);
            }
        }

        //
        // Read `.cargo_vcs_info.json` and `Cargo.toml`
        //

        let mut cargo_vcs_info = None;
        let mut cargo_toml = None;

        let mut tar = registry_crate_package.archive_reader()?;
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry
                .path()?
                .to_str()
                .context("entry path isn't utf-8")?
                .to_owned();

            // TODO: verify that the `.tar` doesn't contain multiple directories

            if path.ends_with(".cargo_vcs_info.json") {
                ensure!(
                    cargo_vcs_info.is_none(),
                    "`.cargo_vcs_info.json` encountered multiple times"
                );

                cargo_vcs_info = serde_json::from_reader::<_, CargoVcsInfo>(&mut entry).ok();
            } else if path.ends_with("Cargo.toml") {
                if cargo_toml.is_some() {
                    println!("`Cargo.toml` encountered multiple times");
                }

                let mut manifest = String::new();
                entry.read_to_string(&mut manifest)?;
                cargo_toml = Some(Manifest::from_str(&manifest)?);
            }
        }

        let cargo_toml = cargo_toml.context("`Cargo.toml` not found")?;
        let Some(repository) = cargo_toml
            .package
            .context("Package metadata missing")?
            .repository
        else {
            println!(
                "Package {} is missing `repository` attribute in Cargo.toml",
                package.name
            );
            continue;
        };

        //
        // Clone repository
        //

        let repository_url = repository
            .get()?
            .parse::<Url>()
            .context("repository isn't a valid url")?;
        let mut git_repository = match GitRepository::obtain(&repos_dir, repository_url.clone()) {
            Ok(git_repository) => git_repository,
            Err(err) => {
                println!(
                    "Couldn't obtain git repository for {} v{} err={:?} url={}",
                    package.name, package.version, err, repository_url
                );
                continue;
            }
        };

        //
        // Get git tags
        //

        let tags = git_repository.tags().context("obtain git tags")?;

        //
        // Find a matching tag
        //

        let commit = match tags.find_tag_for_version(package.name.as_str(), package.version.clone())
        {
            Some(tag) => {
                let commit = tag.commit()?;

                if let Some(cargo_vcs_info) = &cargo_vcs_info {
                    if cargo_vcs_info.git.sha1 != commit {
                        println!(
                            "Commit between crates.io tarball and git tag doesn't match for {} v{}",
                            package.name, package.version
                        );
                    }
                }

                commit
            }
            None => {
                if tags.is_empty() {
                    println!("Package {} has no tags in git repository", package.name);
                } else {
                    println!("Found NO tag match with package {}", package.name);
                }

                match &cargo_vcs_info {
                    Some(cargo_vcs_info) => cargo_vcs_info.git.sha1.clone(),
                    None => {
                        println!("Couldn't determine commit for crate {}", package.name);
                        continue;
                    }
                }
            }
        };

        //
        // Checkout the commit in the repo
        //

        let git_repository_checkout = match git_repository.checkout(&commit) {
            Ok(git_repository_checkout) => git_repository_checkout,
            Err(err) => {
                println!(
                    "Couldn't checkout commit {} for package {} v{} err={:?}",
                    commit, package.name, package.version, err
                );
                continue;
            }
        };

        //
        // Create local package
        //

        let repository_package = match git_repository_checkout.crate_package(
            &default_toolchain,
            package.name.as_str(),
            &package.version,
        ) {
            Ok(repository_package) => repository_package,
            Err(err) => {
                println!(
                    "Couldn't package {} v{} err={:?}",
                    package.name, package.version, err
                );
                continue;
            }
        };

        //
        // Hash file contents
        //

        let repository_package_contents = repository_package
            .contents()
            .context("calculate repository package contents")?;
        let registry_package_contents = registry_crate_package
            .contents()
            .context("calculate registry crate package contents")?;

        //
        // Compare hashes
        //

        let comparison =
            PackageContents::compare(&repository_package_contents, &registry_package_contents);
        for outcome in comparison {
            match outcome {
                PackageComparison::Equal(_) => continue,
                PackageComparison::Different(path) => {
                    println!(
                        "Package {} has mismatching file hashes for {}",
                        package.name,
                        path.display()
                    );
                }
                PackageComparison::OnlyLeft(path) => {
                    println!(
                        "Package {} has file {} in our release but not in crates.io tarball",
                        package.name,
                        path.display()
                    );
                }
                PackageComparison::OnlyRight(path) => {
                    println!(
                        "Package {} has file {} in crates.io release but not ours",
                        package.name,
                        path.display()
                    );
                }
            }
        }
    }

    Ok(())
}
