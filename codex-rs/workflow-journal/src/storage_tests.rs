use super::*;
use crate::RunMetaTag;
use pretty_assertions::assert_eq;
use uuid::Uuid;

fn sample_meta(run_id: &str) -> WorkflowRunMeta {
    WorkflowRunMeta::new(
        run_id.to_string(),
        None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        "triage".to_string(),
        Some(500_000),
        1,
        "2026-07-17T00:00:00Z".to_string(),
    )
}

#[test]
fn paths_resolve_from_run_id_and_honour_codex_home_override() {
    let codex_home = Path::new("/tmp/some-codex-home");
    let run_id = "0190a000-0000-7000-8000-000000000000";
    let paths = WorkflowRunPaths::new(codex_home, run_id);

    let expected_dir = codex_home.join("workflows").join("runs").join(run_id);
    assert_eq!(paths.run_dir(), expected_dir.as_path());
    assert_eq!(paths.journal(), expected_dir.join("journal.jsonl"));
    assert_eq!(paths.script(), expected_dir.join("script.js"));
    assert_eq!(paths.invocation(), expected_dir.join("invocation.json"));
    assert_eq!(paths.meta(), expected_dir.join("meta.json"));
    assert_eq!(paths.progress(), expected_dir.join("progress.json"));
    assert_eq!(paths.lease(), expected_dir.join("lease.lock"));

    // A different CODEX_HOME relocates the whole tree (tempdir-isolatable).
    let other = Path::new("/var/other-home");
    let other_paths = WorkflowRunPaths::new(other, run_id);
    assert_eq!(
        other_paths.run_dir(),
        other.join("workflows").join("runs").join(run_id).as_path()
    );
    assert_ne!(paths.run_dir(), other_paths.run_dir());
}

#[test]
fn runs_root_matches_layout() {
    let codex_home = Path::new("/home/u/.codex");
    assert_eq!(
        runs_root(codex_home),
        codex_home.join("workflows").join("runs")
    );
    assert_eq!(workflows_root(codex_home), codex_home.join("workflows"));
}

#[test]
fn create_dir_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let run_id = mint_run_id();
    let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
    assert!(!paths.run_dir().exists());
    paths.create_dir().expect("first create");
    assert!(paths.run_dir().is_dir());
    // Second call must not error.
    paths.create_dir().expect("second create");
    assert!(paths.run_dir().is_dir());
}

#[test]
fn initialize_writes_script_and_meta_before_body() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let run_id = mint_run_id();
    let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
    let script = "export const meta = { name: 'triage' };\nawait agent('go');\n";
    let meta = sample_meta(&run_id);

    paths.initialize(script, &meta).expect("initialize");

    // Directory and both up-front files exist; journal is NOT created here.
    assert!(paths.run_dir().is_dir());
    assert!(paths.script().is_file());
    assert!(paths.meta().is_file());
    assert!(!paths.journal().exists());

    // script.js is byte-identical to the submitted program.
    let on_disk = fs::read(paths.script()).expect("read script");
    assert_eq!(on_disk, script.as_bytes());

    // meta.json round-trips back to the same WorkflowRunMeta.
    let meta_bytes = fs::read_to_string(paths.meta()).expect("read meta");
    let parsed: WorkflowRunMeta = serde_json::from_str(&meta_bytes).expect("parse meta");
    assert_eq!(parsed, meta);
    assert_eq!(parsed.kind, RunMetaTag::RunMeta);
    assert_eq!(parsed.status, WorkflowRunStatus::Running);
}

