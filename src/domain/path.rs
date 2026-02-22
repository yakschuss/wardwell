use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use thiserror::Error;

use crate::config::types::PathGlob;

#[derive(Debug, Error)]
pub enum PathError {
    #[error("path resolution failed for '{path}': {source}")]
    Resolution {
        path: String,
        source: std::io::Error,
    },

    #[error("path resolution failed for '{path}': {reason}")]
    ResolutionFailed { path: String, reason: String },

    #[error("dangerous path detected: '{path}' — {reason}")]
    DangerousPath { path: String, reason: String },

    #[error("path '{path}' is outside domain boundary ({boundary})")]
    OutsideBoundary { path: String, boundary: String },

    #[error("TOCTOU traversal detected for '{path}': {reason}")]
    TraversalDetected { path: String, reason: String },

    #[error("failed to read file '{path}': {reason}")]
    ReadFailed { path: String, reason: String },
}

/// All known path traversal attack patterns.
/// Preserves the 14 from the Ruby prototype + new unicode/case variants.
const DANGEROUS_PATTERNS: &[(&str, &str)] = &[
    // Classic traversal
    ("../", "directory traversal"),
    ("..\\", "directory traversal (backslash)"),
    // URL-encoded
    ("%2e%2e", "URL-encoded traversal (lowercase)"),
    ("%2E%2E", "URL-encoded traversal (uppercase)"),
    ("%2e%2e%2f", "URL-encoded traversal with slash (lowercase)"),
    ("%2e%2e%5c", "URL-encoded traversal with backslash"),
    ("%2e%2e/", "URL-encoded dots with raw slash"),
    ("%2e%2e\\", "URL-encoded dots with raw backslash"),
    // Double-encoded
    ("%252e%252e", "double-encoded traversal"),
    // Mixed
    ("..%2f", "mixed traversal (dots + encoded slash)"),
    ("..%5c", "mixed traversal (dots + encoded backslash)"),
    ("%2e%2e/", "mixed traversal (encoded dots + raw slash)"),
    // Null bytes
    ("\x00", "null byte injection"),
    ("%00", "URL-encoded null byte"),
    // Unicode normalization attacks
    ("\u{FF0E}\u{FF0E}/", "fullwidth period traversal"),
    ("\u{FF0E}\u{FF0E}\\", "fullwidth period traversal (backslash)"),
    (
        "\u{2025}",
        "two-dot leader (unicode traversal)",
    ),
];

/// Check a raw path string for dangerous patterns BEFORE any filesystem resolution.
/// This is the first line of defense — catches attacks that canonicalize won't see.
pub fn check_dangerous_patterns(path_str: &str) -> Result<(), PathError> {
    let lowered = path_str.to_lowercase();

    for (pattern, reason) in DANGEROUS_PATTERNS {
        let pattern_lower = pattern.to_lowercase();
        if lowered.contains(&pattern_lower) {
            return Err(PathError::DangerousPath {
                path: path_str.to_string(),
                reason: reason.to_string(),
            });
        }
    }

    Ok(())
}

/// Resolve a path to its canonical form using the actual filesystem.
/// This catches symlinks, ../, and case variations on case-insensitive filesystems.
/// Uses realpath(3), not string manipulation.
pub fn resolve_path(path: &Path) -> Result<PathBuf, PathError> {
    std::fs::canonicalize(path).map_err(|e| PathError::Resolution {
        path: path.display().to_string(),
        source: e,
    })
}

/// Full path validation: check for dangerous patterns, then canonicalize.
pub fn validate_path(path_str: &str) -> Result<PathBuf, PathError> {
    // First: pattern-based checks on the raw string
    check_dangerous_patterns(path_str)?;

    // Then: filesystem-based canonicalization
    let path = Path::new(path_str);
    resolve_path(path)
}

/// Check if a canonical path is within any of the given boundary paths.
pub fn is_within_boundaries(path: &Path, boundaries: &[PathBuf]) -> bool {
    boundaries.iter().any(|boundary| path.starts_with(boundary))
}

