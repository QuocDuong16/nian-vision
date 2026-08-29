//! Non-media recovery transaction evidence shared by recorder and retention.

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
}

/// Inspects only filesystem transaction evidence. It never mutates media.
pub fn inspect_recovered_retention(recovered_final: &Path) -> RecoveredRetentionState {
    let Some(paths) = recovery_transaction_paths(recovered_final) else {
        return RecoveredRetentionState::Blocked {
            reason: "not a canonical recovered recording",
        };
    };
    let Ok(final_metadata) = std::fs::symlink_metadata(&paths.recovered_final) else {
        return RecoveredRetentionState::Blocked {
            reason: "recovered final is missing",
        };
    };
    if !final_metadata.is_file() {
        return RecoveredRetentionState::Blocked {
            reason: "recovered final is not a regular file",
        };
    }
    if std::fs::symlink_metadata(&paths.original_partial).is_ok() {
        return RecoveredRetentionState::Blocked {
            reason: "original partial still exists",
        };
    }
    let Ok(tombstone_metadata) = std::fs::symlink_metadata(&paths.tombstone) else {
        return RecoveredRetentionState::Blocked {
            reason: "trusted tombstone is missing",
        };
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
    let Some(transaction) = std::fs::read(&paths.tombstone)
        .ok()
        .and_then(|bytes| parse_recovery_tombstone(&bytes))
    else {
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
}
