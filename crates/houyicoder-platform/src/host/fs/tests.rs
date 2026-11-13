//! The lexical guard, the temp name, and the retry schedule, over the inputs
//! the transaction sees.

use std::time::Duration;

#[cfg(not(target_os = "windows"))]
use std::{env::temp_dir, fs, process::id};

use super::{
    FileIdentity, LexicalRejection, backoff_schedule, reject, temp_name_for, translate_posix_drive,
};

/// A UNC path is rejected before any handle reaches the network.
#[test]
fn test_unc_rejected() {
    assert_eq!(
        reject(r"\\evil\share\a"),
        Some(LexicalRejection::UncShare),
        "a backslash UNC is a network path"
    );
    assert_eq!(
        reject("//evil/share/a"),
        Some(LexicalRejection::UncShare),
        "a forward-slash UNC is still a network path"
    );
}

/// A reserved device name is rejected whether or not it carries an extension,
/// because the kernel reads the device from the stem the way this does.
#[test]
fn test_reserved_device_rejected() {
    assert_eq!(
        reject("NUL"),
        Some(LexicalRejection::ReservedDevice),
        "NUL is a device, not a file"
    );
    assert_eq!(
        reject("NUL.txt"),
        Some(LexicalRejection::ReservedDevice),
        "the extension does not rescue a device name"
    );
    assert_eq!(
        reject(r"C:\path\COM1"),
        Some(LexicalRejection::ReservedDevice),
        "a device under directories is still a device"
    );
    assert_eq!(
        reject("con"),
        Some(LexicalRejection::ReservedDevice),
        "the check is case-insensitive"
    );
}

/// An alternate data stream names a fork the transaction does not replace.
#[test]
fn test_data_stream_rejected() {
    assert_eq!(
        reject("a.txt:stream"),
        Some(LexicalRejection::AlternateDataStream),
        "a colon after the name is a stream fork"
    );
    assert_eq!(
        reject(r"C:\logs\app.log:$DATA"),
        Some(LexicalRejection::AlternateDataStream),
        "a stream under a drive is still a stream"
    );
}

/// A plain local path, with a drive letter, passes the lexical gate.
#[test]
fn test_local_path_passes() {
    assert_eq!(
        reject(r"C:\projects\houyicoder\file.rs"),
        None,
        "a local path is not rejected"
    );
    assert_eq!(reject("file.rs"), None, "a bare name is not rejected");
    assert_eq!(
        reject(r"D:\work\NUL backup.txt"),
        None,
        "NUL as a word inside a name is not the device"
    );
}

/// The drive letter is allowed because it is exactly one alphabetic byte before
/// the colon; a second colon, or a colon behind a name, is the stream form.
#[test]
fn test_drive_letter_passes() {
    assert_eq!(
        reject(r"C:\file"),
        None,
        "a drive letter followed by a separator is not a stream"
    );
    assert_eq!(
        reject("CC:file"),
        Some(LexicalRejection::AlternateDataStream),
        "two letters before the colon is not a drive"
    );
}

/// A Windows extended-length local path is not a network share: the prefix
/// names a local path or a local device, so only the network form under it is.
#[test]
fn test_allows_extended_drive_path() {
    assert_eq!(
        reject(r"\\?\C:\Users\houyicoder\file.rs"),
        None,
        "an extended-length local path is not a share"
    );
    assert_eq!(
        reject(r"\\.\C:\pipe"),
        None,
        "a device-namespace local path is not a share"
    );
    assert_eq!(
        reject(r"\\?\UNC\evil\share\a"),
        Some(LexicalRejection::UncShare),
        "the UNC form under the prefix is still a share"
    );
}

/// A single-letter file name with a stream is caught: the colon at the second
/// byte is a stream fork, not a drive letter, because a drive letter is exactly
/// two bytes.
#[test]
fn test_rejects_one_letter_ads() {
    assert_eq!(
        reject("a:secret"),
        Some(LexicalRejection::AlternateDataStream),
        "a single-letter name with a stream is not a drive"
    );
    assert_eq!(
        reject(r"C:\dir\a:secret"),
        Some(LexicalRejection::AlternateDataStream),
        "a stream on a single-letter name under a drive is still a stream"
    );
}

/// The MSYS single-slash and cygdrive forms become the Windows path they name.
#[test]
fn test_posix_drive_translated() {
    assert_eq!(
        translate_posix_drive("/c/projects/app"),
        Some(r"C:\projects\app".to_string()),
        "the single-slash form lifts the drive"
    );
    assert_eq!(
        translate_posix_drive("/cygdrive/d/work"),
        Some(r"D:\work".to_string()),
        "the cygdrive form lifts the drive"
    );
    assert_eq!(
        translate_posix_drive("/c"),
        Some(r"C:".to_string()),
        "a drive alone keeps its bare form"
    );
}

