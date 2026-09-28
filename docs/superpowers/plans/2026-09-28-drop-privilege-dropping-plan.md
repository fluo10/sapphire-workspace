# Issue #145: Privilege Dropping Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> If your harness has no such skill, execute the tasks in order, one at a time, running the
> listed commands and committing at the end of each task.

- Date: 2026-09-28
- Branch: `feat/issue-145-drop-privilege-dropping` (cut from `origin/feat/p2p-sync-iroh` @ `5780e0c`)
- Issue: sapphire-framework #145 「drop privilege dropping」
- Related: `docs/superpowers/specs/2026-09-16-process-architecture-design.md` §3 (being
  repealed by Task 3); the 2026-09-15 sync/journal specs are historical and are **not** touched.

**Goal:** Remove the privilege-dropping facility entirely (its mechanism, its config types,
its CI job and its docs) **and** remove the system-unit concept from `service install`: after
this branch, installing a service always installs a *user-level* unit — a systemd **user**
unit, a LaunchAgent, or a scheduled task — and nothing else.

**Ruling (agreed with the maintainer, 2026-09-28).** The issue's stated reason: enforcing
per-user permission separation across every syncing node is hard (especially on Windows), a
missed node re-opens the injection hole anyway, and the maintenance cost of per-OS
implementations is not worth a non-root fix. The workspace/`heartbeat` injection risk is
handled by policy instead (shell / fs tools allowed only on admin devices and admin rooms — a
`sapphire-agent` concern, outside this repo). Ruling details:

1. The `Scope { User, System }` enum and the whole scope-decision machinery are **deleted**;
   install is always user-level.
2. The `--user` and `--system` CLI flags are **deleted** (there is no longer a kind to
   choose); `InstallArgs` itself disappears — `ServiceCommand::Install` takes no arguments.
3. `InstallContext.scope` / `.target_user`, `resolve_scope`, `resolve_target_user`,
   `chown_to_user` / `hand_over_to_user` (the `libc` `getpwnam`/`chown` hand-over), the
   `--run-as` flag, `RunAs`, `Error::MissingUser`, `Error::Unsupported`, and the `sudo_user`
   environment fact are all **deleted**. `Environment` keeps only `{ euid, os }` — `euid`
   survives because macOS activation addresses `gui/<uid>` — and `Environment::detect` stops
   reading `SUDO_USER`.
4. `Error::MissingUser` and `Error::Unsupported` go with the machinery that raised them;
   `Error::Io`, `Error::Manager`, `Error::Config` stay.
5. **(a)** The general `post_install` hook machinery — `PostInstall`, `InstallContext`, the
   run-after-activation step — **stays** (it is a generic facility sapphire-sync will use for
   `net.toml`). Only the `--keep-helper` flag, which was the hook's privilege-era gate, is
   **deleted**: after this branch the hook runs on every install, unconditionally.
6. `Environment` final shape: `pub struct Environment { pub euid: u32, pub os: Os }` with
   `Environment::detect()` reading the effective uid and the platform only.

**Deliverables in full:**

- Delete `crates/sapphire-framework-server/src/privilege/` (drop.rs, helper.rs, mod.rs,
  users.rs — ~980 lines), `crates/sapphire-framework-service/src/privilege.rs`,
  `crates/sapphire-framework-server/tests/privilege_root.rs`, the CI `privileged` job,
  golden files `system-privsep.service` and `system-user.service`, and the `libc` dependency
  from both crates' `Cargo.toml` (the privilege code is its only user in both crates).
- Delete the `RunAs` enum, `ServiceSpec.system_run_as` / `.privileges`,
  `AppServer::privileges()` / the `privileges` field, `Error::Privilege` (server), the
  `--run-as` / `--user` / `--system` flags, and the `Scope` machinery per 1–3.
- Docs: rewrite the privilege-separation passages of
  `docs/superpowers/specs/2026-09-16-process-architecture-design.md` and
  `docs/ARCHITECTURE.md`.

