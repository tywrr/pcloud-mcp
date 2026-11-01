//! RcloneStorage against a real `rclone rcd --rc-serve`. Skipped when rclone isn't installed.
//!
//! - `local_*`: a local-disk remote over a scratch folder, so it runs anywhere rclone does.
//! - `pcloud_*`: the owner's remote `pcloud-mcp`, read-only, only when that remote exists on
//!   this machine (set `CHANCE_OF_FILES_NO_PCLOUD=1` to skip it). Contents are read only from
//!   `pcloud-mcp-test/`; nothing else is printed.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chance_of_files_core::{
    ByteRange, Done, JobId, JobState, Policy, Storage, StorageError, StorageWrite, read_capped,
};
use chance_of_files_rclone::{RcloneConfig, RcloneStorage};
use tokio::process::{Child, Command};

fn rclone_available() -> bool {
    std::process::Command::new("rclone")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn random_password() -> String {
    let mut bytes = [0u8; 16];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("random bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Rcd {
    _child: Child,
    storage: RcloneStorage,
}

/// Starts `rclone rcd` on a free port with a random rc password and waits until it answers.
async fn start(remote: &str, dir: Option<&Path>, envs: &[(&str, &str)]) -> Rcd {
    start_staged(remote, dir, envs, None).await
}

async fn start_staged(
    remote: &str,
    dir: Option<&Path>,
    envs: &[(&str, &str)],
    staging: Option<PathBuf>,
) -> Rcd {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let password = random_password();
    let mut command = Command::new("rclone");
    command
        .args([
            "rcd",
            &format!("--rc-addr=127.0.0.1:{port}"),
            "--rc-user=core",
            &format!("--rc-pass={password}"),
            "--rc-serve",
            "--tpslimit=8",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    let child = command.spawn().expect("start rclone rcd");
    let storage = RcloneStorage::new(RcloneConfig {
        url: format!("http://127.0.0.1:{port}"),
        user: "core".into(),
        password,
        remote: remote.into(),
        max_listing_bytes: 64_000_000,
        staging,
    })
    .unwrap();
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Rcd {
                _child: child,
                storage,
            };
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("rclone rcd didn't start");
}

async fn read(
    storage: &RcloneStorage,
    policy: &Policy,
    path: &str,
    range: Option<ByteRange>,
) -> Result<Vec<u8>, StorageError> {
    let stream = storage.open(&policy.check(path)?, range).await?;
    read_capped(stream, u64::MAX).await
}

fn scratch() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rcd-{}", random_password()));
    for (file, data) in [
        ("notes/todo.md", "0123456789"),
        ("notes/a b#?%.txt", "odd name"),
        ("notes/Caf\u{e9}.txt", "nfc"),
        ("empty.txt", ""),
        ("Private/secret.txt", "secret"),
        ("Backups/Big/1.jpg", "jpg"),
        ("Backups/phone/report.pdf", "pdf"),
    ] {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }
    dir
}

#[tokio::test]
async fn local_remote_end_to_end() {
    if !rclone_available() {
        eprintln!("skipped: rclone is not installed");
        return;
    }
    let dir = scratch();
    let rcd = start(
        "cofscratch",
        Some(&dir),
        &[("RCLONE_CONFIG_COFSCRATCH_TYPE", "local")],
    )
    .await;
    let s = &rcd.storage;
    let policy = Policy::new(&["/Private"], &[] as &[&str], &["/Backups/Big"]).unwrap();
    let p = |path: &str| policy.check(path).unwrap();

    s.health().await.unwrap();
    let caps = s.load_capabilities().await.unwrap();
    assert!(!caps.hashes.is_empty(), "{caps:?}");

    let mut names: Vec<String> = s
        .list(&p("/notes"))
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "/notes/Caf\u{e9}.txt",
            "/notes/a b#?%.txt",
            "/notes/todo.md"
        ]
    );
    assert_eq!(
        s.list(&p("/missing")).await.unwrap_err(),
        StorageError::NotFound("/missing".into())
    );
    assert_eq!(
        s.list(&p("/empty.txt")).await.unwrap_err(),
        StorageError::NotADirectory("/empty.txt".into())
    );

    let todo = s.stat(&p("/notes/todo.md"), true).await.unwrap();
    assert_eq!(todo.size, Some(10));
    assert!(!todo.hashes.is_empty());
    assert_eq!(
        s.stat(&p("/nope.txt"), false).await.unwrap_err(),
        StorageError::NotFound("/nope.txt".into())
    );

    let r = |offset, length| Some(ByteRange { offset, length });
    assert_eq!(
        read(s, &policy, "/notes/todo.md", r(3, 4)).await.unwrap(),
        b"3456"
    );
    assert_eq!(
        read(s, &policy, "/notes/todo.md", r(8, 99)).await.unwrap(),
        b"89"
    );
    assert!(
        read(s, &policy, "/notes/todo.md", r(10, 5))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        read(s, &policy, "/notes/todo.md", r(2_000_000, 5))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        read(s, &policy, "/notes/a b#?%.txt", None).await.unwrap(),
        b"odd name"
    );
    assert_eq!(
        read(s, &policy, "/notes/Caf\u{e9}.txt", None)
            .await
            .unwrap(),
        b"nfc"
    );
    assert!(
        read(s, &policy, "/empty.txt", None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        read(s, &policy, "/notes", None).await.unwrap_err(),
        StorageError::IsADirectory("/notes".into())
    );

    // A walk split around a hidden and a skipped folder keeps their entries out.
    let lenient = Policy::default();
    let exclude = [
        lenient.check("/private").unwrap(),
        lenient.check("/Backups/Big").unwrap(),
    ];
    let walked: Vec<String> = s
        .walk(&p("/"), &exclude)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert!(
        walked.contains(&"/Backups/phone/report.pdf".to_owned()),
        "{walked:?}"
    );
    assert!(walked.contains(&"/notes/todo.md".to_owned()), "{walked:?}");
    assert!(
        !walked
            .iter()
            .any(|p| p.starts_with("/Private/") || p.starts_with("/Backups/Big/")),
        "{walked:?}"
    );

    std::fs::remove_dir_all(dir).ok();
}

fn pcloud_remote_available() -> bool {
    if std::env::var_os("CHANCE_OF_FILES_NO_PCLOUD").is_some() || !rclone_available() {
        return false;
    }
    std::process::Command::new("rclone")
        .arg("listremotes")
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.trim() == "pcloud-mcp:")
        })
}

