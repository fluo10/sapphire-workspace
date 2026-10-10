//! The `embedding` subcommands, shared by the bridge's CLI and every app's.
//!
//! Both CLIs parse the same words into an [`EmbedRequest`]; what they do with it differs
//! (the bridge's CLI can write the files itself when no bridge runs), so only the parsing,
//! the key prompt and the printing live here.

use std::io::{BufRead, IsTerminal};

use crate::{
    ApiKey, DEFAULT_DIMENSION, DEFAULT_MAX_TOKENS, EmbedDeviceSetParams, EmbedModelSetParams,
    EmbedRequest, LOCAL_MODEL, LocalModel, RemoteModel, Slot, SlotModel,
};

/// Show and change the embedding settings: a local and a remote model for the workgroup,
/// each switched per device, and this device's API key.
#[derive(Debug, clap::Subcommand)]
pub enum EmbeddingCommand {
    /// Show the settings, where they come from, and what this device does with them.
    Show,
    /// The local model, computed on each device's CPU.
    #[command(subcommand)]
    Local(LocalCommand),
    /// The remote model, an OpenAI-compatible endpoint.
    #[command(subcommand)]
    Remote(RemoteCommand),
    /// The remote model's API key on this device (never synced).
    #[command(subcommand)]
    Key(KeyCommand),
}

/// `embedding local …`.
#[derive(Debug, clap::Subcommand)]
pub enum LocalCommand {
    /// Configure the local model for the workgroup (options not given take their defaults).
    Set {
        /// Output dimension (64-2048).
        #[arg(long, default_value_t = DEFAULT_DIMENSION)]
        dimension: u32,
        /// Content tokens kept before embedding (1-8192).
        #[arg(long, default_value_t = DEFAULT_MAX_TOKENS)]
        max_tokens: usize,
    },
    /// Remove the local model from the workgroup.
    Clear,
    /// Switch the local model on this device.
    Device {
        /// `auto` is off on a CPU without AVX2, on otherwise.
        switch: Switch,
    },
}

/// `embedding remote …`.
#[derive(Debug, clap::Subcommand)]
pub enum RemoteCommand {
    /// Configure the remote model for the workgroup.
    Set {
        /// The endpoint's base URL (requests go to `<endpoint>/v1/embeddings`).
        #[arg(long)]
        endpoint: String,
        /// The model's name, as the endpoint knows it.
        #[arg(long)]
        model: String,
        /// Output dimension; longer vectors are cut to it.
        #[arg(long)]
        dimension: u32,
    },
    /// Remove the remote model from the workgroup.
    Clear,
    /// Switch the remote model on this device.
    Device {
        /// `auto` is on.
        switch: Switch,
    },
}

/// `embedding key …`.
#[derive(Debug, clap::Subcommand)]
pub enum KeyCommand {
    /// Store the key: read from standard input, or asked for without echo on a terminal.
    Set,
    /// Remove the key.
    Clear,
}

/// A slot's switch on this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Switch {
    /// Decided by this device: the local model needs AVX2, the remote one is on.
    Auto,
    /// On.
    On,
    /// Off.
    Off,
}

impl Switch {
    fn enabled(self) -> Option<bool> {
        match self {
            Switch::Auto => None,
            Switch::On => Some(true),
            Switch::Off => Some(false),
        }
    }
}

impl EmbeddingCommand {
    /// The request these words make. `key set` reads the key here, through [`read_key`].
    pub fn request(self) -> std::io::Result<EmbedRequest> {
        let model_set = |slot, model| EmbedRequest::ModelSet(EmbedModelSetParams { slot, model });
        let device_set = |slot, switch: Switch| {
            EmbedRequest::DeviceSet(EmbedDeviceSetParams {
                slot,
                enabled: switch.enabled(),
            })
        };
        Ok(match self {
            EmbeddingCommand::Show => EmbedRequest::Show,
            EmbeddingCommand::Local(LocalCommand::Set {
                dimension,
                max_tokens,
            }) => model_set(
                Slot::Local,
                Some(SlotModel::Local(LocalModel {
                    model: LOCAL_MODEL.to_owned(),
                    dimension,
                    max_tokens,
                })),
            ),
            EmbeddingCommand::Local(LocalCommand::Clear) => model_set(Slot::Local, None),
            EmbeddingCommand::Local(LocalCommand::Device { switch }) => {
                device_set(Slot::Local, switch)
            }
            EmbeddingCommand::Remote(RemoteCommand::Set {
                endpoint,
                model,
                dimension,
            }) => model_set(
                Slot::Remote,
                Some(SlotModel::Remote(RemoteModel {
                    endpoint,
                    model,
                    dimension,
                })),
            ),
            EmbeddingCommand::Remote(RemoteCommand::Clear) => model_set(Slot::Remote, None),
            EmbeddingCommand::Remote(RemoteCommand::Device { switch }) => {
                device_set(Slot::Remote, switch)
            }
            EmbeddingCommand::Key(KeyCommand::Set) => EmbedRequest::KeySet(read_key()?),
            EmbeddingCommand::Key(KeyCommand::Clear) => EmbedRequest::KeyClear,
        })
    }
}