#[test]
fn invocation_artifact_is_canonical_bounded_private_and_create_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let run_id = mint_run_id();
    let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
    paths.create_dir().expect("create run directory");
    let args: serde_json::Value =
        serde_json::from_str(r#"{"z":1,"nested":{"b":2,"a":1}}"#).expect("parse arguments");
    let args_hash = canonical_value_hash(&args);

    paths
        .write_invocation_args(&args, &args_hash)
        .expect("write private invocation");
    assert_eq!(
        fs::read_to_string(paths.invocation()).expect("read invocation bytes"),
        r#"{"nested":{"a":1,"b":2},"z":1}"#
    );
    assert_eq!(
        paths
            .read_invocation_args_bounded(&args_hash)
            .expect("read authenticated invocation"),
        args
    );
    assert_eq!(
        paths
            .write_invocation_args(&args, &args_hash)
            .expect_err("invocation is create-only")
            .kind(),
        io::ErrorKind::AlreadyExists
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(paths.invocation())
                .expect("invocation metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    fs::write(paths.invocation(), r#"{"z":1,"nested":{"a":1,"b":2}}"#)
        .expect("write noncanonical fixture");
    assert!(
        paths.read_invocation_args_bounded(&args_hash).is_err(),
        "noncanonical bytes cannot be resumed"
    );

    fs::write(paths.invocation(), r#"{"nested":{"a":1,"b":2},"z":2}"#)
        .expect("write hash-mismatched fixture");
    assert!(
        paths.read_invocation_args_bounded(&args_hash).is_err(),
        "canonical bytes with a different hash cannot be resumed"
    );

    fs::write(paths.invocation(), r#"{"nested":{"a":1,"b":2},"z":1}"#)
        .expect("restore authenticated invocation fixture");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(paths.invocation(), fs::Permissions::from_mode(0o644))
            .expect("make invocation nonprivate");
        assert_eq!(
            paths
                .read_invocation_args_bounded(&args_hash)
                .expect("legacy invocation is hardened before resume"),
            args
        );
        assert_eq!(
            fs::metadata(paths.invocation())
                .expect("hardened invocation metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[cfg(windows)]
    {
        set_permissive_windows_dacl(&paths.invocation());
        assert_eq!(
            paths
                .read_invocation_args_bounded(&args_hash)
                .expect("legacy invocation DACL is hardened before resume"),
            args
        );
        ensure_private_windows_dacl(
            &File::open(paths.invocation()).expect("open hardened invocation"),
        )
        .expect("hardened invocation DACL");
    }
}

#[cfg(windows)]
fn set_permissive_windows_dacl(path: &Path) {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
    use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
    use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

    let file = OpenOptions::new()
        .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | READ_CONTROL | WRITE_DAC)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .expect("open invocation for DACL fixture");
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the SDDL is static and the output is wrapped immediately.
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                windows_sys::core::w!("D:P(A;;GA;;;WD)"),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        },
        0
    );
    let descriptor = LocalSecurityDescriptor(descriptor);
    let acl = windows_descriptor_dacl(descriptor.0).expect("permissive fixture DACL");
    // SAFETY: the test handle requests WRITE_DAC and the descriptor remains live.
    assert_eq!(
        unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null(),
            )
        },
        ERROR_SUCCESS
    );
}

#[test]
fn invocation_artifact_has_an_independent_hard_cap() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = WorkflowRunPaths::new(tmp.path(), &mint_run_id());
    paths.create_dir().expect("create run directory");
    let args = serde_json::Value::String("x".repeat(MAX_INVOCATION_FILE_BYTES as usize));
    let error = paths
        .write_invocation_args(&args, &canonical_value_hash(&args))
        .expect_err("oversized invocation rejected");
    assert!(error.to_string().contains("byte cap"));
    assert!(!paths.invocation().exists());
}

#[test]
fn missing_invocation_and_oversized_script_reads_are_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = WorkflowRunPaths::new(tmp.path(), &mint_run_id());
    paths.create_dir().expect("create run directory");
    assert!(
        paths
            .read_invocation_args_bounded("blake3:missing")
            .is_err()
    );

    fs::write(
        paths.script(),
        vec![b'x'; MAX_SCRIPT_FILE_BYTES as usize + 1],
    )
    .expect("write oversized script fixture");
    assert!(paths.read_script_bounded().is_err());
}

#[test]
fn journal_state_accepts_only_every_exact_nonempty_line_zero_prefix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let run_id = mint_run_id();
    let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
    let meta = sample_meta(&run_id);
    paths.create_dir().expect("create run directory");
    let expected = serde_json::to_vec(&meta).expect("serialize line zero");

    for prefix_len in 1..=expected.len() {
        fs::write(paths.journal(), &expected[..prefix_len]).expect("write exact torn prefix");
        assert_eq!(
            paths.journal_state(&meta).expect("classify exact prefix"),
            WorkflowJournalState::PartialHeader,
            "prefix length {prefix_len}"
        );
    }

    let mut overlong = expected.clone();
    overlong.push(b'x');
    fs::write(paths.journal(), overlong).expect("write overlong prefix");
    assert!(paths.journal_state(&meta).is_err());

    let mut different = expected.clone();
    different[1] ^= 1;
    fs::write(paths.journal(), different).expect("write different same-length prefix");
    assert!(paths.journal_state(&meta).is_err());

    let mut body_bearing = expected;
    body_bearing.extend_from_slice(b"\n{body evidence}\n");
    fs::write(paths.journal(), body_bearing).expect("write body-bearing journal");
    assert_eq!(
        paths.journal_state(&meta).expect("classify body evidence"),
        WorkflowJournalState::HasBody
    );
}

#[cfg(unix)]
#[test]
fn script_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = WorkflowRunPaths::new(tmp.path(), &mint_run_id());
    paths.create_dir().expect("create run directory");
    let target = tmp.path().join("target.js");
    fs::write(&target, "export default null;").expect("write symlink target");
    symlink(target, paths.script()).expect("create script symlink");
    assert!(paths.read_script_bounded().is_err());
}

