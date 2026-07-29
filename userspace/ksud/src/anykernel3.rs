use std::{
    ffi::OsString,
    fmt::Display,
    io::{self, BufRead, Write},
    path::Path,
};

use anyhow::{Result, bail};
use clap::ValueEnum;

const UPDATE_BINARY_ENTRY: &str = "META-INF/com/google/android/update-binary";
const PATCH_MARKER: &[u8] = b"chmod -R 755 tools bin;";

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    #[cfg(target_os = "android")]
    const fn suffix(self) -> &'static str {
        match self {
            Self::A => "_a",
            Self::B => "_b",
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

enum InstallerOutputLine<'a> {
    UserInterface(&'a [u8]),
    Console(&'a [u8]),
}

fn classify_installer_output(line: &[u8]) -> Option<InstallerOutputLine<'_>> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let first_non_whitespace = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(line.len());
    let command = &line[first_non_whitespace..];

    if command == b"ui_print" {
        return None;
    }
    if let Some(message) = command.strip_prefix(b"ui_print ") {
        return Some(InstallerOutputLine::UserInterface(message));
    }
    Some(InstallerOutputLine::Console(line))
}

fn write_output_line<W: Write>(writer: &mut W, line: &[u8]) -> io::Result<()> {
    writer.write_all(line)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn forward_installer_output<R: BufRead, U: Write, C: Write>(
    mut reader: R,
    mut user_interface: U,
    mut console: C,
) -> io::Result<()> {
    let mut line = Vec::new();
    let mut user_interface_error = None;
    let mut console_error = None;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Some(output) = classify_installer_output(&line) else {
            continue;
        };
        match output {
            InstallerOutputLine::UserInterface(message) if user_interface_error.is_none() => {
                user_interface_error = write_output_line(&mut user_interface, message).err();
            }
            InstallerOutputLine::Console(message) if console_error.is_none() => {
                console_error = write_output_line(&mut console, message).err();
            }
            _ => {}
        }
    }
    user_interface_error.or(console_error).map_or(Ok(()), Err)
}

fn patch_update_binary(script: &[u8], mkbootfs: &Path) -> Result<Vec<u8>> {
    let matches = script
        .windows(PATCH_MARKER.len())
        .enumerate()
        .filter_map(|(index, candidate)| (candidate == PATCH_MARKER).then_some(index))
        .collect::<Vec<_>>();

    if matches.is_empty() {
        bail!("AnyKernel3 update-binary does not contain the mkbootfs injection marker");
    }
    if matches.len() != 1 {
        bail!("AnyKernel3 update-binary contains multiple mkbootfs injection markers");
    }

    let mkbootfs = mkbootfs
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("mkbootfs path is not valid UTF-8"))?;
    if mkbootfs.contains('\0') {
        bail!("mkbootfs path contains a NUL byte");
    }
    let injection = format!(
        "cp -f {} \"$AKHOME/tools/mkbootfs\" || exit 1; ",
        shell_quote(mkbootfs)
    );

    let marker = matches[0];
    let mut patched = Vec::with_capacity(script.len() + injection.len());
    patched.extend_from_slice(&script[..marker]);
    patched.extend_from_slice(injection.as_bytes());
    patched.extend_from_slice(&script[marker..]);
    Ok(patched)
}

fn select_update_binary<I, S>(entries: I) -> Result<usize>
where
    I: IntoIterator<Item = (usize, S)>,
    S: AsRef<str>,
{
    let matches = entries
        .into_iter()
        .filter_map(|(index, name)| (name.as_ref() == UPDATE_BINARY_ENTRY).then_some(index))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [index] => Ok(*index),
        [] => bail!("ZIP does not contain {UPDATE_BINARY_ENTRY}"),
        _ => bail!("ZIP contains multiple {UPDATE_BINARY_ENTRY} entries"),
    }
}

fn combine_results(
    primary: Result<()>,
    secondary: Result<()>,
    secondary_label: &str,
) -> Result<()> {
    match (primary, secondary) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(anyhow::anyhow!("{secondary_label}: {error:#}")),
        (Err(primary), Err(secondary)) => Err(anyhow::anyhow!(
            "{primary:#}; additionally, {secondary_label}: {secondary:#}"
        )),
    }
}

