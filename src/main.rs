#![allow(clippy::cognitive_complexity)]

use std::env;
use std::io;
use std::path::PathBuf;
use std::process::exit;
use std::time::Duration;

use crate::breaking_changes::{first_run_of_major_release, print_breaking_changes, should_skip, write_keep_file};
use clap::CommandFactory;
use clap::{crate_version, Parser};
use color_eyre::eyre::Context;
use color_eyre::eyre::Result;
use console::Key;
use etcetera::base_strategy::BaseStrategy;
#[cfg(windows)]
use etcetera::base_strategy::Windows;
#[cfg(unix)]
use etcetera::base_strategy::Xdg;
use once_cell::sync::Lazy;
use rust_i18n::{i18n, t};
use tracing::debug;

use self::config::{CommandLineArgs, Config, Step};
use self::error::StepFailed;
#[cfg(all(windows, feature = "self-update"))]
use self::error::Upgraded;
#[allow(clippy::wildcard_imports)]
use self::steps::{remote::*, *};
#[allow(clippy::wildcard_imports)]
use self::terminal::*;

use self::utils::{hostname, install_color_eyre, install_tracing, update_tracing};

mod breaking_changes;
mod command;
mod config;
mod ctrlc;
mod error;
mod execution_context;
mod executor;
mod report;
mod runner;
#[cfg(windows)]
mod self_renamer;
#[cfg(feature = "self-update")]
mod self_update;
mod steps;
mod sudo;
mod terminal;
mod utils;

pub(crate) static HOME_DIR: Lazy<PathBuf> = Lazy::new(|| home::home_dir().expect("No home directory"));
#[cfg(unix)]
pub(crate) static XDG_DIRS: Lazy<Xdg> = Lazy::new(|| Xdg::new().expect("No home directory"));

#[cfg(windows)]
pub(crate) static WINDOWS_DIRS: Lazy<Windows> = Lazy::new(|| Windows::new().expect("No home directory"));

// Init and load the i18n files
i18n!("locales", fallback = "en");