/// A path that is not an MSYS drive form is left for the caller to handle.
#[test]
fn test_non_drive_not_translated() {
    assert_eq!(
        translate_posix_drive("/usr/local/bin"),
        None,
        "a unix path is not a drive"
    );
    assert_eq!(
        translate_posix_drive(r"C:\already\windows"),
        None,
        "a windows path is not translated"
    );
}

/// Each writer gets its own temp name, so two writers never share the file one
/// truncates.
#[test]
fn test_temp_name_unique() {
    let a = temp_name_for("served_models.json", 100, 1);
    let b = temp_name_for("served_models.json", 101, 1);
    let c = temp_name_for("served_models.json", 100, 2);
    assert_ne!(a, b, "different pids name different files");
    assert_ne!(a, c, "different counters name different files");
    assert!(
        a.starts_with(".served_models.") && a.ends_with(".json"),
        "the name keeps its stem and extension: {a}"
    );
}

/// A name with no extension gets the tmp extension so a reader that filters by
/// extension still sees the transient file.
#[test]
fn test_temp_name_without_extension() {
    let name = temp_name_for("manifest", 7, 3);
    assert_eq!(name, ".manifest.7.3.tmp");
}

/// A dotfile keeps its leading dot as part of the stem, not as the extension
/// split, so the temp name does not invent an extension from it.
#[test]
fn test_temp_name_dotfile() {
    let name = temp_name_for(".bashrc", 7, 3);
    assert_eq!(name, "..bashrc.7.3.tmp");
}

/// The backoff doubles each step and caps at 32 milliseconds, so a sharing
/// violation a reader or an antivirus holds clears without a long stall and
/// without retrying forever.
#[test]
fn test_backoff_doubles_and_caps() {
    let schedule = backoff_schedule();
    assert_eq!(schedule.len(), 5, "the schedule is five steps");
    assert_eq!(
        schedule[0],
        Duration::from_millis(2),
        "the first step is short"
    );
    assert_eq!(
        schedule[4],
        Duration::from_millis(32),
        "the last step is the cap"
    );
    for window in schedule.windows(2) {
        assert!(
            window[1] >= window[0],
            "each step is at least as long as the one before it"
        );
    }
}

/// Two file identities are equal only when both the volume and the id match, so
/// a replace that swapped the file for another on the same volume is detected.
#[test]
fn test_file_identity_equality() {
    let a = FileIdentity {
        volume: 1,
        file_id: 10,
    };
    assert_eq!(
        a,
        FileIdentity {
            volume: 1,
            file_id: 10
        }
    );
    assert_ne!(
        a,
        FileIdentity {
            volume: 1,
            file_id: 11
        },
        "a different id is a different file"
    );
    assert_ne!(
        a,
        FileIdentity {
            volume: 2,
            file_id: 10
        },
        "a different volume is a different file"
    );
}

/// The atomic replace writes the whole value at once and leaves no temp behind,
/// so a reader between two writes sees one version or the other, never a
/// half-written file. The posix path is exercised here; the Windows path runs
/// the same contract through the kernel rename.
#[cfg(not(target_os = "windows"))]
#[test]
fn test_atomic_replace_writes_whole() {
    let dir = temp_dir().join(format!("houyi-fs-test-{}", id()));
    fs::create_dir_all(&dir).expect("a temp dir for the replace");
    let target = dir.join("target.txt");
    fs::write(&target, b"old").expect("seed the target");
    super::atomic_replace(&target, b"new contents").expect("the replace succeeds");
    assert_eq!(
        fs::read(&target).unwrap(),
        b"new contents",
        "the target holds the new value"
    );
    let leftover: Vec<_> = fs::read_dir(&dir)
        .expect("list the dir")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".target.txt.")
        })
        .collect();
    assert!(leftover.is_empty(), "no temp file is left behind");
    fs::remove_dir_all(&dir).ok();
}

/// A replace that fails the rename deletes the temp it wrote, so a failed write
/// leaves no transient file behind. The target is a directory, so a file cannot
/// rename over it.
#[cfg(not(target_os = "windows"))]
#[test]
fn test_failed_replace_cleans_temp() {
    let dir = temp_dir().join(format!("houyi-fs-fail-{}", id()));
    fs::create_dir_all(&dir).expect("a temp dir for the failed replace");
    let target = dir.join("target");
    fs::create_dir_all(&target).expect("the target is a directory");
    drop(super::atomic_replace(&target, b"x"));
    let leftover: Vec<_> = fs::read_dir(&dir)
        .expect("list the dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".target."))
        .collect();
    assert!(leftover.is_empty(), "no temp file is left on failure");
    fs::remove_dir_all(&dir).ok();
}
