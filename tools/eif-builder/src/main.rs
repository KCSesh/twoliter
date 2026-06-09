// SPDX-License-Identifier: Apache-2.0 OR MIT

use clap::Parser;
use eif_builder::{build_eif, DEFAULT_CMDLINE};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "eif-builder", about = "Build a minimal sidecar EIF")]
struct Args {
    /// Path to kernel (vmlinuz)
    #[arg(long)]
    kernel: PathBuf,

    /// Kernel command line
    #[arg(long, default_value = DEFAULT_CMDLINE)]
    cmdline: String,

    /// Output EIF path
    #[arg(long)]
    output: PathBuf,

    /// Default memory in bytes
    #[arg(long, default_value_t = 512 * 1024 * 1024)]
    mem: u64,

    /// Default vCPU count
    #[arg(long, default_value_t = 2)]
    cpus: u64,

    /// PCIE flags (hex)
    #[arg(long, default_value = "0240", value_parser = parse_hex_u16)]
    pcie_flags: u16,
}

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    u16::from_str_radix(s, 16).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();

    if let Err(e) = build_eif(
        &args.kernel,
        &args.cmdline,
        &args.output,
        args.mem,
        args.cpus,
        args.pcie_flags,
    ) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    println!("Created {}", args.output.display());
    ExitCode::SUCCESS
}
