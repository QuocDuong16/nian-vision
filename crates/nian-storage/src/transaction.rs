//! Non-media recovery transaction evidence shared by recorder and retention.

use std::fs::Metadata;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::{RecordingFileKind, classify_recording_file};

/// Magic first line of the only trusted recovery tombstone format.
pub const RECOVERY_TOMBSTONE_MAGIC: &str = "NIAN-RECOVERY-TOMBSTONE v2";

/// Strictly parsed recovery transaction marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryTombstone {
    pub original: String,
    pub final_name: String,
    pub size_bytes: u64,
}

/// Typed result of inspecting one transaction path without following symlinks.
/// Only `Absent` proves the path does not exist.
#[derive(Debug)]
pub enum PathPresence {
    Present(Metadata),
    Absent,
    Uninspectable(std::io::Error),
}

/// Inspects a path while preserving `NotFound` versus all other I/O errors.
pub fn inspect_path_presence(path: &Path) -> PathPresence {
    inspect_path_presence_with(path, &|candidate| std::fs::symlink_metadata(candidate))
}

fn inspect_path_presence_with<F>(path: &Path, metadata: &F) -> PathPresence
where
    F: Fn(&Path) -> std::io::Result<Metadata>,
{
    match metadata(path) {
        Ok(metadata) => PathPresence::Present(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => PathPresence::Absent,
        Err(error) => PathPresence::Uninspectable(error),
    }
}

/// Serializes the v2 tombstone payload.
pub fn recovery_tombstone_payload(
    original_name: &str,
    final_name: &str,
    size_bytes: u64,
) -> String {
    format!(
        "{RECOVERY_TOMBSTONE_MAGIC}\noriginal: {original_name}\nfinal: {final_name}\nsize: {size_bytes}\n"
    )
}

/// Strict parser. Legacy, malformed, extra-field and non-UTF8 markers are untrusted.
pub fn parse_recovery_tombstone(bytes: &[u8]) -> Option<RecoveryTombstone> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()? != RECOVERY_TOMBSTONE_MAGIC {
        return None;
    }
    let original = lines.next()?.strip_prefix("original: ")?;
    let final_name = lines.next()?.strip_prefix("final: ")?;
    let size = lines.next()?.strip_prefix("size: ")?;
    if original.is_empty() || final_name.is_empty() || size.is_empty() || lines.next().is_some() {
        return None;
    }
    Some(RecoveryTombstone {
        original: original.to_owned(),
        final_name: final_name.to_owned(),
        size_bytes: size.parse().ok()?,
    })
}

/// Refuses symlinks and verifies the current final object still has the published size.
pub fn published_final_matches(final_path: &Path, expected_size: u64) -> bool {
    std::fs::symlink_metadata(final_path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == expected_size)
}

/// Revalidates that a tombstone path is still the same trusted v2 transaction evidence.
pub fn recovery_tombstone_matches(path: &Path, expected: &RecoveryTombstone) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        && std::fs::read(path)
            .ok()
            .and_then(|bytes| parse_recovery_tombstone(&bytes))
            .is_some_and(|current| current == *expected)
}

/// Deterministic paths associated with a recovered final.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryTransactionPaths {
    pub original_partial: PathBuf,
    pub recovered_final: PathBuf,
    pub tombstone: PathBuf,
}

/// Derives transaction paths only from a canonical recovered recording name.
pub fn recovery_transaction_paths(recovered_final: &Path) -> Option<RecoveryTransactionPaths> {
    if classify_recording_file(recovered_final) != RecordingFileKind::RecoveredRecording {
        return None;
    }
    let name = recovered_final.file_name()?.to_str()?;
    let base = name.strip_suffix(".recovered.mkv")?;
    let directory = recovered_final.parent()?;
    Some(RecoveryTransactionPaths {
        original_partial: directory.join(format!("{base}.partial.mkv")),
        recovered_final: recovered_final.to_path_buf(),
        tombstone: directory.join(format!("{base}.recovered.mkv.done")),
    })
}

/// Whether retention can delete a recovered final without enabling resurrection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveredRetentionState {
    /// Final is a trusted regular file, v2 tombstone proves it, original is absent.
    Settled {
        tombstone: PathBuf,
        evidence: RecoveryTombstone,
    },
    /// Transaction is unresolved; preservation wins.
    Blocked { reason: &'static str },
    /// A required filesystem fact could not be inspected. This is never
    /// interpreted as absence and therefore never authorizes deletion.
    InspectionError { path: PathBuf, kind: ErrorKind },
}

/// Inspects only filesystem transaction evidence. It never mutates media.
pub fn inspect_recovered_retention(recovered_final: &Path) -> RecoveredRetentionState {
    inspect_recovered_retention_with(recovered_final, &|candidate| {
        std::fs::symlink_metadata(candidate)
    })
}