fn installer_arguments(zip_path: &Path, slot: Option<Slot>) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("3"),
        OsString::from("1"),
        zip_path.as_os_str().to_owned(),
    ];
    if let Some(slot) = slot {
        arguments.push(OsString::from(match slot {
            Slot::A => "a",
            Slot::B => "b",
        }));
    }
    arguments
}

fn ensure_installer_success(success: bool, status: impl Display) -> Result<()> {
    if !success {
        bail!("AnyKernel3 update-binary exited with status {status}");
    }
    Ok(())
}

fn run_then_restore<P, R>(primary: P, restore: R, restore_label: &str) -> Result<()>
where
    P: FnOnce() -> Result<()>,
    R: FnOnce() -> Result<()>,
{
    let primary_result = primary();
    let restore_result = restore();
    combine_results(primary_result, restore_result, restore_label)
}

#[cfg(target_os = "android")]
mod android {
    use std::{
        fs::{self, File},
        io::{BufReader, Read},
        path::{Path, PathBuf},
        process::{Command, Stdio},
    };

    use anyhow::{Context, Result, ensure};
    use tempfile::{Builder, TempDir};

    use crate::{
        android::{resetprop, utils},
        anykernel3::{
            Slot, UPDATE_BINARY_ENTRY, combine_results, ensure_installer_success,
            forward_installer_output, installer_arguments, patch_update_binary, run_then_restore,
            select_update_binary,
        },
        assets, defs,
    };

    const MAX_UPDATE_BINARY_SIZE: u64 = 4 * 1024 * 1024;

    struct SlotOverride {
        original: String,
        restored: bool,
    }

    impl SlotOverride {
        fn apply(slot: Slot) -> Result<Self> {
            let original = resetprop::get_property_direct("ro.boot.slot_suffix")?
                .filter(|value| !value.is_empty())
                .context("ro.boot.slot_suffix is unavailable; cannot select an A/B slot")?;

            if let Err(set_error) =
                resetprop::set_property_direct("ro.boot.slot_suffix", slot.suffix())
            {
                let restore_result =
                    resetprop::set_property_direct("ro.boot.slot_suffix", &original);
                let set_result: Result<()> =
                    Err(set_error).context("failed to set the target slot");
                combine_results(
                    set_result,
                    restore_result,
                    "failed to restore ro.boot.slot_suffix after setting the target slot",
                )?;
                unreachable!();
            }

            Ok(Self {
                original,
                restored: false,
            })
        }

        fn restore(&mut self) -> Result<()> {
            resetprop::set_property_direct("ro.boot.slot_suffix", &self.original)
                .context("failed to restore ro.boot.slot_suffix")?;
            self.restored = true;
            Ok(())
        }
    }

    impl Drop for SlotOverride {
        fn drop(&mut self) {
            if !self.restored
                && let Err(error) =
                    resetprop::set_property_direct("ro.boot.slot_suffix", &self.original)
            {
                log::error!("failed to restore ro.boot.slot_suffix during drop: {error:#}");
            }
        }
    }

