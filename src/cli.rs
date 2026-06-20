use std::{path::PathBuf, str::FromStr};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::config::{
    layer::{ConfigLayer, LayerAutoPorts, LayerPort},
    ports::{PortSpecSegments, split_port_spec},
    types::{DEFAULT_PORT_HOST_IP, PortProtocol},
};
use crate::down::{CleanOptions, DownOptions};
use crate::up::{UpOptions, run_attached_up, run_detached_up};

#[derive(Debug, Parser)]
#[command(
    name = "decune",
    version,
    about = "Run dev containers from the command line."
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create or start a dev container and attach to a shell.
    Up(UpArgs),
    /// Recreate a dev container.
    Rebuild(RebuildArgs),
    /// Stop a managed dev container.
    Down(DownArgs),
    /// Remove managed dev container resources.
    Clean(CleanArgs),
}

#[derive(Debug, Args)]
struct UpArgs {
    /// Devcontainer metadata file.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Start the container without attaching a shell.
    #[arg(long)]
    detach: bool,
    /// Recreate an existing container while preserving managed volumes.
    #[arg(long)]
    rebuild: bool,
    /// Do not use build cache for Dockerfile or Feature layers.
    #[arg(long)]
    no_cache: bool,
    /// Pull the base image before create or build.
    #[arg(long)]
    pull: bool,
    /// Disable automatic port forwarding.
    #[arg(long)]
    no_auto_forward: bool,
    /// Add a manual port forwarding rule.
    #[arg(short = 'p', long = "port", value_name = "SPEC")]
    ports: Vec<ManualPort>,
    /// Workspace directory.
    #[arg(default_value = ".", value_name = "WORKSPACE")]
    workspace: PathBuf,
}

#[derive(Debug, Args)]
struct RebuildArgs {
    /// Devcontainer metadata file.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Start the container without attaching a shell.
    #[arg(long)]
    detach: bool,
    /// Do not use build cache for Dockerfile or Feature layers.
    #[arg(long)]
    no_cache: bool,
    /// Pull the base image before create or build.
    #[arg(long)]
    pull: bool,
    /// Resolve Feature references without using the lock file.
    #[arg(long)]
    update_features: bool,
    /// Disable automatic port forwarding.
    #[arg(long)]
    no_auto_forward: bool,
    /// Add a manual port forwarding rule.
    #[arg(short = 'p', long = "port", value_name = "SPEC")]
    ports: Vec<ManualPort>,
    /// Workspace directory.
    #[arg(default_value = ".", value_name = "WORKSPACE")]
    workspace: PathBuf,
}

#[derive(Debug, Args)]
struct DownArgs {
    /// Graceful stop timeout in seconds.
    #[arg(long, default_value_t = 10, value_name = "SECONDS")]
    timeout: u64,
    /// Workspace directory.
    #[arg(default_value = ".", value_name = "WORKSPACE")]
    workspace: PathBuf,
}

#[derive(Debug, Args)]
struct CleanArgs {
    /// Remove decune generated workspace images.
    #[arg(long)]
    images: bool,
    /// Remove resources without confirmation.
    #[arg(long)]
    force: bool,
    /// Workspace directory.
    #[arg(default_value = ".", value_name = "WORKSPACE")]
    workspace: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManualPort {
    container: u16,
    host: Option<u16>,
    host_ip: String,
    protocol: PortProtocol,
}

impl FromStr for ManualPort {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        parse_manual_port(value)
    }
}

pub(crate) async fn run() -> Result<i32> {
    let cli = Cli::parse();
    run_cli(cli).await
}

async fn run_cli(cli: Cli) -> Result<i32> {
    match cli.command {
        Commands::Up(args) => run_up(args).await,
        Commands::Rebuild(args) => run_rebuild(args).await,
        Commands::Down(args) => run_down(args).await,
        Commands::Clean(args) => run_clean(args).await,
    }
}

async fn run_up(args: UpArgs) -> Result<i32> {
    let UpArgs {
        config,
        detach,
        rebuild,
        no_cache,
        pull,
        no_auto_forward,
        ports,
        workspace,
    } = args;

    reject_detached_cli_ports(detach, &ports)?;

    let options = UpOptions {
        workspace,
        config_path: config,
        cli_layer: cli_config_layer(ports, no_auto_forward),
        pull,
        rebuild,
        no_cache,
        update_features: false,
    };

    if detach {
        run_detached_up(options).await?;
        return Ok(0);
    }

    run_attached_up(options).await
}

