//! The write tools, jobs and the link tools over an in-memory tree with a trash: what they
//! change, what Policy refuses (protected and hidden paths), the scopes and annotations, the
//! audit log, and the tokens links carry.

use std::sync::Arc;

use chance_of_files_core::memory::{MemoryState, MemoryStore};
use chance_of_files_core::{
    Caller, JobRow, JobStatus, LinkKey, LinkKind, LinkRow, LinkSettings, LinkState, LinkUse,
    McpServer, Policy, Scopes, ServerConfig, StateResult, StateStore, time,
};
use serde_json::{Value, json};

const KEY: &str = "test-link-key-0123456789abcdef0123456789";
const FRONT: &str = "https://cof.example.workers.dev";

struct Fixture {
    server: McpServer<Arc<MemoryStore>>,
    store: Arc<MemoryStore>,
    state: Arc<MemoryState>,
}

fn tree() -> MemoryStore {
    MemoryStore::new()
        .file("/Inbox/report.pdf", "pdf bytes")
        .file("/Inbox/notes.md", "# notes")
        .file("/Projects/a/one.txt", "one")
        .file("/Projects/a/sub/two.txt", "two")
        .dir("/Archive")
        .file("/Backups/Immich/photo.jpg", "jpg")
        .file("/Backups/phone/x.bin", "x")
        .file("/Secret/key.txt", "hidden")
        .file("/Mixed/visible.txt", "v")
        .file("/Mixed/Private/hidden.txt", "h")
}

fn fixture() -> Fixture {
    let store = Arc::new(tree());
    let state = Arc::new(MemoryState::default());
    let policy = Policy::new(
        &["/Secret", "/Mixed/Private"],
        &["/Backups/Immich"],
        &[] as &[&str],
    )
    .unwrap();
    let server = McpServer::new(store.clone(), ServerConfig::default())
        .with_policy(policy)
        .with_state(state.clone() as Arc<dyn StateStore>)
        .with_links(LinkSettings::new(LinkKey::new(KEY).unwrap()));
    Fixture {
        server,
        store,
        state,
    }
}

impl Fixture {
    async fn rpc(&self, scopes: Scopes, method: &str, params: Value) -> Value {
        let caller = Caller {
            scopes,
            public_url: Some(FRONT.into()),
        };
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        self.server.handle_jsonrpc_as(msg, &caller).await.unwrap()
    }

    async fn call_as(&self, scopes: Scopes, tool: &str, args: Value) -> (bool, String) {
        let response = self
            .rpc(
                scopes,
                "tools/call",
                json!({"name": tool, "arguments": args}),
            )
            .await;
        if let Some(error) = response.get("error") {
            return (false, format!("protocol error: {error}"));
        }
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        (result["isError"] != json!(true), text)
    }

    async fn call(&self, tool: &str, args: Value) -> (bool, String) {
        self.call_as(Scopes::all(), tool, args).await
    }

    async fn ok(&self, tool: &str, args: Value) -> String {
        let (ok, text) = self.call(tool, args.clone()).await;
        assert!(ok, "{tool} {args} failed: {text}");
        text
    }

    async fn refused(&self, tool: &str, args: Value) -> String {
        let (ok, text) = self.call(tool, args.clone()).await;
        assert!(!ok, "{tool} {args} should fail: {text}");
        text
    }
}

fn job_id(text: &str) -> String {
    let at = text.find("job_status id=\"").expect("a job id") + 15;
    text[at..].split('"').next().unwrap().to_owned()
}

