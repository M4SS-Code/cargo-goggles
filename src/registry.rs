use std::{
    fs::{self, File},
    io::{self, Read, Write as _},
    path::{Path, PathBuf},
};

use anyhow::Result;
use flate2::read::GzDecoder;
use semver::Version;
use tar::Archive;

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

    pub fn raw_crate_file(&self) -> io::Result<impl Read> {
        File::open(&self.crate_file)
    }

    pub fn decompressed_crate_file(&self) -> io::Result<impl Read> {
        self.raw_crate_file().map(GzDecoder::new)
    }

    pub fn crate_contents(&self) -> io::Result<Archive<impl Read>> {
        self.decompressed_crate_file().map(Archive::new)
    }
}
