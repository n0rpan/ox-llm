//! Publish bounded final/worker reports without exposing a partial file.
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub(crate) fn save_worker(
    directory: &Path,
    run: &str,
    report: &crate::a2a_run::A2AWorkerReport,
) -> io::Result<String> {
    #[derive(serde::Serialize)]
    struct Envelope<'a> {
        version: u32,
        run: &'a str,
        report: &'a crate::a2a_run::A2AWorkerReport,
    }
    publish(
        directory,
        &format!("worker-{}.json", report.worker),
        &Envelope {
            version: 1,
            run,
            report,
        },
        MAX_WORKER,
    )
}
pub(crate) const MAX_WORKER: usize = 64 * 1024 * 1024;

fn publish(directory: &Path, name: &str, report: &impl serde::Serialize, limit: usize) -> io::Result<String> {
    let anchor = crate::namespace::protected_directory(directory)?;
    publish_anchored(&anchor, name, report, limit)
}

pub(crate) fn consume_resume(directory: &File, receipt: &impl serde::Serialize) -> io::Result<()> {
    publish_anchored(directory, "resume.json", receipt, 64 * 1024).map(|_| ())
}

fn publish_anchored(
    anchor: &File,
    name: &str,
    report: &impl serde::Serialize,
    limit: usize,
) -> io::Result<String> {
    if name.is_empty()
        || name.len() > 96
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(io::Error::other("invalid checkpoint name"));
    }
    let name_c = CString::new(name).unwrap();
    let temporary_name = format!(".{name}.pending");
    let temporary_c = CString::new(temporary_name.clone()).unwrap();
    let metadata = anchor.metadata()?;
    // SAFETY: geteuid only returns the current effective user identity.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "report directory must be private",
        ));
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(format!("/proc/self/fd/{}", anchor.as_raw_fd()))?;
    let parent = format!("/proc/self/fd/{}", directory.as_raw_fd());
    let temporary = Path::new(&parent).join(temporary_name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    let pending = Pending(temporary);
    let mut writer = Bounded {
        file,
        remaining: limit,
        digest: Sha256::new(),
    };
    report.serialize(&mut serde_json::Serializer::with_formatter(
        &mut writer,
        ControlSafeJson,
    ))?;
    writer.write_all(b"\n")?;
    writer.file.sync_all()?;
    // SAFETY: live directory descriptor and valid NUL-terminated file names.
    let result = unsafe {
        libc::renameat2(
            directory.as_raw_fd(),
            temporary_c.as_ptr(),
            directory.as_raw_fd(),
            name_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // A sync failure can leave a complete published report. Never remove it or
    // turn a storage failure into permission to repeat inference.
    directory.sync_all()?;
    drop(pending);
    Ok(format!("{:x}", writer.digest.finalize()))
}

// Preserve control characters as JSON data, including Unicode C1 controls.
struct ControlSafeJson;
impl serde_json::ser::Formatter for ControlSafeJson {
    fn write_string_fragment<W: Write + ?Sized>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()> {
        let mut start = 0;
        for (index, ch) in fragment.char_indices() {
            if ch.is_control() {
                writer.write_all(&fragment.as_bytes()[start..index])?;
                write!(writer, "\\u{:04x}", ch as u32)?;
                start = index + ch.len_utf8();
            }
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}

struct Pending(std::path::PathBuf);
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
struct Bounded {
    file: File,
    remaining: usize,
    digest: Sha256,
}
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("final report exceeds the checkpoint bound"));
        }
        let written = self.file.write(bytes)?;
        self.digest.update(&bytes[..written]);
        self.remaining -= written;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    #[test]
    fn publication_is_private_complete_and_never_replaces_existing_data() {
        let env = TestEnv::new();
        let root = env.root().join("checkpoint");
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let report = serde_json::json!({"result":"terminal\u{001b}\u{009d} data"});
        publish(&root, "report.json", &report, MAX_WORKER).unwrap();
        let bytes = fs::read(root.join("report.json")).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            report
        );
        assert!(!bytes.contains(&0x1b));
        assert_eq!(
            fs::metadata(root.join("report.json")).unwrap().mode() & 0o777,
            0o600
        );
        assert!(publish(
            &root,
            "report.json",
            &serde_json::json!({"replacement":true}),
            MAX_WORKER
        )
        .is_err());
        assert_eq!(fs::read(root.join("report.json")).unwrap(), bytes);
        assert!(!root.join(".report.json.pending").exists());
    }

    #[test]
    fn serialization_failure_and_unsafe_paths_never_publish_or_overwrite() {
        struct Fails;
        impl serde::Serialize for Fails {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("synthetic serialization failure"))
            }
        }
        let env = TestEnv::new();
        let root = env.root().join("checkpoint");
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        assert!(publish(&root, "report.json", &Fails, MAX_WORKER).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        let target = env.root().join("keep");
        fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, root.join(".report.json.pending")).unwrap();
        assert!(publish(&root, "report.json", &serde_json::json!({}), MAX_WORKER).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert!(fs::symlink_metadata(root.join(".report.json.pending"))
            .unwrap()
            .file_type()
            .is_symlink());
        fs::remove_file(root.join(".report.json.pending")).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(publish(&root, "report.json", &serde_json::json!({}), MAX_WORKER).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn output_bound_is_enforced_before_writing_excess_bytes() {
        let env = TestEnv::new();
        let path = env.root().join("bounded");
        let mut writer = Bounded {
            file: File::create(&path).unwrap(),
            remaining: 4,
            digest: Sha256::new(),
        };
        writer.write_all(b"123").unwrap();
        assert!(writer.write_all(b"45").is_err());
        writer.write_all(b"4").unwrap();
        assert!(writer.write_all(b"5").is_err());
        assert_eq!(fs::read(path).unwrap(), b"1234");
    }
    #[test]
    fn partial_serialization_and_expanded_controls_never_expose_a_partial_checkpoint() {
        struct Partial;
        impl serde::Serialize for Partial {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("written", &"prefix")?;
                Err(serde::ser::Error::custom("synthetic failure after bytes"))
            }
        }
        let env = TestEnv::new();
        let root = env.root().join("checkpoint");
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        assert!(publish(&root, "partial.json", &Partial, 1024).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        assert!(publish(&root, "controls.json", &"\u{009d}".repeat(10), 16).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        let target = env.write("retained", "operator-owned");
        std::os::unix::fs::symlink(&target, root.join("final.json")).unwrap();
        assert!(publish(&root, "final.json", &"new", 1024).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "operator-owned");
        assert!(fs::symlink_metadata(root.join("final.json"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
    #[test]
    fn actual_partial_storage_error_never_publishes_or_retains_a_writer() {
        const CHILD: &str = "OXA_CHECKPOINT_PARTIAL_IO_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "checkpoint::tests::actual_partial_storage_error_never_publishes_or_retains_a_writer",
                    "--nocapture",
                ])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "child output: {output:?}");
            return;
        }
        let env = TestEnv::new();
        let root = env.root().join("checkpoint");
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let limits = libc::rlimit {
            rlim_cur: 16,
            rlim_max: 16,
        };
        // SAFETY: resource/signal changes are confined to this disposable child.
        unsafe {
            assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limits), 0);
        }
        assert!(publish(&root, "partial.json", &"x".repeat(1024), 2048).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        // A fresh small publication succeeds: failed output owns no live writer
        // and reserves no destination. This does not authorize inference replay.
        publish(&root, "partial.json", &"ok", 16).unwrap();
        assert_eq!(fs::read(root.join("partial.json")).unwrap(), b"\"ok\"\n");
    }
}