    fn read_update_binary(zip_path: &Path) -> Result<Vec<u8>> {
        let file = File::open(zip_path)
            .with_context(|| format!("failed to open {}", zip_path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("invalid ZIP archive {}", zip_path.display()))?;

        let mut names = Vec::with_capacity(archive.len());
        for index in 0..archive.len() {
            let entry = archive
                .by_index(index)
                .with_context(|| format!("failed to inspect ZIP entry {index}"))?;
            names.push((index, entry.name().to_owned()));
        }
        let index = select_update_binary(names.iter().map(|(index, name)| (*index, name)))?;

        let mut entry = archive
            .by_index(index)
            .context("failed to open AnyKernel3 update-binary")?;
        ensure!(
            !entry.is_dir(),
            "{UPDATE_BINARY_ENTRY} is a directory instead of a script"
        );
        ensure!(
            entry.size() <= MAX_UPDATE_BINARY_SIZE,
            "{UPDATE_BINARY_ENTRY} exceeds the {} byte safety limit",
            MAX_UPDATE_BINARY_SIZE
        );

        let expected_size = entry.size();
        let mut script = Vec::with_capacity(expected_size as usize);
        (&mut entry)
            .take(MAX_UPDATE_BINARY_SIZE + 1)
            .read_to_end(&mut script)
            .context("failed to read AnyKernel3 update-binary")?;
        ensure!(
            script.len() as u64 <= MAX_UPDATE_BINARY_SIZE,
            "{UPDATE_BINARY_ENTRY} exceeds the {} byte safety limit",
            MAX_UPDATE_BINARY_SIZE
        );
        ensure!(
            script.len() as u64 == expected_size,
            "AnyKernel3 update-binary was truncated while reading"
        );
        ensure!(
            !script.contains(&0),
            "AnyKernel3 update-binary contains a NUL byte"
        );
        Ok(script)
    }

    fn prepare(temp_dir: &TempDir, zip_path: &Path) -> Result<PathBuf> {
        eprintln!("- Preparing AnyKernel3 package");
        let script = read_update_binary(zip_path)?;
        let patched = patch_update_binary(&script, Path::new(assets::MKBOOTFS_PATH))?;
        assets::ensure_binaries(false).context("failed to extract embedded binary assets")?;
        fs::create_dir_all(temp_dir.path().join("tmp"))
            .context("failed to create the AnyKernel3 POSTINSTALL tmp directory")?;

        let update_binary = temp_dir
            .path()
            .join("META-INF/com/google/android/update-binary");
        fs::create_dir_all(
            update_binary
                .parent()
                .context("update-binary path has no parent")?,
        )
        .context("failed to create AnyKernel3 script directory")?;
        fs::write(&update_binary, patched).context("failed to write patched update-binary")?;
        Ok(update_binary)
    }

    fn run_installer(
        temp_dir: &TempDir,
        update_binary: &Path,
        zip_path: &Path,
        slot: Option<Slot>,
    ) -> Result<()> {
        eprintln!("- Running AnyKernel3 installer");
        let mut command = Command::new("/system/bin/sh");
        command
            .arg(update_binary)
            .args(installer_arguments(zip_path, slot))
            .env("POSTINSTALL", temp_dir.path())
            .env_remove("AKHOME")
            .current_dir(temp_dir.path())
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = command
            .spawn()
            .context("failed to start AnyKernel3 update-binary")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to capture AnyKernel3 update-binary output")?;
        let output_result = forward_installer_output(
            BufReader::new(stdout),
            std::io::stdout().lock(),
            std::io::stderr().lock(),
        )
        .context("failed to forward AnyKernel3 update-binary output");
        let installer_result = child
            .wait()
            .context("failed to wait for AnyKernel3 update-binary")
            .and_then(|status| ensure_installer_success(status.success(), status));
        combine_results(
            installer_result,
            output_result,
            "failed to process AnyKernel3 update-binary output",
        )
    }

    fn flash_inner(temp_dir: &TempDir, zip_path: &Path, slot: Option<Slot>) -> Result<()> {
        let update_binary = prepare(temp_dir, zip_path)?;
        let mut slot_override = slot.map(SlotOverride::apply).transpose()?;
        run_then_restore(
            || run_installer(temp_dir, &update_binary, zip_path, slot),
            || {
                slot_override
                    .as_mut()
                    .map_or_else(|| Ok(()), SlotOverride::restore)
            },
            "failed to restore the original A/B slot",
        )
    }

    pub fn flash(zip_path: &Path, slot: Option<Slot>) -> Result<()> {
        let metadata = fs::metadata(zip_path)
            .with_context(|| format!("failed to stat {}", zip_path.display()))?;
        ensure!(
            metadata.is_file(),
            "{} is not a regular file",
            zip_path.display()
        );
        let zip_path = fs::canonicalize(zip_path)
            .with_context(|| format!("failed to resolve {}", zip_path.display()))?;

        utils::ensure_dir_exists(defs::WORKING_DIR)
            .context("failed to create the KernelSU working directory")?;
        let temp_dir = Builder::new()
            .prefix("anykernel3-")
            .tempdir_in(defs::WORKING_DIR)
            .context("failed to create the AnyKernel3 working directory")?;

        let flash_result = flash_inner(&temp_dir, &zip_path, slot);
        let cleanup_result = temp_dir
            .close()
            .context("failed to remove the AnyKernel3 working directory");
        combine_results(
            flash_result,
            cleanup_result,
            "failed to clean the AnyKernel3 working directory",
        )?;
        eprintln!("- AnyKernel3 installation completed");
        Ok(())
    }
}

#[cfg(target_os = "android")]
pub use android::flash;

#[cfg(test)]
mod tests {
    use std::{cell::Cell, ffi::OsString, io::Cursor, path::Path};