#[tokio::test]
async fn pcloud_remote_read_only() {
    if !pcloud_remote_available() {
        eprintln!("skipped: no rclone remote pcloud-mcp on this machine");
        return;
    }
    let rcd = start("pcloud-mcp", None, &[]).await;
    let s = &rcd.storage;
    let policy = Policy::default();
    let p = |path: &str| policy.check(path).unwrap();

    s.health().await.unwrap();
    let caps = s.load_capabilities().await.unwrap();
    assert!(caps.server_dir_move, "{caps:?}");

    let root = s.list(&p("/")).await.unwrap();
    assert!(
        root.iter()
            .any(|e| e.path == "/pcloud-mcp-test" && e.is_dir),
        "no /pcloud-mcp-test folder at the drive's root"
    );

    let note = "/pcloud-mcp-test/notes/meeting-2026-10-29.md";
    let entry = s.stat(&p(note), false).await.unwrap();
    let size = entry.size.unwrap();
    assert!(entry.id.as_deref().is_some_and(|id| id.starts_with('f')));
    let whole = read(s, &policy, note, None).await.unwrap();
    assert_eq!(whole.len() as u64, size);
    let middle = read(
        s,
        &policy,
        note,
        Some(ByteRange {
            offset: 5,
            length: 10,
        }),
    )
    .await
    .unwrap();
    assert_eq!(middle, whole[5..15]);
    assert!(
        read(
            s,
            &policy,
            note,
            Some(ByteRange {
                offset: size,
                length: 10
            })
        )
        .await
        .unwrap()
        .is_empty()
    );

    let test_root = p("/pcloud-mcp-test");
    let all = s.walk(&test_root, &[]).await.unwrap();
    let deep = p("/pcloud-mcp-test/deep");
    let split = s
        .walk(&test_root, std::slice::from_ref(&deep))
        .await
        .unwrap();
    assert!(
        all.iter()
            .any(|e| e.path.starts_with("/pcloud-mcp-test/deep/"))
    );
    assert!(
        !split
            .iter()
            .any(|e| e.path.starts_with("/pcloud-mcp-test/deep/"))
    );
    assert!(split.iter().any(|e| e.path == "/pcloud-mcp-test/deep"));
    assert_eq!(
        all.iter()
            .filter(|e| !e.path.starts_with("/pcloud-mcp-test/deep/"))
            .count(),
        split.len()
    );
}

