use std::{
    collections::{BTreeMap, HashSet},
    env,
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Write},
    process::Command,
    str,
};

use anyhow::{ensure, Context as _, Result};
use cargo_lock::{package::SourceKind, Checksum, Lockfile};
use cargo_toml::Manifest;
use flate2::read::GzDecoder;
use semver::BuildMetadata;
use serde::Deserialize;
use sha2::{Digest as _, Sha256, Sha512};
use tar::Archive;
use url::Url;

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

    let default_toolchain = Command::new("rustup")
        .arg("default")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|stdout| {
            stdout
                .split_once(' ')
                .map(|(toolchain, _)| toolchain.to_owned())
        })
        .unwrap_or_else(|| "stable".to_owned());

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

        let crate_dir = crates_dir.join(package.name.as_str());
        if !crate_dir.try_exists()? {
            fs::create_dir(&crate_dir)?;
        }
        let crate_path = crate_dir.join(format!("{}.tar.gz", package.version));
        if !crate_path.try_exists()? {
            println!("Downloading {} v{}", package.name, package.version);

            let mut resp = client
                .get(format!(
                    "https://static.crates.io/crates/{}/{}-{}.crate",
                    package.name, package.name, package.version
                ))
                .send()?;
            if !resp.status().is_success() {
                println!(
                    "Couldn't download {} v{}, status: {}",
                    package.name,
                    package.version,
                    resp.status()
                );
                continue;
            }

            let mut tmp_crate_path = crate_path.clone();
            tmp_crate_path.as_mut_os_string().push(".tmp");

            let mut tmp_crate_file = File::create(&tmp_crate_path)?;
            io::copy(&mut resp, &mut tmp_crate_file)?;
            tmp_crate_file.flush()?;
            drop(tmp_crate_file);

            fs::rename(tmp_crate_path, &crate_path)?;
        }

        //
        // Verify the package checksum
        //

        match package.checksum {
            Some(Checksum::Sha256(expected_sha256_hash)) => {
                let mut sha256 = Sha256::new();
                let mut file = File::open(&crate_path)?;
                io::copy(&mut file, &mut sha256)?;
                drop(file);
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

        let mut tar = Archive::new(GzDecoder::new(File::open(&crate_path)?));
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

        let repository = repository
            .get()?
            .parse::<Url>()
            .context("repository isn't a valid url")?;
        ensure!(
            matches!(repository.scheme(), "http" | "https"),
            "Bad repository scheme"
        );
        let host = repository
            .host()
            .context("repository doesn't have a `host`")?
            .to_string();
        let repository = if host == "github.com" || host.starts_with("gitlab.") {
            let mut repository = repository;
            let mut path = repository.path().strip_prefix('/').unwrap().split('/');
            repository.set_path(&format!(
                "/{}/{}.git",
                path.next().context("repository is missing user/org")?,
                path.next()
                    .context("repository is missing repo name")?
                    .trim_end_matches(".git")
            ));
            repository
        } else {
            repository
        };

        let name = format!(
            "{}-{}",
            repository.host().unwrap(),
            repository.path().replace('/', "-")
        );
        let repo_dir = repos_dir.join(&name);
        if !repo_dir.try_exists()? {
            println!("Cloning {}", repository);
            let out = Command::new("git")
                .arg("clone")
                .arg("--filter=blob:none")
                .arg("--")
                .arg(repository.to_string())
                .arg(&repo_dir)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()?;
            if !out.status.success() {
                println!("Couldn't clone {} repo status={}", repository, out.status);
                continue;
            }
        }

        //
        // Get git tags
        //

        let tags = Command::new("git")
            .arg("tag")
            .current_dir(&repo_dir)
            .output()?;
        ensure!(
            tags.status.success(),
            "Couldn't list git tags {} repo status={}",
            repository,
            tags.status
        );
        let tags = str::from_utf8(&tags.stdout)
            .context("couldn't parse git tags")?
            .lines()
            .map(ToOwned::to_owned)
            .collect::<HashSet<_>>();

        //
        // Find a matching tag
        //

        let mut clean_version = package.version.clone();
        clean_version.build = BuildMetadata::EMPTY;

        let possible_tags = [
            // With package name prefix
            format!("{}-v{}", package.name, clean_version),
            format!("{}-{}", package.name, clean_version),
            format!("{}/v{}", package.name, clean_version),
            format!("{}v/{}", package.name, clean_version),
            format!("{}/{}", package.name, clean_version),
            // Just the version
            format!("v{clean_version}"),
            clean_version.to_string(),
            format!("v/{clean_version}"),
        ];
        let commit = match possible_tags
            .iter()
            .find(|&possible_tag| tags.contains(possible_tag))
        {
            Some(tag) => {
                let out = Command::new("git")
                    .arg("rev-list")
                    .arg("-n")
                    .arg("1")
                    .arg(tag)
                    .current_dir(&repo_dir)
                    .output()
                    .context("find out commit behind tag")?;
                ensure!(
                    out.status.success(),
                    "Couldn't determine commit behind tag {} repo status={}",
                    repository,
                    out.status
                );
                let commit = str::from_utf8(&out.stdout)
                    .context("git tag isn't utf-8")?
                    .lines()
                    .next()
                    .context("output is empty")?
                    .to_owned();

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

        let out = Command::new("git")
            .arg("checkout")
            .arg(commit)
            .current_dir(&repo_dir)
            .output()
            .context("checkout the commit")?;
        if !out.status.success() {
            println!(
                "Couldn't checkout the commit in {} repo status={}",
                repository, out.status
            );
            continue;
        }

        let out = Command::new("git")
            .arg("submodule")
            .arg("init")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&repo_dir)
            .output()
            .context("init submodules")?;
        if !out.status.success() {
            println!(
                "Couldn't init submodules in {} repo status={}",
                repository, out.status
            );
            continue;
        }

        let out = Command::new("git")
            .arg("submodule")
            .arg("sync")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&repo_dir)
            .output()
            .context("sync submodules")?;
        if !out.status.success() {
            println!(
                "Couldn't sync submodules in {} repo status={}",
                repository, out.status
            );
            continue;
        }

        let out = Command::new("git")
            .arg("submodule")
            .arg("update")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&repo_dir)
            .output()
            .context("update submodules")?;
        if !out.status.success() {
            println!(
                "Couldn't update submodules in {} repo status={}",
                repository, out.status
            );
            continue;
        }

        //
        // Create local package
        //

        let package_path = repo_dir
            .join("target")
            .join("package")
            .join(format!("{}-{}.crate", package.name, package.version));

        if !package_path.try_exists()? {
            println!("Packaging release {} v{}", package.name, package.version);

            let out = Command::new("cargo")
                .arg("package")
                .arg("--no-verify")
                .arg("--package")
                .arg(package.name.as_str())
                .current_dir(&repo_dir)
                .env("RUSTUP_TOOLCHAIN", &default_toolchain)
                .output()
                .context("cargo package")?;
            if !out.status.success() {
                println!(
                    "Couldn't assemble the package in {} repo status={}",
                    repository, out.status
                );
                continue;
            }

            if !package_path.try_exists()? {
                println!("Package still somehow doesn't exist {}", package.name);
                continue;
            }
        }

        //
        // Hash file contents
        //

        let mut crates_io_tar = Archive::new(GzDecoder::new(File::open(&crate_path)?));
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

        let mut our_tar_gz = Archive::new(GzDecoder::new(File::open(&package_path)?));
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
