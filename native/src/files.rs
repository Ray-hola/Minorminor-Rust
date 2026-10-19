use crate::{
    fail, id,
    index::{self, Identity},
    scan, Result,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resource {
    pub path: PathBuf,
    pub root: PathBuf,
    pub scan: String,
    pub identity: Identity,
    pub directory: bool,
}
#[derive(Default)]
pub struct Resources {
    pub issued: Mutex<HashMap<String, Resource>>,
}
impl Resources {
    pub fn issue(&self, resource: Resource) -> String {
        let key = id();
        let mut map = self.issued.lock().unwrap();
        if map.len() >= 10000 {
            map.clear();
        }
        map.insert(key.clone(), resource);
        key
    }
    pub fn get(&self, id: &str, scan: &str) -> Result<Resource> {
        let resource = self
            .issued
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| fail("文件条目已过期，请重新展开"))?;
        if resource.scan != scan {
            return Err(fail("文件条目属于旧快照"));
        }
        let m = index::checked(&resource.path, &resource.root, resource.identity.dev)?;
        if Identity::of(&m) != resource.identity {
            return Err(fail("文件已变化，请刷新后重新选择"));
        }
        Ok(resource)
    }
}
pub fn read_prefix(
    path: &Path,
    root: &Path,
    limit: u64,
    compressed: bool,
) -> Result<(Vec<u8>, Identity)> {
    if !path.starts_with(root) {
        return Err(fail("文件不在已授权范围"));
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || fs::canonicalize(path)? != path {
        return Err(fail("不读取链接或特殊文件"));
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let identity = Identity::of(&metadata);
    if identity != Identity::of(&file.metadata()?) {
        return Err(fail("读取前文件身份发生变化"));
    }
    let mut bytes = Vec::new();
    let name = path.to_string_lossy();
    if compressed && name.ends_with(".gz") {
        flate2::read::GzDecoder::new((&file).take(8 * 1024 * 1024))
            .take(limit)
            .read_to_end(&mut bytes)?;
    } else if compressed && name.ends_with(".zst") {
        let mut reader = zstd::stream::read::Decoder::new((&file).take(8 * 1024 * 1024))?;
        reader.window_log_max(23)?;
        reader.take(limit).read_to_end(&mut bytes)?;
    } else {
        (&file).take(limit).read_to_end(&mut bytes)?;
    }
    if identity != Identity::of(&file.metadata()?)
        || identity != Identity::of(&fs::symlink_metadata(path)?)
    {
        return Err(fail("读取期间文件发生变化"));
    }
    Ok((bytes, identity))
}
pub fn redact(text: &str) -> String {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| [
        r"(?is)-----BEGIN [^-]*PRIVATE KEY-----.*?-----END [^-]*PRIVATE KEY-----",
        r"(?i)\b(?:sk-[a-z0-9_-]{16,}|gh[pousr]_[a-z0-9]{20,}|github_pat_[a-z0-9_]{20,})",
        r#"(?im)["']?(?:[a-z0-9_]*(?:api[_-]?key|access[_-]?token|secret|password|authorization))["']?\s*[=:]\s*(?:"[^"\n]*"|'[^'\n]*'|[^\s,"'\n}]+)"#,
        r"(?i)\bBearer\s+[a-z0-9._~+/=-]+",
        r"(?i)https?://[^\s/@:]+:[^\s/@]+@",
    ].into_iter().map(|pattern| regex::Regex::new(pattern).expect("static credential pattern")).collect());
    let mut value = text.to_owned();
    for re in patterns {
        if let std::borrow::Cow::Owned(redacted) = re.replace_all(&value, "[已隐藏凭据]") {
            value = redacted;
        }
    }
    value
}
pub fn purpose(path: &Path, kind: &str) -> String {
    if scan::protected(path) {
        return "依赖、配置、凭据、数据库、模型或运行资源；保留".into();
    }
    match kind {
        "cache" => "构建或应用生成的缓存",
        "log" => "程序运行日志",
        "session" => "AI 会话或执行记录",
        "memory" => "项目约定或长期记忆",
        _ => match path.extension().and_then(|s| s.to_str()).unwrap_or("") {
            "rs" => "Rust 源代码",
            "js" | "mjs" | "ts" | "py" | "swift" | "go" | "sh" => "项目源代码或脚本",
            "md" | "txt" => "说明文档",
            "json" | "toml" | "yaml" | "yml" => "结构化配置或数据",
            _ => "项目文件；尚未读取正文",
        },
    }
    .into()
}
#[derive(Deserialize)]
struct StorageIdentity {
    dev: Option<u64>,
    ino: Option<u64>,
}

fn project_storage(db: &Connection, scan: &str, project: &str) -> Result<(i64, i64, Value)> {
    let complete = db
        .query_row(
            "SELECT status='complete' FROM scans WHERE id=?",
            [scan],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false);
    let cache_key = format!("project-storage-cache-{scan}-{project}");
    let revision = index::setting(db, "storage-summary-revision")?
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if complete {
        if let Some(cached) = index::setting(db, &cache_key)? {
            if let (Some(bytes), Some(files)) = (cached["bytes"].as_i64(), cached["files"].as_i64())
            {
                if cached["categories"].is_object() && cached["revision"].as_u64() == Some(revision)
                {
                    return Ok((bytes, files, cached["categories"].clone()));
                }
            }
        }
    }
    // Stream each project once without disk-backed identity sorts. Preserve
    // deduplication even if a single-link file moved during traversal.
    let sql = if crate::compact::is_legacy(db)? {
        "SELECT category,bytes,identity FROM entries WHERE scan=? AND project=? AND directory=0"
    } else if crate::overlay::active(db)? {
        "SELECT category,bytes,CASE WHEN json_type(identity_bits)='array' THEN json_object('dev',json(identity_bits -> '$[0]'),'ino',json(identity_bits -> '$[1]')) ELSE identity_bits END FROM main._entries e WHERE scan=(SELECT id FROM entry_scans WHERE value=?1) AND project=?2 AND directory=0 AND NOT EXISTS(SELECT 1 FROM entry_changes d WHERE d.scan=?1 AND d.path=e.path) UNION ALL SELECT category,bytes,identity FROM entry_changes WHERE scan=?1 AND project=?2 AND directory=0 AND deleted=0"
    } else {
        "SELECT category,bytes,CASE WHEN json_type(identity_bits)='array' THEN json_object('dev',json(identity_bits -> '$[0]'),'ino',json(identity_bits -> '$[1]')) ELSE identity_bits END FROM _entries WHERE scan=(SELECT id FROM entry_scans WHERE value=?) AND project=? AND directory=0"
    };
    let mut query = db.prepare_cached(sql)?;
    let mut entries = query.query(params![scan, project])?;
    let mut bytes = 0_i64;
    let mut files = 0_i64;
    let mut categories: HashMap<String, (i64, i64)> = HashMap::new();
    let mut links = HashMap::new();
    let mut category_links = HashMap::new();
    while let Some(row) = entries.next()? {
        let category: String = row.get(0)?;
        let size: i64 = row.get(1)?;
        let identity: StorageIdentity = serde_json::from_str(row.get_ref(2)?.as_str()?)?;
        files += 1;
        let totals = categories.entry(category.clone()).or_default();
        totals.1 += 1;
        let key = (identity.dev, identity.ino);
        let previous = links.entry(key).or_insert(0_i64);
        bytes += (size - *previous).max(0);
        *previous = (*previous).max(size);
        let previous = category_links
            .entry(category)
            .or_insert_with(HashMap::new)
            .entry(key)
            .or_insert(0_i64);
        totals.0 += (size - *previous).max(0);
        *previous = (*previous).max(size);
    }
    let categories: serde_json::Map<String, Value> = categories
        .into_iter()
        .map(|(kind, (bytes, files))| (kind, json!({"bytes":bytes,"files":files})))
        .collect();
    let categories = json!(categories);
    if complete {
        index::save_setting(
            db,
            &cache_key,
            &json!({"bytes":bytes,"files":files,"categories":categories,"revision":revision}),
        )?;
    }
    Ok((bytes, files, categories))
}

pub fn projects(db: &Connection, scan: &str) -> Result<Value> {
    let mut stmt = db.prepare("SELECT id,path,name,markers FROM projects WHERE scan=?")?;
    let base = stmt
        .query_map([scan], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut rows = Vec::new();
    for (id, path, name, markers) in base {
        let (bytes, count, kinds) = project_storage(db, scan, &id)?;
        let associations: i64 = db.query_row(
            "SELECT count(*) FROM associations WHERE scan=? AND project=? AND kind='session'",
            params![scan, id],
            |r| r.get(0),
        )?;
        rows.push(json!({"id":id,"name":name,"path":path,"exists":Path::new(&path).is_dir(),"bytes":bytes,"files":count,"categories":kinds,"sessions":associations,"markers":serde_json::from_str::<Value>(&markers).unwrap_or(json!([])),"description":index::setting(db,&format!("project-info-{scan}-{id}"))?.unwrap_or(json!({}))["description"]}));
    }
    rows.sort_by(|a, b| {
        b["bytes"]
            .as_i64()
            .cmp(&a["bytes"].as_i64())
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    Ok(json!({"rows":rows}))
}

pub fn list(
    db: &Connection,
    resources: &Resources,
    home: &Path,
    scan_id: &str,
    request: &Value,
) -> Result<Value> {
    index::assert_scan(db, scan_id)?;
    let kind = request["kind"].as_str().unwrap_or("tree");
    let offset = request["offset"].as_u64().unwrap_or(0).min(i64::MAX as u64) as i64;
    let project = request["project"].as_str();
    let tool = request["tool"].as_str();
    let root: PathBuf = if let Some(project) = project {
        PathBuf::from(db.query_row(
            "SELECT path FROM projects WHERE scan=? AND id=?",
            params![scan_id, project],
            |r| r.get::<_, String>(0),
        )?)
    } else if let Some(tool) = tool {
        PathBuf::from(db.query_row(
            "SELECT path FROM tools WHERE scan=? AND id=?",
            params![scan_id, tool],
            |r| r.get::<_, String>(0),
        )?)
    } else {
        let info = index::scan_info(db, scan_id)?;
        let p = info["root"].as_str().unwrap_or("");
        if p == "/System/Volumes/Data" {
            PathBuf::from("/")
        } else {
            PathBuf::from(p)
        }
    };
    let directory = if let Some(key) = request["directory"].as_str() {
        let r = resources.get(key, scan_id)?;
        if !r.directory || !r.path.starts_with(&root) {
            return Err(fail("无效的文件夹条目"));
        }
        r.path
    } else {
        root.clone()
    };
    if let Some(parent_project) = project {
        let nested:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE scan=?1 AND id<>?2 AND path<>?3 AND (path=?4 OR substr(?4,1,length(path)+1)=path||'/') AND substr(path,1,length(?3)+1)=?3||'/')",params![scan_id,parent_project,index::path_string(&root),index::path_string(&directory)],|r|r.get(0))?;
        if nested {
            return Err(fail("此目录属于独立子项目，请从对应项目展开"));
        }
    }
    let query = request["query"].as_str().unwrap_or("").to_lowercase();
    let scope = if kind == "tree" {
        "parent=?3"
    } else if project.is_some() {
        "project=?3 AND category=?4 AND directory=0"
    } else if tool.is_some() {
        "tool=?3 AND category=?4 AND directory=0"
    } else {
        "category=?4 AND directory=0 AND ?3 IS NOT NULL"
    };
    if ![
        "tree",
        "cache",
        "log",
        "session",
        "memory",
        "other",
        "installer",
    ]
    .contains(&kind)
    {
        return Err(fail("无效的文件分类"));
    }
    let order = match request["sort"].as_str().unwrap_or("size") {
        "name" => "name COLLATE NOCASE",
        "time" => "json_extract(identity,'$.mtime') DESC",
        _ => "bytes DESC,name",
    };
    let mut stmt=db.prepare(&format!("SELECT path,name,directory,bytes,logical,files,dirs,identity,category FROM entries WHERE scan=?1 AND path<>?2 AND {scope} AND (?5='' OR instr(lower(name),?5)>0) ORDER BY directory DESC,{order} LIMIT 100 OFFSET ?6"))?;
    let filter = if kind == "tree" {
        index::path_string(&directory)
    } else {
        project.or(tool).unwrap_or("all").to_owned()
    };
    // Numbered parameters keep both tree and category queries identical.
    let values = params![
        scan_id,
        index::path_string(&directory),
        filter,
        kind,
        query,
        offset
    ];
    let raw = stmt
        .query_map(values, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut rows = Vec::new();
    for (path, name, directory, bytes, logical, files, dirs, identity, category) in raw {
        let identity: Identity = serde_json::from_str(&identity)?;
        let file = PathBuf::from(&path);
        let resource_root = if !directory
            && project.is_some()
            && file.starts_with(home)
            && !file.starts_with(&root)
        {
            file.clone()
        } else {
            root.clone()
        };
        let id = resources.issue(Resource {
            path: file.clone(),
            root: resource_root,
            scan: scan_id.into(),
            identity,
            directory,
        });
        let skipped: i64 = db.query_row(
            "SELECT skipped FROM entries WHERE scan=? AND path=?",
            params![scan_id, path],
            |r| r.get(0),
        )?;
        let child_project: Option<String> = if directory {
            db.query_row(
                "SELECT id FROM projects WHERE scan=? AND path=?",
                params![scan_id, path],
                |r| r.get(0),
            )
            .ok()
            .filter(|p: &String| Some(p.as_str()) != project)
        } else {
            None
        };
        rows.push(json!({"projectId":child_project,"id":id,"path":path,"name":name,"directory":directory,"bytes":bytes,"logicalBytes":logical,"files":files,"directories":dirs,"skipped":skipped,"kind":category,"purpose":purpose(&file,&category),"canRead":!directory&&!scan::protected(&file),"canSummarize":file.starts_with(home)}));
    }
    let total:i64=db.query_row(&format!("SELECT count(*) FROM entries WHERE scan=?1 AND path<>?2 AND {scope} AND (?5='' OR instr(lower(name),?5)>0)"),params![scan_id,index::path_string(&directory),filter,kind,query],|r|r.get(0))?;
    let root_resource = fs::symlink_metadata(&directory).ok().map(|m| {
        resources.issue(Resource {
            path: directory.clone(),
            root: root.clone(),
            scan: scan_id.into(),
            identity: Identity::of(&m),
            directory: true,
        })
    });
    Ok(
        json!({"rows":rows,"total":total,"offset":offset,"hasMore":offset+100<total,"path":directory,"rootId":root_resource,"atRoot":directory==root,"scope":request}),
    )
}
pub fn inspect(resource: &Resource) -> Result<Value> {
    if resource.directory {
        return Ok(json!({"mode":"metadata","summary":"文件夹占用来自完整扫描"}));
    }
    if scan::protected(&resource.path) {
        return Ok(json!({"mode":"metadata","summary":"受保护文件，只显示元数据"}));
    }
    let (bytes, identity) = read_prefix(&resource.path, &resource.root, 32768, true)?;
    if identity != resource.identity {
        return Err(fail("文件身份已变化"));
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(json!({"mode":"metadata","summary":"二进制文件，只显示元数据"}));
    };
    if bytes.contains(&0) {
        return Ok(json!({"mode":"metadata","summary":"二进制文件，只显示元数据"}));
    }
    let name = resource.path.to_string_lossy();
    let excerpt = if name.ends_with(".jsonl")
        || name.ends_with(".jsonl.gz")
        || name.ends_with(".jsonl.zst")
    {
        text.lines().filter_map(|line|serde_json::from_str::<Value>(line).ok()).take(12).map(|value|json!({"type":value["type"],"cwd":value["cwd"],"title":value["title"],"timestamp":value["timestamp"]}).to_string()).collect::<Vec<_>>().join("\n")
    } else {
        redact(text).chars().take(4000).collect()
    };
    Ok(
        json!({"mode":"content","excerpt":excerpt,"logMetadata":if resource.path.to_string_lossy().contains("/logs/")||resource.path.extension().is_some_and(|e|e=="log"){crate::project_info::log_metadata(&resource.path,&resource.root)}else{Value::Null},"bytesRead":bytes.len(),"truncated":(bytes.len() as u64)<identity.size||((name.ends_with(".gz")||name.ends_with(".zst"))&&bytes.len()>=32768),"summary":"本机内容片段，不上传"}),
    )
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    #[test]
    fn completed_project_cache_is_scoped_and_running_scans_remain_uncached() {
        let temp = tempfile::tempdir().unwrap();
        let db = index::connect(&index::initialize(temp.path()).unwrap()).unwrap();
        for scan in ["current", "other"] {
            db.execute(
                "INSERT INTO scans(id,root,created,status) VALUES(?,'/','today','complete')",
                [scan],
            )
            .unwrap();
        }
        for (scan, path, ino, bytes) in [
            ("current", "/one", u64::MAX, 10),
            ("current", "/two", u64::MAX - 1, 20),
            ("other", "/one", u64::MAX, 64),
        ] {
            let identity = json!({"dev":1,"ino":ino,"size":bytes,"mtime":-1,"mtime_ns":0,"ctime":-2,"ctime_ns":0,"nlink":1});
            db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category,project) VALUES(?,?,'/','file',0,?,0,0,?,'other','project')",params![scan,path,bytes,identity.to_string()]).unwrap();
        }
        let first = project_storage(&db, "current", "project").unwrap();
        assert_eq!((first.0, first.1), (30, 2));
        assert_eq!(project_storage(&db, "current", "project").unwrap(), first);
        assert_eq!(project_storage(&db, "other", "project").unwrap().0, 64);
        assert_eq!(
            index::setting(&db, "project-storage-cache-current-project")
                .unwrap()
                .unwrap()["bytes"],
            30
        );
        db.execute("UPDATE scans SET status='running' WHERE id='current'", [])
            .unwrap();
        db.execute("UPDATE entries SET bytes=99 WHERE scan='current'", [])
            .unwrap();
        assert_eq!(project_storage(&db, "current", "project").unwrap().0, 198);
        assert_eq!(
            index::setting(&db, "project-storage-cache-current-project")
                .unwrap()
                .unwrap()["bytes"],
            30
        );
    }

    #[test]
    fn streaming_project_storage_matches_full_sql_reference() {
        let temp = tempfile::tempdir().unwrap();
        let file = index::initialize(temp.path()).unwrap();
        let db = index::connect(&file).unwrap();
        let cases = [
            ("scan", "project", "cache", 10, 1, 1, false),
            ("scan", "project", "cache", 10, 1, 1, false),
            ("scan", "project", "log", 20, 2, 3, false),
            ("scan", "project", "log", 30, 2, 3, false),
            ("scan", "project", "memory", 30, 2, 3, false),
            ("scan", "other", "log", 40, 2, 3, false),
            ("other", "project", "log", 100, 2, 3, false),
            ("scan", "project", "other", 500, 3, 1, true),
        ];
        for (i, (scan, project, category, bytes, ino, nlink, directory)) in
            cases.into_iter().enumerate()
        {
            // A moved single-link file can leave two indexed paths too.
            db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category,project) VALUES(?,?,'/','file',?,?,0,0,?,?,?)",
                params![scan,format!("/{i}"),directory,bytes,json!({"dev":1,"ino":ino,"nlink":nlink}).to_string(),category,project]).unwrap();
        }
        for project in ["project", "other", "empty"] {
            let actual = project_storage(&db, "scan", project).unwrap();
            let expected: i64 = db.query_row("SELECT coalesce(sum(bytes),0) FROM (SELECT max(bytes) bytes FROM entries WHERE scan=? AND project=? AND directory=0 GROUP BY json_extract(identity,'$.dev'),json_extract(identity,'$.ino'))",params!["scan",project],|r|r.get(0)).unwrap();
            let files: i64 = db
                .query_row(
                    "SELECT count(*) FROM entries WHERE scan=? AND project=? AND directory=0",
                    params!["scan", project],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!((actual.0, actual.1), (expected, files));
            let mut reference = serde_json::Map::new();
            let mut stmt = db.prepare("SELECT category,sum(bytes),sum(files) FROM (SELECT category,max(bytes) bytes,count(*) files FROM entries WHERE scan=? AND project=? AND directory=0 GROUP BY category,json_extract(identity,'$.dev'),json_extract(identity,'$.ino')) GROUP BY category").unwrap();
            for row in stmt
                .query_map(params!["scan", project], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .unwrap()
            {
                let (kind, bytes, files) = row.unwrap();
                reference.insert(kind, json!({"bytes":bytes,"files":files}));
            }
            assert_eq!(actual.2, json!(reference));
        }
    }
}
