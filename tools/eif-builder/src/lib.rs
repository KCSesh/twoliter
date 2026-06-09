// SPDX-License-Identifier: Apache-2.0 OR MIT
//! EIF (Enclave Image Format) builder.
//!
//! Builds a minimal sidecar EIF: kernel + cmdline + empty ramdisk + metadata.
//! The rootfs is a separate erofs artifact attached as virtio-blk at launch.
//!
//! All multi-byte fields are big-endian.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crc32fast::Hasher as Crc32Hasher;

const EIF_MAGIC: [u8; 4] = [0x2e, 0x65, 0x69, 0x66]; // ".eif"
const EIF_HDR_VERSION: u16 = 4;

const EIF_SECTION_KERNEL: u16 = 1;
const EIF_SECTION_CMDLINE: u16 = 2;
const EIF_SECTION_RAMDISK: u16 = 3;
const EIF_SECTION_METADATA: u16 = 5;

const EIF_ARCH_X86_64: u16 = 0;
const EIF_ARCH_AARCH64: u16 = 1;

const MAX_NUM_SECTIONS: usize = 32;
const EIF_HEADER_SIZE: usize = 548;
const EIF_CRC32_OFFSET: usize = EIF_HEADER_SIZE - 4;
const EIF_SECTION_HEADER_SIZE: usize = 12;

/// PCIE flag constants.
pub const EIF_HDR_FLAG_PCIE: u16 = 1 << 6;
pub const EIF_HDR_FLAG_PCIE_VIRTIO: u16 = 1 << 9;

/// Default PCIE flags for sidecar mode.
pub const DEFAULT_PCIE_FLAGS: u16 = EIF_HDR_FLAG_PCIE | EIF_HDR_FLAG_PCIE_VIRTIO;

/// Default kernel command line for block device boot.
pub const DEFAULT_CMDLINE: &str = "initcall_blacklist=i8042_init console=ttyS0 root=/dev/vda rw";

#[derive(Debug)]
pub enum EifError {
    ReadKernel(io::Error),
    WriteOutput(io::Error),
    EmptyKernel,
}

impl std::fmt::Display for EifError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadKernel(e) => write!(f, "failed to read kernel: {e}"),
            Self::WriteOutput(e) => write!(f, "failed to write EIF: {e}"),
            Self::EmptyKernel => write!(f, "kernel image is empty"),
        }
    }
}

impl std::error::Error for EifError {}

fn eif_arch_flags() -> u16 {
    if cfg!(target_arch = "aarch64") {
        EIF_ARCH_AARCH64
    } else {
        EIF_ARCH_X86_64
    }
}

fn build_metadata() -> String {
    r#"{"ImageName":"bottlerocket-sidecar","ImageVersion":"","BuildMetadata":{},"DockerInfo":{},"CustomMetadata":null}"#.to_string()
}

fn write_section(eif: &mut Vec<u8>, section_type: u16, data: &[u8]) {
    eif.extend_from_slice(&section_type.to_be_bytes());
    eif.extend_from_slice(&0u16.to_be_bytes()); // flags
    eif.extend_from_slice(&(data.len() as u64).to_be_bytes());
    eif.extend_from_slice(data);
}

