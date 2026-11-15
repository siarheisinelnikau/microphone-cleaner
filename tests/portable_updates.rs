mod support;
use microphone_cleaner::updates::{self, Asset, Check, ErrorKind, InstallPlan, Progress, Release};
use semver::Version;
use sha2::{Digest, Sha256};
use std::{io::Cursor, path::Path, sync::atomic::Ordering};

fn release(version: &str) -> Release {
    let name = format!("MicrophoneCleaner-v{version}.exe");
    Release {
        tag_name: format!("v{version}"),
        draft: false,
        prerelease: false,
        assets: vec![Asset {
            name: name.clone(),
            size: 8192,
            browser_download_url: format!(
                "https://github.com/siarheisinelnikau/microphone-cleaner/releases/download/v{version}/{name}"
            ),
            digest: Some(format!("sha256:{}", "a".repeat(64))),
        }],
    }
}

#[test]
fn semantic_versions_and_explicit_architecture_control_asset_selection() {
    let result = updates::select_release(
        release("0.10.0"),
        &Version::parse("0.9.0").unwrap(),
        "x86_64",
    )
    .unwrap();
    assert!(matches!(result, Check::Available(_)));
    assert!(matches!(
        updates::select_release(
            release("0.9.0"),
            &Version::parse("0.10.0").unwrap(),
            "x86_64"
        )
        .unwrap(),
        Check::Current
    ));
    assert_eq!(
        updates::select_release(
            release("0.10.0"),
            &Version::parse("0.9.0").unwrap(),
            "aarch64"
        )
        .unwrap_err()
        .kind,
        ErrorKind::Incompatible
    );
}

#[test]
fn draft_and_prerelease_versions_never_reach_the_stable_updater() {
    for (draft, prerelease, version) in [
        (true, false, "0.2.0"),
        (false, true, "0.2.0"),
        (false, false, "0.2.0-beta.1"),
    ] {
        let mut candidate = release(version);
        candidate.draft = draft;
        candidate.prerelease = prerelease;
        assert!(
            updates::select_release(candidate, &Version::parse("0.1.0").unwrap(), "x86_64")
                .is_err()
        );
    }
}

#[test]
fn foreign_urls_missing_integrity_and_unbounded_assets_are_rejected() {
    let installed = Version::parse("0.1.0").unwrap();
    let mut candidate = release("0.2.0");
    candidate.assets[0].browser_download_url = "https://example.com/app.exe".into();
    assert_eq!(
        updates::select_release(candidate, &installed, "x86_64")
            .unwrap_err()
            .kind,
        ErrorKind::InvalidRelease
    );
    let mut candidate = release("0.2.0");
    candidate.assets[0].digest = None;
    assert_eq!(
        updates::select_release(candidate, &installed, "x86_64")
            .unwrap_err()
            .kind,
        ErrorKind::Integrity
    );
    let mut candidate = release("0.2.0");
    candidate.assets[0].size = 999_000_000;
    assert_eq!(
        updates::select_release(candidate, &installed, "x86_64")
            .unwrap_err()
            .kind,
        ErrorKind::InvalidRelease
    );
}

#[test]
fn checksum_files_bind_the_digest_to_the_selected_executable() {
    let name = "MicrophoneCleaner-v0.2.0.exe";
    let hex = "b".repeat(64);
    assert_eq!(
        updates::checksum_text(&format!("{hex} *{name}\n"), name).as_deref(),
        Some(hex.as_str())
    );
    assert!(updates::checksum_text(&format!("{hex} other.exe"), name).is_none());
    assert!(updates::checksum_text("abcd", name).is_none());
    assert!(updates::checksum_text(&format!("{hex} {name}\n{hex} other.exe"), name).is_none());
}

#[test]
fn verified_downloads_track_progress_and_reject_truncation_tampering_and_extra_bytes() {
    let bytes = vec![0x42; 8192];
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let progress = Progress::default();
    let mut output = Vec::new();
    updates::copy_verified(
        &mut Cursor::new(&bytes),
        &mut output,
        bytes.len() as u64,
        &digest,
        &progress,
    )
    .unwrap();
    assert_eq!(output, bytes);
    assert_eq!(progress.bytes.load(Ordering::Relaxed), 8192);
    for (input, size, hash) in [
        (&bytes[..4096], 8192, digest.clone()),
        (&bytes[..], 4096, digest.clone()),
        (&bytes[..], 8192, "f".repeat(64)),
    ] {
        assert_eq!(
            updates::copy_verified(
                &mut Cursor::new(input),
                &mut Vec::new(),
                size,
                &hash,
                &Progress::default()
            )
            .unwrap_err()
            .kind,
            ErrorKind::Integrity
        );
    }
}

