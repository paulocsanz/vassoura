//! Artefato launchd (P1.1): o plist do LaunchAgent e o meio de instalá-lo.
//! Instalar escreve o plist em `~/Library/LaunchAgents`; CARREGAR na sessão
//! launchd é decisão do operador (o comando é impresso, não executado).

use std::path::{Path, PathBuf};

pub const LABEL: &str = "com.vassoura.daemon";

/// Gera o plist do LaunchAgent (função pura — testada).
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

/// Escreve o plist em `~/Library/LaunchAgents/{LABEL}.plist` e devolve o
/// caminho. Não carrega nada na sessão launchd — isso é com o operador.
pub fn install(bin: &Path, config: &Path, interval_secs: u64) -> Result<PathBuf, String> {
    let home = crate::config::home();
    let dir = home.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&dir).map_err(|e| format!("criar {}: {e}", dir.display()))?;
    let path = dir.join(format!("{LABEL}.plist"));
    std::fs::write(&path, plist_content(bin, config, interval_secs, &home))
        .map_err(|e| format!("escrever {}: {e}", path.display()))?;
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
        assert!(s.contains("<string>daemon</string>"), "invoca o subcomando daemon");
        assert!(s.contains("<string>--config</string>"));
        assert!(s.contains("<string>/Users/t/.vassoura/config.toml</string>"));
        assert!(s.contains("<integer>300</integer>"));
        assert!(s.contains("<key>RunAtLoad</key>"));
    }
}