/// Build and write an EIF to the specified output path.
pub fn build_eif(
    kernel_path: &Path,
    cmdline: &str,
    output_path: &Path,
    default_mem: u64,
    default_cpus: u64,
    pcie_flags: u16,
) -> Result<(), EifError> {
    let kernel_data = fs::read(kernel_path).map_err(EifError::ReadKernel)?;
    if kernel_data.is_empty() {
        return Err(EifError::EmptyKernel);
    }

    let cmdline_data = cmdline.as_bytes();
    let ramdisk_data: &[u8] = &[]; // Empty for blkdev mode
    let metadata = build_metadata();
    let metadata_data = metadata.as_bytes();

    let num_sections: u16 = 4;

    // Calculate total size
    let sections_size = (EIF_SECTION_HEADER_SIZE * num_sections as usize)
        + kernel_data.len()
        + cmdline_data.len()
        + ramdisk_data.len()
        + metadata_data.len();
    let total_size = EIF_HEADER_SIZE + sections_size;

    let mut eif = Vec::with_capacity(total_size);

    // --- Header ---
    eif.extend_from_slice(&EIF_MAGIC);
    eif.extend_from_slice(&EIF_HDR_VERSION.to_be_bytes());
    eif.extend_from_slice(&(eif_arch_flags() | pcie_flags).to_be_bytes());
    eif.extend_from_slice(&default_mem.to_be_bytes());
    eif.extend_from_slice(&default_cpus.to_be_bytes());
    eif.extend_from_slice(&0u16.to_be_bytes()); // reserved
    eif.extend_from_slice(&num_sections.to_be_bytes());

    // Section offsets
    let kernel_offset = EIF_HEADER_SIZE as u64;
    let cmdline_offset = kernel_offset + EIF_SECTION_HEADER_SIZE as u64 + kernel_data.len() as u64;
    let ramdisk_offset = cmdline_offset + EIF_SECTION_HEADER_SIZE as u64 + cmdline_data.len() as u64;
    let metadata_offset = ramdisk_offset + EIF_SECTION_HEADER_SIZE as u64 + ramdisk_data.len() as u64;

    let offsets = [kernel_offset, cmdline_offset, ramdisk_offset, metadata_offset];
    for i in 0..MAX_NUM_SECTIONS {
        let val = if i < offsets.len() { offsets[i] } else { 0 };
        eif.extend_from_slice(&val.to_be_bytes());
    }

    // Section sizes
    let sizes = [
        kernel_data.len() as u64,
        cmdline_data.len() as u64,
        ramdisk_data.len() as u64,
        metadata_data.len() as u64,
    ];
    for i in 0..MAX_NUM_SECTIONS {
        let val = if i < sizes.len() { sizes[i] } else { 0 };
        eif.extend_from_slice(&val.to_be_bytes());
    }

    eif.extend_from_slice(&0u32.to_be_bytes()); // unused
    eif.extend_from_slice(&0u32.to_be_bytes()); // crc32 placeholder

    debug_assert_eq!(eif.len(), EIF_HEADER_SIZE);

    // --- Sections ---
    write_section(&mut eif, EIF_SECTION_KERNEL, &kernel_data);
    write_section(&mut eif, EIF_SECTION_CMDLINE, cmdline_data);
    write_section(&mut eif, EIF_SECTION_RAMDISK, ramdisk_data);
    write_section(&mut eif, EIF_SECTION_METADATA, metadata_data);

    // Compute CRC32 (exclude CRC field itself)
    let mut hasher = Crc32Hasher::new();
    hasher.update(&eif[..EIF_CRC32_OFFSET]);
    hasher.update(&eif[EIF_CRC32_OFFSET + 4..]);
    let crc = hasher.finalize();
    eif[EIF_CRC32_OFFSET..EIF_CRC32_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());

    // Write to file
    let mut file = fs::File::create(output_path).map_err(EifError::WriteOutput)?;
    file.write_all(&eif).map_err(EifError::WriteOutput)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_build_eif() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("kernel");
        let output = dir.path().join("test.eif");

        fs::write(&kernel, b"FAKE_KERNEL").unwrap();

        build_eif(&kernel, "console=ttyS0", &output, 512 << 20, 2, DEFAULT_PCIE_FLAGS).unwrap();

        let eif = fs::read(&output).unwrap();
        assert_eq!(&eif[0..4], &EIF_MAGIC);
        assert_eq!(u16::from_be_bytes([eif[4], eif[5]]), EIF_HDR_VERSION);

        // Verify CRC
        let stored_crc = u32::from_be_bytes(eif[EIF_CRC32_OFFSET..EIF_CRC32_OFFSET + 4].try_into().unwrap());
        let mut hasher = Crc32Hasher::new();
        hasher.update(&eif[..EIF_CRC32_OFFSET]);
        hasher.update(&eif[EIF_CRC32_OFFSET + 4..]);
        assert_eq!(hasher.finalize(), stored_crc);
    }

    #[test]
    fn test_empty_kernel_error() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("kernel");
        let output = dir.path().join("test.eif");

        fs::write(&kernel, b"").unwrap();

        let result = build_eif(&kernel, "", &output, 512 << 20, 2, 0);
        assert!(matches!(result, Err(EifError::EmptyKernel)));
    }
}
