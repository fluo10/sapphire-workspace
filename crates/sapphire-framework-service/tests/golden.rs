//! Generated unit files, compared against copies checked into the repository.
//!
//! When one of these fails, read the diff before regenerating: a unit file is the contract
//! between this crate and the machine, and a change to it is a change of behaviour.

use std::path::PathBuf;

use sapphire_framework_service::{
    InstallContext, ServiceSpec, render_launch_agent, render_task, render_unit,
};

fn golden(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; create it from the failure output", path.display()))
}

fn check(name: &str, rendered: &str) {
    let want = golden(name);
    // The generators always emit LF; the golden file on disk might not, on a checkout where
    // `core.autocrlf` turned it into CRLF despite `.gitattributes` pinning it to LF (a stale
    // working tree from before that was added, or a git client that does not honour it,
    // issue #166). Normalizing both sides is what keeps a failure here about a real change
    // in what is generated, not about how the checkout's line endings happened to land.
    let normalize = |s: &str| s.replace("\r\n", "\n");
    assert_eq!(
        normalize(rendered.trim_end()),
        normalize(want.trim_end()),
        "\n--- generated ---\n{rendered}\n--- {name} ---\n{want}\n"
    );
}

fn spec() -> ServiceSpec {
    ServiceSpec {
        app_name: "sapphire-agent",
        description: "Sapphire agent server".into(),
        args: vec!["server".into(), "run".into()],
        post_install: None,
    }
}

fn ctx() -> InstallContext {
    InstallContext {
        unit_path: PathBuf::from("/dev/null"),
        exe: PathBuf::from("/usr/bin/sapphire-agent"),
    }
}

#[test]
fn a_user_unit() {
    check("user.service", &render_unit(&spec(), &ctx()));
}

#[test]
fn a_unit_needs_no_network_ordering() {
    let rendered = render_unit(&spec(), &ctx());
    assert!(
        !rendered.contains("network-online.target"),
        "a user unit starts after the session is up already"
    );
}

#[test]
fn exec_start_is_absolute_and_carries_the_arguments() {
    let rendered = render_unit(&spec(), &ctx());
    assert!(
        rendered.contains("ExecStart=/usr/bin/sapphire-agent server run"),
        "{rendered}"
    );
}

#[test]
fn a_launch_agent() {
    check("launchagent.plist", &render_launch_agent(&spec(), &ctx()));
}

#[test]
fn a_launch_agent_label_is_namespaced() {
    let rendered = render_launch_agent(&spec(), &ctx());
    assert!(
        rendered.contains("net.fireturtle.sapphire.sapphire-agent"),
        "a LaunchAgent label is a global namespace: {rendered}"
    );
}

#[test]
fn a_scheduled_task() {
    check("task.xml", &render_task(&spec(), &ctx()));
}

#[test]
fn a_scheduled_task_runs_at_logon() {
    let rendered = render_task(&spec(), &ctx());
    assert!(rendered.contains("LogonTrigger"), "{rendered}");
}

#[test]
fn an_argument_with_a_space_survives_the_xml() {
    let mut with_space = spec();
    with_space.args = vec!["server".into(), "--note".into(), "a b".into()];
    let rendered = render_task(&with_space, &ctx());
    assert!(rendered.contains("\"a b\""), "{rendered}");
}

#[test]
fn an_ampersand_in_a_description_is_escaped() {
    let mut awkward = spec();
    awkward.description = "Notes & ledger".into();
    let rendered = render_task(&awkward, &ctx());
    assert!(rendered.contains("Notes &amp; ledger"), "{rendered}");
    assert!(
        !rendered.contains("Notes & ledger"),
        "unescaped XML: {rendered}"
    );
}
