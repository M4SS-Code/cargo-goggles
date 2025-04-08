use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufReader, Read, Seek},
    path::{Path, PathBuf},
};

use flate2::read::GzDecoder;
use sha2::{Digest as _, Sha512};
use tar::Archive;

use crate::io::AsciiWhitespaceSkippingReader;

#[derive(Debug)]
pub struct Package(PathBuf);

#[derive(Debug)]
pub struct PackageContents(BTreeMap<PathBuf, [u8; 64]>);

#[derive(Debug)]
pub enum PackageComparison {
    Equal(#[allow(dead_code)] PathBuf),
    Different(PathBuf),
    OnlyLeft(PathBuf),
    OnlyRight(PathBuf),
}

impl Package {
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    pub fn raw_reader(&self) -> io::Result<impl Read + Seek> {
        File::open(&self.0)
    }

    pub fn decompressed_reader(&self) -> io::Result<impl Read> {
        self.raw_reader().map(GzDecoder::new)
    }

    pub fn archive_reader(&self) -> io::Result<Archive<impl Read>> {
        self.decompressed_reader().map(Archive::new)
    }

    pub fn contents(&self) -> io::Result<PackageContents> {
        let mut hashes = BTreeMap::new();

        let mut archive = self.archive_reader()?;
        for file in archive.entries()? {
            let file = file?;
            let path = file.path()?.into_owned();

            let mut reader = AsciiWhitespaceSkippingReader::new(BufReader::new(file));

            let mut sha512 = Sha512::new();
            io::copy(&mut reader, &mut sha512)?;
            hashes.insert(path, sha512.finalize().into());
        }

        Ok(PackageContents(hashes))
    }
}

impl PackageContents {
    pub fn compare<'a>(
        left: &'a PackageContents,
        right: &'a PackageContents,
    ) -> impl Iterator<Item = PackageComparison> + 'a {
        let a = left
            .0
            .iter()
            .filter(|(path, _)| !is_path_ignored(path))
            .map(|(path, left_hash)| match right.0.get(path) {
                Some(right_hash) if left_hash == right_hash => {
                    PackageComparison::Equal(path.to_owned())
                }
                Some(_) => PackageComparison::Different(path.to_owned()),
                None => PackageComparison::OnlyLeft(path.to_owned()),
            });
        let b = right
            .0
            .iter()
            .filter(|(path, _)| !is_path_ignored(path))
            .filter_map(|(path, _)| {
                if left.0.contains_key(path) {
                    None
                } else {
                    Some(PackageComparison::OnlyRight(path.to_owned()))
                }
            });

        a.chain(b)
    }
}

fn is_path_ignored(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        [".cargo_vcs_info.json", "Cargo.toml"]
            .into_iter()
            .any(|n| n == name)
    })
}