/// Waits for an rclone job to end.
async fn finished(storage: &RcloneStorage, done: Done) -> JobState {
    let Done::Job(id) = done else {
        panic!("expected a job, got {done:?}");
    };
    for _ in 0..100 {
        let info = storage.job(&id).await.unwrap();
        assert!(info.started.is_some());
        if info.state != JobState::Running {
            return info.state;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("job {id} didn't finish");
}

#[tokio::test]
async fn local_remote_writes() {
    if !rclone_available() {
        eprintln!("skipped: rclone is not installed");
        return;
    }
    let dir = scratch();
    let staging =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rcd-staging-{}", random_password()));
    std::fs::create_dir_all(&staging).unwrap();
    let rcd = start_staged(
        "cofscratch",
        Some(&dir),
        &[("RCLONE_CONFIG_COFSCRATCH_TYPE", "local")],
        Some(staging.clone()),
    )
    .await;
    let s = &rcd.storage;
    let policy = Policy::default();
    let p = |path: &str| policy.check(path).unwrap();
    let caps = s.load_capabilities().await.unwrap();
    assert!(!caps.trash_on_delete, "a local remote deletes for good");

    let made = s.mkdir(&p("/new/deep")).await.unwrap();
    assert!(made.is_dir && dir.join("new/deep").is_dir());
    let written = s
        .put_bytes(bytes::Bytes::from_static(b"hello"), &p("/new/deep/x b.txt"))
        .await
        .unwrap();
    assert_eq!(written.size, Some(5));
    assert_eq!(
        std::fs::read(dir.join("new/deep/x b.txt")).unwrap(),
        b"hello"
    );
    assert_eq!(
        std::fs::read_dir(&staging).unwrap().count(),
        0,
        "temp file removed"
    );

    // Files move and copy at once.
    let Done::Now(moved) = s
        .rename(&p("/new/deep/x b.txt"), &p("/new/y.txt"))
        .await
        .unwrap()
    else {
        panic!("a file move is immediate");
    };
    assert_eq!(moved.path, "/new/y.txt");
    assert!(!dir.join("new/deep/x b.txt").exists());
    assert!(matches!(
        s.copy(&p("/new/y.txt"), &p("/new/deep/z.txt"))
            .await
            .unwrap(),
        Done::Now(_)
    ));

    // Folders are jobs.
    let job = s.copy(&p("/new"), &p("/copied")).await.unwrap();
    assert_eq!(finished(s, job).await, JobState::Done);
    assert_eq!(
        std::fs::read(dir.join("copied/deep/z.txt")).unwrap(),
        b"hello"
    );
    let job = s.rename(&p("/copied"), &p("/renamed")).await.unwrap();
    assert_eq!(finished(s, job).await, JobState::Done);
    assert!(dir.join("renamed/y.txt").exists() && !dir.join("copied").exists());

    // Uploads come from the staging folder only.
    std::fs::write(staging.join("up.part"), b"uploaded").unwrap();
    let up = s
        .put_local(&staging.join("up.part"), &p("/renamed/up.bin"))
        .await
        .unwrap();
    assert_eq!(up.size, Some(8));
    let outside = dir.join("notes/todo.md");
    assert!(matches!(
        s.put_local(&outside, &p("/stolen.md")).await,
        Err(StorageError::InvalidPath(_))
    ));

    // No trash here, so nothing is deleted.
    assert!(matches!(
        s.trash(&p("/renamed/y.txt")).await,
        Err(StorageError::Unsupported(_))
    ));
    assert!(dir.join("renamed/y.txt").exists());
    assert_eq!(
        s.job(&JobId("987654".into())).await.unwrap().state,
        JobState::Lost
    );
    let _ = std::fs::remove_dir_all(&staging);
}
