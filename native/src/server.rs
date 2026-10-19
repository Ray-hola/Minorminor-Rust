use crate::{ai, cleanup, discovery, fail, files, index, mole, now, scan, Result};
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use rusqlite::{params, OptionalExtension};
use rust_embed::RustEmbed;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Arc, Mutex, RwLock,
    },
    thread,
};

#[derive(RustEmbed)]
#[folder = "../public/"]
struct Assets;
pub struct Engine {
    pub home: PathBuf,
    pub data: PathBuf,
    pub db: PathBuf,
    pub resources: Mutex<PathBuf>,
    pub token: String,
    pub port: AtomicU16,
    pub files: files::Resources,
    inner: Mutex<Inner>,
    db_gate: RwLock<()>,
}
struct Inner {
    jobs: VecDeque<Value>,
    active: Option<String>,
    pid: Option<u32>,
    cancel: Arc<AtomicBool>,
    config: ai::Config,
    revision: u64,
    previews: HashMap<String, ai::Prepared>,
    models: Value,
    harness_enabled: bool,
}
impl Engine {
    pub fn new(home: PathBuf, data: PathBuf, resources: PathBuf) -> Result<Arc<Self>> {
        let home = fs::canonicalize(home)?;
        let db = index::initialize(&data)?;
        let conn = index::connect(&db)?;
        let config = index::setting(&conn, "ai-config")?
            .and_then(|v| ai::Config::validate(&v, &ai::Config::default()).ok())
            .unwrap_or_default();
        let jobs = index::setting(&conn, "last-job")?
            .filter(|v| !v.is_null())
            .map(|mut job| {
                if job["status"] == "running" {
                    job["status"] = json!("interrupted");
                }
                VecDeque::from([job])
            })
            .unwrap_or_default();
        let models = index::setting(&conn, "models-snapshot")?.unwrap_or(Value::Null);
        let harness_enabled = index::setting(&conn, "harness-enabled")?.is_some_and(|v| v == true);
        Ok(Arc::new(Self {
            home,
            data,
            db,
            resources: Mutex::new(resources),
            token: format!("{}{}", crate::id(), crate::id()),
            port: AtomicU16::new(0),
            files: files::Resources::default(),
            db_gate: RwLock::new(()),
            inner: Mutex::new(Inner {
                jobs,
                active: None,
                pid: None,
                cancel: Arc::new(AtomicBool::new(false)),
                config,
                revision: 0,
                previews: HashMap::new(),
                models,
                harness_enabled,
            }),
        }))
    }
    pub fn cancel(&self) {
        let mut state = self.inner.lock().unwrap();
        state.cancel.store(true, Ordering::SeqCst);
        if let Some(pid) = state.pid {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
        }
        if let Some(active) = state.active.clone() {
            if let Some(job) = state.jobs.iter_mut().find(|j| j["id"] == active) {
                job["cancelling"] = json!(true);
            }
        }
    }
    fn running(&self) -> bool {
        self.inner.lock().unwrap().active.is_some()
    }
    pub fn background_scan_running(&self) -> bool {
        let state = self.inner.lock().unwrap();
        state.active.as_ref().is_some_and(|id| {
            state
                .jobs
                .iter()
                .any(|job| job["id"] == *id && job["kind"] == "scan")
        })
    }
    fn begin(&self, kind: &str) -> Result<(String, Arc<AtomicBool>)> {
        let mut state = self.inner.lock().unwrap();
        if state.active.is_some() {
            return Err(fail("已有后台任务，请完成或停止后再操作"));
        }
        let id = crate::id();
        let cancel = Arc::new(AtomicBool::new(false));
        state.cancel = cancel.clone();
        state.active = Some(id.clone());
        state.jobs.push_back(
            json!({"id":id,"kind":kind,"status":"running","createdAt":now(),"progressAt":now(),"stats":{"stage":"preparing"}}),
        );
        while state.jobs.len() > 40 {
            state.jobs.pop_front();
        }
        Ok((id, cancel))
    }
    fn finish(&self, id: &str, mut result: Result<Value>, cancelled: bool) {
        if cancelled || result.is_err() {
            crate::storage::discard_repack(&self.data);
        }
        if !cancelled && result.as_ref().is_ok_and(|v| v["repackReady"] == true) {
            let _exclusive = self.db_gate.write().unwrap();
            match crate::storage::install_repack(&self.data) {
                Ok(after) => {
                    if let Ok(value) = &mut result {
                        value["after"] = after;
                        value["repackReady"] = json!(false);
                    }
                }
                Err(error) => {
                    crate::storage::discard_repack(&self.data);
                    if let Ok(value) = &mut result {
                        value["compactionWarning"] = json!(error.to_string());
                    }
                }
            }
        }
        let models = result
            .as_ref()
            .ok()
            .filter(|v| v.get("models").is_some())
            .cloned();
        let reset = result.as_ref().is_ok_and(|r| r["reset"] == true);
        if reset {
            self.files.issued.lock().unwrap().clear();
        }
        let saved = {
            let mut state = self.inner.lock().unwrap();
            if reset {
                state.previews.clear();
                state.models = Value::Null;
                state.jobs.retain(|j| j["id"] == id);
            }
            if let Some(models) = &models {
                state.models = models.clone();
            }
            let saved=state.jobs.iter_mut().find(|j|j["id"]==id).map(|job|{
                job["finishedAt"]=json!(now());
                job["status"]=json!(if cancelled{"cancelled"}else if result.is_ok(){"complete"}else{"failed"});
                match result{Ok(value)=>job["result"]=value,Err(error)=>job["error"]=json!(error.to_string())}
                json!({"id":id,"kind":job["kind"],"createdAt":job["createdAt"],"finishedAt":job["finishedAt"],"status":job["status"],"error":job["error"],"stats":job["stats"]})
            });
            state.active = None;
            state.pid = None;
            saved
        };
        // Disk pressure must not block progress, cancellation or status reads.
        let _persistence = self.db_gate.read().unwrap();
        if let Ok(db) = index::connect(&self.db) {
            if let Some(models) = models {
                let _ = index::save_setting(&db, "models-snapshot", &models);
            }
            if let Some(saved) = saved {
                let _ = index::save_setting(&db, "last-job", &saved);
                if saved["kind"] == "mole-execute" && saved["status"] != "complete" {
                    let status = if saved["status"] == "cancelled" {
                        "mole-cancelled"
                    } else {
                        "mole-failed"
                    };
                    let output = saved["stats"]["output"].as_str().unwrap_or("");
                    let result = json!({"jobId":id,"createdAt":now(),"status":status,"output":output,"error":saved["error"],"success":false});
                    let _ = index::save_setting(&db, "mole-result", &result);
                    let _ = index::save_setting(&db, "last-receipt", &result);
                    let _=db.execute("INSERT INTO receipts(job,created,path,status,bytes,reason) SELECT ?,?,?,?,0,? WHERE NOT EXISTS(SELECT 1 FROM receipts WHERE job=? AND status=?)",params![id,now(),index::path_string(&self.home),status,format!("{}\n{}",output,saved["error"]),id,status]);
                }
            }
        }
    }
    fn worker(&self, id: &str, payload: Value, cancel: &AtomicBool) -> Result<Value> {
        if cancel.load(Ordering::SeqCst) {
            return Err(fail("任务已停止"));
        }
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--worker")
            .env("MINORMINOR_WORKER", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        self.inner.lock().unwrap().pid = Some(pid);
        if cancel.load(Ordering::SeqCst) {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
        }
        let mut input = child.stdin.take().unwrap();
        let encoded = serde_json::to_vec(&payload)?;
        let writer = thread::spawn(move || {
            let _ = input.write_all(&encoded);
        });
        let stderr = child.stderr.take().unwrap();
        let errors = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.take(32768).read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let mut outcome = None;
        for line in BufReader::new(child.stdout.take().unwrap()).lines() {
            let line = line?;
            if line.len() > 8 * 1024 * 1024 {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
                let _ = child.wait();
                return Err(fail("后台响应超出限制"));
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match value["event"].as_str() {
                Some("progress") => {
                    let mut state = self.inner.lock().unwrap();
                    if let Some(job) = state.jobs.iter_mut().find(|j| j["id"] == id) {
                        merge_progress(job, &value["stats"]);
                    }
                }
                Some("complete") => outcome = Some(value),
                Some("error") => outcome = Some(value),
                _ => {}
            }
        }
        let status = child.wait()?;
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        let _ = writer.join();
        let errors = errors.join().unwrap_or_default();
        if cancel.load(Ordering::SeqCst) {
            return Err(fail(if payload["action"] == "index-recover" {
                "索引优化已停止；已有扫描结果保留"
            } else {
                "任务已停止；已发生的移动请查看逐文件记录"
            }));
        }
        let value = outcome.ok_or_else(|| {
            fail(if errors.trim().is_empty() {
                "后台进程未完整结束".into()
            } else {
                errors.chars().take(1000).collect::<String>()
            })
        })?;
        if !status.success() || value["event"] == "error" {
            return Err(fail(value["error"].as_str().unwrap_or("后台任务失败")));
        }
        Ok(value.get("result").cloned().unwrap_or(value))
    }
    fn background(self: &Arc<Self>, kind: &str, mut payload: Value) -> Result<Value> {
        let (id, cancel) = self.begin(kind)?;
        let initial_stats = if kind == "scan" {
            json!({"stage":"preparing","diskSnapshot":payload["diskSnapshot"],"estimatedBytes":payload["diskSnapshot"]["used"],"estimateSource":"volume-used","scanScope":if payload["disk"]==true{"disk"}else{"home"}})
        } else {
            json!({"stage":"preparing"})
        };
        if let Some(job) = self
            .inner
            .lock()
            .unwrap()
            .jobs
            .iter_mut()
            .find(|j| j["id"] == id)
        {
            job["stats"] = initial_stats.clone();
        }
        payload["jobId"] = json!(id);
        payload["home"] = json!(self.home);
        payload["data"] = json!(self.data);
        payload["resources"] = json!(*self.resources.lock().unwrap());
        payload["parentPid"] = json!(std::process::id());
        let engine = self.clone();
        let job_id = id.clone();
        let kind = kind.to_owned();
        thread::spawn(move || {
            if kind == "index-recover" {
                let _drain = engine.db_gate.write().unwrap();
            }
            let result = engine.worker(&job_id, payload, &cancel);
            let cancelled = cancel.load(Ordering::SeqCst);
            if kind == "scan" && result.is_err() && !cancelled {
                let cleanup = json!({"action":"index-recover","reset":false,"home":engine.home,"data":engine.data,"resources":*engine.resources.lock().unwrap(),"jobId":job_id,"parentPid":std::process::id()});
                let _ = engine.worker(&job_id, cleanup, &cancel);
            }
            engine.finish(&job_id, result, cancelled);
            if kind == "scan" && cancelled {
                if let Ok(db) = index::connect(&engine.db) {
                    let _ = db.execute("UPDATE scans SET status='cancelled' WHERE id=?", [&job_id]);
                }
            }
        });
        Ok(json!({"id":id,"stats":initial_stats}))
    }
    pub fn recover_index_if_needed(self: &Arc<Self>) -> Result<()> {
        let db = index::connect(&self.db)?;
        let keep = index::latest(&db)?.unwrap_or_default();
        let stale: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM scans WHERE id<>?)",
            [keep],
            |r| r.get(0),
        )?;
        let optimize = crate::compact::is_legacy(&db)?
            || (db.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))? != 2
                && db.query_row("PRAGMA freelist_count", [], |r| r.get::<_, u64>(0))? > 256);
        if (stale || optimize) && !self.running() {
            self.background(
                "index-recover",
                json!({"action":"index-recover","reset":false}),
            )?;
        }
        Ok(())
    }
    pub fn dispatch(self: &Arc<Self>, action: &str, body: Value) -> Result<Value> {
        if action == "jobs" {
            let state = self.inner.lock().unwrap();
            let total = state.jobs.len();
            let rows: Vec<Value> = state
                .jobs
                .iter()
                .enumerate()
                .map(|(i, j)| {
                    let mut j = j.clone();
                    if i + 1 < total && j["status"] != "running" {
                        j.as_object_mut().unwrap().remove("result");
                        if let Some(stats) = j["stats"].as_object_mut() {
                            stats.remove("output");
                        }
                    }
                    j
                })
                .collect();
            return Ok(json!({"jobs":rows}));
        }
        if action == "cancel" {
            self.cancel();
            return Ok(json!({"ok":true}));
        }
        let _database = self.db_gate.read().unwrap();
        let maintenance = {
            let state = self.inner.lock().unwrap();
            state.active.as_ref().is_some_and(|id| {
                state
                    .jobs
                    .iter()
                    .any(|j| j["id"] == *id && j["kind"] == "index-recover")
            })
        };
        if maintenance && !["bootstrap", "storage/info", "access/status"].contains(&action) {
            return Err(fail("正在优化扫描索引，请完成或停止后再操作"));
        }
        if action == "scan" {
            let disk = body["scope"].as_str() == Some("disk");
            let root = if disk && Path::new("/System/Volumes/Data").is_dir() {
                PathBuf::from("/System/Volumes/Data")
            } else if disk {
                PathBuf::from("/")
            } else {
                self.home.clone()
            };
            let (config, enabled) = {
                let state = self.inner.lock().unwrap();
                (state.config.clone(), state.harness_enabled)
            };
            let capacity = disk_status(&root)?;
            return self.background("scan", json!({"action":"scan","root":root,"disk":disk,"diskSnapshot":capacity,"ollamaBase":if config.provider=="ollama"{config.base_url.clone()}else{"http://127.0.0.1:11434".into()},"harnessConfig":if enabled&&!config.model.is_empty(){private_config(&config)}else{Value::Null}}));
        }
        if action == "storage/recover" {
            let reset = body["reset"] == true;
            if reset && body["confirmation"] != "REBUILD_SCAN_INDEX" {
                return Err(fail("请确认重建扫描索引"));
            }
            return self.background(
                "index-recover",
                json!({"action":"index-recover","reset":reset}),
            );
        }
        let mut db = index::connect(&self.db)?;
        let latest = index::latest(&db)?;
        let scan = body["scanId"]
            .as_str()
            .map(str::to_owned)
            .or(latest.clone());
        if [
            "overview",
            "projects",
            "tools",
            "files",
            "inspect",
            "files/measure",
            "projects/details",
            "candidates",
            "harnesses",
            "harnesses/analyze",
            "errors",
            "associations",
            "ai/prepare",
        ]
        .contains(&action)
        {
            index::assert_scan(&db, scan.as_deref().ok_or_else(|| fail("请先扫描"))?)?;
        }
        match action {
            "files/measure" => {
                let scan = scan.as_deref().ok_or_else(|| fail("请先扫描"))?;
                let resource = crate::measure::authorized(
                    &self.files,
                    body["id"].as_str().ok_or_else(|| fail("缺少目录ID"))?,
                    scan,
                )?;
                self.background(
                    "directory-size",
                    json!({"action":"directory-size","resource":resource}),
                )
            }
            "projects/details" => crate::project_info::details(
                &db,
                scan.as_deref().ok_or_else(|| fail("请先扫描"))?,
                body["projectId"]
                    .as_str()
                    .ok_or_else(|| fail("缺少项目ID"))?,
                body["offset"].as_i64().unwrap_or(0),
            ),
            "candidates" => crate::candidates::list(
                &db,
                &self.files,
                scan.as_deref().ok_or_else(|| fail("请先扫描"))?,
                &self.home,
                body["offset"].as_i64().unwrap_or(0),
            ),
            "history" => crate::reports::history(&db, body["offset"].as_i64().unwrap_or(0)),
            "import/legacy" => {
                if self.running() {
                    return Err(fail("请先结束当前任务"));
                }
                if body["confirmation"] != "IMPORT_WEB_HISTORY" {
                    return Err(fail("请确认导入选定 Web 导出"));
                }
                crate::reports::import_legacy(
                    &db,
                    &self.home,
                    Path::new(
                        body["path"]
                            .as_str()
                            .ok_or_else(|| fail("缺少导出文件路径"))?,
                    ),
                )
            }
            "legacy" => crate::reports::legacy_page(&db, &body),
            "harnesses" => crate::discovery_extra::candidates(
                &db,
                scan.as_deref().ok_or_else(|| fail("请先扫描"))?,
            ),
            "harnesses/analyze" => {
                if body["confirmation"] != "SEND_METADATA_FOR_ADVICE" {
                    return Err(fail("请先审阅候选结构元数据"));
                }
                let config = self.inner.lock().unwrap().config.clone();
                self.background("harness-analysis",json!({"action":"harness-analysis","scanId":scan,"offset":body["offset"],"all":body["all"],"config":private_config(&config)}))
            }
            "ai/review-payload" => {
                let (revision, config) = {
                    let state = self.inner.lock().unwrap();
                    (state.revision, state.config.clone())
                };
                let mut batch = crate::advice::batch(
                    &db,
                    scan.as_deref().unwrap_or("mole-only"),
                    &body,
                    &self.home,
                    revision,
                )?;
                batch["destination"] = json!(config.destination()?);
                batch["model"] = json!(config.model);
                Ok(batch)
            }
            "ai/review" => {
                if body["confirmation"] != "SEND_METADATA_FOR_ADVICE" {
                    return Err(fail("请先审阅将要发送的元数据"));
                }
                let id = body["inputId"]
                    .as_str()
                    .ok_or_else(|| fail("缺少元数据预览"))?;
                let input = index::setting(&db, &format!("advice-input-{id}"))?
                    .ok_or_else(|| fail("预览已失效"))?;
                let created = chrono::DateTime::parse_from_rfc3339(
                    input["createdAt"].as_str().unwrap_or(""),
                )?;
                let state = self.inner.lock().unwrap();
                if input["revision"] != state.revision
                    || chrono::Utc::now().timestamp() - created.timestamp() > 1800
                {
                    return Err(fail("连接或预览已变化，请重新审阅"));
                }
                let config = state.config.clone();
                drop(state);
                if input["source"] != "mole-plan" {
                    index::assert_scan(&db, input["scanId"].as_str().unwrap_or(""))?;
                }
                db.execute(
                    "DELETE FROM settings WHERE key=?",
                    [format!("advice-input-{id}")],
                )?;
                self.background(
                    "advice",
                    json!({"action":"advice","input":input,"config":private_config(&config)}),
                )
            }
            "mole/plan" => {
                if self.running() {
                    return Err(fail("已有任务"));
                }
                self.background(
                    "mole-plan",
                    json!({"action":"mole-plan","previewId":body["previewId"]}),
                )
            }
            "mole/plan/files" => crate::review::detail(
                &db,
                body["planId"].as_str().ok_or_else(|| fail("缺少清单ID"))?,
                body["groupId"].as_str(),
                body["offset"].as_i64().unwrap_or(0),
            ),
            "mole/plan/execute" => {
                if self.running() {
                    return Err(fail("已有任务"));
                }
                let resources = self.resources.lock().unwrap().clone();
                crate::review::claim(
                    &db,
                    body["planId"].as_str().ok_or_else(|| fail("缺少清单ID"))?,
                    body["ids"].as_array().ok_or_else(|| fail("请勾选分组"))?,
                    body["method"] == "permanent",
                    body["confirmation"].as_str().unwrap_or(""),
                    &resources,
                )?;
                self.background(
                    "mole-selection",
                    json!({"action":"mole-selection","selection":body}),
                )
            }
            "mole/execute" => {
                if self.running() {
                    return Err(fail("已有任务"));
                }
                crate::review::claim_mole(
                    &db,
                    body["previewId"]
                        .as_str()
                        .ok_or_else(|| fail("缺少预览ID"))?,
                    body["confirmation"].as_str().unwrap_or(""),
                    &self.resources.lock().unwrap(),
                )?;
                self.background(
                    "mole-execute",
                    json!({"action":"mole-execute","previewId":body["previewId"]}),
                )
            }
            "mole/output" => {
                let latest = [
                    index::setting(&db, "mole-result")?,
                    index::setting(&db, "mole-preview")?,
                ]
                .into_iter()
                .flatten()
                .filter(|v| !v.is_null())
                .max_by_key(|v| v["createdAt"].as_str().unwrap_or("").to_owned());
                Ok(latest.unwrap_or(Value::Null))
            }
            "bootstrap" => {
                let state = self.inner.lock().unwrap();
                let scan_info = latest
                    .as_deref()
                    .map(|id| index::scan_info(&db, id))
                    .transpose()?;
                let stale = index::setting(&db, "dirty")?.is_some_and(|v| !v.is_null());
                Ok(
                    json!({"token":self.token,"preferences":index::setting(&db,"ui-preferences")?.unwrap_or_else(||json!({})),"native":true,"version":env!("CARGO_PKG_VERSION"),"access":crate::access::status(&db)?,"indexStorage":crate::storage::status(&db)?,"home":self.home,"disk":disk_status(&self.home)?,"scan":scan_info,"stale":stale,"jobs":state.jobs,"config":state.config.public(state.revision),"models":state.models,"mole":mole::metadata(&self.resources.lock().unwrap()),"lastReceipt":index::setting(&db,"last-receipt")?,"molePreview":index::setting(&db,"mole-preview")?,"advice":index::setting(&db,"ai-advice")?,"harnessAnalysis":index::setting(&db,"harness-enabled")?.unwrap_or(json!(false)),"legacy":index::setting(&db,"legacy-import")?.map(|v|json!({"createdAt":v["createdAt"],"stale":true}))}),
                )
            }
            "access/status" => crate::access::status(&db),
            "access/acknowledge" => {
                crate::access::acknowledge(&db)?;
                crate::access::status(&db)
            }
            "storage/info" => crate::storage::status(&db),
            "preferences" => {
                if !["zh-CN", "en"].contains(&body["locale"].as_str().unwrap_or(""))
                    || body.as_object().is_none_or(|v| v.len() != 1)
                {
                    return Err(fail("Unsupported preferences"));
                }
                let value = json!({"locale":body["locale"]});
                index::save_setting(&db, "ui-preferences", &value)?;
                Ok(value)
            }
            "scan" => {
                let disk = body["scope"].as_str() == Some("disk");
                let root = if disk && Path::new("/System/Volumes/Data").is_dir() {
                    PathBuf::from("/System/Volumes/Data")
                } else if disk {
                    PathBuf::from("/")
                } else {
                    self.home.clone()
                };
                self.background("scan", json!({"action":"scan","root":root,"disk":disk}))
            }
            "cancel" => {
                self.cancel();
                Ok(json!({"ok":true}))
            }
            "overview" => scan::overview(&db, scan.as_deref().ok_or_else(|| fail("请先扫描"))?),
            "projects" => files::projects(&db, scan.as_deref().ok_or_else(|| fail("请先扫描"))?),
            "tools" => discovery::tools(&db, scan.as_deref().ok_or_else(|| fail("请先扫描"))?),
            "files" => files::list(
                &db,
                &self.files,
                &self.home,
                scan.as_deref().ok_or_else(|| fail("请先扫描"))?,
                &body,
            ),
            "inspect" => {
                let scan = scan.ok_or_else(|| fail("请先扫描"))?;
                let key = body["id"].as_str().ok_or_else(|| fail("缺少文件 ID"))?;
                files::inspect(&self.files.get(key, &scan)?)
            }
            "preview" => {
                if self.running() {
                    return Err(fail("请先结束后台任务"));
                }
                {
                    let scan = scan.as_deref().ok_or_else(|| fail("请先扫描"))?;
                    let mut body = body.clone();
                    body.as_object_mut()
                        .ok_or_else(|| fail("无效请求"))?
                        .remove("paths");
                    if let Some(ids) = body["fileIds"].as_array() {
                        let mut paths = Vec::new();
                        for id in ids {
                            paths.push(json!(
                                self.files
                                    .get(id.as_str().ok_or_else(|| fail("无效文件ID"))?, scan)?
                                    .path
                            ));
                        }
                        body["paths"] = json!(paths);
                    }
                    cleanup::prepare(&mut db, scan, &body, &self.home)
                }
            }
            "preview/files" => cleanup::plan_files(
                &db,
                body["planId"].as_str().ok_or_else(|| fail("缺少预览 ID"))?,
                body["offset"].as_u64().unwrap_or(0),
            ),
            "execute" => {
                if self.running() {
                    return Err(fail("已有后台任务"));
                }
                let _ = cleanup::claim(&mut db, &body)?;
                self.background("cleanup", json!({"action":"cleanup","selection":body}))
            }
            "receipts" => {
                let job = body["jobId"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| {
                        index::setting(&db, "last-receipt")
                            .ok()
                            .flatten()
                            .and_then(|v| v["jobId"].as_str().map(str::to_owned))
                    })
                    .ok_or_else(|| fail("暂无整理记录"))?;
                cleanup::receipt(&db, &job, body["offset"].as_i64().unwrap_or(0))
            }
            "errors" | "associations" => crate::reports::diagnostics(
                &db,
                scan.as_deref().ok_or_else(|| fail("请先扫描"))?,
                action,
                &body,
            ),
            "ai/config" => {
                if self.running() {
                    return Err(fail("请先结束当前任务后修改模型连接"));
                }
                let mut state = self.inner.lock().unwrap();
                let config = ai::Config::validate(&body, &state.config)?;
                index::save_setting(&db, "ai-config", &config.public(state.revision + 1))?;
                if body.get("harnessAnalysis").is_some() {
                    index::save_setting(
                        &db,
                        "harness-enabled",
                        &json!(body["harnessAnalysis"] == true),
                    )?;
                    state.harness_enabled = body["harnessAnalysis"] == true;
                }
                state.config = config;
                state.revision += 1;
                state.previews.clear();
                Ok(state.config.public(state.revision))
            }
            "ai/prepare" => {
                let scan = scan.ok_or_else(|| fail("请先扫描"))?;
                let resource = self.files.get(
                    body["id"]
                        .as_str()
                        .ok_or_else(|| fail("请点选文件或文件夹"))?,
                    &scan,
                )?;
                let (revision, config) = {
                    let state = self.inner.lock().unwrap();
                    (state.revision, state.config.clone())
                };
                if config.model.is_empty() {
                    return Err(fail("请先连接模型"));
                }
                let mut stmt=db.prepare("SELECT path FROM projects WHERE scan=?1 AND path<>?2 AND EXISTS(SELECT 1 FROM projects WHERE scan=?1 AND path=?3)")?;
                let excluded = stmt
                    .query_map(
                        params![
                            scan,
                            index::path_string(&resource.path),
                            index::path_string(&resource.root)
                        ],
                        |r| r.get::<_, String>(0),
                    )?
                    .collect::<std::result::Result<Vec<_>, _>>()?
                    .into_iter()
                    .map(PathBuf::from)
                    .filter(|p| p.starts_with(&resource.path))
                    .collect::<Vec<_>>();
                let prepared = ai::prepare_excluding(resource, &self.home, revision, &excluded)?;
                let public = json!({"id":prepared.id,"name":prepared.resource.path.file_name().unwrap_or_default().to_string_lossy(),"path":prepared.resource.path,"coverage":prepared.coverage,"files":prepared.public_files,"destination":config.destination()?,"model":config.model,"revision":revision});
                let mut state = self.inner.lock().unwrap();
                state
                    .previews
                    .retain(|_, p| chrono::Utc::now().timestamp() - p.created <= 600);
                if state.previews.len() >= 20 {
                    state.previews.clear();
                }
                state.previews.insert(prepared.id.clone(), prepared);
                Ok(public)
            }
            "ai/summary" => {
                if body["confirmation"] != "SUMMARIZE_SELECTED_RESOURCE" {
                    return Err(fail("请确认发送点选内容进行总结"));
                }
                let (prepared, config) = {
                    let mut state = self.inner.lock().unwrap();
                    let key = body["previewId"]
                        .as_str()
                        .ok_or_else(|| fail("缺少总结预览"))?;
                    let prepared = state
                        .previews
                        .remove(key)
                        .ok_or_else(|| fail("预览已使用或过期"))?;
                    if body["revision"].as_u64() != Some(state.revision)
                        || prepared.revision != state.revision
                    {
                        return Err(fail("模型连接已变化，请重新预览"));
                    }
                    (prepared, state.config.clone())
                };
                self.background("summary",json!({"action":"summary","prepared":prepared,"config":private_config(&config)}))
            }
            "ai/payload" => {
                let scan = scan.ok_or_else(|| fail("请先扫描"))?;
                let tools = discovery::tools(&db, &scan)?;
                let items:Vec<Value>=tools["rows"].as_array().unwrap().iter().take(20).map(|row|json!({"id":row["id"],"name":row["name"],"bytes":row["bytes"],"files":row["files"],"uncertain":row["uncertain"],"evidence":crate::advice::safe_evidence(&row["evidence"])})).collect();
                let state = self.inner.lock().unwrap();
                Ok(
                    json!({"scanId":scan,"revision":state.revision,"destination":state.config.destination()?,"model":state.config.model,"payload":{"items":items},"analysisDocumentRead":false}),
                )
            }
            "ai/analyze" => {
                if body["confirmation"] != "SEND_METADATA_FOR_ADVICE" {
                    return Err(fail("请先审阅将要发送的元数据"));
                }
                // Avoid recursively taking the reader lock while maintenance waits.
                drop(db);
                drop(_database);
                let payload = self.dispatch("ai/payload", json!({"scanId":scan}))?;
                if payload["revision"] != body["revision"] || payload["scanId"] != body["scanId"] {
                    return Err(fail("扫描或模型连接已变化，请重新核对"));
                }
                let config = self.inner.lock().unwrap().config.clone();
                self.background("analysis",json!({"action":"analysis","payload":payload["payload"],"config":private_config(&config)}))
            }
            "models" => {
                let config = self.inner.lock().unwrap().config.clone();
                self.background("models",json!({"action":"models","baseUrl":if config.provider=="ollama"{config.base_url}else{"http://127.0.0.1:11434".into()}}))
            }
            "models/delete" => {
                if body["confirmation"] != "DELETE_OLLAMA_MODEL" {
                    return Err(fail("请先确认移除具体模型"));
                }
                let snapshot = self.inner.lock().unwrap().models.clone();
                self.background(
                    "model-delete",
                    json!({"action":"model-delete","snapshot":snapshot,"modelId":body["id"]}),
                )
            }
            "mole/preview" => {
                if self.running() {
                    return Err(fail("已有后台任务"));
                }
                index::save_setting(&db, "mole-preview", &Value::Null)?;
                self.background("mole-preview",json!({"action":"mole-preview","command":body["command"].as_str().unwrap_or("clean")}))
            }
            "export" => self.export(&db, &body, scan.as_deref()),
            "privacy" => {
                crate::access::open_settings()?;
                Ok(json!({"ok":true}))
            }
            _ => Err(fail("此接口不存在")),
        }
    }
    fn export(&self, db: &rusqlite::Connection, body: &Value, scan: Option<&str>) -> Result<Value> {
        crate::reports::export(db, &self.home, scan, body)
    }
}
pub fn merge_progress(job: &mut Value, update: &Value) {
    if update.get("stage").is_some() && update["stage"] != job["stats"]["stage"] {
        if let Some(stats) = job["stats"].as_object_mut() {
            for key in ["completed", "total", "removed", "indexTable"] {
                stats.remove(key);
            }
        }
    }
    if !job["stats"].is_object() {
        job["stats"] = json!({});
    }
    if let Some(update) = update.as_object() {
        for (key, value) in update {
            job["stats"][key] = value.clone();
        }
    }
    job["progressAt"] = json!(now());
}
fn private_config(config: &ai::Config) -> Value {
    json!({"provider":config.provider,"baseUrl":config.base_url,"model":config.model,"apiKey":config.api_key})
}
pub fn worker(payload: Value) -> Result<()> {
    let home = PathBuf::from(
        payload["home"]
            .as_str()
            .ok_or_else(|| fail("缺少用户目录"))?,
    );
    let data = PathBuf::from(
        payload["data"]
            .as_str()
            .ok_or_else(|| fail("缺少数据目录"))?,
    );
    let resources = PathBuf::from(payload["resources"].as_str().unwrap_or(""));
    let job = payload["jobId"]
        .as_str()
        .ok_or_else(|| fail("缺少任务 ID"))?;
    let mut recovery_progress = std::time::Instant::now();
    let result = match payload["action"].as_str().unwrap_or("") {
        "index-recover" => {
            let db = index::connect(&data.join("index.sqlite3"))?;
            let mut result = crate::storage::recover(
                &db,
                payload["reset"] == true,
                |table, removed| {
                    if recovery_progress.elapsed().as_millis() >= 250 || removed == 0 {
                        println!(
                            "{}",
                            json!({"event":"progress","stats":{"stage":"index-recovery","removed":if table=="compact"{0}else{removed},"indexTable":table,"currentPath":data.join("index.sqlite3")}})
                        );
                        std::io::stdout().flush()?;
                        recovery_progress = std::time::Instant::now();
                    }
                    Ok(())
                },
            )?;
            if payload["reset"] != true {
                match crate::storage::prepare_repack(&db, |table, removed| {
                    println!(
                        "{}",
                        json!({"event":"progress","stats":{"stage":"index-recovery","indexTable":table,"removed":removed}})
                    );
                    std::io::stdout().flush()?;
                    Ok(())
                }) {
                    Ok(ready) => result["repackReady"] = json!(ready),
                    Err(error) => result["compactionWarning"] = json!(error.to_string()),
                }
            }
            result
        }
        "scan" => {
            let db = index::connect(&data.join("index.sqlite3"))?;
            let estimate = scan_estimate(
                &db,
                Path::new(payload["root"].as_str().unwrap_or("")),
                &payload["diskSnapshot"],
            )?;
            println!("{}", json!({"event":"progress","stats":estimate}));
            std::io::stdout().flush()?;
            drop(db);
            scan::run(scan::ScanSpec {
                id: job.into(),
                root: PathBuf::from(payload["root"].as_str().unwrap_or("")),
                home: home.clone(),
                data: data.clone(),
                disk: payload["disk"].as_bool().unwrap_or(false),
                publish: false,
            })?;
            if !payload["harnessConfig"].is_null() {
                let config: ai::Config = serde_json::from_value(payload["harnessConfig"].clone())?;
                let db = index::connect(&data.join("index.sqlite3"))?;
                let result = crate::discovery_extra::analyze_candidates(&db, job, &config, 0, true);
                if let Err(error) = result {
                    index::save_setting(&db, "harness-analysis-error", &json!(error.to_string()))?;
                }
            }
            println!("{}", json!({"event":"progress","stats":{"stage":"models"}}));
            std::io::stdout().flush()?;
            let mut models = ai::ollama_models_from(
                payload["ollamaBase"]
                    .as_str()
                    .unwrap_or("http://127.0.0.1:11434"),
            )
            .unwrap_or_else(
                |_| json!({"createdAt":now(),"models":[],"reason":"未能连接到本机 Ollama"}),
            );
            models["scanId"] = json!(job);
            let db = index::connect(&data.join("index.sqlite3"))?;
            db.execute_batch("BEGIN IMMEDIATE")?;
            index::save_setting(&db, "models-snapshot", &models)?;
            index::save_setting(&db, "dirty", &Value::Null)?;
            db.execute("UPDATE scans SET status='complete' WHERE id=?", [job])?;
            db.execute_batch("COMMIT")?;
            let recovery = crate::storage::recover(&db, false, |table, removed| {
                if recovery_progress.elapsed().as_millis() >= 250 || removed == 0 {
                    println!(
                        "{}",
                        json!({"event":"progress","stats":{"stage":"index-recovery","removed":if table=="compact"{0}else{removed},"indexTable":table,"currentPath":data.join("index.sqlite3")}})
                    );
                    std::io::stdout().flush()?;
                    recovery_progress = std::time::Instant::now();
                }
                Ok(())
            });
            if let Err(error) = recovery {
                models["indexWarning"] = json!(error.to_string());
            }
            models
        }
        "summary" => ai::summary(
            serde_json::from_value(payload["prepared"].clone())?,
            &serde_json::from_value(payload["config"].clone())?,
        )?,
        "analysis" => ai::analyze(
            &serde_json::from_value(payload["config"].clone())?,
            &payload["payload"],
        )?,
        "models" => ai::ollama_models_from(
            payload["baseUrl"]
                .as_str()
                .unwrap_or("http://127.0.0.1:11434"),
        )?,
        "model-delete" => {
            let result = ai::delete_model(
                &payload["snapshot"],
                payload["modelId"].as_str().unwrap_or(""),
            )?;
            let mut models = ai::ollama_models_from(
                payload["snapshot"]["endpoint"]
                    .as_str()
                    .unwrap_or("http://127.0.0.1:11434"),
            )?;
            models["deletion"] = result;
            models
        }
        "directory-size" => {
            let mut db = index::connect(&data.join("index.sqlite3"))?;
            crate::measure::run(
                &mut db,
                serde_json::from_value(payload["resource"].clone())?,
                &home,
                &data,
            )?
        }
        "advice" => {
            let db = index::connect(&data.join("index.sqlite3"))?;
            let result = crate::advice::review(
                &serde_json::from_value(payload["config"].clone())?,
                &payload["input"],
            )?;
            let old = index::setting(&db, "ai-advice")?;
            let mut merged = result.clone();
            if let Some(old) = old {
                if old["scanId"] == result["scanId"]
                    && old["source"] == result["source"]
                    && old["planId"] == result["planId"]
                {
                    let mut items = old["items"].as_array().cloned().unwrap_or_default();
                    for row in result["items"].as_array().unwrap() {
                        items.retain(|r| r["id"] != row["id"]);
                        items.push(row.clone());
                    }
                    merged["items"] = json!(items);
                }
            }
            index::save_setting(&db, "ai-advice", &merged)?;
            merged
        }
        "harness-analysis" => {
            let db = index::connect(&data.join("index.sqlite3"))?;
            crate::discovery_extra::analyze_candidates(
                &db,
                payload["scanId"].as_str().ok_or_else(|| fail("缺少扫描"))?,
                &serde_json::from_value(payload["config"].clone())?,
                payload["offset"].as_u64().unwrap_or(0) as usize,
                payload["all"] == true,
            )?
        }
        "mole-preview" => {
            let db = index::connect(&data.join("index.sqlite3"))?;
            crate::review::run_preview(
                &db,
                &resources,
                &home,
                payload["command"].as_str().unwrap_or("clean"),
                job,
            )?
        }
        "mole-plan" => {
            let db = index::connect(&data.join("index.sqlite3"))?;
            crate::review::prepare(
                &db,
                &home,
                &resources,
                payload["previewId"]
                    .as_str()
                    .ok_or_else(|| fail("缺少预览ID"))?,
            )?
        }
        "mole-selection" => {
            let db = index::connect(&data.join("index.sqlite3"))?;
            let p = &payload["selection"];
            crate::review::execute(
                &db,
                &home,
                &resources,
                p["planId"].as_str().unwrap_or(""),
                p["ids"].as_array().ok_or_else(|| fail("缺少勾选范围"))?,
                p["method"] == "permanent",
                job,
            )?
        }
        "mole-execute" => {
            let db = index::connect(&data.join("index.sqlite3"))?;
            index::save_setting(
                &db,
                "last-receipt",
                &json!({"jobId":job,"method":"Mole永久清理","createdAt":now(),"status":"running"}),
            )?;
            db.execute("INSERT INTO receipts(job,created,path,status,bytes,reason) VALUES(?,?,?,'mole-started',0,'已明确确认整次Mole永久清理')",params![job,now(),index::path_string(&home)])?;
            let mut result = mole::run_stream(&resources, &home, "clean", true, |stats| {
                println!("{}", json!({"event":"progress","stats":stats}));
                let _ = std::io::stdout().flush();
            })?;
            result["jobId"] = json!(job);
            index::save_setting(&db, "mole-result", &result)?;
            index::save_setting(
                &db,
                "last-receipt",
                &json!({"jobId":job,"method":"Mole永久清理","createdAt":now()}),
            )?;
            db.execute(
                "INSERT INTO receipts(job,created,path,status,bytes,reason) VALUES(?,?,?,?,0,?)",
                params![
                    job,
                    now(),
                    index::path_string(&home),
                    if result["success"] == true {
                        "mole-complete"
                    } else {
                        "mole-failed"
                    },
                    result["output"].as_str().unwrap_or("")
                ],
            )?;
            if result["success"] != true {
                return Err(fail("Mole 未成功结束，请查看原始输出和记录"));
            }
            result
        }
        "cleanup" => {
            let db = index::connect(&data.join("index.sqlite3"))?;
            let files = cleanup::claimed_files(&db, &payload["selection"])?;
            index::save_setting(
                &db,
                "last-receipt",
                &json!({"jobId":job,"createdAt":now(),"status":"running"}),
            )?;
            let mut moved = 0;
            let mut skipped = 0;
            let mut bytes = 0;
            for (i, file) in files.iter().enumerate() {
                match cleanup::process_reviewed(
                    &db,
                    job,
                    file,
                    &home,
                    false,
                    payload["selection"]["method"] == "permanent",
                    |file| {
                        if ["cache", "log"].contains(&file.kind.as_str()) {
                            mole::guard(&resources, &home, &file.root)?;
                        }
                        mole::unused(&file.path)
                    },
                ) {
                    Ok(_) => {
                        moved += 1;
                        bytes += file.bytes;
                    }
                    Err(error) => {
                        cleanup::record_skip(&db, job, file, &error.to_string())?;
                        skipped += 1;
                    }
                }
                println!(
                    "{}",
                    json!({"event":"progress","stats":{"completed":i+1,"total":files.len(),"moved":moved,"skipped":skipped,"bytes":bytes,"currentPath":file.path}})
                );
                std::io::stdout().flush()?;
            }
            let result = json!({"jobId":job,"moved":moved,"skipped":skipped,"bytes":bytes,"createdAt":now(),"method":if payload["selection"]["method"]=="permanent"{"永久删除"}else{"移到废纸篓"}});
            index::save_setting(&db, "last-receipt", &result)?;
            result
        }
        _ => return Err(fail("后台任务类型无效")),
    };
    println!("{}", json!({"event":"complete","result":result}));
    Ok(())
}
// An estimate is frozen before traversal. A previous *complete, same-root* scan
// is a better baseline for a home-only scope than the entire volume's used space.
pub fn scan_estimate(db: &rusqlite::Connection, root: &Path, capacity: &Value) -> Result<Value> {
    let previous:Option<String>=db.query_row("SELECT stats FROM scans WHERE root=? AND status='complete' ORDER BY rowid DESC LIMIT 1",[index::path_string(root)],|r|r.get(0)).optional()?;
    let previous = previous
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|s| s["bytes"].as_u64())
        .filter(|v| *v > 0);
    Ok(
        json!({"estimatedBytes":previous.or_else(||capacity["used"].as_u64().filter(|v|*v>0)),"estimateSource":if previous.is_some(){"previous-scan"}else{"volume-used"}}),
    )
}
pub fn disk_status(home: &Path) -> Result<Value> {
    let path = std::ffi::CString::new(home.as_os_str().as_encoded_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stats = unsafe { stats.assume_init() };
    let total = stats.f_blocks as u64 * stats.f_frsize as u64;
    let free = stats.f_bavail as u64 * stats.f_frsize as u64;
    Ok(
        json!({"total":total,"free":free,"used":total.saturating_sub(free),"capturedAt":now(),"home":home,"platform":std::env::consts::OS,"arch":std::env::consts::ARCH}),
    )
}
pub async fn serve(engine: Arc<Engine>, listener: tokio::net::TcpListener) -> Result<()> {
    engine
        .port
        .store(listener.local_addr()?.port(), Ordering::Relaxed);
    axum::serve(
        listener,
        Router::new().fallback(any(handler)).with_state(engine),
    )
    .await?;
    Ok(())
}
async fn handler(State(engine): State<Arc<Engine>>, request: Request) -> Response {
    let port = engine.port.load(Ordering::Relaxed);
    let host = request
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !allowed.iter().any(|s| s == host) {
        return response(StatusCode::FORBIDDEN, json!({"error":"仅允许本机访问"}));
    }
    if let Some(origin) = request
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
    {
        if !allowed
            .iter()
            .any(|host| origin == format!("http://{host}"))
        {
            return response(StatusCode::FORBIDDEN, json!({"error":"拒绝跨站请求"}));
        }
    }
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    if let Some(action) = path.strip_prefix("/api/") {
        let read = [
            "jobs",
            "history",
            "candidates",
            "projects/details",
            "legacy",
            "harnesses",
            "ai/review-payload",
            "mole/plan/files",
            "mole/output",
            "bootstrap",
            "overview",
            "projects",
            "tools",
            "files",
            "preview/files",
            "receipts",
            "errors",
            "associations",
            "ai/payload",
        ];
        if method != "POST" && !read.contains(&action) {
            return response(
                StatusCode::METHOD_NOT_ALLOWED,
                json!({"error":"操作必须使用 POST"}),
            );
        }
        if method == "POST"
            && request
                .headers()
                .get("x-minorminor-token")
                .and_then(|v| v.to_str().ok())
                != Some(engine.token.as_str())
        {
            return response(
                StatusCode::FORBIDDEN,
                json!({"error":"会话已过期，请重新打开应用"}),
            );
        }
        let query = request.uri().query().unwrap_or("").to_owned();
        let body = match to_bytes(request.into_body(), 256 * 1024).await {
            Ok(v) => v,
            Err(_) => {
                return response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({"error":"请求超出限制"}),
                )
            }
        };
        let mut value = if body.is_empty() {
            json!({})
        } else {
            match serde_json::from_slice::<Value>(&body) {
                Ok(v) => v,
                Err(_) => {
                    return response(StatusCode::BAD_REQUEST, json!({"error":"请求 JSON 无效"}))
                }
            }
        };
        if !value.is_object() {
            return response(StatusCode::BAD_REQUEST, json!({"error":"请求必须是对象"}));
        }
        if let Ok(url) = reqwest::Url::parse(&format!("http://localhost/?{query}")) {
            for (key, data) in url.query_pairs() {
                value[key.as_ref()] = data
                    .parse::<u64>()
                    .map(|v| json!(v))
                    .unwrap_or_else(|_| json!(data));
            }
        }
        let action = action.to_owned();
        let task = tokio::task::spawn_blocking(move || engine.dispatch(&action, value)).await;
        match task {
            Ok(Ok(value)) => response(StatusCode::OK, value),
            Ok(Err(error)) => response(StatusCode::BAD_REQUEST, json!({"error":error.to_string()})),
            Err(_) => response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"后台任务异常"}),
            ),
        }
    } else {
        if method != "GET" {
            return response(
                StatusCode::METHOD_NOT_ALLOWED,
                json!({"error":"不支持的方法"}),
            );
        }
        let key = if path == "/" {
            "index.html"
        } else {
            path.trim_start_matches('/')
        };
        let Some(asset) = Assets::get(key) else {
            return response(StatusCode::NOT_FOUND, json!({"error":"文件不存在"}));
        };
        let mime = if key.ends_with(".js") {
            "text/javascript; charset=utf-8"
        } else if key.ends_with(".css") {
            "text/css; charset=utf-8"
        } else if key.ends_with(".svg") {
            "image/svg+xml"
        } else if key.ends_with(".woff2") {
            "font/woff2"
        } else if key.ends_with(".html") {
            "text/html; charset=utf-8"
        } else {
            "application/octet-stream"
        };
        let data = if key == "index.html" {
            String::from_utf8_lossy(&asset.data)
                .replace("/app.js", "/native.js")
                .replace(
                    "</head>",
                    "<link rel=\"stylesheet\" href=\"/native.css\" /></head>",
                )
                .into_bytes()
        } else {
            asset.data.to_vec()
        };
        let mut output = Response::new(Body::from(data));
        output
            .headers_mut()
            .insert("content-type", HeaderValue::from_static(mime));
        secure_headers(&mut output);
        output
    }
}
fn response(status: StatusCode, value: Value) -> Response {
    let mut response = (status, axum::Json(value)).into_response();
    secure_headers(&mut response);
    response
}
fn secure_headers(response: &mut Response) {
    for (name,value) in [("x-content-type-options","nosniff"),("referrer-policy","no-referrer"),("cache-control","no-store"),("x-frame-options","DENY"),("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")]{response.headers_mut().insert(name,HeaderValue::from_static(value));}
}