#[tokio::test]
async fn tools_follow_scopes_and_carry_destructive_hints() {
    let f = fixture();
    let names = |list: &Value| -> Vec<String> {
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let read = f.rpc(Scopes::read_only(), "tools/list", json!({})).await;
    let read_names = names(&read);
    for tool in [
        "job_status",
        "create_download_link",
        "list_links",
        "revoke_link",
    ] {
        assert!(read_names.contains(&tool.to_owned()), "{tool}");
    }
    for tool in [
        "create_folder",
        "move",
        "copy",
        "write_file",
        "delete",
        "create_upload_link",
    ] {
        assert!(!read_names.contains(&tool.to_owned()), "{tool}");
    }
    let all = f.rpc(Scopes::all(), "tools/list", json!({})).await;
    let tools = all["result"]["tools"].as_array().unwrap();
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let destructive = tool["annotations"]["destructiveHint"] == json!(true);
        let expected = [
            "create_folder",
            "move",
            "copy",
            "write_file",
            "delete",
            "create_upload_link",
            "create_download_link",
            "revoke_link",
        ]
        .contains(&name);
        assert_eq!(destructive, expected, "{name}");
        if expected {
            assert_eq!(tool["annotations"]["readOnlyHint"], json!(false), "{name}");
        }
    }
    // A read-only grant can't call a write tool even by name.
    let (ok, text) = f
        .call_as(
            Scopes::read_only(),
            "create_folder",
            json!({"path": "/New"}),
        )
        .await;
    assert!(!ok && text.contains("files:write"), "{text}");
    assert!(!f.store.exists("/New"));

    // Instructions say what this connection may change.
    let init = |scopes| {
        f.rpc(
            scopes,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
        )
    };
    let writer = init(Scopes::all()).await;
    let text = writer["result"]["instructions"].as_str().unwrap();
    assert!(
        text.contains("Read-only folders: /Backups/Immich"),
        "{text}"
    );
    let reader = init(Scopes::read_only()).await;
    assert!(
        reader["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("can't change anything")
    );
}

#[tokio::test]
async fn without_state_or_with_writes_off_there_are_no_write_tools() {
    let store = Arc::new(tree());
    let bare = McpServer::new(store.clone(), ServerConfig::default());
    let caller = Caller {
        scopes: Scopes::all(),
        public_url: None,
    };
    let list = bare
        .handle_jsonrpc_as(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            &caller,
        )
        .await
        .unwrap();
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 5);

    let config = ServerConfig {
        write_tools: vec!["create_folder".into()],
        ..ServerConfig::default()
    };
    let some = McpServer::new(store, config).with_state(Arc::new(MemoryState::default()));
    let list = some
        .handle_jsonrpc_as(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            &caller,
        )
        .await
        .unwrap();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"create_folder") && !names.contains(&"delete"),
        "{names:?}"
    );
}