fn inspect_recovered_retention_with<F>(
    recovered_final: &Path,
    metadata: &F,
) -> RecoveredRetentionState
where
    F: Fn(&Path) -> std::io::Result<Metadata>,
{
    let Some(paths) = recovery_transaction_paths(recovered_final) else {
        return RecoveredRetentionState::Blocked {
            reason: "not a canonical recovered recording",
        };
    };
    let final_metadata = match inspect_path_presence_with(&paths.recovered_final, metadata) {
        PathPresence::Present(metadata) => metadata,
        PathPresence::Absent => {
            return RecoveredRetentionState::Blocked {
                reason: "recovered final is missing",
            };
        }
        PathPresence::Uninspectable(error) => {
            return RecoveredRetentionState::InspectionError {
                path: paths.recovered_final,
                kind: error.kind(),
            };
        }
    };
    if !final_metadata.is_file() {
        return RecoveredRetentionState::Blocked {
            reason: "recovered final is not a regular file",
        };
    }
    match inspect_path_presence_with(&paths.original_partial, metadata) {
        PathPresence::Present(_) => {
            return RecoveredRetentionState::Blocked {
                reason: "original partial still exists",
            };
        }
        PathPresence::Absent => {}
        PathPresence::Uninspectable(error) => {
            return RecoveredRetentionState::InspectionError {
                path: paths.original_partial,
                kind: error.kind(),
            };
        }
    }
    let tombstone_metadata = match inspect_path_presence_with(&paths.tombstone, metadata) {
        PathPresence::Present(metadata) => metadata,
        PathPresence::Absent => {
            return RecoveredRetentionState::Blocked {
                reason: "trusted tombstone is missing",
            };
        }
        PathPresence::Uninspectable(error) => {
            return RecoveredRetentionState::InspectionError {
                path: paths.tombstone,
                kind: error.kind(),
            };
        }
    };
    if !tombstone_metadata.is_file() {
        return RecoveredRetentionState::Blocked {
            reason: "tombstone is not a regular file",
        };
    }
    let Some(final_name) = paths
        .recovered_final
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return RecoveredRetentionState::Blocked {
            reason: "recovered final name is not UTF-8",
        };
    };
    let Some(original_name) = paths
        .original_partial
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return RecoveredRetentionState::Blocked {
            reason: "original partial name is not UTF-8",
        };
    };
    let tombstone_bytes = match std::fs::read(&paths.tombstone) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return RecoveredRetentionState::Blocked {
                reason: "trusted tombstone disappeared during inspection",
            };
        }
        Err(error) => {
            return RecoveredRetentionState::InspectionError {
                path: paths.tombstone,
                kind: error.kind(),
            };
        }
    };
    let Some(transaction) = parse_recovery_tombstone(&tombstone_bytes) else {
        return RecoveredRetentionState::Blocked {
            reason: "tombstone is malformed or untrusted",
        };
    };
    if transaction.original != original_name
        || transaction.final_name != final_name
        || transaction.size_bytes != final_metadata.len()
    {
        return RecoveredRetentionState::Blocked {
            reason: "tombstone does not prove the current recovered final",
        };
    }
    RecoveredRetentionState::Settled {
        tombstone: paths.tombstone,
        evidence: transaction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_is_strict_and_rejects_legacy_or_extra_content() {
        let valid =
            recovery_tombstone_payload("08-30-00.partial.mkv", "08-30-00.recovered.mkv", 123);
        assert_eq!(
            parse_recovery_tombstone(valid.as_bytes())
                .unwrap()
                .size_bytes,
            123
        );
        assert!(parse_recovery_tombstone(b"NIAN-RECOVERY-TOMBSTONE v1\n").is_none());
        assert!(parse_recovery_tombstone(format!("{valid}extra\n").as_bytes()).is_none());
    }

    #[test]
    fn settled_requires_absent_original_and_size_bound_tombstone() {
        let temp = tempfile::tempdir().unwrap();
        let final_path = temp.path().join("08-30-00.recovered.mkv");
        let original = temp.path().join("08-30-00.partial.mkv");
        let tombstone = temp.path().join("08-30-00.recovered.mkv.done");
        std::fs::write(&final_path, b"footage").unwrap();
        std::fs::write(
            &tombstone,
            recovery_tombstone_payload("08-30-00.partial.mkv", "08-30-00.recovered.mkv", 7),
        )
        .unwrap();

        assert!(matches!(
            inspect_recovered_retention(&final_path),
            RecoveredRetentionState::Settled { .. }
        ));

        std::fs::write(&original, b"old-partial").unwrap();
        assert!(matches!(
            inspect_recovered_retention(&final_path),
            RecoveredRetentionState::Blocked { .. }
        ));
    }

    #[test]
    fn original_metadata_errors_never_authorize_settled_retention() {
        let temp = tempfile::tempdir().unwrap();
        let final_path = temp.path().join("08-30-00.recovered.mkv");
        let original = temp.path().join("08-30-00.partial.mkv");
        let tombstone = temp.path().join("08-30-00.recovered.mkv.done");
        std::fs::write(&final_path, b"footage").unwrap();
        std::fs::write(
            &tombstone,
            recovery_tombstone_payload("08-30-00.partial.mkv", "08-30-00.recovered.mkv", 7),
        )
        .unwrap();

        for kind in [ErrorKind::PermissionDenied, ErrorKind::Other] {
            let state = inspect_recovered_retention_with(&final_path, &|candidate| {
                if candidate == original {
                    Err(std::io::Error::from(kind))
                } else {
                    std::fs::symlink_metadata(candidate)
                }
            });
            assert!(matches!(
                state,
                RecoveredRetentionState::InspectionError {
                    ref path,
                    kind: observed,
                } if path == &original && observed == kind
            ));
            assert!(
                final_path.is_file(),
                "inspection failure must preserve footage"
            );
        }
    }
}
