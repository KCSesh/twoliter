/*!
Shared vendoring abstraction for Go and Rust modules.

This module provides a unified implementation for vendoring dependencies
from upstream tar archives. Language-specific behavior is controlled via
`VendorConfig` structs.
*/

pub(crate) mod error;

use buildsys::manifest;
use duct::cmd;
use error::Result;
use filetime::{set_file_mtime, FileTime};
use snafu::{ensure, OptionExt, ResultExt};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::{env, fs};

/// Configuration for language-specific vendoring behavior.
pub(crate) struct VendorConfig {
    pub script_name: &'static str,
    pub script_template: &'static str,
    pub docker_tool: &'static str,
    pub cache_dir: &'static str,
    pub cache_arg_name: &'static str,
}

// Go vendoring configuration
const GO_SCRIPT_TMPL: &str = r#"#!/bin/bash

set -e

toplevel=$(tar tf "__LOCAL_FILE_NAME__" | head -1)
if [ -z "__MOD_DIR__" ] ; then
    targetdir="${toplevel}"
else
    targetdir="__MOD_DIR__"
fi

tar xf "__LOCAL_FILE_NAME__"

pushd "${targetdir}"
    go list -mod=readonly ./... >/dev/null && go mod vendor
popd

tar czf "__OUTPUT__" "${targetdir}"/vendor
rm -rf "${targetdir}"
touch -r "__LOCAL_FILE_NAME__" "__OUTPUT__"
"#;

pub(crate) const GO_CONFIG: VendorConfig = VendorConfig {
    script_name: "docker-go-script.sh",
    script_template: GO_SCRIPT_TMPL,
    docker_tool: "docker-go",
    cache_dir: ".gomodcache",
    cache_arg_name: "--go-mod-cache",
};

// Rust vendoring configuration
const RUST_SCRIPT_TMPL: &str = r#"#!/bin/bash

set -e

toplevel=$(tar tf "__LOCAL_FILE_NAME__" | head -1)
if [ -z "__MOD_DIR__" ] ; then
    targetdir="${toplevel}"
else
    targetdir="__MOD_DIR__"
fi

tar xf "__LOCAL_FILE_NAME__"

pushd "${targetdir}"
    mkdir -p .cargo
    cargo metadata --locked --format-version 1 >/dev/null && cargo vendor --locked > .cargo/config.toml
    cargo deny --config /deny.toml check --disable-fetch licenses bans sources
popd

tar czf "__OUTPUT__" -C "${targetdir}" vendor .cargo/config.toml
rm -rf "${targetdir}"
touch -r "__LOCAL_FILE_NAME__" "__OUTPUT__"
"#;

pub(crate) const RUST_CONFIG: VendorConfig = VendorConfig {
    script_name: "docker-cargo-script.sh",
    script_template: RUST_SCRIPT_TMPL,
    docker_tool: "docker-cargo",
    cache_dir: ".cargo",
    cache_arg_name: "--cargo-home",
};

pub(crate) struct VendorMod;

impl VendorMod {
    pub(crate) fn vendor(
        config: &VendorConfig,
        root_dir: &Path,
        package_dir: &Path,
        external_file: &manifest::ExternalFile,
        sdk: &str,
        mtime: FileTime,
    ) -> Result<()> {
        let url_file_name = extract_file_name(&external_file.url)?;
        let local_file_name = external_file.path.as_ref().unwrap_or(&url_file_name);
        ensure!(
            local_file_name.components().count() == 1,
            error::InputFileSnafu
        );

        let full_path = package_dir.join(local_file_name);
        ensure!(
            full_path.is_file(),
            error::InputFileBadSnafu { path: full_path }
        );

        let default_empty_path = PathBuf::from("");
        let mod_dir = external_file
            .bundle_root_path
            .as_ref()
            .unwrap_or(&default_empty_path);

        let default_output_path =
            PathBuf::from(format!("bundled-{}", local_file_name.to_string_lossy()));
        let output_path_arg = external_file
            .bundle_output_path
            .as_ref()
            .unwrap_or(&default_output_path);
        println!(
            "cargo:rerun-if-changed={}",
            output_path_arg.to_string_lossy()
        );

        let script_contents = config
            .script_template
            .replace("__LOCAL_FILE_NAME__", &local_file_name.to_string_lossy())
            .replace("__MOD_DIR__", &mod_dir.to_string_lossy())
            .replace("__OUTPUT__", &output_path_arg.to_string_lossy());
        let script_path = package_dir.join(config.script_name);

        {
            let mut script_file = fs::File::create(&script_path)
                .context(error::CreateFileSnafu { path: &script_path })?;
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o777))
                .context(error::SetFilePermissionsSnafu { path: &script_path })?;
            script_file
                .write_all(script_contents.as_bytes())
                .context(error::WriteFileSnafu { path: &script_path })?;
        }

        let res = run_docker_tool(
            config,
            package_dir,
            sdk,
            &root_dir.join(config.cache_dir),
            &format!("./{}", config.script_name),
            root_dir,
        );
        fs::remove_file(&script_path).context(error::RemoveFileSnafu { path: &script_path })?;

        if res.is_ok() {
            set_file_mtime(output_path_arg, mtime).context(error::SetMtimeSnafu {
                path: output_path_arg,
            })?;
        }

        res
    }
}

fn extract_file_name(url: &str) -> Result<PathBuf> {
    let parsed = reqwest::Url::parse(url).context(error::InputUrlSnafu { url })?;
    let name = parsed
        .path_segments()
        .context(error::InputFileBadSnafu { path: url })?
        .next_back()
        .context(error::InputFileBadSnafu { path: url })?;
    Ok(name.into())
}

fn run_docker_tool(
    config: &VendorConfig,
    module_path: &Path,
    sdk_image: &str,
    cache_dir: &Path,
    command: &str,
    root_dir: &Path,
) -> Result<()> {
    let deny_config_path = root_dir.join("sources").join("deny.toml");
    ensure!(
        deny_config_path.is_file(),
        error::InputFileBadSnafu {
            path: &deny_config_path
        }
    );
    let deny_config_str = deny_config_path.to_string_lossy().to_string();

    let mut args = vec![
        "--module-path",
        module_path.to_str().context(error::InputFileSnafu)?,
        "--sdk-image",
        sdk_image,
        config.cache_arg_name,
        cache_dir.to_str().context(error::InputFileSnafu)?,
        "--deny-config",
        &deny_config_str,
    ];

    args.push("--command");
    args.push(command);

    let arg_string = args.join(" ");
    let twoliter_tools_dir = env::var("TWOLITER_TOOLS_DIR").context(error::EnvironmentSnafu {
        var: "TWOLITER_TOOLS_DIR",
    })?;
    let program = PathBuf::from(twoliter_tools_dir).join(config.docker_tool);
    println!("program: {}", program.to_string_lossy());
    let output = cmd(program, args)
        .stderr_to_stdout()
        .stdout_capture()
        .unchecked()
        .run()
        .context(error::CommandStartSnafu)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{}", &stdout);
    ensure!(
        output.status.success(),
        error::DockerExecutionSnafu {
            tool: config.docker_tool,
            args: arg_string
        }
    );
    Ok(())
}