#[tokio::test]
async fn create_write_move_copy_delete() {
    let f = fixture();
    assert!(
        f.ok("create_folder", json!({"path": "/Projects/2026/q1"}))
            .await
            .contains("Created")
    );
    assert!(f.store.exists("/Projects/2026/q1"));
    assert!(
        f.ok("create_folder", json!({"path": "/Projects/2026/q1/"}))
            .await
            .contains("already")
    );

    let text = f
        .ok(
            "write_file",
            json!({"path": "/Projects/2026/q1/plan.md", "text": "hello"}),
        )
        .await;
    assert!(text.contains("5 bytes"), "{text}");
    assert_eq!(
        f.store.contents("/Projects/2026/q1/plan.md").unwrap(),
        b"hello"
    );
    let exists = f
        .refused(
            "write_file",
            json!({"path": "/Projects/2026/q1/plan.md", "text": "x"}),
        )
        .await;
    assert!(exists.contains("already exists"), "{exists}");
    f.ok(
        "write_file",
        json!({"path": "/Projects/2026/q1/plan.md", "base64": "aGk=", "overwrite": true}),
    )
    .await;
    assert_eq!(
        f.store.contents("/Projects/2026/q1/plan.md").unwrap(),
        b"hi"
    );
    let missing = f
        .refused("write_file", json!({"path": "/Nowhere/x.md", "text": "x"}))
        .await;
    assert!(missing.contains("create_folder"), "{missing}");
    let big = "x".repeat(5_000_001);
    assert!(
        f.refused("write_file", json!({"path": "/Inbox/big.txt", "text": big}))
            .await
            .contains("create_upload_link")
    );

    // A file moves now; into an existing folder it keeps its name.
    let text = f
        .ok(
            "move",
            json!({"from": "/Inbox/report.pdf", "to": "/Archive"}),
        )
        .await;
    assert!(text.contains("/Archive/report.pdf"), "{text}");
    assert!(f.store.exists("/Archive/report.pdf") && !f.store.exists("/Inbox/report.pdf"));
    f.ok(
        "move",
        json!({"from": "/Inbox/notes.md", "to": "/Inbox/renamed.md"}),
    )
    .await;
    assert!(f.store.exists("/Inbox/renamed.md"));
    let clash = f
        .refused(
            "move",
            json!({"from": "/Inbox/renamed.md", "to": "/Archive/report.pdf"}),
        )
        .await;
    assert!(clash.contains("already exists"), "{clash}");
    let inside = f
        .refused(
            "move",
            json!({"from": "/Projects", "to": "/Projects/a/deeper"}),
        )
        .await;
    assert!(inside.contains("inside itself"), "{inside}");

    // A folder copy is a job, and job_status follows it to the end.
    let text = f
        .ok(
            "copy",
            json!({"from": "/Projects/a", "to": "/Archive/a-copy"}),
        )
        .await;
    let job = job_id(&text);
    assert!(f.store.exists("/Archive/a-copy/sub/two.txt"));
    let status = f.ok("job_status", json!({"id": job})).await;
    assert!(status.contains("running"), "{status}");
    let status = f.ok("job_status", json!({"id": job})).await;
    assert!(status.contains("done"), "{status}");
    assert!(f.ok("job_status", json!({})).await.contains(&job));

    // A file copy is done at once.
    f.ok(
        "copy",
        json!({"from": "/Projects/a/one.txt", "to": "/Archive/one.txt"}),
    )
    .await;
    assert_eq!(f.store.contents("/Archive/one.txt").unwrap(), b"one");

    // Deletes go to the trash; a folder delete is a job.
    let text = f.ok("delete", json!({"path": "/Archive/one.txt"})).await;
    assert!(text.contains("trash"), "{text}");
    let text = f.ok("delete", json!({"path": "/Archive/a-copy"})).await;
    let job = job_id(&text);
    assert!(!f.store.exists("/Archive/a-copy") && !f.store.exists("/Archive/one.txt"));
    assert_eq!(f.store.trash(), ["/Archive/one.txt", "/Archive/a-copy"]);
    f.ok("job_status", json!({"id": job})).await;
    assert!(
        f.ok("job_status", json!({"id": job}))
            .await
            .contains("done")
    );

    // The audit log has every write, without file contents.
    let audit = f.state.audit_log();
    assert!(audit.iter().any(|(tool, args, result)| tool == "write_file"
        && args.contains("(5 characters)")
        && !args.contains("hello")
        && result.starts_with("ok")));
    assert!(
        audit
            .iter()
            .any(|(tool, _, result)| tool == "move" && result.starts_with("error"))
    );
}