**Architecture after the change.** `-service` becomes a crate that renders and installs exactly
one kind of unit per platform: `unit_path` → `~/.config/systemd/user/<app>.service` (Linux),
`~/Library/LaunchAgents/<label>.plist` (macOS), a scheduled task from a temp XML (Windows);
activation is `systemctl --user daemon-reload` + `enable --now`, `launchctl bootstrap
gui/<euid>`, or `schtasks /create`. The whole install flow is a straight line: resolve the path
from the platform and `HOME`/`USERPROFILE`, render, write, activate, run the (always-present)
post-install hook. Every platform's `Environment` still arrives bundled in one value so every
flow stays testable with a `RecordingManager` on one machine.

**Tech stack:** Rust, clap derive, thiserror, libc (removed), systemd/launchctl/schtasks, cargo test.

**Spec:** `docs/superpowers/specs/2026-09-16-process-architecture-design.md` (its §3 is being
repealed by this plan — read it for what is being removed, not what to build).

## Global Constraints

- Commits: conventional commits, imperative subject, one commit per task, matching `git log`
  style (`refactor(service): … (issue #145)`).
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace --all-features` must pass at the end of every task.
- Doc comments are load-bearing in this codebase: where this plan says "update the doc", the
  replacement text is given — never leave a comment describing machinery that no longer exists.
- Do not touch `docs/superpowers/specs/2026-09-15-*.md` (historical) or any
  `docs/superpowers/plans/` file.
- The golden files `tests/golden/user.service`, `tests/golden/launchagent.plist`,
  `tests/golden/task.xml` must come out of this change **byte-identical** (user-level rendering
  does not change; only the code that *also* rendered system units does).

**File map:**

| File | Change |
|---|---|
| `crates/sapphire-framework-server/src/privilege/` | deleted (whole directory) |
| `crates/sapphire-framework-server/src/lib.rs` | drop `pub mod privilege;`, the re-exports, the `privileges` field, the `privileges()` builder, the service_spec field lines |
| `crates/sapphire-framework-server/src/error.rs` | drop `Error::Privilege`; fix the `Error::Service` doc |
| `crates/sapphire-framework-server/src/command.rs` | drop privilege imports/tests/parse case |
| `crates/sapphire-framework-server/tests/privilege_root.rs` | deleted |
| `crates/sapphire-framework-server/Cargo.toml` | drop both `libc` entries (dependency and dev-dependency) |
| `crates/sapphire-framework-service/src/privilege.rs` | deleted |
| `crates/sapphire-framework-service/src/scope.rs` | keep `Environment` (trimmed), `Os`, `ServiceSpec`, `InstallContext`, `PostInstall`; delete `Scope`, `RunAs`, `resolve_scope`, `resolve_target_user` + their tests |
| `crates/sapphire-framework-service/src/error.rs` | drop `Unsupported`, `MissingUser` |
| `crates/sapphire-framework-service/src/manager.rs` | remove scope/target-user/`InstallArgs`/`chown_to_user` machinery; simplify flows and tests |
| `crates/sapphire-framework-service/src/systemd.rs` | one unit kind; scope/target-user rendering gone |
| `crates/sapphire-framework-service/src/launchd.rs`, `windows.rs` | doc references to `resolve_scope` updated |
| `crates/sapphire-framework-service/src/lib.rs` | re-exports and module docs updated |
| `crates/sapphire-framework-service/Cargo.toml` | drop `libc` (keep it as a dev-dependency only if Task 2 Step 3's surviving test keeps `geteuid`) |
| `crates/sapphire-framework-service/tests/golden.rs` | rewritten in Task 2; the two system goldens deleted |
| `crates/sapphire-framework-service/tests/golden/system-privsep.service`, `system-user.service` | deleted |
| `crates/sapphire-framework-bridge/src/command.rs` (+`error.rs`), `apps/sapphire-bridge/src/main.rs` | spec literal + doc + tests updated |
| `.github/workflows/ci.yml` | `privileged` job deleted |
| the two docs files | per Task 3 |

---

## Task 1: the consumers — `-server` and `-bridge` stop consuming privilege separation

Green after this task: the `-service` crate still owns the types, but nothing outside it
constructs or reads them. (The `ServiceSpec` literal keeps its `system_run_as:` / `privileges:`
field lines until Task 2 deletes the fields — keep them verbatim in this task.)

**Files:**
- Delete: `crates/sapphire-framework-server/src/privilege/` (directory)
- Delete: `crates/sapphire-framework-server/tests/privilege_root.rs`
- Modify: `crates/sapphire-framework-server/src/lib.rs` (line ~30 import keeps `RunAs`; 47; 59; 72–76; 141; 182–190; 219–220)
- Modify: `crates/sapphire-framework-server/src/error.rs` (~65–71 delete `Error::Privilege`; 73–76 fix the `Error::Service` doc — drop the `--run-as` mention, keep "Carries the service crate's own message")
- Modify: `crates/sapphire-framework-server/src/command.rs` (delete `privileges_for`, the `the_generated_spec_carries_the_apps_privileges` and `the_generated_spec_runs_a_system_unit_as_the_invoking_user` tests, the `HelperSpec, PrivilegeConfig` and `RunAs` imports; at ~line 612 delete the `["app", "service", "install", "--run-as", "alice"]` parse case)
- Modify: `crates/sapphire-framework-server/Cargo.toml` (delete both `libc` entries, incl. the dev-dependency comment block ~55–62)
- Modify: `crates/sapphire-framework-bridge/src/command.rs` (the doc at ~73–79 and the tests at ~886–903)
- Modify: `crates/sapphire-framework-bridge/src/error.rs` (~44–49: `Error::Service` doc, drop the `--run-as` mention)
- Modify: `.github/workflows/ci.yml` (delete the `privileged` job and its leading comment block)

- [ ] **Step 1: delete the server's privilege module and its root test**

```bash
git rm -r crates/sapphire-framework-server/src/privilege
git rm crates/sapphire-framework-server/tests/privilege_root.rs
```

- [ ] **Step 2: edit `crates/sapphire-framework-server/src/lib.rs`**

Delete `pub mod privilege;` and `pub use privilege::{HelperSpec, PrivilegeConfig, UserSpec};`.
Delete the `privileges: Option<PrivilegeConfig>` field with its three-line doc comment, the
`privileges: None,` initializer in `new()`, and the whole `privileges()` builder (doc + body).
Keep `use sapphire_framework_service::RunAs;` — the `service_spec()` literal keeps
`system_run_as: RunAs::InvokingUser,` and `privileges: self.privileges.clone(),` one task
longer; Task 2 removes both lines together with the fields.

- [ ] **Step 3: edit `crates/sapphire-framework-server/src/command.rs`**

In `mod service_spec_tests`: delete `use crate::privilege::{HelperSpec, PrivilegeConfig};`,
delete `use sapphire_framework_service::RunAs;`, delete the `privileges_for` helper with its
doc, delete the tests `the_generated_spec_carries_the_apps_privileges` and
`the_generated_spec_runs_a_system_unit_as_the_invoking_user`. In
`the_service_subcommands_parse`, delete the `vec!["app", "service", "install", "--run-as",
"alice"]` case (the `--system` case stays until Task 2 — it is parsed by the service crate,
whose flags change there).

- [ ] **Step 4: edit the bridge and CI**

`crates/sapphire-framework-bridge/src/command.rs` — the `bridge_service_spec` doc becomes
(replacing the privilege sentences at lines 73–79):

```rust
/// The service this binary installs.
///
/// The unit runs the bare binary with `serve`, so a service manager starts a bridge and
/// nothing else. The description names `version`, so whoever reads the installed unit can
/// tell which build it starts without inspecting the binary; the frame is what an app's own
/// spec says too.
```

The literal itself keeps `system_run_as` / `privileges` until Task 2. In
`the_bridge_service_spec_runs_the_bridge`, delete the `matches!(spec.system_run_as,
RunAs::InvokingUser)` assertion and the now-unused `RunAs` import; keep the `args` assertion.
In `the_bridge_service_spec_names_the_bridge`, delete
`assert!(spec.privileges.is_none(), "the bridge separates nothing");` and the doc sentence it
rests on ("`privileges` is `None` … no filesystem access to separate" — deleted in Step 4's doc
rewrite). `.github/workflows/ci.yml`: delete the whole `privileged:` job (name, steps and the
comment block above it, ~lines 42–73).

- [ ] **Step 5: run and commit**

```bash
cargo test -p sapphire-framework-server -p sapphire-framework-bridge -p sapphire-framework-service
cargo clippy -p sapphire-framework-server -p sapphire-framework-bridge -p sapphire-framework-service --all-targets -- -D warnings
git add -A && git commit -m "refactor(server,bridge): drop privilege separation from the app server and the bridge CLI (issue #145)"
```

Expected: all pass — the service crate's own tests keep passing because Task 2 has not moved
its types yet; nothing outside it references what was deleted here.

---

## Task 2: the `-service` crate — one unit kind, no privileges, no flags

**Files:** as in the File map for `crates/sapphire-framework-service/`, plus the two
constructor sites losing their field lines:
`crates/sapphire-framework-server/src/lib.rs` `service_spec()` and
`crates/sapphire-framework-bridge/src/command.rs` `bridge_service_spec()` (their docs lose the
privilege/run-as sentences, keeping "the executable's bare invocation is `serve`", the app-name
and version sentences).

**Interfaces:**
- Produces: `ServiceSpec { app_name, description, args, post_install }`;
  `InstallContext { unit_path: PathBuf, exe: PathBuf }`;
  `Environment { euid: u32, os: Os }` with `Environment::detect()`;
  `ServiceCommand::{Install, Uninstall, Status}` (Install carries no arguments);
  `ServiceManager` without `chown_to_user`; `unit_path(app_name: &str, home: &Path) -> PathBuf`;
  `activation(app_name: &str) -> Vec<Vec<String>>`; `linger_hint() -> Option<String>`;
  `render_unit` / `render_launch_agent` / `render_task` keep `(spec, ctx)`.

- [ ] **Step 1: delete `src/privilege.rs` and its exports**

```bash
git rm crates/sapphire-framework-service/src/privilege.rs
```

Delete `pub use privilege::{HelperSpec, PrivilegeConfig, UserSpec};` from `src/lib.rs` and the
`pub mod privilege;` line. If any surviving test needs `geteuid` (Step 3's does), move `libc`
from `[dependencies]` (line 21) to `[dev-dependencies]`; if none does, delete it.

- [ ] **Step 2: rewrite the golden tests first, delete the two system goldens**

```bash
git rm crates/sapphire-framework-service/tests/golden/system-privsep.service \
       crates/sapphire-framework-service/tests/golden/system-user.service
