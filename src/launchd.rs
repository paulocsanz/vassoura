//! launchd artifact (P1.1): the LaunchAgent plist and the way to install it.
//! Installing writes the plist to `~/Library/LaunchAgents`. LOADING it into
//! the launchd session is your decision (the command is printed, not run).

use std::path::{Path, PathBuf};

pub const LABEL: &str = "com.vassoura.daemon";

/// Build the LaunchAgent plist (pure function — tested).
pub fn plist_content(bin: &Path, config: &Path, interval_secs: u64, home: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>--config</string>
        <string>{config}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StartInterval</key>
    <integer>{interval_secs}</integer>
    <key>StandardOutPath</key>
    <string>{home}/.vassoura/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>{home}/.vassoura/daemon.log</string>
</dict>
</plist>
"#,
        bin = bin.display(),
        config = config.display(),
        interval_secs = interval_secs,
        home = home.display(),
    )
}

/// Write the plist to `~/Library/LaunchAgents/{LABEL}.plist` and return the
/// path. Does not load anything into the launchd session — that is up to you.
pub fn install(bin: &Path, config: &Path, interval_secs: u64) -> Result<PathBuf, String> {
    let home = crate::config::home();
    let dir = home.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{LABEL}.plist"));
    std::fs::write(&path, plist_content(bin, config, interval_secs, &home))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_points_at_daemon_and_never_touches_other_keys() {
        let s = plist_content(
            Path::new("/usr/local/bin/vassoura"),
            Path::new("/Users/t/.vassoura/config.toml"),
            300,
            Path::new("/Users/t"),
        );
        assert!(s.contains("<string>com.vassoura.daemon</string>"));
        assert!(s.contains("<string>daemon</string>"), "invokes the daemon subcommand");
        assert!(s.contains("<string>--config</string>"));
        assert!(s.contains("<string>/Users/t/.vassoura/config.toml</string>"));
        assert!(s.contains("<integer>300</integer>"));
        assert!(s.contains("<key>RunAtLoad</key>"));
        assert!(s.contains("<key>KeepAlive</key>"));
    }
}
