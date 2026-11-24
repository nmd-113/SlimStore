#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppError(pub String);
impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AppError {}
impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}

pub fn describe_exit(code: u32) -> &'static str {
    match code {
        0 => "Completed",
        0x8a150006 => "The installer failed. Open View log for details.",
        0x8a150014 => "No matching packages were found.",
        0x8a15002b => "No applicable update is available.",
        0x8a150011 => {
            "WinGet blocked the installer because its checksum differs from the catalog. Try again after the catalog entry is corrected."
        }
        0x80190194 | 0x8019019a => {
            "The installer download is unavailable. Refresh the package and check for a newer release."
        }
        0x8a15000f => {
            "The package source is unavailable. Check the network and configured sources."
        }
        0x8a15000c => "This source name is already configured.",
        0x8a150109 | 3010 => "Completed; restart Windows to finish installation.",
        0x8a15010a => "Restart Windows before trying this installation again.",
        0x8a15010c | 0x800704c7 => {
            "Cancelled by the user. Refresh the inventory to check the package state."
        }
        0x8a15010d => "The application is already installed. Refresh the inventory.",
        0x8a150102 => "Another installation is in progress. Wait and try again.",
        0x8a150107 | 0x80072ee7 => {
            "A network connection is required. Check your connection and try again."
        }
        0x80070005 => {
            "Permission was denied. The installer may need elevation or your organization may restrict it."
        }
        0x8a15000e => "The MSIX file is a bundle rather than an individual package.",
        0x8a150041 => {
            "Publisher agreements were not accepted. Review package details before retrying."
        }
        0x8a150046 => {
            "Source agreements need consent. Review source information in Settings before enabling acceptance."
        }
        0x8a15007d => {
            "Open SlimStore normally to update or remove this current-user package. WinGet blocks this action in an administrator session."
        }
        _ => "WinGet reported a failure. Review the diagnostic output below.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn errors_are_actionable() {
        assert!(describe_exit(0x80070005).contains("Permission"));
        assert!(describe_exit(0x8a150109).contains("restart"));
        assert!(describe_exit(0x8a15007d).contains("current-user"));
        assert!(describe_exit(0x8a150011).contains("checksum"));
        assert!(describe_exit(0x80190194).contains("download is unavailable"));
        assert!(describe_exit(42).contains("diagnostic"));
    }
}