async fn run_rebuild(args: RebuildArgs) -> Result<i32> {
    let detach = args.detach;
    let options = rebuild_up_options_from_args(args)?;

    if detach {
        run_detached_up(options).await?;
        return Ok(0);
    }

    run_attached_up(options).await
}

fn rebuild_up_options_from_args(args: RebuildArgs) -> Result<UpOptions> {
    let RebuildArgs {
        config,
        detach,
        no_cache,
        pull,
        update_features,
        no_auto_forward,
        ports,
        workspace,
    } = args;

    reject_detached_cli_ports(detach, &ports)?;

    Ok(UpOptions {
        workspace,
        config_path: config,
        cli_layer: cli_config_layer(ports, no_auto_forward),
        pull,
        rebuild: true,
        no_cache,
        update_features,
    })
}

async fn run_down(args: DownArgs) -> Result<i32> {
    let DownArgs { timeout, workspace } = args;

    crate::down::run_down(DownOptions {
        workspace,
        timeout_seconds: timeout,
    })
    .await?;
    Ok(0)
}

async fn run_clean(args: CleanArgs) -> Result<i32> {
    let CleanArgs {
        images,
        force,
        workspace,
    } = args;

    crate::down::run_clean(CleanOptions {
        workspace,
        images,
        force,
    })
    .await?;
    Ok(0)
}

fn reject_detached_cli_ports(detach: bool, ports: &[ManualPort]) -> Result<()> {
    if detach && !ports.is_empty() {
        bail!(
            "Port forwarding is not supported with --detach; use appPort for detached publishing"
        );
    }

    Ok(())
}

fn cli_config_layer(ports: Vec<ManualPort>, no_auto_forward: bool) -> ConfigLayer {
    ConfigLayer {
        ports: ports.into_iter().map(ManualPort::into_layer_port).collect(),
        auto_ports: no_auto_forward.then(|| LayerAutoPorts {
            enabled: Some(false),
            ..LayerAutoPorts::default()
        }),
        ..ConfigLayer::default()
    }
}

fn parse_manual_port(value: &str) -> std::result::Result<ManualPort, String> {
    let (port, protocol) = parse_port_protocol(value)?;

    match split_port_spec(port)? {
        PortSpecSegments::One { container } => Ok(ManualPort {
            container: parse_u16_port(container, "container port", value)?,
            host: None,
            host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
            protocol,
        }),
        PortSpecSegments::Two {
            left: host,
            container,
            bracketed_host_ip: false,
        } if is_numeric_port_candidate(host) => Ok(ManualPort {
            container: parse_u16_port(container, "container port", value)?,
            host: Some(parse_u16_port(host, "host port", value)?),
            host_ip: DEFAULT_PORT_HOST_IP.to_owned(),
            protocol,
        }),
        PortSpecSegments::Two {
            left: host_ip,
            container,
            ..
        } => Ok(ManualPort {
            container: parse_u16_port(container, "container port", value)?,
            host: None,
            host_ip: normalize_host_ip(host_ip)?,
            protocol,
        }),
        PortSpecSegments::Three {
            host_ip,
            host,
            container,
            ..
        } => Ok(ManualPort {
            container: parse_u16_port(container, "container port", value)?,
            host: Some(parse_u16_port(host, "host port", value)?),
            host_ip: normalize_host_ip(host_ip)?,
            protocol,
        }),
    }
}

impl ManualPort {
    fn into_layer_port(self) -> LayerPort {
        LayerPort {
            enabled: true,
            service: None,
            container: self.container,
            host: self.host,
            host_ip: self.host_ip,
            protocol: self.protocol,
            require_local: false,
            label: None,
        }
    }
}

fn parse_port_protocol(value: &str) -> std::result::Result<(&str, PortProtocol), String> {
    match value.split_once('/') {
        None => Ok((value, PortProtocol::Tcp)),
        Some((port, "tcp")) => Ok((port, PortProtocol::Tcp)),
        Some((_, protocol)) => Err(format!(
            "unsupported manual port protocol: {protocol}. decune v0.1 supports tcp only"
        )),
    }
}

fn parse_u16_port(value: &str, label: &str, original: &str) -> std::result::Result<u16, String> {
    value
        .parse()
        .map_err(|error| format!("invalid {label} in manual port {original}: {error}"))
}

fn is_numeric_port_candidate(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|character| character.is_ascii_digit())
}

