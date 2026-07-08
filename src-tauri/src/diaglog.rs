// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 ArchR
//
// Streaming diagnostic log for the flash pipeline, shared by every
// platform. Reset at the start of each flash and flushed line by line,
// so crashes and kills still leave a complete trace the user can attach
// to a bug report.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Single well-known location, independent of the platform flash path.
pub fn log_path() -> PathBuf {
    std::env::temp_dir().join("archr-flasher-flash.log")
}

pub struct DiagLog {
    file: Option<File>,
}

impl DiagLog {
    /// Truncate the previous run's log and write the header.
    pub fn new() -> Self {
        let file = File::create(log_path()).ok();
        let mut log = DiagLog { file };
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        log.push_str(&format!(
            "archr-flasher {} flash log (os: {}, started: {} unix)\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            epoch
        ));
        log
    }

    pub fn push_str(&mut self, line: &str) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}

/// Append the log location to an error message without disturbing the
/// stable err:* tokens the frontend matches at the start of the string.
pub fn with_log_hint(err: String) -> String {
    if err == "cancelled" {
        return err;
    }
    format!("{} (log: {})", err, log_path().display())
}
