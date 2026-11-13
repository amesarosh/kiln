//! The program that asks for passwords when apps need admin rights (a polkit agent), started
//! at login from `[general] polkit_agent`.

use std::path::Path;

/// Agents tried in order when `polkit_agent` is empty: program names are looked up in PATH,
/// full paths as they are.
const AGENTS: &[&str] = &[
    "kiln-polkit",
    "/usr/lib/polkit-gnome/polkit-gnome-authentication-agent-1",
    "/usr/libexec/polkit-gnome-authentication-agent-1",
    "/usr/lib/x86_64-linux-gnu/libexec/polkit-kde-authentication-agent-1",
    "/usr/libexec/polkit-kde-authentication-agent-1",
    "/usr/lib/polkit-kde-authentication-agent-1",
    "/usr/libexec/polkit-mate-authentication-agent-1",
    "/usr/lib/mate-polkit/polkit-mate-authentication-agent-1",
    "lxpolkit",
];

/// The command to start, if any: the configured one, else the first agent installed. None
/// for "none", or when autostart already starts one.
pub fn agent_command(configured: &str, autostart: &[String]) -> Option<String> {
    let configured = configured.trim();
    if configured == "none" {
        return None;
    }
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    if autostart
        .iter()
        .any(|c| c.contains("polkit") || c.contains("authentication-agent"))
    {
        return None;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    AGENTS
        .iter()
        .find(|a| {
            if a.starts_with('/') {
                Path::new(a).is_file()
            } else {
                std::env::split_paths(&path).any(|dir| dir.join(a).is_file())
            }
        })
        .map(|a| a.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picking() {
        assert_eq!(agent_command("none", &[]), None);
        assert_eq!(agent_command(" my-agent ", &[]).as_deref(), Some("my-agent"));
        let mine = vec!["/usr/lib/polkit-gnome/polkit-gnome-authentication-agent-1".to_string()];
        assert_eq!(agent_command("", &mine), None, "autostart has one already");
    }
}
