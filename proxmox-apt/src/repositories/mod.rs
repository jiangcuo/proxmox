use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Error};

mod repository;
pub use repository::{
    APTRepository, APTRepositoryFileType, APTRepositoryOption, APTRepositoryPackageType,
};

mod file;
pub use file::{APTRepositoryFile, APTRepositoryFileError, APTRepositoryInfo};

mod release;
pub use release::{get_current_release_codename, DebianCodename};

mod standard;
pub use standard::{APTRepositoryHandle, APTStandardRepository};

const APT_SOURCES_LIST_FILENAME: &str = "/etc/apt/sources.list";
const APT_SOURCES_LIST_DIRECTORY: &str = "/etc/apt/sources.list.d/";

/// Calculates a common digest for successfully parsed repository files.
///
/// The digest is invariant with respect to file order.
///
/// Files without a digest are ignored.
fn common_digest(files: &[APTRepositoryFile]) -> [u8; 32] {
    let mut digests = BTreeMap::new();

    for file in files.iter() {
        digests.insert(file.path.clone(), &file.digest);
    }

    let mut common_raw = Vec::<u8>::with_capacity(digests.len() * 32);
    for digest in digests.values() {
        match digest {
            Some(digest) => common_raw.extend_from_slice(&digest[..]),
            None => (),
        }
    }

    openssl::sha::sha256(&common_raw[..])
}

/// Provides additional information about the repositories.
///
/// The kind of information can be:
/// `warnings` for bad suites.
/// `ignore-pre-upgrade-warning` when the next stable suite is configured.
/// `badge` for official URIs.
pub fn check_repositories(
    files: &[APTRepositoryFile],
    current_suite: DebianCodename,
) -> Vec<APTRepositoryInfo> {
    let mut infos = vec![];

    for file in files.iter() {
        infos.append(&mut file.check_suites(current_suite));
        infos.append(&mut file.check_uris());
    }

    infos
}

/// Get the repository associated to the handle and the path where it is usually configured.
pub fn get_standard_repository(
    handle: APTRepositoryHandle,
    product: &str,
    suite: DebianCodename,
) -> (APTRepository, String) {
    let repo = handle.to_repository(product, &suite.to_string());
    let path = handle.path(product);

    (repo, path)
}

/// Return handles for standard Proxmox repositories and their status, where
/// `None` means not configured, and `Some(bool)` indicates enabled or disabled.
pub fn standard_repositories(
    files: &[APTRepositoryFile],
    product: &str,
    suite: DebianCodename,
) -> Vec<APTStandardRepository> {
    let mut result = vec![
        APTStandardRepository::from(APTRepositoryHandle::Enterprise),
        APTStandardRepository::from(APTRepositoryHandle::NoSubscription),
        APTStandardRepository::from(APTRepositoryHandle::Test),
    ];

    if product == "pve" {
        result.append(&mut vec![
            APTStandardRepository::from(APTRepositoryHandle::CephQuincyEnterprise),
            APTStandardRepository::from(APTRepositoryHandle::CephQuincyNoSubscription),
            APTStandardRepository::from(APTRepositoryHandle::CephQuincyTest),
        ]);
        if suite == DebianCodename::Bookworm {
            result.append(&mut vec![
                APTStandardRepository::from(APTRepositoryHandle::CephReefEnterprise),
                APTStandardRepository::from(APTRepositoryHandle::CephReefNoSubscription),
                APTStandardRepository::from(APTRepositoryHandle::CephReefTest),
            ]);
        }
    }

    for file in files.iter() {
        for repo in file.repositories.iter() {
            for entry in result.iter_mut() {
                if entry.status == Some(true) {
                    continue;
                }

                if repo.is_referenced_repository(entry.handle, product, &suite.to_string()) {
                    entry.status = Some(repo.enabled);
                }
            }
        }
    }

    result
}

/// Type containing successfully parsed files, a list of errors for files that
/// could not be read and a common digest for the successfully parsed files.
pub type Repositories = (
    Vec<APTRepositoryFile>,
    Vec<APTRepositoryFileError>,
    [u8; 32],
);

/// Returns all APT repositories configured in `/etc/apt/sources.list` and
/// in `/etc/apt/sources.list.d` including disabled repositories.
///
/// The digest is guaranteed to be set for each successfully parsed file.
pub fn repositories() -> Result<Repositories, Error> {
    let to_result = |files: Vec<APTRepositoryFile>, errors: Vec<APTRepositoryFileError>| {
        let common_digest = common_digest(&files);

        (files, errors, common_digest)
    };

    let mut files = vec![];
    let mut errors = vec![];

    let sources_list_path = PathBuf::from(APT_SOURCES_LIST_FILENAME);

    let sources_list_d_path = PathBuf::from(APT_SOURCES_LIST_DIRECTORY);

    if sources_list_path.exists() {
        if sources_list_path.is_file() {
            match APTRepositoryFile::new(sources_list_path) {
                Ok(Some(mut file)) => match file.parse() {
                    Ok(()) => files.push(file),
                    Err(err) => errors.push(err),
                },
                _ => bail!("internal error with '{}'", APT_SOURCES_LIST_FILENAME),
            }
        } else {
            errors.push(APTRepositoryFileError {
                path: APT_SOURCES_LIST_FILENAME.to_string(),
                error: "not a regular file!".to_string(),
            });
        }
    }

    if !sources_list_d_path.exists() {
        return Ok(to_result(files, errors));
    }

    if !sources_list_d_path.is_dir() {
        errors.push(APTRepositoryFileError {
            path: APT_SOURCES_LIST_DIRECTORY.to_string(),
            error: "not a directory!".to_string(),
        });
        return Ok(to_result(files, errors));
    }

    for entry in std::fs::read_dir(sources_list_d_path)? {
        let path = entry?.path();

        match APTRepositoryFile::new(path) {
            Ok(Some(mut file)) => match file.parse() {
                Ok(()) => {
                    if file.digest.is_none() {
                        bail!("internal error - digest not set");
                    }
                    files.push(file);
                }
                Err(err) => errors.push(err),
            },
            Ok(None) => (),
            Err(err) => errors.push(err),
        }
    }

    Ok(to_result(files, errors))
}
