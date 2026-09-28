use crate::model::{Assignment, Claim, Job, Target, safe_component};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Mutex};

pub struct Store {
    pub root: PathBuf,
    db: Mutex<Connection>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    struct TestStore {
        store: Store,
        root: PathBuf,
    }
    impl TestStore {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("conan-server-unit-{}", uuid::Uuid::new_v4()));
            Self {
                store: Store::open(root.clone()).unwrap(),
                root,
            }
        }
        fn put(&self, scope: &Scope, name: &str, contents: &str, targets: &[Target]) -> Result<()> {
            let hash = format!("{:x}", Sha256::digest(contents.as_bytes()));
            std::fs::write(self.root.join("blobs").join(&hash), contents)?;
            self.store
                .commit_file(scope, name, &hash, contents.len() as u64, targets)
        }
    }
    impl Drop for TestStore {
        fn drop(&mut self) {
            // SQLite may keep the file open on Windows; close it before removing the directory.
            let mut conn = self.store.db.lock().unwrap();
            let old = std::mem::replace(&mut *conn, Connection::open_in_memory().unwrap());
            drop(old);
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }
    fn target() -> Target {
        Target {
            id: "linux".into(),
            runner_os: "Linux".into(),
            os: "Linux".into(),
            arch: "x86_64".into(),
            build_type: "Release".into(),
            sdk: None,
            os_version: None,
        }
    }
    fn scope() -> Scope {
        Scope::parse(&["files", "1.0", "team", "dev", "revisions", "abc"]).unwrap()
    }
    const MANIFEST: &str = "1\nconanfile.py: 00000000000000000000000000000000\n";

    #[test]
    fn completion_gate_idempotency_and_durability() {
        let test = TestStore::new();
        let scope = scope();
        let manifest =
            format!("{MANIFEST}export_source/hello world.c: 00000000000000000000000000000000\n");
        test.put(&scope, "conanmanifest.txt", &manifest, &[target()])
            .unwrap();
        test.put(&scope, "conanfile.py", "recipe", &[target()])
            .unwrap();
        assert!(test.store.snapshot(&scope.key).unwrap().is_none());
        assert!(test.store.jobs().unwrap().is_empty());
        test.put(&scope, "conan_sources.tgz", "archive", &[target()])
            .unwrap();
        assert!(test.store.snapshot(&scope.key).unwrap().is_some());
        test.put(&scope, "conanmanifest.txt", &manifest, &[target()])
            .unwrap();
        assert_eq!(test.store.jobs().unwrap().len(), 1);
        assert!(
            test.put(&scope, "conanfile.py", "different", &[target()])
                .is_err()
        );
        assert!(
            test.put(&scope, "conan_export.tgz", "late file", &[target()])
                .is_err()
        );
        let reopened = Store::open(test.root.clone()).unwrap();
        assert_eq!(reopened.recipes().unwrap(), vec!["files/1.0@team/dev"]);
        assert_eq!(
            reopened.jobs().unwrap()[0].reference,
            "files/1.0@team/dev#abc"
        );
    }

    #[test]
    fn expired_lease_cannot_renew_or_complete_reassigned_job() {
        let test = TestStore::new();
        let scope = scope();
        test.put(&scope, "conanfile.py", "recipe", &[target()])
            .unwrap();
        test.put(&scope, "conanmanifest.txt", MANIFEST, &[target()])
            .unwrap();
        let claim = Claim {
            worker_id: "first".into(),
            runner_os: "Linux".into(),
            target_ids: vec!["linux".into()],
        };
        let first = test.store.claim(&claim).unwrap().unwrap();
        assert!(test.store.claim(&claim).unwrap().is_none());
        assert!(
            !test
                .store
                .complete(first.job.id, "wrong-lease", true, "")
                .unwrap()
        );
        test.store
            .db
            .lock()
            .unwrap()
            .execute("UPDATE jobs SET lease_until=0", [])
            .unwrap();
        assert!(!test.store.heartbeat(first.job.id, &first.lease).unwrap());
        let second = test.store.claim(&claim).unwrap().unwrap();
        assert_eq!(second.job.attempts, 2);
        assert!(
            !test
                .store
                .complete(first.job.id, &first.lease, true, "stale")
                .unwrap()
        );
        assert!(
            test.store
                .complete(second.job.id, &second.lease, false, "compiler failed")
                .unwrap()
        );
        assert!(test.store.retry(second.job.id).unwrap());
        let retried = test.store.claim(&claim).unwrap().unwrap();
        assert_eq!(retried.job.attempts, 1);
        assert!(
            test.store
                .complete(retried.job.id, &retried.lease, true, "done")
                .unwrap()
        );
        assert!(!test.store.retry(retried.job.id).unwrap());
    }

    #[test]
    fn path_validation_and_failed_manifest_do_not_publish() {
        assert!(Scope::parse(&["..", "1", "_", "_", "revisions", "abc"]).is_err());
        assert!(Scope::parse(&["pkg", "1", "_", "dev", "revisions", "abc"]).is_err());
        let test = TestStore::new();
        let scope = scope();
        test.put(&scope, "conanfile.py", "recipe", &[target()])
            .unwrap();
        assert!(
            test.put(&scope, "conanmanifest.txt", "not a manifest", &[target()])
                .is_err()
        );
        assert!(test.store.jobs().unwrap().is_empty());
        test.put(&scope, "conanmanifest.txt", MANIFEST, &[target()])
            .unwrap();
        assert_eq!(test.store.jobs().unwrap().len(), 1);
    }
}

