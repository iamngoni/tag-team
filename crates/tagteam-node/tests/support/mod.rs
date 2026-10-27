//! Shared capability checks for integration tests that exercise OS confinement.

/// True when this host can run the Linux confinement supervisor, or when the
/// test is running on another platform whose sandbox does not need systemd.
pub(crate) fn linux_sandbox_missing() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::path::Path;

        if !Path::new("/usr/bin/systemd-run").exists()
            || !Path::new("/usr/bin/bwrap").exists()
            || std::env::var_os("XDG_RUNTIME_DIR").is_none()
            || std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
        {
            return true;
        }
        !std::process::Command::new("/usr/bin/systemd-run")
            .args(["--user", "--scope", "--quiet", "--", "/usr/bin/true"])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Skip when Linux cannot start the user-scoped sandbox. A provisioned Linux
/// host can set TAGTEAM_REQUIRE_LINUX_SANDBOX=1 to turn a missing capability
/// into a hard failure instead of a skip.
pub(crate) fn skip_without_linux_sandbox(test_name: &str) -> bool {
    if !linux_sandbox_missing() {
        return false;
    }
    let message = format!(
        "a working Linux user systemd manager, /usr/bin/systemd-run, and /usr/bin/bwrap are required; skipping {test_name}"
    );
    if std::env::var_os("TAGTEAM_REQUIRE_LINUX_SANDBOX").is_some() {
        panic!("{message}");
    }
    eprintln!("{message}");
    true
}