```

Replace `crates/sapphire-framework-service/tests/golden.rs` wholesale:

```rust
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
    assert_eq!(
        rendered.trim_end(),
        want.trim_end(),
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
```

This fails to compile until Step 5 lands — Steps 2–6 land as one commit.

- [ ] **Step 3: `src/scope.rs` — keep only what a single-kind install needs**

Delete `Scope`, `RunAs`, `resolve_scope`, `resolve_target_user`, the `Environment.sudo_user`
field and the `SUDO_USER` read in `detect()` (and its doc sentence), and every scope/target-user
test from `mod tests` (`a_regular_user_gets_a_user_unit` through
`root_without_sudo_user_is_refused_with_an_explanation`, including the `spec(run_as)` helper and
the `linux(euid, sudo_user)` helper's second parameter). The module doc's first paragraph
becomes:

```rust
//! What an application wants installed, and the facts about the machine an install needs.
//!
//! Every rule here turns on the effective uid and the platform; they arrive bundled in an
//! [`Environment`] so all combinations are testable on one machine, and so the rules are
//! functions of values rather than of the world they run in.
```

The surviving shapes:

```rust
/// What an application wants installed. (doc as before, minus the scope sentences)
pub struct ServiceSpec {
    pub app_name: &'static str,
    pub description: String,
    pub args: Vec<String>,
    /// Runs after activation, with the resolved context.
    pub post_install: Option<PostInstall>,
}
// impl Debug: same shape, minus the two deleted field lines.

/// A hook that runs after activation, with everything the install resolved.
pub type PostInstall = Box<dyn Fn(&InstallContext) -> Result<()> + Send + Sync>;

/// Everything an install resolved, handed to [`PostInstall`].
#[derive(Clone, Debug)]
pub struct InstallContext {
    /// Where the unit was written.
    pub unit_path: std::path::PathBuf,
    /// The absolute path of the running executable, the unit's `ExecStart` head.
    pub exe: std::path::PathBuf,
}

/// Every environment fact an install needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    pub euid: u32,
    pub os: Os,
}
```

`Os`, `Os::current()` and `effective_uid()` stay verbatim (keep the "not zero: there is no root
to report" doc — the euid still identifies the `gui/<uid>` session). The surviving tests:

```rust
#[test]
fn a_detected_environment_describes_this_machine() {
    let env = Environment::detect();
    assert_eq!(env.os, Os::current(), "the platform is this build's own");
    #[cfg(unix)]
    assert_eq!(
        env.euid,
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() },
        "the effective uid is this process's own"
    );
}

