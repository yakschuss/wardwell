#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::{Path, PathBuf};
use wardwell::enforcement::path::{check_dangerous_patterns, is_within_boundaries};

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        // Exercise dangerous pattern detection on raw input
        let _ = check_dangerous_patterns(input);

        // Exercise it again via Path round-trip (lossy conversion)
        let path = Path::new(input);
        let _ = check_dangerous_patterns(&path.to_string_lossy());

        // Exercise boundary checking with a dummy boundary
        let boundaries = vec![PathBuf::from("/tmp/allowed")];
        let _ = is_within_boundaries(path, &boundaries);
    }
});