#[derive(Debug, Clone)]
pub struct Scope {
    pub key: String,
    pub recipe: String,
    pub revision: String,
    pub package_id: String,
    pub package_revision: String,
}

impl Scope {
    /// All storage is addressed by blob digest, never by a user-supplied filesystem path.
    pub fn parse(parts: &[&str]) -> Result<Self> {
        ensure!(
            parts.len() == 6 || parts.len() == 10,
            "invalid reference path"
        );
        ensure!(
            parts.iter().all(|s| safe_component(s)),
            "invalid reference component"
        );
        ensure!(parts[4] == "revisions", "missing recipe revision");
        if parts.len() == 10 {
            ensure!(
                parts[6] == "packages" && parts[8] == "revisions",
                "invalid package path"
            );
        }
        let recipe = if parts[2] == "_" && parts[3] == "_" {
            format!("{}/{}", parts[0], parts[1])
        } else {
            ensure!(
                parts[2] != "_" && parts[3] != "_",
                "user and channel must be supplied together"
            );
            format!("{}/{}@{}/{}", parts[0], parts[1], parts[2], parts[3])
        };
        Ok(Self {
            key: parts.join("/"),
            recipe,
            revision: parts[5].into(),
            package_id: parts.get(7).unwrap_or(&"").to_string(),
            package_revision: parts.get(9).unwrap_or(&"").to_string(),
        })
    }
    pub fn is_recipe(&self) -> bool {
        self.package_id.is_empty()
    }
    pub fn full_ref(&self) -> String {
        format!("{}#{}", self.recipe, self.revision)
    }
}

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn row_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let target: String = row.get(2)?;
    Ok(Job {
        id: row.get(0)?,
        reference: row.get(1)?,
        target: serde_json::from_str(&target).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?,
        status: row.get(3)?,
        attempts: row.get(4)?,
        worker_id: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        message: row.get(8)?,
    })
}
const JOB_COLUMNS: &str =
    "id, reference, target, status, attempts, worker_id, created_at, updated_at, message";

impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(root.join("blobs"))?;
        std::fs::create_dir_all(root.join("staging"))?;
        let db = Connection::open(root.join("registry.sqlite3"))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("
            CREATE TABLE IF NOT EXISTS files (
              scope TEXT NOT NULL, name TEXT NOT NULL, digest TEXT NOT NULL, size INTEGER NOT NULL,
              PRIMARY KEY(scope, name));
            CREATE TABLE IF NOT EXISTS revisions (
              scope TEXT PRIMARY KEY, recipe TEXT NOT NULL, revision TEXT NOT NULL,
              package_id TEXT NOT NULL, package_revision TEXT NOT NULL, time TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS recipe_lookup ON revisions(recipe, revision, package_id, time);
            CREATE TABLE IF NOT EXISTS jobs (
              id INTEGER PRIMARY KEY, reference TEXT NOT NULL, target_id TEXT NOT NULL, target TEXT NOT NULL,
              runner_os TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued', attempts INTEGER NOT NULL DEFAULT 0,
              worker_id TEXT, lease TEXT, lease_until INTEGER, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
              message TEXT, UNIQUE(reference, target_id));
        ")?;
        Ok(Self {
            root,
            db: Mutex::new(db),
        })
    }

    pub fn commit_file(
        &self,
        scope: &Scope,
        name: &str,
        digest: &str,
        size: u64,
        targets: &[Target],
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT digest FROM files WHERE scope=? AND name=?",
                params![scope.key, name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            ensure!(
                previous == digest,
                "immutable file already exists with different contents"
            );
            return Ok(());
        }
        let complete: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM revisions WHERE scope=?)",
            [&scope.key],
            |r| r.get(0),
        )?;
        ensure!(!complete, "revision is already complete and immutable");
        tx.execute(
            "INSERT INTO files(scope,name,digest,size) VALUES(?,?,?,?)",
            params![scope.key, name, digest, size],
        )?;
        let files: BTreeMap<String, String> = {
            let mut query = tx.prepare("SELECT name,digest FROM files WHERE scope=?")?;
            query
                .query_map([&scope.key], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let ready = match files.get("conanmanifest.txt") {
            Some(hash) => {
                let manifest = std::fs::read_to_string(self.root.join("blobs").join(hash))?;
                Self::is_complete(scope, &files, &manifest)?
            }
            None => false,
        };
        if ready {
            let time = now();
            tx.execute(
                "INSERT INTO revisions VALUES(?,?,?,?,?,?)",
                params![
                    scope.key,
                    scope.recipe,
                    scope.revision,
                    scope.package_id,
                    scope.package_revision,
                    time
                ],
            )?;
            if scope.is_recipe() {
                for target in targets {
                    tx.execute("INSERT OR IGNORE INTO jobs(reference,target_id,target,runner_os,created_at,updated_at) VALUES(?,?,?,?,?,?)",
                        params![scope.full_ref(), target.id, serde_json::to_string(target)?, target.runner_os, time, time])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn is_complete(
        scope: &Scope,
        files: &BTreeMap<String, String>,
        manifest: &str,
    ) -> Result<bool> {
        ensure!(manifest.len() <= 8 * 1024 * 1024, "manifest too large");
        let mut lines = manifest.lines();
        lines
            .next()
            .context("empty manifest")?
            .parse::<u64>()
            .context("invalid manifest timestamp")?;
        let mut needs_sources = false;
        let mut needs_export = false;
        let mut has_main = false;
        for line in lines.filter(|line| !line.is_empty()) {
            let (path, hash) = line.rsplit_once(": ").context("invalid manifest entry")?;
            ensure!(
                hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid manifest checksum"
            );
            ensure!(
                !path.starts_with('/')
                    && !path.contains('\\')
                    && path.as_bytes().get(1) != Some(&b':')
                    && path
                        .split('/')
                        .all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains('\0')),
                "invalid manifest path"
            );
            has_main |= path
                == if scope.is_recipe() {
                    "conanfile.py"
                } else {
                    "conaninfo.txt"
                };
            needs_sources |= path.starts_with("export_source/");
            needs_export |= path != "conanfile.py" && !path.starts_with("export_source/");
        }
        ensure!(has_main, "manifest is missing the main file");
        Ok(if scope.is_recipe() {
            files.contains_key("conanfile.py")
                && (!needs_sources || files.contains_key("conan_sources.tgz"))
                && (!needs_export || files.contains_key("conan_export.tgz"))
        } else {
            files.contains_key("conaninfo.txt") && files.contains_key("conan_package.tgz")
        })
    }

    pub fn snapshot(&self, scope: &str) -> Result<Option<Value>> {
        let db = self.db.lock().unwrap();
        let complete: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM revisions WHERE scope=?)",
            [scope],
            |r| r.get(0),
        )?;
        if !complete {
            return Ok(None);
        }
        let mut query = db.prepare("SELECT name FROM files WHERE scope=? ORDER BY name")?;
        let files = query
            .query_map([scope], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(
            json!({"files": files.into_iter().map(|f| (f, json!({}))).collect::<serde_json::Map<_,_>>()}),
        ))
    }

    pub fn file(&self, scope: &str, name: &str) -> Result<Option<(PathBuf, u64, String)>> {
        let db = self.db.lock().unwrap();
        let item: Option<(String, u64)> = db.query_row("SELECT f.digest,f.size FROM files f JOIN revisions r ON f.scope=r.scope WHERE f.scope=? AND f.name=?",
            params![scope, name], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        Ok(item.map(|(hash, size)| (self.root.join("blobs").join(&hash), size, hash)))
    }

    pub fn recipes(&self) -> Result<Vec<String>> {
        let db = self.db.lock().unwrap();
        let mut query = db
            .prepare("SELECT DISTINCT recipe FROM revisions WHERE package_id='' ORDER BY recipe")?;
        Ok(query
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn revisions(
        &self,
        recipe: &str,
        revision: Option<&str>,
        package: Option<&str>,
    ) -> Result<Vec<Value>> {
        let db = self.db.lock().unwrap();
        let (sql, arguments) = match (revision, package) {
            (Some(rev), Some(pkg)) => (
                "SELECT package_revision,time FROM revisions WHERE recipe=? AND revision=? AND package_id=? ORDER BY time DESC,rowid DESC",
                vec![recipe, rev, pkg],
            ),
            _ => (
                "SELECT revision,time FROM revisions WHERE recipe=? AND package_id='' ORDER BY time DESC,rowid DESC",
                vec![recipe],
            ),
        };
        let mut query = db.prepare(sql)?;
        Ok(query
            .query_map(rusqlite::params_from_iter(arguments), |r| {
                Ok(json!({"revision":r.get::<_,String>(0)?,"time":r.get::<_,String>(1)?}))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn packages(&self, recipe: &str, revision: &str) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare("SELECT r.package_id,f.digest FROM revisions r JOIN files f ON f.scope=r.scope AND f.name='conaninfo.txt' WHERE r.recipe=? AND r.revision=? AND r.package_id!='' ORDER BY r.time ASC,r.rowid ASC")?;
        let files = query
            .query_map(params![recipe, revision], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut result = serde_json::Map::new();
        for (id, digest) in files {
            let info = std::fs::read_to_string(self.root.join("blobs").join(digest))?;
            let mut data = json!({"settings":{},"options":{},"requires":[]});
            let mut section = "";
            for line in info.lines().map(str::trim).filter(|s| !s.is_empty()) {
                if line.starts_with('[') && line.ends_with(']') {
                    section = &line[1..line.len() - 1];
                } else if ["settings", "options"].contains(&section) {
                    if let Some((key, value)) = line.split_once('=') {
                        data[section][key] = value.into();
                    }
                } else if section == "requires" {
                    data["requires"].as_array_mut().unwrap().push(line.into());
                }
            }
            result.insert(id, data);
        }
        Ok(result.into())
    }

    pub fn jobs(&self) -> Result<Vec<Job>> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs ORDER BY id DESC LIMIT 1000"
        ))?;
        Ok(query
            .query_map([], row_job)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn claim(&self, claim: &Claim) -> Result<Option<Assignment>> {
        ensure!(
            safe_component(&claim.worker_id)
                && !claim.target_ids.is_empty()
                && claim.target_ids.len() <= 100,
            "invalid worker claim"
        );
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let epoch = Utc::now().timestamp();
        tx.execute("UPDATE jobs SET status=CASE WHEN attempts>=3 THEN 'failed' ELSE 'queued' END, lease=NULL, worker_id=NULL, updated_at=?, message='worker lease expired' WHERE status='running' AND lease_until<?", params![now(), epoch])?;
        let candidates = {
            let mut query = tx.prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE status='queued' AND runner_os=? ORDER BY id"
            ))?;
            query
                .query_map([&claim.runner_os], row_job)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let Some(mut job) = candidates
            .into_iter()
            .find(|j| claim.target_ids.contains(&j.target.id))
        else {
            tx.commit()?;
            return Ok(None);
        };
        let lease = uuid::Uuid::new_v4().to_string();
        let time = now();
        tx.execute("UPDATE jobs SET status='running',attempts=attempts+1,worker_id=?,lease=?,lease_until=?,updated_at=?,message=NULL WHERE id=?",
            params![claim.worker_id, lease, epoch + 90, time, job.id])?;
        tx.commit()?;
        job.status = "running".into();
        job.attempts += 1;
        job.worker_id = Some(claim.worker_id.clone());
        job.updated_at = time;
        Ok(Some(Assignment { job, lease }))
    }

    pub fn heartbeat(&self, id: i64, lease: &str) -> Result<bool> {
        let db = self.db.lock().unwrap();
        Ok(db.execute("UPDATE jobs SET lease_until=?,updated_at=? WHERE id=? AND status='running' AND lease=? AND lease_until>=?",
            params![Utc::now().timestamp()+90, now(), id, lease, Utc::now().timestamp()])? == 1)
    }

    pub fn complete(&self, id: i64, lease: &str, success: bool, message: &str) -> Result<bool> {
        ensure!(message.len() <= 32768, "report too large");
        let db = self.db.lock().unwrap();
        Ok(db.execute("UPDATE jobs SET status=?,message=?,lease=NULL,updated_at=? WHERE id=? AND status='running' AND lease=? AND lease_until>=?",
            params![if success {"succeeded"} else {"failed"}, message, now(), id, lease, Utc::now().timestamp()])? == 1)
    }

    pub fn retry(&self, id: i64) -> Result<bool> {
        let db = self.db.lock().unwrap();
        Ok(db.execute("UPDATE jobs SET status='queued',attempts=0,worker_id=NULL,lease=NULL,message=NULL,updated_at=? WHERE id=? AND status='failed'", params![now(),id])? == 1)
    }

    pub fn validate_filename(scope: &Scope, name: &str) -> Result<()> {
        let allowed = if scope.is_recipe() {
            [
                "conanfile.py",
                "conanmanifest.txt",
                "conan_export.tgz",
                "conan_sources.tgz",
            ]
            .as_slice()
        } else {
            ["conaninfo.txt", "conanmanifest.txt", "conan_package.tgz"].as_slice()
        };
        if !allowed.contains(&name) {
            bail!(
                "unsupported artifact; this prototype supports Conan .tgz files without metadata"
            );
        }
        Ok(())
    }
}