#[test]
fn a_detected_environment_is_not_root_where_there_is_no_root() {
    // A platform without an effective uid must not report one that reads as root.
    #[cfg(not(unix))]
    assert_ne!(Environment::detect().euid, 0);
}
```

Update the module docs of `launchd.rs` (drop the `resolve_scope` sentence; keep the first
paragraph) and `windows.rs` (the `resolve_scope` sentence becomes: "Real Windows services (and
LaunchDaemons) are per-machine services this crate does not install; it installs the per-user
kind everywhere — the counterpart of a systemd user unit.").

- [ ] **Step 4: `src/error.rs`** — delete `Unsupported` and `MissingUser` with their docs; keep
  `Io`, `Manager`, `Config`.

- [ ] **Step 5: `src/systemd.rs` — one kind of unit**

```rust
/// The unit file an install writes: under the invoking user's own `~/.config`, so it is
/// theirs to remove.
pub fn unit_path(app_name: &str, home: &Path) -> PathBuf {
    home.join(".config/systemd/user")
        .join(format!("{app_name}.service"))
}

/// The commands that activate an installed unit: `daemon-reload` first, so the manager sees
/// the file just written; then `enable --now`, which both registers the unit for future boots
/// and starts it now. A user unit goes through the invoking user's own manager.
pub fn activation(app_name: &str) -> Vec<Vec<String>> {
    vec![
        words(&["systemctl", "--user", "daemon-reload"]),
        words(&["systemctl", "--user", "enable", "--now", app_name]),
    ]
}