#[test]
fn cancellation_writes_no_more_download_data() {
    let progress = Progress::default();
    progress.cancel.store(true, Ordering::Release);
    let mut output = Vec::new();
    let result = updates::copy_verified(
        &mut Cursor::new(vec![1u8; 8192]),
        &mut output,
        8192,
        &"0".repeat(64),
        &progress,
    );
    assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
    assert!(output.is_empty());
}

fn fake_pe(value: u8) -> Vec<u8> {
    let mut data = vec![value; 8192];
    data[..2].copy_from_slice(b"MZ");
    data[60..64].copy_from_slice(&128u32.to_le_bytes());
    data[128..132].copy_from_slice(b"PE\0\0");
    let machine: u16 = match std::env::consts::ARCH {
        "x86_64" => 0x8664,
        "aarch64" => 0xaa64,
        "x86" => 0x14c,
        _ => 0,
    };
    data[132..134].copy_from_slice(&machine.to_le_bytes());
    data
}

fn plan(directory: &Path) -> InstallPlan {
    let directory = std::fs::canonicalize(directory).unwrap();
    let stem = ".microphone-cleaner-a1b2";
    InstallPlan {
        target: directory.join("MicrophoneCleaner-v0.1.0.exe"),
        staged: directory.join(format!("{stem}.new")),
        helper: directory.join(format!("{stem}.helper.exe")),
        backup: directory.join(format!("{stem}.backup.exe")),
        acknowledgement: directory.join(format!("{stem}.ready")),
        waiting: directory.join(format!("{stem}.waiting")),
        parent_pid: std::process::id(),
        sha256: format!("{:x}", Sha256::digest(fake_pe(2))),
        nonce: "a1b2".into(),
    }
}

#[test]
fn failed_restart_restores_the_previous_executable_and_preserves_settings() {
    let directory = support::Scratch::new();
    let plan = plan(&directory.0);
    let previous = fake_pe(1);
    std::fs::write(&plan.target, &previous).unwrap();
    std::fs::write(&plan.staged, fake_pe(2)).unwrap();
    let settings = directory.0.join("settings.json");
    std::fs::write(&settings, b"do not change").unwrap();
    let result = updates::install_transaction(&plan, |_, _| Err("fake restart failed".into()));
    assert_eq!(result.unwrap_err().kind, ErrorKind::Install);
    assert_eq!(std::fs::read(&plan.target).unwrap(), previous);
    assert_eq!(std::fs::read(&settings).unwrap(), b"do not change");
}

#[test]
fn acknowledged_restart_commits_only_a_verified_update() {
    let directory = support::Scratch::new();
    let plan = plan(&directory.0);
    std::fs::write(&plan.target, fake_pe(1)).unwrap();
    std::fs::write(&plan.staged, fake_pe(2)).unwrap();
    updates::install_transaction(&plan, |target, _| {
        assert_eq!(std::fs::read(target).unwrap(), fake_pe(2));
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read(&plan.target).unwrap(), fake_pe(2));
    assert!(!plan.backup.exists());
}

#[test]
fn a_modified_staged_binary_never_replaces_the_working_version() {
    let directory = support::Scratch::new();
    let plan = plan(&directory.0);
    std::fs::write(&plan.target, fake_pe(1)).unwrap();
    std::fs::write(&plan.staged, fake_pe(9)).unwrap();
    let result = updates::install_transaction(&plan, |_, _| panic!("Must not restart"));
    assert_eq!(result.unwrap_err().kind, ErrorKind::Integrity);
    assert_eq!(std::fs::read(&plan.target).unwrap(), fake_pe(1));
}

#[test]
fn helper_paths_cannot_escape_the_portable_application_directory() {
    let directory = support::Scratch::new();
    let mut plan = plan(&directory.0);
    std::fs::write(&plan.helper, fake_pe(1)).unwrap();
    assert!(plan.validate(&plan.helper).is_ok());
    plan.staged = directory.0.join("../unrelated.exe");
    assert!(plan.validate(&plan.helper).is_err());
}

#[test]
fn portable_updates_reject_a_pe_for_another_architecture() {
    let directory = support::Scratch::new();
    let path = directory.0.join("wrong.exe");
    std::fs::write(&path, fake_pe(2)).unwrap();
    let wrong = if std::env::consts::ARCH == "aarch64" {
        "x86_64"
    } else {
        "aarch64"
    };
    assert_eq!(
        updates::verify_pe(&path, wrong).unwrap_err().kind,
        ErrorKind::Incompatible
    );
}
