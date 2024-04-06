use std::{
    fs::{self, File},
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use anyhow::Result;
use semver::Version;

use crate::package::Package;

pub struct RegistryCrate {
    crate_file: PathBuf,
}

impl RegistryCrate {
    pub fn obtain(
        http_client: &reqwest::blocking::Client,
        cache_dir: &Path,
        name: &str,
        version: &Version,
    ) -> Result<Self> {
        let crate_dir = cache_dir.join(name);
        if !crate_dir.try_exists()? {
            fs::create_dir(&crate_dir)?;
        }

        let crate_path = crate_dir.join(format!("{version}.tar.gz"));
        if !crate_path.try_exists()? {
            let mut resp = http_client
                .get(format!(
                    "https://static.crates.io/crates/{name}/{name}-{version}.crate",
                ))
                .send()?
                .error_for_status()?;

            let mut tmp_crate_path = crate_path.clone();
            tmp_crate_path.as_mut_os_string().push(".tmp");

            let mut tmp_crate_file = File::create(&tmp_crate_path)?;
            io::copy(&mut resp, &mut tmp_crate_file)?;
            tmp_crate_file.flush()?;
            drop(tmp_crate_file);

            fs::rename(tmp_crate_path, &crate_path)?;
        }

        Ok(Self {
            crate_file: crate_path,
        })
    }

    pub fn package(&self) -> Package {
        Package::new(self.crate_file.clone())
    }
}