fn normalize_host_ip(value: &str) -> std::result::Result<String, String> {
    match value {
        "" => Err("manual port host IP must not be empty".to_owned()),
        "localhost" => Ok(DEFAULT_PORT_HOST_IP.to_owned()),
        value => Ok(value.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::{CommandFactory, Parser};

    use super::Cli;
    use super::{
        Commands, PortProtocol, cli_config_layer, rebuild_up_options_from_args,
        reject_detached_cli_ports,
    };

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_up_options() {
        let cli = Cli::parse_from([
            "decune",
            "up",
            "--config",
            ".devcontainer/rust/devcontainer.json",
            "--detach",
            "--rebuild",
            "--no-cache",
            "--pull",
            "--no-auto-forward",
            "-p",
            "3000",
            "--port",
            "127.0.0.1:8080:3000",
            "workspace",
        ]);

        let Commands::Up(args) = cli.command else {
            panic!("expected up command");
        };

        assert_eq!(args.workspace, PathBuf::from("workspace"));
        assert_eq!(
            args.config.as_deref(),
            Some(PathBuf::from(".devcontainer/rust/devcontainer.json").as_path())
        );
        assert!(args.detach);
        assert!(args.rebuild);
        assert!(args.no_cache);
        assert!(args.pull);
        assert!(args.no_auto_forward);
        assert_eq!(args.ports.len(), 2);
        assert_eq!(args.ports[0].container, 3000);
        assert_eq!(args.ports[0].host, None);
        assert_eq!(args.ports[0].host_ip, "127.0.0.1");
        assert_eq!(args.ports[0].protocol, PortProtocol::Tcp);
        assert_eq!(args.ports[1].container, 3000);
        assert_eq!(args.ports[1].host, Some(8080));
        assert_eq!(args.ports[1].host_ip, "127.0.0.1");

        let cli_layer = cli_config_layer(args.ports.clone(), args.no_auto_forward);

        assert_eq!(cli_layer.auto_ports.unwrap().enabled, Some(false));
    }

    #[test]
    fn parses_manual_port_forms() {
        let cli = Cli::parse_from([
            "decune",
            "up",
            "--port",
            "3000",
            "--port",
            "3000:3000",
            "--port",
            "127.0.0.1:8080:3000",
            "--port",
            "localhost:5173",
            "--port",
            "0.0.0.0:8081:5173",
        ]);

        let Commands::Up(args) = cli.command else {
            panic!("expected up command");
        };

        assert_eq!(args.ports.len(), 5);
        assert_eq!(args.ports[0].container, 3000);
        assert_eq!(args.ports[0].host, None);
        assert_eq!(args.ports[0].host_ip, "127.0.0.1");
        assert_eq!(args.ports[1].container, 3000);
        assert_eq!(args.ports[1].host, Some(3000));
        assert_eq!(args.ports[1].host_ip, "127.0.0.1");
        assert_eq!(args.ports[2].container, 3000);
        assert_eq!(args.ports[2].host, Some(8080));
        assert_eq!(args.ports[2].host_ip, "127.0.0.1");
        assert_eq!(args.ports[3].container, 5173);
        assert_eq!(args.ports[3].host, None);
        assert_eq!(args.ports[3].host_ip, "127.0.0.1");
        assert_eq!(args.ports[4].container, 5173);
        assert_eq!(args.ports[4].host, Some(8081));
        assert_eq!(args.ports[4].host_ip, "0.0.0.0");
    }

    #[test]
    fn parses_manual_port_ipv6_forms() {
        let cli = Cli::parse_from([
            "decune",
            "up",
            "--port",
            "[::1]:8080:3000",
            "--port",
            "[::1]:3000",
            "--port",
            "[2001:db8::1]:8080:3000/tcp",
        ]);

        let Commands::Up(args) = cli.command else {
            panic!("expected up command");
        };

        assert_eq!(args.ports.len(), 3);
        assert_eq!(args.ports[0].container, 3000);
        assert_eq!(args.ports[0].host, Some(8080));
        assert_eq!(args.ports[0].host_ip, "::1");
        assert_eq!(args.ports[1].container, 3000);
        assert_eq!(args.ports[1].host, None);
        assert_eq!(args.ports[1].host_ip, "::1");
        assert_eq!(args.ports[2].container, 3000);
        assert_eq!(args.ports[2].host, Some(8080));
        assert_eq!(args.ports[2].host_ip, "2001:db8::1");
        assert_eq!(args.ports[2].protocol, PortProtocol::Tcp);
    }

    #[test]
    fn parses_rebuild_options() {
        let cli = Cli::parse_from([
            "decune",
            "rebuild",
            "--config",
            ".devcontainer.json",
            "--detach",
            "--no-cache",
            "--pull",
            "--update-features",
            "--no-auto-forward",
            "-p",
            "8080:80",
        ]);

        let Commands::Rebuild(args) = cli.command else {
            panic!("expected rebuild command");
        };

        assert_eq!(args.workspace, PathBuf::from("."));
        assert_eq!(
            args.config.as_deref(),
            Some(PathBuf::from(".devcontainer.json").as_path())
        );
        assert!(args.detach);
        assert!(args.no_cache);
        assert!(args.pull);
        assert!(args.update_features);
        assert!(args.no_auto_forward);
        assert_eq!(args.ports.len(), 1);
        assert_eq!(args.ports[0].container, 80);
        assert_eq!(args.ports[0].host, Some(8080));
    }

    #[test]
    fn rebuild_update_features_is_passed_to_up_options() {
        let cli = Cli::parse_from(["decune", "rebuild", "--update-features"]);
        let Commands::Rebuild(args) = cli.command else {
            panic!("expected rebuild command");
        };

        let options = rebuild_up_options_from_args(args).unwrap();

        assert!(options.update_features);
    }

    #[test]
    fn rebuild_no_auto_forward_disables_auto_ports() {
        let cli = Cli::parse_from(["decune", "rebuild", "--no-auto-forward"]);
        let Commands::Rebuild(args) = cli.command else {
            panic!("expected rebuild command");
        };

        let options = rebuild_up_options_from_args(args).unwrap();

        assert_eq!(options.cli_layer.auto_ports.unwrap().enabled, Some(false));
    }

    #[test]
    fn rebuild_detach_allows_no_auto_forward_without_manual_ports() {
        let cli = Cli::parse_from(["decune", "rebuild", "--detach", "--no-auto-forward"]);
        let Commands::Rebuild(args) = cli.command else {
            panic!("expected rebuild command");
        };

        let options = rebuild_up_options_from_args(args).unwrap();

        assert_eq!(options.cli_layer.auto_ports.unwrap().enabled, Some(false));
    }

    #[test]
    fn detached_cli_ports_are_rejected() {
        let cli = Cli::parse_from(["decune", "up", "--detach", "-p", "3000"]);
        let Commands::Up(args) = cli.command else {
            panic!("expected up command");
        };

        let error = reject_detached_cli_ports(args.detach, &args.ports).unwrap_err();

        assert!(error.to_string().contains("use appPort"));
    }

    #[test]
    fn parses_down_and_clean_options() {
        let down = Cli::parse_from(["decune", "down", "--timeout", "20", "workspace"]);
        let Commands::Down(down_args) = down.command else {
            panic!("expected down command");
        };

        assert_eq!(down_args.workspace, PathBuf::from("workspace"));
        assert_eq!(down_args.timeout, 20);

        let clean = Cli::parse_from(["decune", "clean", "--images", "--force", "workspace"]);
        let Commands::Clean(clean_args) = clean.command else {
            panic!("expected clean command");
        };

        assert_eq!(clean_args.workspace, PathBuf::from("workspace"));
        assert!(clean_args.images);
        assert!(clean_args.force);
    }

    #[test]
    fn down_timeout_defaults_to_ten_seconds() {
        let cli = Cli::parse_from(["decune", "down"]);
        let Commands::Down(args) = cli.command else {
            panic!("expected down command");
        };

        assert_eq!(args.timeout, 10);
    }

    #[test]
    fn invalid_manual_port_is_rejected() {
        let error =
            Cli::try_parse_from(["decune", "up", "--port", "127.0.0.1:abc:3000"]).unwrap_err();

        assert!(error.to_string().contains("invalid host port"));
    }

    #[test]
    fn malformed_ipv6_manual_ports_are_rejected() {
        for value in [
            "::1:8080:3000",
            "[::1:8080:3000",
            "[]:3000",
            "[::1]",
            "[::1]:8080:3000:extra",
            "[::1]:abc:3000",
        ] {
            let error = Cli::try_parse_from(["decune", "up", "--port", value]).unwrap_err();
            assert!(
                error.to_string().contains("port") || error.to_string().contains("IPv6"),
                "{value}: {error}"
            );
        }
    }

    #[test]
    fn unsupported_manual_port_protocol_is_rejected() {
        let error = Cli::try_parse_from(["decune", "up", "--port", "3000/udp"]).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported manual port protocol: udp. decune v0.1 supports tcp only")
        );
    }

    #[test]
    fn numeric_manual_host_port_outside_u16_is_rejected() {
        let error = Cli::try_parse_from(["decune", "up", "--port", "99999:3000"]).unwrap_err();

        assert!(error.to_string().contains("invalid host port"));
    }
}
