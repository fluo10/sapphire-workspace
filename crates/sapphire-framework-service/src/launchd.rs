//! Rendering a macOS LaunchAgent, user level only.
//!
//! A LaunchAgent starts with the user's session and lives in their own
//! `~/Library/LaunchAgents`, so it is theirs to remove — the macOS counterpart of a systemd
//! user unit.

use std::path::{Path, PathBuf};

use crate::scope::{InstallContext, ServiceSpec};

/// The label a LaunchAgent is registered under.
///
/// A LaunchAgent label is a global namespace on the machine: a generic label such as
/// `sapphire-agent` would collide with anything else that claims the same one, so the label
/// carries the repository owner, matching the `repository` field in `Cargo.toml`
/// (`github.com/fluo10/sapphire-framework`): `net.fireturtle.sapphire.<app>`.
pub fn label(app_name: &str) -> String {
    format!("net.fireturtle.sapphire.{app_name}")
}

/// Where a LaunchAgent is installed: `~/Library/LaunchAgents/<label>.plist`.
///
/// The file is named by the label, so `launchctl` finds it by the label's reverse-DNS path.
pub fn agent_path(app_name: &str, home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{}.plist", label(app_name)))
}

/// Escape a string for inclusion as XML character data.
///
/// A single pass: ampersands are escaped first, then the angle brackets. Only the three
/// entities that make text unparseable: quotes are legal unescaped in text nodes, where
/// every dynamic value lands, and the template's own attribute values are static. An
/// entity form already present in the input (& typed literally) is itself escaped,
/// the way XML escaping works.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + 12);
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Make a string safe inside an XML comment.
///
/// A double hyphen is forbidden in comments by the XML grammar, so a description like
/// `a -- b` would make the file unparseable; a trailing hyphen would too, so the
/// trailing space keeps the `-->` closer from reading as one.
fn comment_safe(text: &str) -> String {
    text.replace("--", "- -").trim_end().to_owned()
}

/// Turn a [`ServiceSpec`] and a resolved [`InstallContext`] into a LaunchAgent plist.
///
/// The agent runs one program with arguments: the plist's `ProgramArguments`, which launchd
/// execs directly, with no shell in between — so each argument becomes its own `<string>`
/// and a space inside an argument needs no quoting at all. `RunAtLoad` starts the agent
/// when the session loads it and `KeepAlive`/`SuccessfulExit=false` restarts it on failure,
/// the counterparts of a user unit's activation and `Restart=on-failure`; the description
/// lands in an XML comment, the place a human looks and the one place the XML
/// grammar (unlike the plist) has room for it.
pub fn render_launch_agent(spec: &ServiceSpec, ctx: &InstallContext) -> String {
    let mut agent = String::new();
    agent.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    agent.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ");
    agent.push_str("\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    agent.push_str("<plist version=\"1.0\">\n<dict>\n");
    agent.push_str("  <key>Label</key>\n");
    agent.push_str(&format!(
        "  <string>{}</string>\n",
        escape(&label(spec.app_name))
    ));
    agent.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    agent.push_str(&format!(
        "    <string>{}</string>\n",
        escape(&ctx.exe.display().to_string())
    ));
    for arg in &spec.args {
        agent.push_str(&format!("    <string>{}</string>\n", escape(arg)));
    }
    agent.push_str("  </array>\n");
    agent.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    agent.push_str("  <key>KeepAlive</key>\n  <dict>\n");
    agent.push_str("    <key>SuccessfulExit</key>\n    <false/>\n");
    agent.push_str("  </dict>\n");
    agent.push_str("  <key>ThrottleInterval</key>\n  <integer>5</integer>\n");
    // The description for `launchctl list`, as an XML comment: launchd reads the plist
    // through an XML parser, so `--` runs (illegal inside a comment) become `- -`.
    agent.push_str(&format!("  <!-- {} -->\n", comment_safe(&spec.description)));
    agent.push_str("</dict>\n</plist>\n");
    agent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_lives_under_the_users_launch_agents() {
        assert_eq!(
            agent_path("sapphire-agent", Path::new("/Users/alice")),
            PathBuf::from(
                "/Users/alice/Library/LaunchAgents/net.fireturtle.sapphire.sapphire-agent.plist"
            )
        );
    }

    #[test]
    fn entity_text_escapes_to_a_single_pass() {
        // A raw ampersand is escaped; entity forms already in the input are escaped whole.
        assert_eq!(escape("a & b"), "a &amp; b");
        assert_eq!(escape("&"), "&amp;");
        assert_eq!(escape("<&>"), "&lt;&amp;&gt;");
    }
}