#[test]
fn update_status_is_durable_and_preserves_run_metadata() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let run_id = mint_run_id();
    let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
    let meta = sample_meta(&run_id);
    paths
        .initialize("export default null;", &meta)
        .expect("initialize");

    let updated = paths
        .update_status(WorkflowRunStatus::Completed)
        .expect("update status");
    let persisted: WorkflowRunMeta =
        serde_json::from_str(&fs::read_to_string(paths.meta()).expect("read updated meta"))
            .expect("parse updated meta");

    assert_eq!(updated, persisted);
    assert_eq!(persisted.status, WorkflowRunStatus::Completed);
    assert_eq!(persisted.run_id, meta.run_id);
    assert_eq!(persisted.script_hash, meta.script_hash);
    assert_eq!(persisted.created_at, meta.created_at);

    let conflicting = paths
        .update_status(WorkflowRunStatus::Stopped)
        .expect("later terminal update returns the winning metadata");
    assert_eq!(conflicting, persisted);
    assert_eq!(
        paths.read_meta_bounded().expect("reread winning metadata"),
        persisted
    );
    assert!(
        paths.update_status(WorkflowRunStatus::Running).is_err(),
        "terminal status writes cannot transition back to running"
    );
}

#[test]
fn paused_resume_successor_claim_is_first_writer_and_source_stays_paused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source_run_id = mint_run_id();
    let source_paths = WorkflowRunPaths::new(tmp.path(), &source_run_id);
    let mut source_meta = sample_meta(&source_run_id);
    source_meta.status = WorkflowRunStatus::Paused;
    source_paths
        .initialize("export default null;", &source_meta)
        .expect("initialize paused source");
    let lease = match WorkflowRunLease::try_acquire(&source_paths).expect("acquire source lease") {
        crate::WorkflowRunLeaseAcquire::Acquired(lease) => lease,
        crate::WorkflowRunLeaseAcquire::Held => panic!("fresh source lease is available"),
    };
    let first = mint_run_id();
    let later = mint_run_id();

    assert_eq!(
        source_paths
            .claim_resume_successor(&lease, &first)
            .expect("claim successor"),
        first
    );
    assert_eq!(
        source_paths
            .claim_resume_successor(&lease, &later)
            .expect("join existing claim"),
        first
    );
    let persisted = source_paths.read_meta_bounded().expect("read source meta");
    assert_eq!(persisted.status, WorkflowRunStatus::Paused);
    assert_eq!(persisted.resumed_by_run_id.as_deref(), Some(first.as_str()));
    assert_eq!(
        source_paths
            .read_resume_successor_claim()
            .expect("read claim"),
        Some(first)
    );
}

#[test]
fn resume_successor_claim_requires_the_exact_source_lease_and_canonical_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source_run_id = mint_run_id();
    let source_paths = WorkflowRunPaths::new(tmp.path(), &source_run_id);
    let mut source_meta = sample_meta(&source_run_id);
    source_meta.status = WorkflowRunStatus::Paused;
    source_paths
        .initialize("export default null;", &source_meta)
        .expect("initialize paused source");
    let other_run_id = mint_run_id();
    let other_paths = WorkflowRunPaths::new(tmp.path(), &other_run_id);
    other_paths.create_dir().expect("create other run");
    let other_lease = match WorkflowRunLease::try_acquire(&other_paths).expect("other lease") {
        crate::WorkflowRunLeaseAcquire::Acquired(lease) => lease,
        crate::WorkflowRunLeaseAcquire::Held => panic!("fresh other lease is available"),
    };

    assert!(
        source_paths
            .claim_resume_successor(&other_lease, &mint_run_id())
            .is_err()
    );
    assert!(
        source_paths
            .claim_resume_successor(&other_lease, "not-a-run-id")
            .is_err()
    );
    assert_eq!(
        source_paths
            .read_resume_successor_claim()
            .expect("claim remains absent"),
        None
    );
}

#[test]
fn stopped_status_round_trips_as_explicit_terminal_metadata() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let run_id = mint_run_id();
    let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
    let mut expected = sample_meta(&run_id);
    paths
        .initialize("export default null;", &expected)
        .expect("initialize");
    expected.status = WorkflowRunStatus::Stopped;

    let updated = paths
        .update_status(WorkflowRunStatus::Stopped)
        .expect("persist stopped status");

    assert_eq!(updated, expected);
    assert_eq!(
        paths.read_meta_bounded().expect("read stopped metadata"),
        expected
    );
    assert!(
        fs::read_to_string(paths.meta())
            .expect("read stopped JSON")
            .contains("\"status\": \"stopped\"")
    );
}

#[test]
fn missing_status_in_legacy_meta_defaults_to_running() {
    let value = serde_json::json!({
        "type": "run_meta",
        "run_id": "legacy",
        "parent_run_id": null,
        "script_hash": "blake3:script",
        "args_hash": "blake3:args",
        "name": "triage",
        "budget_total": 10,
        "key_algo_version": 1,
        "created_at": "2026-07-17T00:00:00Z"
    });
    let parsed: WorkflowRunMeta = serde_json::from_value(value).expect("legacy meta");

    assert_eq!(parsed.status, WorkflowRunStatus::Running);
}

#[test]
fn mint_run_id_is_unique_uuid_v7() {
    let a = mint_run_id();
    let b = mint_run_id();
    assert_ne!(a, b);
    let parsed = Uuid::parse_str(&a).expect("valid uuid");
    assert_eq!(parsed.get_version_num(), 7);
}