#[tokio::test]
async fn protected_and_hidden_paths_refuse_every_write() {
    let f = fixture();
    let calls: Vec<(&str, Value)> = vec![
        ("create_folder", json!({"path": "/Backups/Immich/new"})),
        (
            "write_file",
            json!({"path": "/backups/immich/x.txt", "text": "x"}),
        ),
        ("delete", json!({"path": "/Backups/Immich/photo.jpg"})),
        ("delete", json!({"path": "/Backups/Immich"})),
        // Above a protected folder: deleting or moving it would take the folder along.
        ("delete", json!({"path": "/Backups"})),
        (
            "move",
            json!({"from": "/Backups", "to": "/Archive/Backups"}),
        ),
        // Into and out of it, files and folders, moves and copies.
        (
            "move",
            json!({"from": "/Inbox/notes.md", "to": "/Backups/Immich/notes.md"}),
        ),
        (
            "move",
            json!({"from": "/Inbox/notes.md", "to": "/Backups/Immich"}),
        ),
        (
            "copy",
            json!({"from": "/Inbox/notes.md", "to": "/Backups/Immich/n.md"}),
        ),
        (
            "move",
            json!({"from": "/Backups/Immich/photo.jpg", "to": "/Archive/photo.jpg"}),
        ),
        (
            "copy",
            json!({"from": "/Backups/Immich/photo.jpg", "to": "/Archive/photo.jpg"}),
        ),
        (
            "copy",
            json!({"from": "/Backups/Immich", "to": "/Archive/Immich"}),
        ),
        (
            "create_upload_link",
            json!({"folder": "/Backups/Immich", "name": "x.bin", "size": 10}),
        ),
    ];
    for (tool, args) in calls {
        let text = f.refused(tool, args.clone()).await;
        assert!(text.contains("read-only"), "{tool} {args}: {text}");
    }
    // Hidden paths don't exist, as a source, a destination or a link.
    for (tool, args) in [
        ("delete", json!({"path": "/Secret/key.txt"})),
        ("move", json!({"from": "/Secret", "to": "/Archive/s"})),
        (
            "copy",
            json!({"from": "/Secret/key.txt", "to": "/Archive/k.txt"}),
        ),
        (
            "write_file",
            json!({"path": "/secret/new.txt", "text": "x"}),
        ),
        (
            "create_download_link",
            json!({"paths": ["/Secret/key.txt"]}),
        ),
    ] {
        let text = f.refused(tool, args.clone()).await;
        assert!(text.contains("not found"), "{tool} {args}: {text}");
    }
    // A folder with a hidden folder inside can't be moved, copied or deleted whole.
    for (tool, args) in [
        ("delete", json!({"path": "/Mixed"})),
        ("move", json!({"from": "/Mixed", "to": "/Archive/Mixed"})),
        ("copy", json!({"from": "/Mixed", "to": "/Archive/Mixed"})),
    ] {
        let text = f.refused(tool, args.clone()).await;
        assert!(text.contains("doesn't manage"), "{tool}: {text}");
        assert!(!text.contains("Private"), "{text}");
    }
    assert!(
        f.refused("delete", json!({"path": "/"}))
            .await
            .contains("root")
    );
    let calls = f.store.take_calls();
    assert!(calls.iter().all(|c| !c.starts_with("trash")
        && !c.starts_with("rename")
        && !c.starts_with("copy")
        && !c.starts_with("put")
        && !c.starts_with("mkdir")));
    // Next to a protected folder is fine: into /Backups, not into /Backups/Immich.
    f.ok("copy", json!({"from": "/Inbox/notes.md", "to": "/Backups"}))
        .await;
    assert!(f.store.exists("/Backups/notes.md"));
    // Reading from a protected folder still works.
    f.ok(
        "create_download_link",
        json!({"paths": ["/Backups/Immich/photo.jpg"]}),
    )
    .await;
}

#[tokio::test]
async fn deletes_need_a_trash() {
    let store = Arc::new(tree().without_trash());
    let server = McpServer::new(store.clone(), ServerConfig::default())
        .with_state(Arc::new(MemoryState::default()));
    let caller = Caller {
        scopes: Scopes::all(),
        public_url: None,
    };
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "delete", "arguments": {"path": "/Inbox/notes.md"}}});
    let response = server.handle_jsonrpc_as(msg, &caller).await.unwrap();
    assert_eq!(response["result"]["isError"], json!(true));
    assert!(store.exists("/Inbox/notes.md"));
}

#[tokio::test]
async fn download_links() {
    let f = fixture();
    let text = f
        .ok("create_download_link", json!({"paths": ["/Projects/a"]}))
        .await;
    assert!(text.contains("2 download links"), "{text}");
    assert!(text.contains("curl -fL -C -"), "{text}");
    assert!(text.contains("sha1sum -c"), "{text}");
    let links = f.state.links(10).unwrap();
    assert_eq!(links.len(), 2);
    let key = LinkKey::new(KEY).unwrap();
    for link in &links {
        assert_eq!(link.kind, LinkKind::Download);
        assert!(link.expires - time::now() <= 15 * 60 && link.expires - time::now() > 14 * 60);
        assert_eq!(link.max_bytes, 1024 * 1024, "3x a tiny file is the floor");
        let token = key.token(LinkKind::Download, &link.id, link.expires);
        assert!(
            text.contains(&format!("{FRONT}/dl/{token}/{}", link.name)),
            "{text}"
        );
    }

    let zip = f
        .ok(
            "create_download_link",
            json!({"paths": ["/Projects"], "zip": true, "expires_in": "2h"}),
        )
        .await;
    assert!(
        zip.contains("Projects.zip") && zip.contains("unzip -t"),
        "{zip}"
    );
    let row = &f.state.links(1).unwrap()[0];
    assert!(row.zip && row.paths == ["/Projects"]);
    assert!(row.expires - time::now() > 7000);

    assert!(
        f.refused(
            "create_download_link",
            json!({"paths": ["/Inbox"], "expires_in": "25h"})
        )
        .await
        .contains("at most")
    );
    assert!(
        f.refused("create_download_link", json!({"paths": ["/Nope"]}))
            .await
            .contains("not found")
    );

    let listed = f.ok("list_links", json!({})).await;
    assert!(
        listed.contains("zip download") && listed.contains("/Projects/a/one.txt"),
        "{listed}"
    );
    let id = links[0].id.clone();
    assert!(
        f.ok("revoke_link", json!({"id": id}))
            .await
            .contains("Revoked")
    );
    assert_eq!(
        f.state.link(&id).unwrap().unwrap().state,
        LinkState::Revoked
    );
    assert!(
        f.ok("revoke_link", json!({"id": id}))
            .await
            .contains("already")
    );
}

