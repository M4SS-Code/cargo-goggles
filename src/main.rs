use std::{
    collections::BTreeMap,
    env, fs,
    io::{self as std_io, Read},
    path::Path,
    str,
};

use anyhow::{ensure, Context, Result};
use cargo_lock::{package::SourceKind, Checksum, Lockfile};
use cargo_toml::Manifest;
use git::GitUrl;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
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

const CRATES_IO_INDEX: &str = "https://github.com/rust-lang/crates.io-index";

#[derive(Debug, Deserialize)]
struct CargoVcsInfo {
    git: CargoGitVcsInfo,
    // path_in_vcs: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CargoGitVcsInfo {
    sha1: String,
}

#[derive(Debug)]
struct ResolvedPackage {
    lock_info: cargo_lock::Package,
    registry_crate: RegistryCrate,
    repository_url: GitUrl,
    cargo_vcs_info: Option<CargoVcsInfo>,
}

fn main() -> Result<()> {
    let http_client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()?;

    let default_toolchain = self::rustup::default_toolchain();

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

    let resolved_packages = lock
        .packages
        .into_par_iter()
        .filter_map(|lock_info| {
            let name = lock_info.name.clone();
            let version = lock_info.version.clone();

            match resolve_package(&http_client, &crates_dir, lock_info) {
                Ok(resolved_package) => Some(resolved_package),
                Err(err) => {
                    println!("Couldn't resolve package {name} v{version} err={err:?}");
                    None
                }
            }
        })
        .collect::<Vec<_>>();

    let mut grouped_resolved_packages = BTreeMap::<_, Vec<_>>::new();
    for resolved_package in resolved_packages {
        grouped_resolved_packages
            .entry(resolved_package.repository_url.clone())
            .or_default()
            .push(resolved_package);
    }

    grouped_resolved_packages
        .into_par_iter()
        .for_each(|(repository_url, resolved_packages)| {
            let mut git_repository = match GitRepository::obtain(&repos_dir, repository_url) {
                Ok(git_repository) => git_repository,
                Err(err) => {
                    println!(
                        "Couldn't obtain git repository for {} v{} err={:?} url={}",
                        resolved_packages[0].lock_info.name,
                        resolved_packages[0].lock_info.version,
                        err,
                        resolved_packages[0].repository_url
                    );
                    return;
                }
            };

            for resolved_package in resolved_packages {
                if let Err(err) =
                    analyze_package(&default_toolchain, &resolved_package, &mut git_repository)
                {
                    println!(
                        "Couldn't analyze package for {} v{} err={:?} url={}",
                        resolved_package.lock_info.name,
                        resolved_package.lock_info.version,
                        err,
                        resolved_package.repository_url
                    );
                }
            }
        });

    Ok(())
}

fn resolve_package(
    http_client: &reqwest::blocking::Client,
    cache_dir: &Path,
    lock_info: cargo_lock::Package,
) -> Result<ResolvedPackage> {
    //
    // Check that it's the official crates.io registry
    //

    let source = lock_info
        .source
        .as_ref()
        .context("package doesn't have a `source`")?;
    ensure!(
        source.kind() == &SourceKind::Registry,
        "package source isn't Registry"
    );
    ensure!(
        source.url().as_str() == CRATES_IO_INDEX,
        "package is part of the official crates.io registry"
    );

    //
    // Download the package
    //

    let registry_crate = RegistryCrate::obtain(
        http_client,
        cache_dir,
        lock_info.name.as_str(),
        &lock_info.version,
    )
    .context("couldn't obtain package")?;
    let registry_crate_package = registry_crate.package();

    //
    // Verify the package checksum
    //

    match lock_info.checksum {
        Some(Checksum::Sha256(expected_sha256_hash)) => {
            let mut sha256 = Sha256::new();
            std_io::copy(&mut registry_crate_package.raw_reader()?, &mut sha256)?;
            let sha256 = sha256.finalize();

            ensure!(
                <[u8; 32]>::from(sha256) == expected_sha256_hash,
                "package {} digest doesn't match",
                lock_info.name
            );
        }
        None => {
            println!("package {} doesn't have a checksum", lock_info.name);
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

    let manifest = cargo_toml.context("`Cargo.toml` not found")?;
    let repository = manifest
        .package
        .as_ref()
        .context("Package metadata missing")?
        .repository
        .as_ref()
        .context("missing `repository` attribute in Cargo.toml")?;

    //
    // Clone repository
    //

    let repository_url = repository
        .get()?
        .parse::<Url>()
        .context("repository isn't a valid url")?
        .try_into()
        .context("repository url isn't valid")?;

    Ok(ResolvedPackage {
        lock_info,
        registry_crate,
        repository_url,
        cargo_vcs_info,
    })
}

fn analyze_package(
    default_toolchain: &str,
    resolved_package: &ResolvedPackage,
    git_repository: &mut GitRepository,
) -> Result<()> {
    let ResolvedPackage {
        lock_info,
        registry_crate,
        repository_url: _,
        cargo_vcs_info,
    } = resolved_package;

    let registry_crate_package = registry_crate.package();

    //
    // Get git tags
    //

    let tags = git_repository.tags().context("obtain git tags")?;

    //
    // Find a matching tag
    //

    let commit = match tags.find_tag_for_version(lock_info.name.as_str(), lock_info.version.clone())
    {
        Some(tag) => {
            let commit = tag.commit()?;

            if let Some(cargo_vcs_info) = &cargo_vcs_info {
                if cargo_vcs_info.git.sha1 != commit {
                    println!(
                        "Commit between crates.io tarball and git tag doesn't match for {} v{}",
                        lock_info.name, lock_info.version
                    );
                }
            }

            commit
        }
        None => {
            if tags.is_empty() {
                println!("Package {} has no tags in git repository", lock_info.name);
            } else {
                println!("Found NO tag match with package {}", lock_info.name);
            }

            cargo_vcs_info
                .as_ref()
                .context("couldn't determine commit matching registry release")?
                .git
                .sha1
                .clone()
        }
    };

    //
    // Checkout the commit in the repo
    //

    let git_repository_checkout = git_repository
        .checkout(&commit)
        .context("couldn't checkout commit")?;

    //
    // Create local package
    //

    let repository_package = git_repository_checkout
        .crate_package(
            default_toolchain,
            lock_info.name.as_str(),
            &lock_info.version,
        )
        .context("couldn't package")?;

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
                    lock_info.name,
                    path.display()
                );
            }
            PackageComparison::OnlyLeft(path) => {
                println!(
                    "Package {} has file {} in our release but not in crates.io tarball",
                    lock_info.name,
                    path.display()
                );
            }
            PackageComparison::OnlyRight(path) => {
                println!(
                    "Package {} has file {} in crates.io release but not ours",
                    lock_info.name,
                    path.display()
                );
            }
        }
    }

    Ok(())
}
