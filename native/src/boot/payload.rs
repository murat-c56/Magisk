use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};

use anyhow::{anyhow, Context};
use byteorder::{BigEndian, ReadBytesExt};
use quick_protobuf::{BytesReader, MessageRead};

use base::libc::c_char;
use base::{ReadSeekExt, StrErr, Utf8CStr};
use base::{ResultExt, WriteExt};

use crate::ffi;
use crate::proto::update_metadata::mod_InstallOperation::Type;
use crate::proto::update_metadata::DeltaArchiveManifest;

macro_rules! bad_payload {
    ($msg:literal) => {
        anyhow!(concat!("invalid payload: ", $msg))
    };
    ($($args:tt)*) => {
        anyhow!("invalid payload: {}", format_args!($($args)*))
    };
}

const PAYLOAD_MAGIC: &str = "CrAU";

fn do_extract_boot_from_payload(
    in_path: &Utf8CStr,
    partition_name: Option<&Utf8CStr>,
    out_path: Option<&Utf8CStr>,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(if in_path == "-" {
        unsafe { File::from_raw_fd(0) }
    } else {
        File::open(in_path).with_context(|| format!("cannot open '{in_path}'"))?
    });

    let buf = &mut [0u8; 4];
    reader.read_exact(buf)?;

    if buf != PAYLOAD_MAGIC.as_bytes() {
        return Err(bad_payload!("invalid magic"));
    }

    let version = reader.read_u64::<BigEndian>()?;
    if version != 2 {
        return Err(bad_payload!("unsupported version: {}", version));
    }

    let manifest_len = reader.read_u64::<BigEndian>()? as usize;
    if manifest_len == 0 {
        return Err(bad_payload!("manifest length is zero"));
    }

    let manifest_sig_len = reader.read_u32::<BigEndian>()?;
    if manifest_sig_len == 0 {
        return Err(bad_payload!("manifest signature length is zero"));
    }

    let mut buf = Vec::new();
    buf.resize(manifest_len, 0u8);

    let manifest = {
        let manifest = &mut buf[..manifest_len];
        reader.read_exact(manifest)?;
        let mut br = BytesReader::from_bytes(manifest);
        DeltaArchiveManifest::from_reader(&mut br, manifest)?
    };
    if manifest.get_minor_version() != 0 {
        return Err(bad_payload!(
            "delta payloads are not supported, please use a full payload file"
        ));
    }

    let block_size = manifest.get_block_size() as u64;

    let partition = match partition_name {
        None => {
            let boot = manifest
                .partitions
                .iter()
                .find(|p| p.partition_name == "init_boot");
            let boot = match boot {
                Some(boot) => Some(boot),
                None => manifest
                    .partitions
                    .iter()
                    .find(|p| p.partition_name == "boot"),
            };
            boot.ok_or(anyhow!("boot partition not found"))?
        }
        Some(name) => manifest
            .partitions
            .iter()
            .find(|p| p.partition_name.as_str() == name)
            .ok_or(anyhow!("partition '{name}' not found"))?,
    };

    let out_str: String;
    let out_path = match out_path {
        None => {
            out_str = format!("{}.img", partition.partition_name);
            out_str.as_str()
        }
        Some(s) => s,
    };

    let mut out_file =
        File::create(out_path).with_context(|| format!("cannot write to '{out_path}'"))?;

    // Skip the manifest signature
    reader.skip(manifest_sig_len as usize)?;

    // Sort the install operations with data_offset so we will only ever need to seek forward
    // This makes it possible to support non-seekable input file descriptors
    let mut operations = partition.operations.clone();
    operations.sort_by_key(|e| e.data_offset.unwrap_or(0));
    let mut curr_data_offset: u64 = 0;

    for operation in operations.iter() {
        let data_len = operation
            .data_length
            .ok_or(bad_payload!("data length not found"))? as usize;

        let data_offset = operation
            .data_offset
            .ok_or(bad_payload!("data offset not found"))?;

        let data_type = operation.type_pb;

        buf.resize(data_len, 0u8);
        let data = &mut buf[..data_len];

        // Skip to the next offset and read data
        let skip = data_offset - curr_data_offset;
        reader.skip(skip as usize)?;
        reader.read_exact(data)?;
        curr_data_offset = data_offset + data_len as u64;

        let out_offset = operation
            .dst_extents
            .get(0)
            .ok_or(bad_payload!("dst extents not found"))?
            .start_block
            .ok_or(bad_payload!("start block not found"))?
            * block_size;

        match data_type {
            Type::REPLACE => {
                out_file.seek(SeekFrom::Start(out_offset))?;
                out_file.write_all(data)?;
            }
            Type::ZERO => {
                for ext in operation.dst_extents.iter() {
                    let out_seek = ext
                        .start_block
                        .ok_or(bad_payload!("start block not found"))?
                        * block_size;
                    let num_blocks = ext.num_blocks.ok_or(bad_payload!("num blocks not found"))?;
                    out_file.seek(SeekFrom::Start(out_seek))?;
                    out_file.write_zeros(num_blocks as usize)?;
                }
            }
            Type::REPLACE_BZ | Type::REPLACE_XZ => {
                out_file.seek(SeekFrom::Start(out_offset))?;
                if !ffi::decompress(data, out_file.as_raw_fd()) {
                    return Err(bad_payload!("decompression failed"));
                }
            }
            _ => return Err(bad_payload!("unsupported operation type")),
        };
    }

    Ok(())
}

pub fn extract_boot_from_payload(
    in_path: *const c_char,
    partition: *const c_char,
    out_path: *const c_char,
) -> bool {
    fn inner(
        in_path: *const c_char,
        partition: *const c_char,
        out_path: *const c_char,
    ) -> anyhow::Result<()> {
        let in_path = unsafe { Utf8CStr::from_ptr(in_path) }?;
        let partition = match unsafe { Utf8CStr::from_ptr(partition) } {
            Ok(s) => Some(s),
            Err(StrErr::NullPointerError) => None,
            Err(e) => Err(e)?,
        };
        let out_path = match unsafe { Utf8CStr::from_ptr(out_path) } {
            Ok(s) => Some(s),
            Err(StrErr::NullPointerError) => None,
            Err(e) => Err(e)?,
        };
        do_extract_boot_from_payload(in_path, partition, out_path)
            .context("Failed to extract from payload")?;
        Ok(())
    }
    inner(in_path, partition, out_path).log().is_ok()
}
