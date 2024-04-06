use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, BufRead, BufReader, Read},
    str,
};

use anyhow::{ensure, Context, Result};
use cargo_lock::{package::SourceKind, Checksum, Lockfile};
use cargo_toml::Manifest;
use serde::Deserialize;
use sha2::{Digest as _, Sha256, Sha512};
use url::Url;

use self::git::GitRepository;
use self::registry::RegistryCrate;

mod git;
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
    let temp_dir = env::temp_dir().join("cargo-crates-check");
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

        //
        // Verify the package checksum
        //

        match package.checksum {
            Some(Checksum::Sha256(expected_sha256_hash)) => {
                let mut sha256 = Sha256::new();
                io::copy(&mut registry_crate.raw_crate_file()?, &mut sha256)?;
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

        let mut tar = registry_crate.crate_contents()?;
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
                println!("Found NO tag match with package {}", package.name);

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

        let mut our_tar_gz = match git_repository_checkout.crate_package(
            &default_toolchain,
            package.name.as_str(),
            &package.version,
        ) {
            Ok(our_tar_gz) => our_tar_gz,
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

        let mut crates_io_tar = registry_crate.crate_contents()?;
        let mut crates_io_hashes = BTreeMap::new();
        for file in crates_io_tar.entries()? {
            let file = file?;
            let path = file.path()?.into_owned();
            if path.ends_with(".cargo_vcs_info.json") {
                continue;
            }
            // TODO: remove this
            if path.ends_with("Cargo.toml") || path.ends_with("Cargo.toml.orig") {
                continue;
            }

            let mut reader = AsciiWhitespaceSkippingReader(BufReader::new(file));

            let mut sha512 = Sha512::new();
            io::copy(&mut reader, &mut sha512)?;
            crates_io_hashes.insert(path, sha512.finalize());
        }

        let mut our_hashes = BTreeMap::new();
        for file in our_tar_gz.entries()? {
            let file = file?;
            let path = file.path()?.into_owned();
            if path.ends_with(".cargo_vcs_info.json") {
                continue;
            }
            // TODO: remove this
            if path.ends_with("Cargo.toml") || path.ends_with("Cargo.toml.orig") {
                continue;
            }

            let mut reader = AsciiWhitespaceSkippingReader(BufReader::new(file));

            let mut sha512 = Sha512::new();
            io::copy(&mut reader, &mut sha512)?;
            our_hashes.insert(path, sha512.finalize());
        }

        //
        // Compare hashes
        //

        for (our_filename, our_sha512_hash) in &our_hashes {
            match crates_io_hashes.get(our_filename) {
                Some(crates_io_sha512) if our_sha512_hash == crates_io_sha512 => {}
                Some(_) => {
                    println!(
                        "Package {} has mismatching file hashes for {}",
                        package.name,
                        our_filename.display()
                    );
                }
                None => {
                    println!(
                        "Package {} has file {} in our release but not in crates.io tarball",
                        package.name,
                        our_filename.display()
                    );
                }
            }
        }

        for crates_io_filename in crates_io_hashes.keys() {
            if !our_hashes.contains_key(crates_io_filename) {
                println!(
                    "Package {} has file {} in crates.io release but not ours",
                    package.name,
                    crates_io_filename.display()
                );
            }
        }
    }

    Ok(())
}

struct AsciiWhitespaceSkippingReader<R>(R);

impl<R> Read for AsciiWhitespaceSkippingReader<R>
where
    R: BufRead,
{
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let mut written = 0;

        loop {
            if buf.is_empty() {
                break;
            }

            let mut read_buf = self.0.fill_buf()?;
            if read_buf.is_empty() {
                break;
            }

            let mut read = 0;
            while !read_buf.is_empty() && !buf.is_empty() {
                read += 1;
                let b = read_buf[0];
                read_buf = &read_buf[1..];
                if b.is_ascii_whitespace() {
                    continue;
                }

                buf[0] = b;
                buf = &mut buf[1..];
                written += 1;
            }

            self.0.consume(read);
        }

        Ok(written)
    }
}
