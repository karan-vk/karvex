use crate::api::schema::IntegrationTarget;

pub(super) fn run_integration_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_integration_help();
        return Ok(2);
    };

    match subcommand {
        "install" => integration_install(&args[1..]),
        "uninstall" => integration_uninstall(&args[1..]),
        "status" => integration_status(&args[1..]),
        "help" | "--help" | "-h" => {
            print_integration_help();
            Ok(0)
        }
        _ => {
            print_integration_help();
            Ok(2)
        }
    }
}

fn integration_status(args: &[String]) -> std::io::Result<i32> {
    let outdated_only = match args {
        [] => false,
        [flag] if flag == "--outdated-only" => true,
        _ => {
            eprintln!("usage: kvx integration status [--outdated-only]");
            return Ok(2);
        }
    };

    if outdated_only {
        crate::integration::print_outdated_update_notice();
        return Ok(0);
    }

    for status in crate::integration::installed_integration_statuses() {
        let target = crate::integration::integration_target_label(status.target);
        let version = match status.installed_version {
            Some(version) => format!("v{version}"),
            None => "legacy".to_string(),
        };
        let state = match status.state {
            crate::integration::IntegrationStatusKind::NotInstalled => "not installed".to_string(),
            crate::integration::IntegrationStatusKind::Current => {
                format!("current ({version})")
            }
            crate::integration::IntegrationStatusKind::Outdated => {
                format!("outdated ({version} < v{})", status.expected_version)
            }
        };
        println!("{target}: {state} ({})", status.path.display());
    }

    Ok(0)
}

fn integration_install(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = parse_integration_target(args, "install")? else {
        return Ok(2);
    };

    match crate::integration::install_target(target) {
        Ok(messages) => {
            print_integration_messages(messages);
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn integration_uninstall(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = parse_integration_target(args, "uninstall")? else {
        return Ok(2);
    };

    match crate::integration::uninstall_target(target) {
        Ok(messages) => {
            print_integration_messages(messages);
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn print_integration_messages(messages: Vec<String>) {
    for message in messages {
        println!("{message}");
    }
}

fn parse_integration_target(
    args: &[String],
    action: &str,
) -> std::io::Result<Option<IntegrationTarget>> {
    let Some(target) = args.first().map(|arg| arg.as_str()) else {
        eprintln!(
            "usage: kvx integration {action} <pi|omp|claude|codex|copilot|devin|droid|kimi|opencode|kilo|hermes|qodercli|cursor|mastracode|grok>"
        );
        return Ok(None);
    };
    if args.len() != 1 {
        eprintln!(
            "usage: kvx integration {action} <pi|omp|claude|codex|copilot|devin|droid|kimi|opencode|kilo|hermes|qodercli|cursor|mastracode|grok>"
        );
        return Ok(None);
    }

    let parsed = match target {
        "pi" => IntegrationTarget::Pi,
        "omp" => IntegrationTarget::Omp,
        "claude" => IntegrationTarget::Claude,
        "codex" => IntegrationTarget::Codex,
        "copilot" => IntegrationTarget::Copilot,
        "devin" => IntegrationTarget::Devin,
        "droid" => IntegrationTarget::Droid,
        "kimi" => IntegrationTarget::Kimi,
        "opencode" => IntegrationTarget::Opencode,
        "kilo" => IntegrationTarget::Kilo,
        "hermes" => IntegrationTarget::Hermes,
        "qodercli" => IntegrationTarget::Qodercli,
        "cursor" => IntegrationTarget::Cursor,
        "mastracode" => IntegrationTarget::Mastracode,
        "antigravity-cli" | "antigravity_cli" => IntegrationTarget::AntigravityCli,
        "grok" => IntegrationTarget::Grok,
        _ => {
            eprintln!("unknown integration target: {target}");
            eprintln!(
                "currently supported: pi, omp, claude, codex, copilot, devin, droid, kimi, opencode, kilo, hermes, qodercli, cursor, mastracode, antigravity-cli, grok"
            );
            return Ok(None);
        }
    };

    Ok(Some(parsed))
}

fn print_integration_help() {
    eprintln!("kvx integration commands:");
    eprintln!("  kvx integration install pi");
    eprintln!("  kvx integration install omp");
    eprintln!("  kvx integration install claude");
    eprintln!("  kvx integration install codex");
    eprintln!("  kvx integration install copilot");
    eprintln!("  kvx integration install devin");
    eprintln!("  kvx integration install droid");
    eprintln!("  kvx integration install kimi");
    eprintln!("  kvx integration install opencode");
    eprintln!("  kvx integration install kilo");
    eprintln!("  kvx integration install hermes");
    eprintln!("  kvx integration install qodercli");
    eprintln!("  kvx integration install cursor");
    eprintln!("  kvx integration install mastracode");
    eprintln!("  kvx integration install antigravity-cli");
    eprintln!("  kvx integration install grok");
    eprintln!("  kvx integration uninstall pi");
    eprintln!("  kvx integration uninstall omp");
    eprintln!("  kvx integration uninstall claude");
    eprintln!("  kvx integration uninstall codex");
    eprintln!("  kvx integration uninstall copilot");
    eprintln!("  kvx integration uninstall devin");
    eprintln!("  kvx integration uninstall droid");
    eprintln!("  kvx integration uninstall kimi");
    eprintln!("  kvx integration uninstall opencode");
    eprintln!("  kvx integration uninstall kilo");
    eprintln!("  kvx integration uninstall hermes");
    eprintln!("  kvx integration uninstall qodercli");
    eprintln!("  kvx integration uninstall cursor");
    eprintln!("  kvx integration uninstall mastracode");
    eprintln!("  kvx integration uninstall antigravity-cli");
    eprintln!("  kvx integration uninstall grok");
    eprintln!("  kvx integration status [--outdated-only]");
}