#[tokio::test]
async fn links_need_the_public_address() {
    let f = fixture();
    let caller = Caller {
        scopes: Scopes::all(),
        public_url: None,
    };
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "create_download_link", "arguments": {"paths": ["/Inbox/notes.md"]}}});
    let response = f.server.handle_jsonrpc_as(msg, &caller).await.unwrap();
    assert_eq!(response["result"]["isError"], json!(true));
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("public address")
    );
}

#[tokio::test]
async fn upload_links() {
    let f = fixture();
    let text = f
        .ok(
            "create_upload_link",
            json!({"folder": "/Inbox", "name": "big.tar", "size": 200_000_000u64}),
        )
        .await;
    assert!(
        text.contains("3 parts") && text.contains("split -b 90M"),
        "{text}"
    );
    let row = &f.state.links(1).unwrap()[0];
    assert_eq!(row.kind, LinkKind::Upload);
    assert_eq!(row.paths, ["/Inbox/big.tar"]);
    assert_eq!(row.max_bytes, 600_000_000);
    assert!(text.contains(&format!("{FRONT}/ul/")), "{text}");
    // The reservation counts against the staging budget.
    assert!(
        f.refused(
            "create_upload_link",
            json!({"folder": "/Inbox", "name": "huge.tar", "size": 29_900_000_000u64}),
        )
        .await
        .contains("staging space")
    );
    assert!(
        f.refused(
            "create_upload_link",
            json!({"folder": "/Inbox", "name": "notes.md", "size": 5})
        )
        .await
        .contains("already exists")
    );
    assert!(
        f.refused(
            "create_upload_link",
            json!({"folder": "/Nope", "name": "a", "size": 5})
        )
        .await
        .contains("create_folder")
    );
    assert!(
        f.refused(
            "create_upload_link",
            json!({"folder": "/Inbox", "name": "a/b", "size": 5})
        )
        .await
        .contains("plain file name")
    );
}

/// A MemoryState whose first `link` read is stale: the row as it was before another request
/// moved the link on, as when the server starts finishing an upload while revoke_link runs.
struct StaleFirstRead {
    inner: MemoryState,
    stale: std::sync::Mutex<Option<LinkRow>>,
}

