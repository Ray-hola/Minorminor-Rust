use crate::{ai, cleanup, discovery, fail, files, index, mole, now, scan, Result};
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use rusqlite::params;
use rust_embed::RustEmbed;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Arc, Mutex,
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
        Ok(Arc::new(Self {
            home,
            data,
            db,
            resources: Mutex::new(resources),
            token: format!("{}{}", crate::id(), crate::id()),
            port: AtomicU16::new(0),
            files: files::Resources::default(),
            inner: Mutex::new(Inner {
                jobs,
                active: None,
                pid: None,
                cancel: Arc::new(AtomicBool::new(false)),
                config,
                revision: 0,
                previews: HashMap::new(),
                models,
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
            json!({"id":id,"kind":kind,"status":"running","createdAt":now(),"stats":{}}),
        );
        while state.jobs.len() > 40 {
            state.jobs.pop_front();
        }
        Ok((id, cancel))
    }
    fn finish(&self, id: &str, result: Result<Value>, cancelled: bool) {
        let mut state = self.inner.lock().unwrap();
        let models = result
            .as_ref()
            .ok()
            .filter(|v| v.get("models").is_some())
            .cloned();
        if let Some(models) = models {
            state.models = models.clone();
            if let Ok(db) = index::connect(&self.db) {
                let _ = index::save_setting(&db, "models-snapshot", &models);
            }
        }
        if let Some(job) = state.jobs.iter_mut().find(|j| j["id"] == id) {
            job["finishedAt"] = json!(now());
            job["status"] = json!(if cancelled {
                "cancelled"
            } else if result.is_ok() {
                "complete"
            } else {
                "failed"
            });
            match result {
                Ok(value) => job["result"] = value,
                Err(error) => job["error"] = json!(error.to_string()),
            }
            // Persist status only. AI excerpts and generated summaries stay in memory.
            if let Ok(db) = index::connect(&self.db) {
                let saved = json!({"id":id,"kind":job["kind"],"createdAt":job["createdAt"],"finishedAt":job["finishedAt"],"status":job["status"],"error":job["error"]});
                let _ = index::save_setting(&db, "last-job", &saved);
            }
        }
        state.active = None;
        state.pid = None;
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
                        job["stats"] = value["stats"].clone();
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
            return Err(fail("任务已停止；已发生的移动请查看逐文件记录"));
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
        payload["jobId"] = json!(id);
        payload["home"] = json!(self.home);
        payload["data"] = json!(self.data);
        payload["resources"] = json!(*self.resources.lock().unwrap());
        payload["parentPid"] = json!(std::process::id());
        let engine = self.clone();
        let job_id = id.clone();
        let kind = kind.to_owned();
        thread::spawn(move || {
            let result = engine.worker(&job_id, payload, &cancel);
            if kind == "scan" && cancel.load(Ordering::SeqCst) {
                if let Ok(db) = index::connect(&engine.db) {
                    let _ = db.execute("UPDATE scans SET status='cancelled' WHERE id=?", [&job_id]);
                }
            }
            engine.finish(&job_id, result, cancel.load(Ordering::SeqCst));
        });
        Ok(json!({"id":id}))
    }
    pub fn dispatch(self: &Arc<Self>, action: &str, body: Value) -> Result<Value> {
        let mut db = index::connect(&self.db)?;
        let latest = index::latest(&db)?;
        let scan = body["scanId"]
            .as_str()
            .map(str::to_owned)
            .or(latest.clone());
        match action {
            "bootstrap"=>{
                let state=self.inner.lock().unwrap();let scan_info=latest.as_deref().map(|id|index::scan_info(&db,id)).transpose()?;
                let stale=index::setting(&db,"dirty")?.is_some_and(|v|!v.is_null());
                Ok(json!({"token":self.token,"preferences":index::setting(&db,"ui-preferences")?.unwrap_or_else(||json!({})),"native":true,"version":"0.2.0","home":self.home,"disk":disk_status(&self.home)?,"scan":scan_info,"stale":stale,"jobs":state.jobs,"config":state.config.public(state.revision),"models":state.models,"mole":mole::metadata(&self.resources.lock().unwrap()),"lastReceipt":index::setting(&db,"last-receipt")?}))
            }
            "preferences"=>{
                if !["zh-CN","en"].contains(&body["locale"].as_str().unwrap_or(""))||body.as_object().is_none_or(|v|v.len()!=1){return Err(fail("Unsupported preferences"));}
                let value=json!({"locale":body["locale"]});index::save_setting(&db,"ui-preferences",&value)?;Ok(value)
            }
            "scan"=>{
                let disk=body["scope"].as_str()==Some("disk");let root=if disk&&Path::new("/System/Volumes/Data").is_dir(){PathBuf::from("/System/Volumes/Data")}else if disk{PathBuf::from("/")}else{self.home.clone()};
                self.background("scan",json!({"action":"scan","root":root,"disk":disk}))
            }
            "cancel"=>{self.cancel();Ok(json!({"ok":true}))}
            "overview"=>scan::overview(&db,scan.as_deref().ok_or_else(||fail("请先扫描"))?),
            "projects"=>files::projects(&db,scan.as_deref().ok_or_else(||fail("请先扫描"))?),
            "tools"=>discovery::tools(&db,scan.as_deref().ok_or_else(||fail("请先扫描"))?),
            "files"=>files::list(&db,&self.files,&self.home,scan.as_deref().ok_or_else(||fail("请先扫描"))?,&body),
            "inspect"=>{let scan=scan.ok_or_else(||fail("请先扫描"))?;let key=body["id"].as_str().ok_or_else(||fail("缺少文件 ID"))?;files::inspect(&self.files.get(key,&scan)?)},
            "preview"=>{if self.running(){return Err(fail("请先结束后台任务"));}cleanup::prepare(&mut db,scan.as_deref().ok_or_else(||fail("请先扫描"))?,&body,&self.home)},
            "preview/files"=>cleanup::plan_files(&db,body["planId"].as_str().ok_or_else(||fail("缺少预览 ID"))?,body["offset"].as_u64().unwrap_or(0)),
            "execute"=>{
                if self.running(){return Err(fail("已有后台任务"));}
                let _=cleanup::claim(&mut db,&body)?;
                self.background("cleanup",json!({"action":"cleanup","selection":body}))
            }
            "receipts"=>{
                let job=body["jobId"].as_str().map(str::to_owned).or_else(||index::setting(&db,"last-receipt").ok().flatten().and_then(|v|v["jobId"].as_str().map(str::to_owned))).ok_or_else(||fail("暂无整理记录"))?;
                cleanup::receipt(&db,&job,body["offset"].as_i64().unwrap_or(0))
            }
            "errors"=>{
                let scan=scan.ok_or_else(||fail("请先扫描"))?;let mut stmt=db.prepare("SELECT path,code,reason FROM errors WHERE scan=? LIMIT 100 OFFSET ?")?;
                let rows=stmt.query_map(params![scan,body["offset"].as_i64().unwrap_or(0).max(0)],|r|Ok(json!({"path":r.get::<_,String>(0)?,"code":r.get::<_,String>(1)?,"reason":r.get::<_,String>(2)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
                Ok(json!({"rows":rows}))
            }
            "associations"=>{
                let scan=scan.ok_or_else(||fail("请先扫描"))?;let mut stmt=db.prepare("SELECT a.path,a.indexed_path,a.title,a.status,a.reason,a.managed,t.name,p.name FROM associations a LEFT JOIN tools t ON t.scan=a.scan AND t.id=a.tool LEFT JOIN projects p ON p.scan=a.scan AND p.id=a.project WHERE a.scan=? ORDER BY a.status,a.path LIMIT 100 OFFSET ?")?;
                let rows=stmt.query_map(params![scan,body["offset"].as_i64().unwrap_or(0).max(0)],|r|Ok(json!({"path":r.get::<_,String>(0)?,"indexedPath":r.get::<_,String>(1)?,"title":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"reason":r.get::<_,String>(4)?,"managed":r.get::<_,bool>(5)?,"tool":r.get::<_,Option<String>>(6)?,"project":r.get::<_,Option<String>>(7)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;Ok(json!({"rows":rows}))
            }
            "ai/config"=>{
                if self.running(){return Err(fail("请先结束当前任务后修改模型连接"));}
                let mut state=self.inner.lock().unwrap();let config=ai::Config::validate(&body,&state.config)?;
                index::save_setting(&db,"ai-config",&config.public(state.revision+1))?;state.config=config;state.revision+=1;state.previews.clear();Ok(state.config.public(state.revision))
            }
            "ai/prepare"=>{
                let scan=scan.ok_or_else(||fail("请先扫描"))?;let resource=self.files.get(body["id"].as_str().ok_or_else(||fail("请点选文件或文件夹"))?,&scan)?;
                let (revision,config)={let state=self.inner.lock().unwrap();(state.revision,state.config.clone())};
                if config.model.is_empty(){return Err(fail("请先连接模型"));}
                let prepared=ai::prepare(resource,&self.home,revision)?;let public=json!({"id":prepared.id,"name":prepared.resource.path.file_name().unwrap_or_default().to_string_lossy(),"path":prepared.resource.path,"coverage":prepared.coverage,"files":prepared.public_files,"destination":config.destination()?,"model":config.model,"revision":revision});
                let mut state=self.inner.lock().unwrap();state.previews.retain(|_,p|chrono::Utc::now().timestamp()-p.created<=600);if state.previews.len()>=20{state.previews.clear();}state.previews.insert(prepared.id.clone(),prepared);Ok(public)
            }
            "ai/summary"=>{
                if body["confirmation"]!="SUMMARIZE_SELECTED_RESOURCE"{return Err(fail("请确认发送点选内容进行总结"));}
                let (prepared,config)={let mut state=self.inner.lock().unwrap();let key=body["previewId"].as_str().ok_or_else(||fail("缺少总结预览"))?;
                    let prepared=state.previews.remove(key).ok_or_else(||fail("预览已使用或过期"))?;
                    if body["revision"].as_u64()!=Some(state.revision)||prepared.revision!=state.revision{return Err(fail("模型连接已变化，请重新预览"));}(prepared,state.config.clone())};
                self.background("summary",json!({"action":"summary","prepared":prepared,"config":private_config(&config)}))
            }
            "ai/payload"=>{
                let scan=scan.ok_or_else(||fail("请先扫描"))?;let tools=discovery::tools(&db,&scan)?;
                let items:Vec<Value>=tools["rows"].as_array().unwrap().iter().take(20).map(|row|json!({"id":row["id"],"name":row["name"],"bytes":row["bytes"],"files":row["files"],"uncertain":row["uncertain"],"evidence":row["evidence"]})).collect();
                let state=self.inner.lock().unwrap();Ok(json!({"scanId":scan,"revision":state.revision,"destination":state.config.destination()?,"model":state.config.model,"payload":{"items":items},"analysisDocumentRead":false}))
            }
            "ai/analyze"=>{
                if body["confirmation"]!="SEND_METADATA_FOR_ADVICE"{return Err(fail("请先审阅将要发送的元数据"));}
                let payload=self.dispatch("ai/payload",json!({"scanId":scan}))?;
                if payload["revision"]!=body["revision"]||payload["scanId"]!=body["scanId"]{return Err(fail("扫描或模型连接已变化，请重新核对"));}
                let config=self.inner.lock().unwrap().config.clone();self.background("analysis",json!({"action":"analysis","payload":payload["payload"],"config":private_config(&config)}))
            }
            "models"=>self.background("models",json!({"action":"models"})),
            "models/delete"=>{if body["confirmation"]!="DELETE_OLLAMA_MODEL"{return Err(fail("请先确认移除具体模型"));}let snapshot=self.inner.lock().unwrap().models.clone();self.background("model-delete",json!({"action":"model-delete","snapshot":snapshot,"modelId":body["id"]}))},
            "mole/preview"=>self.background("mole-preview",json!({"action":"mole-preview","command":body["command"].as_str().unwrap_or("clean")})),
            "export"=>self.export(&db,&body,scan.as_deref()),
            "privacy"=>{#[cfg(target_os="macos")] {Command::new("/usr/bin/open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles").spawn()?;}Ok(json!({"ok":true}))},
            _=>Err(fail("此接口不存在")),
        }
    }
    fn export(&self, db: &rusqlite::Connection, body: &Value, scan: Option<&str>) -> Result<Value> {
        let kind = body["kind"].as_str().unwrap_or("projects");
        let scan = scan.ok_or_else(|| fail("请先扫描"))?;
        let value = match kind {
            "projects" => files::projects(db, scan)?,
            "tools" => discovery::tools(db, scan)?,
            "receipts" => {
                let job = body["jobId"].as_str().ok_or_else(|| fail("缺少记录 ID"))?;
                let mut rows = Vec::new();
                let mut offset = 0;
                loop {
                    let page = cleanup::receipt(db, job, offset)?;
                    rows.extend(page["rows"].as_array().unwrap().iter().cloned());
                    if page["hasMore"] != true {
                        break;
                    }
                    offset += 100;
                }
                json!({"jobId":job,"total":rows.len(),"rows":rows})
            }
            _ => return Err(fail("不支持的导出类型")),
        };
        let downloads = self.home.join("Downloads");
        fs::create_dir_all(&downloads)?;
        if fs::symlink_metadata(&downloads)?.file_type().is_symlink()
            || fs::canonicalize(&downloads)? != downloads
        {
            return Err(fail("下载目录为链接，未导出"));
        }
        let path = downloads.join(format!("Minorminor-{kind}-{}.json", crate::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        serde_json::to_writer_pretty(&mut file, &value)?;
        file.sync_all()?;
        Ok(json!({"path":path,"bytes":file.metadata()?.len()}))
    }
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
    let result = match payload["action"].as_str().unwrap_or("") {
        "scan" => {
            scan::run(scan::ScanSpec {
                id: job.into(),
                root: PathBuf::from(payload["root"].as_str().unwrap_or("")),
                home,
                data: data.clone(),
                disk: payload["disk"].as_bool().unwrap_or(false),
                publish: false,
            })?;
            println!("{}", json!({"event":"progress","stats":{"stage":"models"}}));
            std::io::stdout().flush()?;
            let mut models = ai::ollama_models().unwrap_or_else(
                |_| json!({"createdAt":now(),"models":[],"reason":"未能连接到本机 Ollama"}),
            );
            models["scanId"] = json!(job);
            let db = index::connect(&data.join("index.sqlite3"))?;
            db.execute_batch("BEGIN IMMEDIATE")?;
            index::save_setting(&db, "models-snapshot", &models)?;
            index::save_setting(&db, "dirty", &Value::Null)?;
            db.execute("UPDATE scans SET status='complete' WHERE id=?", [job])?;
            db.execute_batch("COMMIT")?;
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
        "models" => ai::ollama_models()?,
        "model-delete" => {
            let result = ai::delete_model(
                &payload["snapshot"],
                payload["modelId"].as_str().unwrap_or(""),
            )?;
            let mut models = ai::ollama_models()?;
            models["deletion"] = result;
            models
        }
        "mole-preview" => mole::preview(
            &resources,
            &home,
            payload["command"].as_str().unwrap_or("clean"),
        )?,
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
                match cleanup::move_one(&db, job, file, &home, |file| {
                    if ["cache", "log"].contains(&file.kind.as_str()) {
                        mole::guard(&resources, &home, &file.root)?;
                    }
                    mole::unused(&file.path)
                }) {
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
            let result = json!({"jobId":job,"moved":moved,"skipped":skipped,"bytes":bytes,"createdAt":now(),"method":"移到废纸篓"});
            index::save_setting(&db, "last-receipt", &result)?;
            result
        }
        _ => return Err(fail("后台任务类型无效")),
    };
    println!("{}", json!({"event":"complete","result":result}));
    Ok(())
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
        json!({"total":total,"free":free,"used":total-free,"home":home,"platform":std::env::consts::OS,"arch":std::env::consts::ARCH}),
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
            "bootstrap",
            "overview",
            "projects",
            "tools",
            "files",
            "inspect",
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