/// Read an API key: without echo on a terminal, else one line of standard input.
///
/// Never from an argument, which would leave the key in the shell's history and in the
/// process list.
pub fn read_key() -> std::io::Result<ApiKey> {
    let key = if std::io::stdin().is_terminal() {
        ApiKey::new(rpassword::prompt_password("API key: ")?)
    } else {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        ApiKey::new(line)
    };
    if key.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no key was given",
        ));
    }
    Ok(key)
}

/// Add, list, retire, restore and rotate the workgroup's external devices: clients that
/// reach its apps with a key instead of syncing.
#[derive(Debug, clap::Subcommand)]
pub enum ExternalDeviceCommand {
    /// List the external devices.
    List,
    /// Add one, and print its token — the only time it is shown.
    Add {
        /// Its name.
        name: String,
        /// An application it may use; repeat for several.
        #[arg(long = "app")]
        apps: Vec<String>,
        /// A note.
        #[arg(long)]
        description: Option<String>,
    },
    /// Retire one: its token stops working; the record stays.
    Retire {
        /// Its name or id.
        selector: String,
    },
    /// Bring a retired one back, with the token it had.
    Restore {
        /// Its name or id.
        selector: String,
    },
    /// Give one a new token, keeping its id; the old token stops working at once.
    Rotate {
        /// Its name or id.
        selector: String,
    },
    /// Set the applications one may use (none: it may use nothing).
    Apps {
        /// Its name or id.
        selector: String,
        /// The applications.
        apps: Vec<String>,
    },
}

impl ExternalDeviceCommand {
    /// The request these words make. An `add` that names no application gets
    /// `default_app` — the application whose CLI ran it — when there is one.
    pub fn request(self, default_app: Option<&str>) -> crate::ExternalDeviceRequest {
        use crate::{ExternalDeviceAddParams, ExternalDeviceRequest, ExternalDeviceSetAppsParams};
        match self {
            ExternalDeviceCommand::List => ExternalDeviceRequest::List,
            ExternalDeviceCommand::Add {
                name,
                mut apps,
                description,
            } => {
                if apps.is_empty()
                    && let Some(app) = default_app
                {
                    apps.push(app.to_owned());
                }
                ExternalDeviceRequest::Add(ExternalDeviceAddParams {
                    name,
                    description,
                    apps,
                })
            }
            ExternalDeviceCommand::Retire { selector } => ExternalDeviceRequest::Retire(selector),
            ExternalDeviceCommand::Restore { selector } => ExternalDeviceRequest::Restore(selector),
            ExternalDeviceCommand::Rotate { selector } => ExternalDeviceRequest::Rotate(selector),
            ExternalDeviceCommand::Apps { selector, apps } => {
                ExternalDeviceRequest::SetApps(ExternalDeviceSetAppsParams { selector, apps })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: EmbeddingCommand,
    }

    fn parse(args: &[&str]) -> EmbedRequest {
        let mut argv = vec!["x"];
        argv.extend_from_slice(args);
        Cli::parse_from(argv).command.request().unwrap()
    }

    #[test]
    fn local_set_fills_in_the_defaults() {
        let EmbedRequest::ModelSet(p) = parse(&["local", "set", "--dimension", "512"]) else {
            panic!("not a model set");
        };
        assert_eq!(p.slot, Slot::Local);
        assert_eq!(
            p.model,
            Some(SlotModel::Local(LocalModel {
                dimension: 512,
                ..LocalModel::default()
            }))
        );
    }

    #[test]
    fn remote_set_needs_every_field() {
        let argv = [
            "x",
            "remote",
            "set",
            "--endpoint",
            "https://e",
            "--model",
            "m",
        ];
        assert!(Cli::try_parse_from(argv).is_err());
    }

    #[test]
    fn device_switches_map_to_options() {
        let EmbedRequest::DeviceSet(p) = parse(&["remote", "device", "off"]) else {
            panic!("not a device set");
        };
        assert_eq!((p.slot, p.enabled), (Slot::Remote, Some(false)));
        let EmbedRequest::DeviceSet(p) = parse(&["local", "device", "auto"]) else {
            panic!("not a device set");
        };
        assert_eq!((p.slot, p.enabled), (Slot::Local, None));
    }

    #[test]
    fn clear_is_a_model_set_without_a_model() {
        let EmbedRequest::ModelSet(p) = parse(&["remote", "clear"]) else {
            panic!("not a model set");
        };
        assert_eq!((p.slot, p.model), (Slot::Remote, None));
    }

    #[derive(clap::Parser)]
    struct XCli {
        #[command(subcommand)]
        command: ExternalDeviceCommand,
    }

    #[test]
    fn add_defaults_to_the_running_app_and_takes_several() {
        let parse = |args: &[&str], app| {
            let mut argv = vec!["x"];
            argv.extend_from_slice(args);
            XCli::parse_from(argv).command.request(app)
        };
        let crate::ExternalDeviceRequest::Add(p) = parse(&["add", "pendant"], Some("agent")) else {
            panic!("not an add")
        };
        assert_eq!(p.apps, vec!["agent"]);
        let crate::ExternalDeviceRequest::Add(p) =
            parse(&["add", "hook", "--app", "a", "--app", "b"], Some("agent"))
        else {
            panic!("not an add")
        };
        assert_eq!(p.apps, vec!["a", "b"], "named apps replace the default");
    }
}
