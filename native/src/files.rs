use crate::{
    fail, id,
    index::{self, Identity},
    scan, Result,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
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
    let mut value = text.to_owned();
    for pattern in [
        r"(?is)-----BEGIN [^-]*PRIVATE KEY-----.*?-----END [^-]*PRIVATE KEY-----",
        r"(?i)\b(?:sk-[a-z0-9_-]{16,}|gh[pousr]_[a-z0-9]{20,}|github_pat_[a-z0-9_]{20,})",
        r#"(?im)["']?(?:[a-z0-9_]*(?:api[_-]?key|access[_-]?token|secret|password|authorization))["']?\s*[=:]\s*(?:"[^"\n]*"|'[^'\n]*'|[^\s,"'\n}]+)"#,
        r"(?i)\bBearer\s+[a-z0-9._~+/=-]+",
        r"(?i)https?://[^\s/@:]+:[^\s/@]+@",
    ] {
        if let Ok(re) = regex::Regex::new(pattern) {
            value = re.replace_all(&value, "[已隐藏凭据]").into_owned();
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
pub fn projects(db: &Connection, scan: &str) -> Result<Value> {
    let mut stmt=db.prepare("SELECT p.id,p.path,p.name,p.markers,coalesce(sum(e.bytes),0),count(e.path) FROM projects p LEFT JOIN entries e ON e.scan=p.scan AND e.project=p.id AND e.directory=0 WHERE p.scan=? GROUP BY p.id ORDER BY 5 DESC")?;
    let base = stmt
        .query_map([scan], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut rows = Vec::new();
    for (id, path, name, markers, _bytes, count) in base {
        let bytes:i64=db.query_row("SELECT coalesce(sum(bytes),0) FROM (SELECT max(bytes) AS bytes FROM entries WHERE scan=? AND project=? AND directory=0 GROUP BY json_extract(identity,'$.dev'),json_extract(identity,'$.ino'))",params![scan,id],|r|r.get(0))?;
        let mut kinds = HashMap::new();
        let mut query=db.prepare("SELECT category,sum(bytes),sum(files) FROM (SELECT category,max(bytes) AS bytes,count(*) AS files FROM entries WHERE scan=? AND project=? AND directory=0 GROUP BY category,json_extract(identity,'$.dev'),json_extract(identity,'$.ino')) GROUP BY category")?;
        for row in query.query_map(params![scan, id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                json!({"bytes":r.get::<_,i64>(1)?,"files":r.get::<_,i64>(2)?}),
            ))
        })? {
            let (k, v) = row?;
            kinds.insert(k, v);
        }
        let associations: i64 = db.query_row(
            "SELECT count(*) FROM associations WHERE scan=? AND project=? AND kind='session'",
            params![scan, id],
            |r| r.get(0),
        )?;
        rows.push(json!({"id":id,"name":name,"path":path,"exists":Path::new(&path).is_dir(),"bytes":bytes,"files":count,"categories":kinds,"sessions":associations,"markers":serde_json::from_str::<Value>(&markers).unwrap_or(json!([]))}));
    }
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
    if !["tree", "cache", "log", "session", "memory", "other"].contains(&kind) {
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
        rows.push(json!({"id":id,"path":path,"name":name,"directory":directory,"bytes":bytes,"logicalBytes":logical,"files":files,"directories":dirs,"skipped":skipped,"kind":category,"purpose":purpose(&file,&category),"canRead":!directory&&!scan::protected(&file),"canSummarize":file.starts_with(home)}));
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
        json!({"mode":"content","excerpt":excerpt,"bytesRead":bytes.len(),"truncated":(bytes.len() as u64)<identity.size||((name.ends_with(".gz")||name.ends_with(".zst"))&&bytes.len()>=32768),"summary":"本机内容片段，不上传"}),
    )
}