/// TOCTOU-safe file open. Opens the file first, then verifies the opened fd
/// points to a file within the allowed boundary paths.
///
/// The classic check-then-open pattern has a race condition: the file could be
/// swapped (e.g., via symlink replacement) between the check and the open.
/// This function eliminates that race by:
///   1. Checking the raw path string for dangerous patterns (pre-open defense)
///   2. Opening the file to obtain a real file descriptor (bound to an inode)
///   3. Canonicalizing the original path to resolve symlinks
///   4. Verifying device+inode match between the open fd and the canonical path
///      (catches races where the path is swapped between open and canonicalize)
///   5. Checking the canonical path against the boundary globs
///
/// The key insight: even though step 3 is racy (the path could be swapped between
/// open and canonicalize), step 4 catches this. If an attacker swaps a symlink
/// after we open but before we canonicalize, the dev+inode of the canonical path
/// will differ from the fd's dev+inode, and we reject the request.
///
/// Returns the open `File` handle if allowed, or a `PathError` if the file
/// is outside the boundary or any step fails.
pub fn safe_open(path: &Path, boundaries: &[PathGlob]) -> Result<File, PathError> {
    // Step 1: Check dangerous patterns on the raw string before touching the filesystem
    check_dangerous_patterns(path.to_string_lossy().as_ref())?;

    // Step 2: Open the file — this gives us a real fd bound to an inode.
    //         The kernel resolves symlinks at open time and binds the fd to the
    //         target inode. No subsequent symlink manipulation can change what
    //         this fd points to.
    let file = File::open(path).map_err(|e| PathError::ResolutionFailed {
        path: path.to_string_lossy().to_string(),
        reason: e.to_string(),
    })?;

    // Step 3: Canonicalize the original path to resolve symlinks to a real path.
    //         This may race with symlink swaps, but step 4 catches that.
    let canonical = std::fs::canonicalize(path).map_err(|e| PathError::ResolutionFailed {
        path: path.to_string_lossy().to_string(),
        reason: format!("canonicalization failed: {e}"),
    })?;

    // Step 4: Verify device+inode match between the open fd and the canonical path.
    //         The fd's metadata comes from fstat(2) on the file descriptor — it
    //         reflects the actual inode the fd is bound to, not the path.
    //         If someone swapped the symlink between our open() and canonicalize(),
    //         the canonical path will point to a different inode than our fd, and
    //         this check will catch it.
    let fd_meta = file.metadata().map_err(|e| PathError::ResolutionFailed {
        path: path.to_string_lossy().to_string(),
        reason: e.to_string(),
    })?;
    let canonical_meta = std::fs::metadata(&canonical).map_err(|e| PathError::ResolutionFailed {
        path: path.to_string_lossy().to_string(),
        reason: e.to_string(),
    })?;

    if fd_meta.dev() != canonical_meta.dev() || fd_meta.ino() != canonical_meta.ino() {
        return Err(PathError::TraversalDetected {
            path: path.to_string_lossy().to_string(),
            reason: "fd does not match canonical path (device/inode mismatch — possible TOCTOU attack)".to_string(),
        });
    }

    // Step 5: Check the canonical path against boundary globs
    let allowed = boundaries.iter().any(|boundary| boundary.matches(&canonical));

    if !allowed {
        return Err(PathError::OutsideBoundary {
            path: canonical.to_string_lossy().to_string(),
            boundary: boundaries
                .iter()
                .map(|b| b.as_str().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        });
    }

    Ok(file)
}

/// TOCTOU-safe file open and read. Opens the file with `safe_open`, then reads
/// the entire contents into a `String`.
///
/// This combines the TOCTOU-safe open with a full read, guaranteeing that the
/// content returned came from a file verified to be within the boundary.
pub fn safe_open_read(path: &Path, boundaries: &[PathGlob]) -> Result<String, PathError> {
    let mut file = safe_open(path, boundaries)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|e| PathError::ReadFailed {
            path: path.to_string_lossy().to_string(),
            reason: e.to_string(),
        })?;
    Ok(contents)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // --- Dangerous pattern tests (all 14+ variants) ---

    #[test]
    fn blocks_classic_traversal() {
        let r1 = check_dangerous_patterns("../etc/passwd");
        assert!(r1.is_err(), "{r1:?}");
        let r2 = check_dangerous_patterns("..\\windows\\system32");
        assert!(r2.is_err(), "{r2:?}");
    }

    #[test]
    fn blocks_url_encoded_traversal() {
        let r1 = check_dangerous_patterns("%2e%2e/etc/passwd");
        assert!(r1.is_err(), "{r1:?}");
        let r2 = check_dangerous_patterns("%2E%2E/etc/passwd");
        assert!(r2.is_err(), "{r2:?}");
        let r3 = check_dangerous_patterns("%2e%2e%2f");
        assert!(r3.is_err(), "{r3:?}");
        let r4 = check_dangerous_patterns("%2e%2e%5c");
        assert!(r4.is_err(), "{r4:?}");
    }

    #[test]
    fn blocks_double_encoded() {
        let result = check_dangerous_patterns("%252e%252e/etc");
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn blocks_mixed_encoding() {
        let r1 = check_dangerous_patterns("..%2f");
        assert!(r1.is_err(), "{r1:?}");
        let r2 = check_dangerous_patterns("..%5c");
        assert!(r2.is_err(), "{r2:?}");
        let r3 = check_dangerous_patterns("%2e%2e/");
        assert!(r3.is_err(), "{r3:?}");
    }

    #[test]
    fn blocks_null_bytes() {
        let r1 = check_dangerous_patterns("/tmp/test\x00.txt");
        assert!(r1.is_err(), "{r1:?}");
        let r2 = check_dangerous_patterns("/tmp/test%00.txt");
        assert!(r2.is_err(), "{r2:?}");
    }

    #[test]
    fn blocks_unicode_normalization() {
        let result = check_dangerous_patterns("\u{FF0E}\u{FF0E}/etc/passwd");
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn allows_safe_paths() {
        let r1 = check_dangerous_patterns("/tmp/safe/file.txt");
        assert!(r1.is_ok(), "{r1:?}");
        let r2 = check_dangerous_patterns("/home/user/notes.md");
        assert!(r2.is_ok(), "{r2:?}");
        let r3 = check_dangerous_patterns("relative/path/ok.rs");
        assert!(r3.is_ok(), "{r3:?}");
    }

    // --- Path resolution tests (require filesystem) ---

    #[test]
    fn resolve_real_path() {
        // /tmp should exist on all unix systems
        let result = resolve_path(Path::new("/tmp"));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn resolve_nonexistent_path_errors() {
        let result = resolve_path(Path::new("/nonexistent/path/that/doesnt/exist"));
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn symlink_resolution() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().ok();
        if let Some(ref dir) = dir {
            let real_file = dir.path().join("real.txt");
            let link_file = dir.path().join("link.txt");
            std::fs::write(&real_file, "content").ok();
            symlink(&real_file, &link_file).ok();

            let resolved = resolve_path(&link_file);
            if let Ok(resolved) = resolved {
                // Resolved path should point to real file, not symlink
                let real_canonical = resolve_path(&real_file).ok();
                assert_eq!(Some(resolved), real_canonical);
            }
        }
    }

    #[test]
    fn symlink_outside_boundary_detected() {
        use std::os::unix::fs::symlink;
        let inside_dir = tempfile::tempdir().ok();
        let outside_dir = tempfile::tempdir().ok();

        if let (Some(inside), Some(outside)) = (&inside_dir, &outside_dir) {
            // Create a file outside the boundary
            let outside_file = outside.path().join("secret.txt");
            std::fs::write(&outside_file, "secret").ok();

            // Create a symlink inside the boundary pointing outside
            let link = inside.path().join("sneaky_link.txt");
            symlink(&outside_file, &link).ok();

            // Resolve the symlink
            let resolved = resolve_path(&link);
            if let Ok(resolved) = resolved {
                // The resolved path should be OUTSIDE the boundary
                let boundary = resolve_path(inside.path()).ok();
                if let Some(ref boundary) = boundary {
                    assert!(
                        !is_within_boundaries(&resolved, std::slice::from_ref(boundary)),
                        "symlink to outside file should not be within boundary"
                    );
                }
            }
        }
    }

    #[test]
    fn symlink_chain_outside_boundary_detected() {
        use std::os::unix::fs::symlink;
        let inside_dir = tempfile::tempdir().ok();
        let mid_dir = tempfile::tempdir().ok();
        let outside_dir = tempfile::tempdir().ok();

        if let (Some(inside), Some(mid), Some(outside)) =
            (&inside_dir, &mid_dir, &outside_dir)
        {
            let outside_file = outside.path().join("secret.txt");
            std::fs::write(&outside_file, "secret").ok();

            // Chain: inside/link_a -> mid/link_b -> outside/secret.txt
            let link_b = mid.path().join("link_b.txt");
            symlink(&outside_file, &link_b).ok();

            let link_a = inside.path().join("link_a.txt");
            symlink(&link_b, &link_a).ok();

            let resolved = resolve_path(&link_a);
            if let Ok(resolved) = resolved {
                let boundary = resolve_path(inside.path()).ok();
                if let Some(ref boundary) = boundary {
                    assert!(
                        !is_within_boundaries(&resolved, std::slice::from_ref(boundary)),
                        "chained symlink to outside should not be within boundary"
                    );
                }
            }
        }
    }

    #[test]
    fn boundary_check_works() {
        let boundaries = vec![PathBuf::from("/tmp/allowed")];
        assert!(is_within_boundaries(
            Path::new("/tmp/allowed/file.txt"),
            &boundaries
        ));
        assert!(!is_within_boundaries(
            Path::new("/tmp/forbidden/file.txt"),
            &boundaries
        ));
    }

    // --- TOCTOU-safe open tests ---

    /// Helper to create a PathGlob boundary from a tempdir path.
    /// Returns None if the glob cannot be created (avoids unwrap).
    fn boundary_for_dir(dir: &std::path::Path) -> Option<PathGlob> {
        // Canonicalize the dir so the boundary matches canonicalized fd paths
        let canonical = std::fs::canonicalize(dir).ok()?;
        let pattern = format!("{}/*", canonical.display());
        PathGlob::new(&pattern).ok()
    }

    #[test]
    fn safe_open_within_boundary() {
        let dir = tempfile::tempdir().ok();
        if let Some(ref dir) = dir {
            let file_path = dir.path().join("allowed.txt");
            std::fs::write(&file_path, "hello").ok();

            if let Some(boundary) = boundary_for_dir(dir.path()) {
                let result = safe_open(&file_path, &[boundary]);
                assert!(result.is_ok(), "file inside boundary should open successfully");
            }
        }
    }

    #[test]
    fn safe_open_outside_boundary() {
        // Create two separate temp dirs: one for the boundary, one for the file
        let boundary_dir = tempfile::tempdir().ok();
        let outside_dir = tempfile::tempdir().ok();

        if let (Some(boundary_dir), Some(outside_dir)) = (&boundary_dir, &outside_dir) {
            let outside_file = outside_dir.path().join("secret.txt");
            std::fs::write(&outside_file, "secret data").ok();

            if let Some(boundary) = boundary_for_dir(boundary_dir.path()) {
                let result = safe_open(&outside_file, &[boundary]);
                assert!(result.is_err(), "file outside boundary should be rejected");

                if let Err(PathError::OutsideBoundary { path, .. }) = &result {
                    assert!(
                        path.contains("secret.txt"),
                        "error should reference the file path"
                    );
                }
            }
        }
    }

    #[test]
    fn safe_open_nonexistent() {
        let dir = tempfile::tempdir().ok();
        if let Some(ref dir) = dir
            && let Some(boundary) = boundary_for_dir(dir.path()) {
                let missing = dir.path().join("does_not_exist.txt");
                let result = safe_open(&missing, &[boundary]);
                assert!(
                    result.is_err(),
                    "nonexistent file should return an error"
                );

                if let Err(PathError::ResolutionFailed { reason, .. }) = &result {
                    assert!(
                        reason.contains("No such file") || reason.contains("not found"),
                        "error should mention the file is missing, got: {reason}"
                    );
                }
        }
    }

    #[test]
    fn safe_open_read_works() {
        let dir = tempfile::tempdir().ok();
        if let Some(ref dir) = dir {
            let file_path = dir.path().join("readable.txt");
            let expected_content = "TOCTOU-safe content here";
            std::fs::write(&file_path, expected_content).ok();

            if let Some(boundary) = boundary_for_dir(dir.path()) {
                let result = safe_open_read(&file_path, &[boundary]);
                assert!(result.is_ok(), "safe_open_read should succeed");
                if let Ok(content) = result {
                    assert_eq!(content, expected_content);
                }
            }
        }
    }

    #[test]
    fn safe_open_symlink_outside_boundary() {
        use std::os::unix::fs::symlink;

        let inside_dir = tempfile::tempdir().ok();
        let outside_dir = tempfile::tempdir().ok();

        if let (Some(inside), Some(outside)) = (&inside_dir, &outside_dir) {
            // Create a real file outside the boundary
            let outside_file = outside.path().join("secret.txt");
            std::fs::write(&outside_file, "secret").ok();

            // Create a symlink inside the boundary pointing to the outside file
            let link = inside.path().join("sneaky_link.txt");
            symlink(&outside_file, &link).ok();

            if let Some(boundary) = boundary_for_dir(inside.path()) {
                // safe_open should resolve through the symlink via the fd
                // and detect the real file is outside the boundary
                let result = safe_open(&link, &[boundary]);
                assert!(
                    result.is_err(),
                    "symlink pointing outside boundary should be rejected"
                );

                // Verify it's specifically an OutsideBoundary error (not just any error)
                assert!(
                    matches!(&result, Err(PathError::OutsideBoundary { .. })),
                    "should be OutsideBoundary error, got: {result:?}"
                );
            }
        }
    }
}