    use super::{
        Slot, combine_results, ensure_installer_success, forward_installer_output,
        installer_arguments, patch_update_binary, run_then_restore, select_update_binary,
    };

    #[test]
    fn patches_the_only_supported_marker() {
        let script = b"before\n  chmod -R 755 tools bin;\nafter\n";
        let patched = patch_update_binary(script, Path::new("/data/adb/ksu/mkbootfs")).unwrap();
        let patched = String::from_utf8(patched).unwrap();
        assert_eq!(
            patched,
            "before\n  cp -f '/data/adb/ksu/mkbootfs' \"$AKHOME/tools/mkbootfs\" || exit 1; \
             chmod -R 755 tools bin;\nafter\n"
        );
    }

    #[test]
    fn quotes_the_embedded_binary_path() {
        let patched =
            patch_update_binary(PATCH_SCRIPT, Path::new("/data/it's here/mkbootfs")).unwrap();
        assert!(
            String::from_utf8(patched)
                .unwrap()
                .contains("'/data/it'\"'\"'s here/mkbootfs'")
        );
    }

    #[test]
    fn filters_recovery_output_protocol() {
        let input = b"progress 1.34 25\r\n\
                      ui_print ExampleKernel by osm0sis\r\n\
                            ui_print\r\n\
                      normal output\r\n\
                      ui_print  \r\n\
                            ui_print\r\n\
                      set_progress 0.5\r\n";
        let mut user_interface = Vec::new();
        let mut console = Vec::new();

        forward_installer_output(Cursor::new(input), &mut user_interface, &mut console).unwrap();

        assert_eq!(user_interface, b"ExampleKernel by osm0sis\n \n");
        assert_eq!(
            console,
            b"progress 1.34 25\nnormal output\nset_progress 0.5\n"
        );
    }

    #[test]
    fn rejects_missing_or_duplicate_markers() {
        assert!(patch_update_binary(b"echo no marker", Path::new("/tmp/mkbootfs")).is_err());
        assert!(
            patch_update_binary(
                b"chmod -R 755 tools bin;\nchmod -R 755 tools bin;",
                Path::new("/tmp/mkbootfs")
            )
            .is_err()
        );
    }

    #[test]
    fn requires_one_exact_update_binary_entry() {
        assert_eq!(
            select_update_binary([(0, "other"), (4, super::UPDATE_BINARY_ENTRY)]).unwrap(),
            4
        );
        assert!(
            select_update_binary([(0, "prefix/META-INF/com/google/android/update-binary")])
                .is_err()
        );
        assert!(
            select_update_binary([
                (0, super::UPDATE_BINARY_ENTRY),
                (1, super::UPDATE_BINARY_ENTRY)
            ])
            .is_err()
        );
    }

    #[test]
    fn reports_primary_and_cleanup_errors() {
        let result = combine_results(
            Err(anyhow::anyhow!("flash failed")),
            Err(anyhow::anyhow!("cleanup failed")),
            "cleanup",
        );
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("flash failed"));
        assert!(message.contains("cleanup failed"));
    }

    #[test]
    fn appends_only_the_requested_slot_argument() {
        let zip = Path::new("/data/local/tmp/kernel image.zip");
        assert_eq!(
            installer_arguments(zip, None),
            vec![
                OsString::from("3"),
                OsString::from("1"),
                zip.as_os_str().to_owned()
            ]
        );
        assert_eq!(
            installer_arguments(zip, Some(Slot::B)).last(),
            Some(&OsString::from("b"))
        );
    }

    #[test]
    fn rejects_a_failed_installer_exit() {
        let error = ensure_installer_success(false, "exit code 7").unwrap_err();
        assert!(error.to_string().contains("exit code 7"));
        ensure_installer_success(true, "exit code 0").unwrap();
    }

    #[test]
    fn restores_the_slot_after_installer_failure() {
        let restored = Cell::new(false);
        let result = run_then_restore(
            || Err(anyhow::anyhow!("installer failed")),
            || {
                restored.set(true);
                Ok(())
            },
            "slot restoration",
        );
        assert!(result.is_err());
        assert!(restored.get());
    }

    const PATCH_SCRIPT: &[u8] = b"chmod -R 755 tools bin;";
}
