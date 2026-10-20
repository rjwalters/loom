//! Is this host an EC2 instance? Answered from DMI, with no network traffic,
//! so a non-EC2 host never touches `169.254.169.254`.

use std::path::Path;

const DMI_FILES: [&str; 3] = ["sys_vendor", "board_vendor", "bios_vendor"];

/// Whether the DMI vendor strings under `dir` name Amazon.
#[must_use]
pub fn is_ec2_at(dir: &Path) -> bool {
    DMI_FILES.iter().any(|f| {
        std::fs::read_to_string(dir.join(f))
            .is_ok_and(|v| v.to_ascii_lowercase().contains("amazon"))
    })
}

/// [`is_ec2_at`] for `/sys/class/dmi/id` (false where it does not exist, as
/// on macOS).
#[must_use]
pub fn is_ec2() -> bool {
    is_ec2_at(Path::new("/sys/class/dmi/id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmi_vendor_decides() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_ec2_at(dir.path()), "no DMI files: not EC2");
        std::fs::write(dir.path().join("sys_vendor"), "Dell Inc.\n").unwrap();
        assert!(!is_ec2_at(dir.path()));
        std::fs::write(dir.path().join("board_vendor"), "Amazon EC2\n").unwrap();
        assert!(is_ec2_at(dir.path()));
    }
}
