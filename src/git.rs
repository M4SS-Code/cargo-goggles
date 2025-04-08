use std::{
    cmp::Ordering,
    collections::BTreeSet,
    fmt::{self, Display},
    path::{Path, PathBuf},
    process::Command,
    str,
};

use anyhow::{ensure, Context as _, Result};
use semver::Version;
use url::Url;

use crate::package::Package;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitUrl(Url);

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitRepository {
    repo_dir: PathBuf,
}

#[derive(Debug)]
pub struct GitRepositoryCheckout<'a> {
    repository: &'a GitRepository,
}

#[derive(Debug)]
pub struct GitTags<'a>(BTreeSet<GitTag<'a>>);

#[derive(Debug)]
pub struct GitTag<'a> {
    repository: &'a GitRepository,
    tag: String,
}

impl GitRepository {
    pub fn obtain(dir: &Path, GitUrl(url): GitUrl) -> Result<Self> {
        let name = format!("{}-{}", url.host().unwrap(), url.path().replace('/', "-"));
        let repo_dir = dir.join(name);
        if !repo_dir.try_exists()? {
            let out = Command::new("git")
                .arg("clone")
                .arg("--filter=blob:none")
                .arg("--")
                .arg(url.to_string())
                .arg(&repo_dir)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()?;
            ensure!(out.status.success(), "`git clone` is successful");
        }

        Ok(Self { repo_dir })
    }

    pub fn tags(&self) -> Result<GitTags> {
        let out = Command::new("git")
            .arg("tag")
            .current_dir(&self.repo_dir)
            .output()?;
        ensure!(out.status.success(), "`git tag` is successful");

        let tags = str::from_utf8(&out.stdout)
            .context("couldn't parse git tags")?
            .lines()
            .map(|tag| GitTag {
                repository: self,
                tag: tag.to_owned(),
            })
            .collect::<BTreeSet<_>>();
        Ok(GitTags(tags))
    }

    pub fn checkout<'a>(&'a mut self, commit: &str) -> Result<GitRepositoryCheckout<'a>> {
        let out = Command::new("git")
            .arg("checkout")
            .arg(commit)
            .current_dir(&self.repo_dir)
            .output()
            .context("checkout the commit")?;
        ensure!(out.status.success(), "`git checkout` is successful");

        let out = Command::new("git")
            .arg("submodule")
            .arg("init")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.repo_dir)
            .output()
            .context("init submodules")?;
        ensure!(out.status.success(), "`git submodule init` is successful");

        let out = Command::new("git")
            .arg("submodule")
            .arg("sync")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.repo_dir)
            .output()
            .context("sync submodules")?;
        ensure!(out.status.success(), "`git submodule sync` is successful");

        let out = Command::new("git")
            .arg("submodule")
            .arg("update")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.repo_dir)
            .output()
            .context("update submodules")?;
        ensure!(out.status.success(), "`git submodule update` is successful");

        Ok(GitRepositoryCheckout { repository: self })
    }
}

impl GitRepositoryCheckout<'_> {
    pub fn crate_package(
        &self,
        default_toolchain: &str,
        name: &str,
        version: &Version,
    ) -> Result<Package> {
        let package_path = self
            .repository
            .repo_dir
            .join("target")
            .join("package")
            .join(format!("{name}-{version}.crate"));

        if !package_path.try_exists()? {
            let out = Command::new("cargo")
                .arg("publish")
                .arg("--dry-run")
                .arg("--no-verify")
                .arg("--package")
                .arg(name)
                .current_dir(&self.repository.repo_dir)
                .env("RUSTUP_TOOLCHAIN", default_toolchain)
                .output()
                .context("cargo package")?;
            ensure!(out.status.success(), "`cargo package` is successful");
            ensure!(
                package_path.try_exists()?,
                "`cargo package` generated a file"
            );
        }

        Ok(Package::new(package_path))
    }
}

impl<'a> GitTags<'a> {
    pub fn find_tag_for_version(&'a self, name: &str, version: Version) -> Option<&'a GitTag<'a>> {
        let mut clean_version = version;
        clean_version.build = semver::BuildMetadata::EMPTY;

        let possible_tags = [
            // With package name prefix
            format!("{name}-v{clean_version}"),
            format!("{name}-{clean_version}"),
            format!("{name}_v{clean_version}"),
            format!("{name}_{clean_version}"),
            format!("{name}/v{clean_version}"),
            format!("{name}v/{clean_version}"),
            format!("{name}/{clean_version}"),
            format!("{name}@v{clean_version}"),
            format!("{name}@{clean_version}"),
            // Just the version
            format!("v{clean_version}"),
            clean_version.to_string(),
            format!("v/{clean_version}"),
        ];
        possible_tags
            .iter()
            .find_map(|possible_tag| self.0.iter().find(|&tag| tag.tag == **possible_tag))
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl GitTag<'_> {
    pub fn commit(&self) -> Result<String> {
        let out = Command::new("git")
            .arg("rev-list")
            .arg("-n")
            .arg("1")
            .arg(&self.tag)
            .current_dir(&self.repository.repo_dir)
            .output()
            .context("find out commit behind tag")?;
        ensure!(out.status.success(), "`git rev-list` is successful");

        let commit = str::from_utf8(&out.stdout)
            .context("git tag isn't utf-8")?
            .lines()
            .next()
            .context("output is empty")?
            .to_owned();
        Ok(commit)
    }
}

impl PartialEq for GitTag<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.tag.eq(&other.tag)
    }
}

impl Eq for GitTag<'_> {}

impl PartialOrd for GitTag<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GitTag<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.tag.cmp(&other.tag)
    }
}

impl TryFrom<Url> for GitUrl {
    type Error = anyhow::Error;

    fn try_from(url: Url) -> Result<Self, Self::Error> {
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "Bad repository scheme"
        );
        let host = url
            .host()
            .context("repository doesn't have a `host`")?
            .to_string();

        Ok(Self(
            if host == "github.com" || host.starts_with("gitlab.") {
                let mut url = url;
                let mut path = url.path().strip_prefix('/').unwrap().split('/');
                url.set_path(&format!(
                    "/{}/{}.git",
                    path.next().context("repository is missing user/org")?,
                    path.next()
                        .context("repository is missing repo name")?
                        .trim_end_matches(".git")
                ));
                url
            } else {
                url
            },
        ))
    }
}

impl Display for GitUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}