/// The advice an install should print for a machine meant to run the service without a login.
///
/// A user unit dies with its session; `loginctl enable-linger` lets it survive one.
pub fn linger_hint() -> Option<String> {
    Some("to run without a login, run: loginctl enable-linger".to_owned())
}
```

Move `words` (currently private in `manager.rs`) into `systemd.rs` as a `pub(crate)` helper —
`manager.rs` already needs it for launchctl/schtasks words — or give `activation` an inline
equivalent; keep exactly one copy. `render_unit`: delete the `scope` match and both the
network-ordering block and the "No `User=`" comment block; `[Install]` is always
`WantedBy=default.target`; module doc becomes "Rendering and activating systemd **user**
units, …" and the "three answers" sentence loses its second member. Module doc/tests: keep
`a_user_unit_lives_under_the_users_own_config` (call `unit_path("sapphire-bridge",
Path::new("/home/alice"))`), keep the activation test renamed `a_unit_activates_through_the_user_manager`,
keep `a_user_install_hints_at_linger` (call `linger_hint()` with no arguments); delete
`a_system_unit_lives_in_etc`, `a_system_unit_activates_without_the_user_flag`,
`a_linger_hint_for_a_named_user_uses_sudo`, `a_system_install_has_no_linger_hint`.

- [ ] **Step 6: `src/manager.rs` — the flows in one line each**

Delete: `chown_to_user` (trait default + `SystemManager` impl), both `hand_over_to_user` fns
and their doc, `InstallArgs` and `requested_scope`, the `RunAs` import, `systemctl_command`
(replaced by `words` + `systemd::activation`). `ServiceCommand::Install(InstallArgs)` becomes
`ServiceCommand::Install` (doc unchanged). `run(&self, spec, env, manager)` keeps its shape;
the linger branch becomes:

```rust
if env.os == Os::Linux
    && let Some(hint) = linger_hint()
{
    println!("{hint}");
}
```

`install(spec, env, manager)` becomes (module doc: drop the scope/target-user sentences, keep
the RecordingManager and cleanup-on-failure paragraphs):

```rust
pub fn install(spec: &ServiceSpec, env: &Environment, manager: &dyn ServiceManager) -> Result<InstallContext> {
    let context = InstallContext {
        unit_path: install_path(spec, env)?,
        exe: std::env::current_exe()?,
    };
    let body = render_install(spec, &context, env.os);
    manager.write_unit(&context.unit_path, &body)?;
    for command in install_commands(spec, &context, env) {
        if let Err(error) = manager.run(&command) {
            if let Err(cleanup) = manager.remove_unit(&context.unit_path) {
                return Err(Error::Manager(format!(
                    "{error}; the partially written unit at {} could not be removed either \
                     ({cleanup})",
                    context.unit_path.display()
                )));
            }
            return Err(error);
        }
    }
    if env.os == Os::Windows {
        // The XML was a hand-over: `/create` copies the task into the scheduler, and the
        // copy left under the temporary directory has done its job.
        let _ = manager.remove_unit(&context.unit_path);
    }
    // The hook runs last, on an installed, running service, with what the install resolved.
    if let Some(hook) = spec.post_install.as_ref()
        && let Err(error) = hook(&context)
    {
        return Err(Error::Manager(format!(
            "{error}; the service is installed and running, so fix what the hook needs \
             and rerun the uninstall and install of your choice"
        )));
    }
    Ok(context)
}
```

`install_path` (doc: "Where each platform keeps the file an install writes: the invoking
user's own directory; a Windows task's hand-over lives in the temporary directory."):

```rust
fn install_path(spec: &ServiceSpec, env: &Environment) -> Result<PathBuf> {
    match env.os {
        Os::Linux => Ok(unit_path(spec.app_name, &home_dir(env)?)),
        Os::MacOs => Ok(agent_path(spec.app_name, &home_dir(env)?)),
        Os::Windows => Ok(std::env::temp_dir().join(format!("{}-task.xml", spec.app_name))),
    }
}
```

`install_commands`: the Linux arm becomes `activation(spec.app_name)`; the macOS/Windows arms
stay (they keep `env.euid` for `gui/<uid>`). `uninstall_commands` Linux arm:
`[words(&["systemctl", "--user", "stop", spec.app_name]), words(&["systemctl", "--user",
"disable", spec.app_name])]`. `status_command` Linux arm:
`words(&["systemctl", "--user", "--no-pager", "status", spec.app_name])`. `uninstall` and
`status` lose their `args` parameter and everything that used it.

`home_dir`'s doc: drop the system-unit sentence; the function itself is unchanged.

Tests in `mod tests`: delete `privileges_for`, `linux_root`, `a_system_install_does_not`,
`asking_for_both_scopes_is_refused`, `post_install_sees_the_resolved_target_user`,
`keep_helper_skips_post_install`,
`a_privilege_separated_spec_installs_as_a_root_unit_whatever_run_as_says`,
`a_system_install_reads_no_home_directory`. Keep the rest; the environment helpers become
`fn linux() -> Environment { Environment { euid: 1000, os: Os::Linux } }` (same shape for
`macos()`/`windows()`, dropping the `sudo_user` field), and
`a_user_install_uses_the_user_flag` keeps its name and now asserts that the single install
kind goes through the user manager (`--user` in every command). `spec()` builds the three-field
`ServiceSpec`. `post_install_runs_after_activation`,
`a_failing_post_install_fails_the_install_and_says_what_was_done`,
`a_failing_activation_leaves_no_unit_behind`, both uninstall tests,
`status_reports_what_the_manager_said`, the macOS/Windows install tests and
`no_test_touches_the_real_service_manager` (keep its self-counting comment byte-identical —
the "SystemManager" count drops to 2: definition + impl, which the assertion already allows).

- [ ] **Step 7: `src/lib.rs` (service crate)** — re-exports become:

```rust
pub use error::{Error, Result};
pub use launchd::{agent_path, label, render_launch_agent};
pub use manager::{
    Calls, RecordingManager, ServiceCommand, ServiceManager, SystemManager, install, status,
    uninstall,
};
pub use scope::{Environment, InstallContext, Os, PostInstall, ServiceSpec};
pub use systemd::{activation, linger_hint, render_unit, unit_path};
pub use windows::render_task;
```

Module doc: replace the two-row decision table with the single user-level row (per platform);
delete the `RunAs`/`--run-as` paragraph and the paragraph about hosting the privilege types (the
dependency-direction sentence moves into the manager paragraph or is dropped with them).

- [ ] **Step 8: constructor sites** — delete the `system_run_as: RunAs::InvokingUser,` and
  `privileges: …` lines from `crates/sapphire-framework-server/src/lib.rs::service_spec()` and
  `crates/sapphire-framework-bridge/src/command.rs::bridge_service_spec()`; drop the `RunAs`
  import where unused (server: `use sapphire_framework_service::ServiceSpec;`); update both
  doc comments per Task 1 Step 4's wording (the server's keeps "a service manager starts the
  executable directly, and the executable's bare invocation is `serve`").

- [ ] **Step 9: run and commit**

```bash
cargo test -p sapphire-framework-service -p sapphire-framework-server -p sapphire-framework-bridge --all-features
cargo clippy -p sapphire-framework-service -p sapphire-framework-server -p sapphire-framework-bridge --all-targets -- -D warnings
git add -A && git commit -m "refactor(service): one per-user unit kind — scope, run-as and privilege machinery removed (issue #145)"
```

Expected: all green; the three surviving golden files byte-identical.

---

## Task 3: the two docs, and the whole-workspace gate

**Files:** `docs/superpowers/specs/2026-09-16-process-architecture-design.md`,
`docs/ARCHITECTURE.md`.

- [ ] **Step 1: the process-architecture spec**
  - Decision 9 (~line 73): replace with: **"Privilege separation was removed** (`sapphire-agent`
    #257 withdrawn). Per-node permission separation had to be enforced on every syncing node —
    hardest on Windows — and one node missing it re-opened the injection hole, so the mechanism
    paid per-OS maintenance for partial protection. Shell / generic fs tools are restricted by
    policy instead: admin devices and admin rooms only (a `sapphire-agent` concern).**"
  - ~line 222: delete the "**Start-on-demand is disabled for apps configured with privilege
    separation**" paragraph.
  - §3 (~227–287): replace the body with a short section marked
    `## 3. Privilege separation (removed — issue #145)`: the removal reason (decision 9); and
    which of the two §3.2 consequences survive — **the 0700/0600 file-mode rule stands** (it
    protects the workspace whatever runs the server), and **`ServiceSpec` carries no `run_as` /
    `helper_as` anymore** — every install is a per-user unit and the app runs as the user who
    installed it. The startup-sequence table and steps 1–7 are deleted, not marked historical:
    §3.1's machinery no longer exists anywhere.
  - §4: delete the `.privileges(privilege_config)` line from the `AppServer` snippet.
  - §9: step 5 becomes "Privilege separation — removed (issue #145)"; step 10 loses
    "run_as / helper_as" (just "Service"); the crate table's `-server` row loses "privilege
    separation," and the `-service` row becomes "`service install` — one per-user unit per
    platform".
  - The supersession table (step 11 row): "still true, extended with §3's `run_as` /
    `helper_as`" → "still true; the §3 extension (`run_as` / `helper_as`) was itself removed
    (issue #145)".
  - §10: delete the "Helper exits unexpectedly (§3)" row; in the "App server not running" row
    delete the "(or, under §3, …)" and "except for a privilege-separated server…" clauses —
    start-on-demand and the bridge's wake behavior stay.
  - §11: delete the "**Privilege separation**: needs root…" paragraph.
  - Constraints (~line 544): delete item 3 ("Privilege separation is Unix-only…" — renumber).

- [ ] **Step 2: `docs/ARCHITECTURE.md`**
  - Table row for `sapphire-framework-service` (line ~101) becomes:
    「OS のサービスマネージャへの登録（`ServiceSpec`・systemd user unit・LaunchAgent・タスクスケジューラ）」.
  - The 「特権分離（Unix のみ）」 section (~line 234): replace the whole section with:
    「特権分離は **撤去済み**（issue #145）。全ノード同一の権限管理は Windows 等で困難で、1ノードの
    漏れが穴を再び開くため、機構ではなく運用（shell / fs ツールは管理者デバイス・管理者ルームのみ許可）で
    対応する。`service install` は常にユーザーレベルの unit をインストールし、アプリはインストールした
    ユーザーとして走る。framework の作るファイルとディレクトリはすべて `0700` / `0600`（これは維持）。
    詳細はプロセス構成仕様 §3。」
  - The progress bullet (~line 290): 「サーバ機能（組込み relay・`wake_on_sync`）、`-service`
    （`run_as` / `helper_as`）」→「サーバ機能（組込み relay・`wake_on_sync`）、`-service`
    （特権分離は撤去済み — issue #145）」.

- [ ] **Step 3: gates and commit**

```bash
cargo test --workspace --all-features
cargo clippy --workspace --all-targets -- -D warnings
grep -rn "privilege\|run_as\|run-as\|keep.helper\|keep_helper" crates apps --include="*.rs" | grep -v target
# expected: no hits (the mechanism is gone from every crate)
git add -A && git commit -m "docs: privilege separation and the system unit are withdrawn (issue #145)"
```

## Self-review notes (written with the plan)

- Spec coverage: §3/decision 9 (Task 3), the `-service` machinery and both flag pairs
  (Task 2), `-server`/`-bridge` consumption + CI + root tests (Task 1), docs (Task 3). The
  `--keep-helper` flag itself is deleted in Task 2 Step 6 (task item "keep_helper" test
  deletion + the flag's deletion with `InstallArgs`); the `post_install` machinery stays per
  ruling 5.
- Type consistency: `InstallContext { unit_path, exe }` is used identically in Tasks 2's
  manager, systemd, launchd and golden-test snippets; `Environment { euid, os }` in Tasks 1–2's
  tests matches the struct in Step 3; `activation(app_name)` one-arg signature matches its use
  in Step 6.
- No placeholder steps: every deletion names the exact symbol; every surviving replacement is
  given verbatim.