impl StateStore for StaleFirstRead {
    fn insert_job(&self, job: &JobRow) -> StateResult<()> {
        self.inner.insert_job(job)
    }
    fn finish_job(
        &self,
        id: &str,
        state: JobStatus,
        error: Option<&str>,
        at: i64,
    ) -> StateResult<()> {
        self.inner.finish_job(id, state, error, at)
    }
    fn job(&self, id: &str) -> StateResult<Option<JobRow>> {
        self.inner.job(id)
    }
    fn jobs(&self, limit: usize) -> StateResult<Vec<JobRow>> {
        self.inner.jobs(limit)
    }
    fn running_jobs(&self) -> StateResult<Vec<JobRow>> {
        self.inner.running_jobs()
    }
    fn insert_link(&self, link: &LinkRow) -> StateResult<()> {
        self.inner.insert_link(link)
    }
    fn link(&self, id: &str) -> StateResult<Option<LinkRow>> {
        if let Some(row) = self.stale.lock().unwrap().take() {
            return Ok(Some(row));
        }
        self.inner.link(id)
    }
    fn links(&self, limit: usize) -> StateResult<Vec<LinkRow>> {
        self.inner.links(limit)
    }
    fn transition(
        &self,
        id: &str,
        from: &[LinkState],
        to: LinkState,
        note: Option<&str>,
    ) -> StateResult<bool> {
        self.inner.transition(id, from, to, note)
    }
    fn uses(&self, link_id: &str, limit: usize) -> StateResult<Vec<LinkUse>> {
        self.inner.uses(link_id, limit)
    }
    fn reserved_bytes(&self, now: i64) -> StateResult<u64> {
        self.inner.reserved_bytes(now)
    }
    fn audit(&self, at: i64, tool: &str, args: &str, result: &str) -> StateResult<()> {
        self.inner.audit(at, tool, args, result)
    }
}

#[tokio::test]
async fn revoke_leaves_an_upload_that_is_finishing_alone() {
    let f = fixture();
    f.ok(
        "create_upload_link",
        json!({"folder": "/Inbox", "name": "big.tar", "size": 1000}),
    )
    .await;
    let id = f.state.links(1).unwrap()[0].id.clone();
    assert!(
        f.state
            .transition(&id, &[LinkState::Active], LinkState::Finishing, None)
            .unwrap()
    );
    assert!(
        f.refused("revoke_link", json!({"id": id}))
            .await
            .contains("sending its file to pCloud")
    );
    assert_eq!(
        f.state.link(&id).unwrap().unwrap().state,
        LinkState::Finishing
    );

    // Expired links are reported, not revoked.
    assert!(
        f.state
            .transition(&id, &[LinkState::Finishing], LinkState::Expired, None)
            .unwrap()
    );
    assert!(
        f.ok("revoke_link", json!({"id": id}))
            .await
            .contains("expired")
    );
    assert_eq!(
        f.state.link(&id).unwrap().unwrap().state,
        LinkState::Expired
    );

    // A failed completion can still be revoked.
    let row = f.state.link(&id).unwrap().unwrap();
    let failed = LinkRow {
        id: "failed-upload".into(),
        state: LinkState::Failed,
        ..row
    };
    f.state.insert_link(&failed).unwrap();
    assert!(
        f.ok("revoke_link", json!({"id": "failed-upload"}))
            .await
            .contains("Revoked")
    );
    assert_eq!(
        f.state.link("failed-upload").unwrap().unwrap().state,
        LinkState::Revoked
    );
}

#[tokio::test]
async fn revoke_rechecks_a_link_that_started_finishing_meanwhile() {
    let f = fixture();
    f.ok(
        "create_upload_link",
        json!({"folder": "/Inbox", "name": "big.tar", "size": 1000}),
    )
    .await;
    let row = f.state.links(1).unwrap()[0].clone();
    // revoke_link reads the row while it is still active; by the time it writes, the last part
    // has arrived and the server has moved the upload on to finishing.
    let state = Arc::new(StaleFirstRead {
        inner: MemoryState::default(),
        stale: std::sync::Mutex::new(Some(row.clone())),
    });
    state
        .inner
        .insert_link(&LinkRow {
            state: LinkState::Finishing,
            ..row.clone()
        })
        .unwrap();
    let server = McpServer::new(f.store.clone(), ServerConfig::default())
        .with_state(state.clone() as Arc<dyn StateStore>)
        .with_links(LinkSettings::new(LinkKey::new(KEY).unwrap()));
    let caller = Caller {
        scopes: Scopes::all(),
        public_url: Some(FRONT.into()),
    };
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "revoke_link", "arguments": {"id": row.id}}});
    let response = server.handle_jsonrpc_as(msg, &caller).await.unwrap();
    let result = &response["result"];
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(result["isError"], json!(true), "{text}");
    assert!(text.contains("sending its file to pCloud"), "{text}");
    assert_eq!(
        state.inner.link(&row.id).unwrap().unwrap().state,
        LinkState::Finishing,
        "still finishing, not revoked mid-copy"
    );
}