#[allow(clippy::too_many_lines)]
fn run() -> Result<()> {
    install_color_eyre()?;
    ctrlc::set_handler();

    let opt = CommandLineArgs::parse();
    // Set up the logger with the filter directives from:
    //     1. CLI option `--log-filter`
    //     2. `debug` if the `--verbose` option is present
    // We do this because we need our logger to work while loading the
    // configuration file.
    //
    // When the configuration file is loaded, update the logger with the full
    // filter directives.
    //
    // For more info, see the comments in `CommandLineArgs::tracing_filter_directives()`
    // and `Config::tracing_filter_directives()`.
    let reload_handle = install_tracing(&opt.tracing_filter_directives())?;

    // Get current system locale and set it as the default locale
    let system_locale = sys_locale::get_locale().unwrap_or("en".to_string());
    rust_i18n::set_locale(&system_locale);
    debug!("Current system locale is {system_locale}");

    if let Some(shell) = opt.gen_completion {
        let cmd = &mut CommandLineArgs::command();
        clap_complete::generate(shell, cmd, clap::crate_name!(), &mut io::stdout());
        return Ok(());
    }

    if opt.gen_manpage {
        let man = clap_mangen::Man::new(CommandLineArgs::command());
        man.render(&mut io::stdout())?;
        return Ok(());
    }

    for env in opt.env_variables() {
        let mut splitted = env.split('=');
        let var = splitted.next().unwrap();
        let value = splitted.next().unwrap();
        env::set_var(var, value);
    }

    if opt.edit_config() {
        Config::edit()?;
        return Ok(());
    };

    if opt.show_config_reference() {
        print!("{}", config::EXAMPLE_CONFIG);
        return Ok(());
    }

    let config = Config::load(opt)?;
    // Update the logger with the full filter directives.
    update_tracing(&reload_handle, &config.tracing_filter_directives())?;
    set_title(config.set_title());
    display_time(config.display_time());
    set_desktop_notifications(config.notify_each_step());

    debug!("Version: {}", crate_version!());
    debug!("OS: {}", env!("TARGET"));
    debug!("{:?}", std::env::args());
    debug!("Binary path: {:?}", std::env::current_exe());
    debug!("self-update Feature Enabled: {:?}", cfg!(feature = "self-update"));
    debug!("Configuration: {:?}", config);

    if config.run_in_tmux() && env::var("TOPGRADE_INSIDE_TMUX").is_err() {
        #[cfg(unix)]
        {
            tmux::run_in_tmux(config.tmux_config()?)?;
            return Ok(());
        }
    }

    let powershell = powershell::Powershell::new();
    let should_run_powershell = powershell.profile().is_some() && config.should_run(Step::Powershell);
    let emacs = emacs::Emacs::new();
    #[cfg(target_os = "linux")]
    let distribution = linux::Distribution::detect();

    let sudo = config.sudo_command().map_or_else(sudo::Sudo::detect, sudo::Sudo::new);
    let run_type = executor::RunType::new(config.dry_run());
    let ctx = execution_context::ExecutionContext::new(run_type, sudo, &config);
    let mut runner = runner::Runner::new(&ctx);

    // If
    //
    // 1. the breaking changes notification shouldnot be skipped
    // 2. this is the first execution of a major release
    //
    // inform user of breaking changes
    if !should_skip() && first_run_of_major_release()? {
        print_breaking_changes();

        if prompt_yesno("Confirmed?")? {
            write_keep_file()?;
        } else {
            exit(1);
        }
    }

    // Self-Update step, this will execute only if:
    // 1. the `self-update` feature is enabled
    // 2. it is not disabled from configuration (env var/CLI opt/file)
    #[cfg(feature = "self-update")]
    {
        let should_self_update = env::var("TOPGRADE_NO_SELF_UPGRADE").is_err() && !config.no_self_update();

        if should_self_update {
            runner.execute(Step::SelfUpdate, "Self Update", || self_update::self_update(&ctx))?;
        }
    }

    #[cfg(windows)]
    let _self_rename = if config.self_rename() {
        Some(crate::self_renamer::SelfRenamer::create()?)
    } else {
        None
    };

    if let Some(commands) = config.pre_commands() {
        for (name, command) in commands {
            generic::run_custom_command(name, command, &ctx)?;
        }
    }

    if config.pre_sudo() {
        if let Some(sudo) = ctx.sudo() {
            sudo.elevate(&ctx)?;
        }
    }

    if let Some(topgrades) = config.remote_topgrades() {
        for remote_topgrade in topgrades.iter().filter(|t| config.should_execute_remote(hostname(), t)) {
            runner.execute(Step::Remotes, format!("Remote ({remote_topgrade})"), || {
                ssh::ssh_step(&ctx, remote_topgrade)
            })?;
        }
    }

    #[cfg(windows)]
    {
        runner.execute(Step::Wsl, "WSL", || windows::run_wsl_topgrade(&ctx))?;
        runner.execute(Step::WslUpdate, "WSL", || windows::update_wsl(&ctx))?;
        runner.execute(Step::Chocolatey, "Chocolatey", || windows::run_chocolatey(&ctx))?;
        runner.execute(Step::Scoop, "Scoop", || windows::run_scoop(&ctx))?;
        runner.execute(Step::Winget, "Winget", || windows::run_winget(&ctx))?;
        runner.execute(Step::System, "Windows update", || windows::windows_update(&ctx))?;
        runner.execute(Step::MicrosoftStore, "Microsoft Store", || {
            windows::microsoft_store(&ctx)
        })?;
    }

    #[cfg(target_os = "linux")]
    {
        // NOTE: Due to breaking `nu` updates, `packer.nu` needs to be updated before `nu` get updated
        // by other package managers.
        runner.execute(Step::Shell, "packer.nu", || linux::run_packer_nu(&ctx))?;

        match &distribution {
            Ok(distribution) => {
                runner.execute(Step::System, "System update", || distribution.upgrade(&ctx))?;
            }
            Err(e) => {
                println!("{}", t!("Error detecting current distribution: {error}", error = e));
            }
        }
        runner.execute(Step::ConfigUpdate, "config-update", || linux::run_config_update(&ctx))?;

        runner.execute(Step::AM, "am", || linux::run_am(&ctx))?;
        runner.execute(Step::AppMan, "appman", || linux::run_appman(&ctx))?;
        runner.execute(Step::DebGet, "deb-get", || linux::run_deb_get(&ctx))?;
        runner.execute(Step::Toolbx, "toolbx", || toolbx::run_toolbx(&ctx))?;
        runner.execute(Step::Snap, "snap", || linux::run_snap(&ctx))?;
        runner.execute(Step::Pacstall, "pacstall", || linux::run_pacstall(&ctx))?;
        runner.execute(Step::Pacdef, "pacdef", || linux::run_pacdef(&ctx))?;
        runner.execute(Step::Protonup, "protonup", || linux::run_protonup_update(&ctx))?;
        runner.execute(Step::Distrobox, "distrobox", || linux::run_distrobox_update(&ctx))?;
        runner.execute(Step::DkpPacman, "dkp-pacman", || linux::run_dkp_pacman_update(&ctx))?;
        runner.execute(Step::System, "pihole", || linux::run_pihole_update(&ctx))?;
        runner.execute(Step::Firmware, "Firmware upgrades", || linux::run_fwupdmgr(&ctx))?;
        runner.execute(Step::Restarts, "Restarts", || linux::run_needrestart(&ctx))?;

        runner.execute(Step::Flatpak, "Flatpak", || linux::run_flatpak(&ctx))?;
        runner.execute(Step::BrewFormula, "Brew", || {
            unix::run_brew_formula(&ctx, unix::BrewVariant::Path)
        })?;
        runner.execute(Step::Lure, "LURE", || linux::run_lure_update(&ctx))?;
        runner.execute(Step::Waydroid, "Waydroid", || linux::run_waydroid(&ctx))?;
        runner.execute(Step::AutoCpufreq, "auto-cpufreq", || linux::run_auto_cpufreq(&ctx))?;
        runner.execute(Step::CinnamonSpices, "Cinnamon spices", || {
            linux::run_cinnamon_spices_updater(&ctx)
        })?;
    }

    #[cfg(target_os = "macos")]
    {
        runner.execute(Step::BrewFormula, "Brew (ARM)", || {
            unix::run_brew_formula(&ctx, unix::BrewVariant::MacArm)
        })?;
        runner.execute(Step::BrewFormula, "Brew (Intel)", || {
            unix::run_brew_formula(&ctx, unix::BrewVariant::MacIntel)
        })?;
        runner.execute(Step::BrewFormula, "Brew", || {
            unix::run_brew_formula(&ctx, unix::BrewVariant::Path)
        })?;
        runner.execute(Step::BrewCask, "Brew Cask (ARM)", || {
            unix::run_brew_cask(&ctx, unix::BrewVariant::MacArm)
        })?;
        runner.execute(Step::BrewCask, "Brew Cask (Intel)", || {
            unix::run_brew_cask(&ctx, unix::BrewVariant::MacIntel)
        })?;
        runner.execute(Step::BrewCask, "Brew Cask", || {
            unix::run_brew_cask(&ctx, unix::BrewVariant::Path)
        })?;
        runner.execute(Step::Macports, "MacPorts", || macos::run_macports(&ctx))?;
        runner.execute(Step::Xcodes, "Xcodes", || macos::update_xcodes(&ctx))?;
        runner.execute(Step::Sparkle, "Sparkle", || macos::run_sparkle(&ctx))?;
        runner.execute(Step::Mas, "App Store", || macos::run_mas(&ctx))?;
        runner.execute(Step::System, "System upgrade", || macos::upgrade_macos(&ctx))?;
    }

    #[cfg(target_os = "dragonfly")]
    {
        runner.execute(Step::Pkg, "DragonFly BSD Packages", || {
            dragonfly::upgrade_packages(&ctx)
        })?;
        runner.execute(Step::Audit, "DragonFly Audit", || dragonfly::audit_packages(&ctx))?;
    }

    #[cfg(target_os = "freebsd")]
    {
        runner.execute(Step::Pkg, "FreeBSD Packages", || freebsd::upgrade_packages(&ctx))?;
        runner.execute(Step::System, "FreeBSD Upgrade", || freebsd::upgrade_freebsd(&ctx))?;
        runner.execute(Step::Audit, "FreeBSD Audit", || freebsd::audit_packages(&ctx))?;
    }

    #[cfg(target_os = "openbsd")]
    {
        runner.execute(Step::Pkg, "OpenBSD Packages", || openbsd::upgrade_packages(&ctx))?;
        runner.execute(Step::System, "OpenBSD Upgrade", || openbsd::upgrade_openbsd(&ctx))?;
    }

    #[cfg(target_os = "android")]
    {
        runner.execute(Step::Pkg, "Termux Packages", || android::upgrade_packages(&ctx))?;
    }

    #[cfg(unix)]
    {
        runner.execute(Step::Yadm, "yadm", || unix::run_yadm(&ctx))?;
        runner.execute(Step::Nix, "nix", || unix::run_nix(&ctx))?;
        runner.execute(Step::Nix, "nix upgrade-nix", || unix::run_nix_self_upgrade(&ctx))?;
        runner.execute(Step::NixHelper, "nh", || unix::run_nix_helper(&ctx))?;
        runner.execute(Step::Guix, "guix", || unix::run_guix(&ctx))?;
        runner.execute(Step::HomeManager, "home-manager", || unix::run_home_manager(&ctx))?;
        runner.execute(Step::Asdf, "asdf", || unix::run_asdf(&ctx))?;
        runner.execute(Step::Mise, "mise", || unix::run_mise(&ctx))?;
        runner.execute(Step::Pkgin, "pkgin", || unix::run_pkgin(&ctx))?;
        runner.execute(Step::BunPackages, "bun-packages", || unix::run_bun_packages(&ctx))?;
        runner.execute(Step::Shell, "zr", || zsh::run_zr(&ctx))?;
        runner.execute(Step::Shell, "antibody", || zsh::run_antibody(&ctx))?;
        runner.execute(Step::Shell, "antidote", || zsh::run_antidote(&ctx))?;
        runner.execute(Step::Shell, "antigen", || zsh::run_antigen(&ctx))?;
        runner.execute(Step::Shell, "zgenom", || zsh::run_zgenom(&ctx))?;
        runner.execute(Step::Shell, "zplug", || zsh::run_zplug(&ctx))?;
        runner.execute(Step::Shell, "zinit", || zsh::run_zinit(&ctx))?;
        runner.execute(Step::Shell, "zi", || zsh::run_zi(&ctx))?;
        runner.execute(Step::Shell, "zim", || zsh::run_zim(&ctx))?;
        runner.execute(Step::Shell, "oh-my-zsh", || zsh::run_oh_my_zsh(&ctx))?;
        runner.execute(Step::Shell, "oh-my-bash", || unix::run_oh_my_bash(&ctx))?;
        runner.execute(Step::Shell, "fisher", || unix::run_fisher(&ctx))?;
        runner.execute(Step::Shell, "bash-it", || unix::run_bashit(&ctx))?;
        runner.execute(Step::Shell, "oh-my-fish", || unix::run_oh_my_fish(&ctx))?;
        runner.execute(Step::Shell, "fish-plug", || unix::run_fish_plug(&ctx))?;
        runner.execute(Step::Shell, "fundle", || unix::run_fundle(&ctx))?;
        runner.execute(Step::Tmux, "tmux", || tmux::run_tpm(&ctx))?;
        runner.execute(Step::Tldr, "TLDR", || unix::run_tldr(&ctx))?;
        runner.execute(Step::Pearl, "pearl", || unix::run_pearl(&ctx))?;
        #[cfg(not(any(target_os = "macos", target_os = "android")))]
        runner.execute(Step::GnomeShellExtensions, "Gnome Shell Extensions", || {
            unix::upgrade_gnome_extensions(&ctx)
        })?;
        runner.execute(Step::Pyenv, "pyenv", || unix::run_pyenv(&ctx))?;
        runner.execute(Step::Sdkman, "SDKMAN!", || unix::run_sdkman(&ctx))?;
        runner.execute(Step::Rcm, "rcm", || unix::run_rcm(&ctx))?;
        runner.execute(Step::Maza, "maza", || unix::run_maza(&ctx))?;
    }

    #[cfg(not(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    {
        runner.execute(Step::Atom, "apm", || generic::run_apm(&ctx))?;
    }

    // The following update function should be executed on all OSes.
    runner.execute(Step::Fossil, "fossil", || generic::run_fossil(&ctx))?;
    runner.execute(Step::Elan, "elan", || generic::run_elan(&ctx))?;
    runner.execute(Step::Rye, "rye", || generic::run_rye(&ctx))?;
    runner.execute(Step::Rustup, "rustup", || generic::run_rustup(&ctx))?;
    runner.execute(Step::Juliaup, "juliaup", || generic::run_juliaup(&ctx))?;
    runner.execute(Step::Dotnet, ".NET", || generic::run_dotnet_upgrade(&ctx))?;
    runner.execute(Step::Choosenim, "choosenim", || generic::run_choosenim(&ctx))?;
    runner.execute(Step::Cargo, "cargo", || generic::run_cargo_update(&ctx))?;
    runner.execute(Step::Flutter, "Flutter", || generic::run_flutter_upgrade(&ctx))?;
    runner.execute(Step::Go, "go-global-update", || go::run_go_global_update(&ctx))?;
    runner.execute(Step::Go, "gup", || go::run_go_gup(&ctx))?;
    runner.execute(Step::Emacs, "Emacs", || emacs.upgrade(&ctx))?;
    runner.execute(Step::Opam, "opam", || generic::run_opam_update(&ctx))?;
    runner.execute(Step::Vcpkg, "vcpkg", || generic::run_vcpkg_update(&ctx))?;
    runner.execute(Step::Pipx, "pipx", || generic::run_pipx_update(&ctx))?;
    runner.execute(Step::Pipxu, "pipxu", || generic::run_pipxu_update(&ctx))?;
    runner.execute(Step::Vscode, "Visual Studio Code extensions", || {
        generic::run_vscode_extensions_update(&ctx)
    })?;
    runner.execute(Step::Vscodium, "VSCodium extensions", || {
        generic::run_vscodium_extensions_update(&ctx)
    })?;
    runner.execute(Step::Conda, "conda", || generic::run_conda_update(&ctx))?;
    runner.execute(Step::Mamba, "mamba", || generic::run_mamba_update(&ctx))?;
    runner.execute(Step::Pixi, "pixi", || generic::run_pixi_update(&ctx))?;
    runner.execute(Step::Miktex, "miktex", || generic::run_miktex_packages_update(&ctx))?;
    runner.execute(Step::Pip3, "pip3", || generic::run_pip3_update(&ctx))?;
    runner.execute(Step::PipReview, "pip-review", || generic::run_pip_review_update(&ctx))?;
    runner.execute(Step::PipReviewLocal, "pip-review (local)", || {
        generic::run_pip_review_local_update(&ctx)
    })?;
    runner.execute(Step::Pipupgrade, "pipupgrade", || generic::run_pipupgrade_update(&ctx))?;
    runner.execute(Step::Ghcup, "ghcup", || generic::run_ghcup_update(&ctx))?;
    runner.execute(Step::Stack, "stack", || generic::run_stack_update(&ctx))?;
    runner.execute(Step::Tlmgr, "tlmgr", || generic::run_tlmgr_update(&ctx))?;
    runner.execute(Step::Myrepos, "myrepos", || generic::run_myrepos_update(&ctx))?;
    runner.execute(Step::Chezmoi, "chezmoi", || generic::run_chezmoi_update(&ctx))?;
    runner.execute(Step::Jetpack, "jetpack", || generic::run_jetpack(&ctx))?;
    runner.execute(Step::Vim, "vim", || vim::upgrade_vim(&ctx))?;
    runner.execute(Step::Vim, "Neovim", || vim::upgrade_neovim(&ctx))?;
    runner.execute(Step::Vim, "The Ultimate vimrc", || vim::upgrade_ultimate_vimrc(&ctx))?;
    runner.execute(Step::Vim, "voom", || vim::run_voom(&ctx))?;
    runner.execute(Step::Kakoune, "Kakoune", || kakoune::upgrade_kak_plug(&ctx))?;
    runner.execute(Step::Helix, "helix", || generic::run_helix_grammars(&ctx))?;
    runner.execute(Step::Node, "npm", || node::run_npm_upgrade(&ctx))?;
    runner.execute(Step::Yarn, "yarn", || node::run_yarn_upgrade(&ctx))?;
    runner.execute(Step::Pnpm, "pnpm", || node::run_pnpm_upgrade(&ctx))?;
    runner.execute(Step::VoltaPackages, "volta packages", || {
        node::run_volta_packages_upgrade(&ctx)
    })?;
    runner.execute(Step::Containers, "Containers", || containers::run_containers(&ctx))?;
    runner.execute(Step::Deno, "deno", || node::deno_upgrade(&ctx))?;
    runner.execute(Step::Composer, "composer", || generic::run_composer_update(&ctx))?;
    runner.execute(Step::Krew, "krew", || generic::run_krew_upgrade(&ctx))?;
    runner.execute(Step::Helm, "helm", || generic::run_helm_repo_update(&ctx))?;
    runner.execute(Step::Gem, "gem", || generic::run_gem(&ctx))?;
    runner.execute(Step::RubyGems, "rubygems", || generic::run_rubygems(&ctx))?;
    runner.execute(Step::Julia, "julia", || generic::update_julia_packages(&ctx))?;
    runner.execute(Step::Haxelib, "haxelib", || generic::run_haxelib_update(&ctx))?;
    runner.execute(Step::Sheldon, "sheldon", || generic::run_sheldon(&ctx))?;
    runner.execute(Step::Stew, "stew", || generic::run_stew(&ctx))?;
    runner.execute(Step::Rtcl, "rtcl", || generic::run_rtcl(&ctx))?;
    runner.execute(Step::Bin, "bin", || generic::bin_update(&ctx))?;
    runner.execute(Step::Gcloud, "gcloud", || generic::run_gcloud_components_update(&ctx))?;
    runner.execute(Step::Micro, "micro", || generic::run_micro(&ctx))?;
    runner.execute(Step::Raco, "raco", || generic::run_raco_update(&ctx))?;
    runner.execute(Step::Spicetify, "spicetify", || generic::spicetify_upgrade(&ctx))?;
    runner.execute(Step::GithubCliExtensions, "GitHub CLI Extensions", || {
        generic::run_ghcli_extensions_upgrade(&ctx)
    })?;
    runner.execute(Step::Bob, "Bob", || generic::run_bob(&ctx))?;
    runner.execute(Step::Certbot, "Certbot", || generic::run_certbot(&ctx))?;
    runner.execute(Step::GitRepos, "Git Repositories", || git::run_git_pull(&ctx))?;
    runner.execute(Step::ClamAvDb, "ClamAV Databases", || generic::run_freshclam(&ctx))?;
    runner.execute(Step::PlatformioCore, "PlatformIO Core", || {
        generic::run_platform_io(&ctx)
    })?;
    runner.execute(Step::Lensfun, "Lensfun's database update", || {
        generic::run_lensfun_update_data(&ctx)
    })?;
    runner.execute(Step::Poetry, "Poetry", || generic::run_poetry(&ctx))?;
    runner.execute(Step::Uv, "uv", || generic::run_uv(&ctx))?;
    runner.execute(Step::Zvm, "ZVM", || generic::run_zvm(&ctx))?;
    runner.execute(Step::Aqua, "aqua", || generic::run_aqua(&ctx))?;
    runner.execute(Step::Bun, "bun", || generic::run_bun(&ctx))?;
    runner.execute(Step::Zigup, "zigup", || generic::run_zigup(&ctx))?;
    runner.execute(Step::JetbrainsToolbox, "JetBrains Toolbox", || {
        generic::run_jetbrains_toolbox(&ctx)
    })?;
    runner.execute(Step::AndroidStudio, "Android Studio plugins", || {
        generic::run_android_studio(&ctx)
    })?;
    runner.execute(Step::JetbrainsAqua, "JetBrains Aqua plugins", || {
        generic::run_jetbrains_aqua(&ctx)
    })?;
    runner.execute(Step::JetbrainsClion, "JetBrains CLion plugins", || {
        generic::run_jetbrains_clion(&ctx)
    })?;
    runner.execute(Step::JetbrainsDatagrip, "JetBrains DataGrip plugins", || {
        generic::run_jetbrains_datagrip(&ctx)
    })?;
    runner.execute(Step::JetbrainsDataspell, "JetBrains DataSpell plugins", || {
        generic::run_jetbrains_dataspell(&ctx)
    })?;
    // JetBrains dotCover has no CLI
    // JetBrains dotMemory has no CLI
    // JetBrains dotPeek has no CLI
    // JetBrains dotTrace has no CLI
    // JetBrains Fleet has a different CLI without a `fleet update` command.
    runner.execute(Step::JetbrainsGateway, "JetBrains Gateway plugins", || {
        generic::run_jetbrains_gateway(&ctx)
    })?;
    runner.execute(Step::JetbrainsGoland, "JetBrains GoLand plugins", || {
        generic::run_jetbrains_goland(&ctx)
    })?;
    runner.execute(Step::JetbrainsIdea, "JetBrains IntelliJ IDEA plugins", || {
        generic::run_jetbrains_idea(&ctx)
    })?;
    runner.execute(Step::JetbrainsMps, "JetBrains MPS plugins", || {
        generic::run_jetbrains_mps(&ctx)
    })?;
    runner.execute(Step::JetbrainsPhpstorm, "JetBrains PhpStorm plugins", || {
        generic::run_jetbrains_phpstorm(&ctx)
    })?;
    runner.execute(Step::JetbrainsPycharm, "JetBrains PyCharm plugins", || {
        generic::run_jetbrains_pycharm(&ctx)
    })?;
    // JetBrains ReSharper has no CLI (it's a VSCode extension)
    // JetBrains ReSharper C++ has no CLI (it's a VSCode extension)
    runner.execute(Step::JetbrainsRider, "JetBrains Rider plugins", || {
        generic::run_jetbrains_rider(&ctx)
    })?;
    runner.execute(Step::JetbrainsRubymine, "JetBrains RubyMine plugins", || {
        generic::run_jetbrains_rubymine(&ctx)
    })?;
    runner.execute(Step::JetbrainsRustrover, "JetBrains RustRover plugins", || {
        generic::run_jetbrains_rustrover(&ctx)
    })?;
    // JetBrains Space Desktop does not have a CLI
    runner.execute(Step::JetbrainsWebstorm, "JetBrains WebStorm plugins", || {
        generic::run_jetbrains_webstorm(&ctx)
    })?;
    runner.execute(Step::Yazi, "Yazi packages", || generic::run_yazi(&ctx))?;

    if should_run_powershell {
        runner.execute(Step::Powershell, "Powershell Modules Update", || {
            powershell.update_modules(&ctx)
        })?;
    }

    if let Some(commands) = config.commands() {
        for (name, command) in commands {
            if config.should_run_custom_command(name) {
                runner.execute(Step::CustomCommands, name, || {
                    generic::run_custom_command(name, command, &ctx)
                })?;
            }
        }
    }

    if config.should_run(Step::Vagrant) {
        if let Ok(boxes) = vagrant::collect_boxes(&ctx) {
            for vagrant_box in boxes {
                runner.execute(Step::Vagrant, format!("Vagrant ({})", vagrant_box.smart_name()), || {
                    vagrant::topgrade_vagrant_box(&ctx, &vagrant_box)
                })?;
            }
        }
    }
    runner.execute(Step::Vagrant, "Vagrant boxes", || vagrant::upgrade_vagrant_boxes(&ctx))?;

    if !runner.report().data().is_empty() {
        print_separator(t!("Summary"));

        for (key, result) in runner.report().data() {
            print_result(key, result);
        }

        #[cfg(target_os = "linux")]
        {
            if let Ok(distribution) = &distribution {
                distribution.show_summary();
            }
        }
    }

    let mut post_command_failed = false;
    if let Some(commands) = config.post_commands() {
        for (name, command) in commands {
            if generic::run_custom_command(name, command, &ctx).is_err() {
                post_command_failed = true;
            }
        }
    }

    if config.keep_at_end() {
        print_info(t!("\n(R)eboot\n(S)hell\n(Q)uit"));
        loop {
            match get_key() {
                Ok(Key::Char('s' | 'S')) => {
                    run_shell().context("Failed to execute shell")?;
                }
                Ok(Key::Char('r' | 'R')) => {
                    reboot().context("Failed to reboot")?;
                }
                Ok(Key::Char('q' | 'Q')) => (),
                _ => {
                    continue;
                }
            }
            break;
        }
    }

    let failed = post_command_failed || runner.report().data().iter().any(|(_, result)| result.failed());

    if !config.skip_notify() {
        notify_desktop(
            if failed {
                t!("Topgrade finished with errors")
            } else {
                t!("Topgrade finished successfully")
            },
            Some(Duration::from_secs(10)),
        );
    }

    if failed {
        Err(StepFailed.into())
    } else {
        Ok(())
    }
}

fn main() {
    match run() {
        Ok(()) => {
            exit(0);
        }
        Err(error) => {
            #[cfg(all(windows, feature = "self-update"))]
            {
                if let Some(Upgraded(status)) = error.downcast_ref::<Upgraded>() {
                    exit(status.code().unwrap());
                }
            }

            let skip_print = (error.downcast_ref::<StepFailed>().is_some())
                || (error
                    .downcast_ref::<io::Error>()
                    .filter(|io_error| io_error.kind() == io::ErrorKind::Interrupted)
                    .is_some());

            if !skip_print {
                // The `Debug` implementation of `eyre::Result` prints a multi-line
                // error message that includes all the 'causes' added with
                // `.with_context(...)` calls.
                println!("{}", t!("Error: {error}", error = format!("{:?}", error)));
            }
            exit(1);
        }
    }
}
