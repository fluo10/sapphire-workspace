//! Rendering a Windows Task Scheduler task, user level only.
//!
//! The task starts when the user logs on (a `LogonTrigger`) and runs their own copy of the
//! app from their own profile — the Windows counterpart of a LaunchAgent. Real Windows
//! services (and LaunchDaemons) are per-machine services this crate does not install; it
//! installs the per-user kind everywhere — the counterpart of a systemd user unit.

use crate::scope::{InstallContext, ServiceSpec};

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

/// Build the `Arguments` value: the spec's arguments as one Windows command line.
///
/// The scheduler hands the line to `CreateProcess`, which splits on spaces unless an
/// argument is quoted, so an argument containing a space is wrapped in double quotes to
/// survive as one argument — the one quoting rule a `Command`/`Arguments` pair needs.
fn arguments(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.contains(' ') {
                format!("\"{arg}\"")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Turn a [`ServiceSpec`] and a resolved [`InstallContext`] into Task Scheduler XML.
///
/// The XML is schema version 1.2, what `schtasks /create /xml` and the scheduler's COM
/// import read. A `LogonTrigger` starts the task when the user logs on — the counterpart
/// of a user unit's activation — and `MultipleInstancesPolicy=IgnoreNew` keeps a second
/// copy from stepping on the first, the counterpart of a single-instance service. The
/// limit-free `ExecutionTimeLimit` matters: the scheduler's default thirty days would stop
/// a long-running agent.
pub fn render_task(spec: &ServiceSpec, ctx: &InstallContext) -> String {
    let mut task = String::new();
    task.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    task.push_str(
        "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n",
    );
    task.push_str("  <RegistrationInfo>\n");
    task.push_str(&format!(
        "    <Description>{}</Description>\n",
        escape(&spec.description)
    ));
    task.push_str("  </RegistrationInfo>\n");
    task.push_str("  <Triggers>\n");
    task.push_str("    <LogonTrigger>\n");
    task.push_str("      <Enabled>true</Enabled>\n");
    task.push_str("    </LogonTrigger>\n");
    task.push_str("  </Triggers>\n");
    task.push_str("  <Principals>\n");
    task.push_str("    <Principal id=\"Author\">\n");
    task.push_str("      <LogonType>InteractiveToken</LogonType>\n");
    task.push_str("    </Principal>\n");
    task.push_str("  </Principals>\n");
    task.push_str("  <Settings>\n");
    task.push_str("    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n");
    task.push_str("    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n");
    task.push_str("    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n");
    task.push_str("    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n");
    task.push_str("  </Settings>\n");
    task.push_str("  <Actions Context=\"Author\">\n");
    task.push_str("    <Exec>\n");
    task.push_str(&format!(
        "      <Command>{}</Command>\n",
        escape(&ctx.exe.display().to_string())
    ));
    task.push_str(&format!(
        "      <Arguments>{}</Arguments>\n",
        escape(&arguments(&spec.args))
    ));
    task.push_str("    </Exec>\n");
    task.push_str("  </Actions>\n");
    task.push_str("</Task>\n");
    task
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_text_escapes_to_a_single_pass() {
        // A raw ampersand is escaped; entity forms already in the input are escaped whole.
        assert_eq!(escape("a & b"), "a &amp; b");
        assert_eq!(escape("&"), "&amp;");
        assert_eq!(escape("<&>"), "&lt;&amp;&gt;");
    }

    #[test]
    fn an_argument_with_a_space_is_quoted_for_the_command_line() {
        let args = vec![
            "server".to_owned(),
            "--note".to_owned(),
            "a b".to_owned(),
            "tail".to_owned(),
        ];
        assert_eq!(
            arguments(&args),
            "server --note \"a b\" tail",
            "the quoted argument stays one argument; its neighbours keep their own"
        );
    }
}
