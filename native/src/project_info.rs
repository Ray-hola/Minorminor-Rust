use crate::{files, index, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
fn excerpt(text: &str) -> String {
    files::redact(text)
        .chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}
fn text(path: &Path, home: &Path, limit: u64) -> Option<String> {
    if crate::scan::protected(path) {
        return None;
    }
    files::read_prefix(path, home, limit, false)
        .ok()
        .and_then(|(bytes, _)| String::from_utf8(bytes).ok())
}
fn document(path: &Path, home: &Path) -> Value {
    let Some(text) = text(path, home, 12288) else {
        return json!({});
    };
    let mut title = String::new();
    let mut description = String::new();
    if text.starts_with("---\n") {
        for line in text.lines().skip(1).take_while(|l| *l != "---") {
            if let Some(v) = line.strip_prefix("name:") {
                title = excerpt(v.trim().trim_matches(['\'', '"']));
            }
            if let Some(v) = line.strip_prefix("description:") {
                description = excerpt(v.trim().trim_matches(['\'', '"']));
            }
        }
    }
    if title.is_empty() {
        title = text
            .lines()
            .find_map(|l| l.strip_prefix("# "))
            .map(excerpt)
            .unwrap_or_default();
    }
    json!({"title":title,"description":description})
}
pub fn enrich(db: &Connection, scan: &str, home: &Path) -> Result<()> {
    enrich_with_progress(db, scan, home, |_| Ok(()))
}
pub fn enrich_with_progress(
    db: &Connection,
    scan: &str,
    home: &Path,
    mut progress: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    progress(home)?;
    let mut p = db.prepare("SELECT id,path FROM projects WHERE scan=?")?;
    let projects = p
        .query_map([scan], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, raw) in &projects {
        let root = Path::new(raw);
        progress(root)?;
        let mut description = String::new();
        let mut name = String::new();
        for file in ["package.json", "pyproject.toml", "Cargo.toml", "README.md"] {
            let path = root.join(file);
            if let Some(source) = text(&path, home, 12288) {
                if file == "package.json" {
                    if let Ok(v) = serde_json::from_str::<Value>(&source) {
                        description = excerpt(v["description"].as_str().unwrap_or(""));
                        name = excerpt(v["name"].as_str().unwrap_or(""));
                    }
                } else if description.is_empty() {
                    if file.ends_with(".toml") {
                        description = source
                            .lines()
                            .find_map(|l| {
                                l.trim()
                                    .strip_prefix("description")
                                    .and_then(|s| s.trim().strip_prefix('='))
                            })
                            .map(|s| excerpt(s.trim().trim_matches('"')))
                            .unwrap_or_default();
                    } else {
                        description = source
                            .lines()
                            .find(|l| {
                                !l.trim().is_empty() && !l.starts_with('#') && !l.starts_with("---")
                            })
                            .map(excerpt)
                            .unwrap_or_default();
                    }
                }
            }
            if !description.is_empty() {
                break;
            }
        }
        index::save_setting(
            db,
            &format!("project-info-{scan}-{id}"),
            &json!({"name":name,"description":description}),
        )?;
    }
    let rows: Vec<(String, String)> = {
        let mut stmt=db.prepare("SELECT path,tool FROM entries WHERE scan=? AND directory=0 AND category='memory' AND tool IS NOT NULL")?;
        let rows = stmt.query_map([scan], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (raw, tool) in rows {
        let file = Path::new(&raw);
        progress(file)?;
        let existing: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM associations WHERE scan=? AND path=? AND kind='memory')",
            params![scan, raw],
            |r| r.get(0),
        )?;
        if !existing {
            let names: Vec<_> = file
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let encoded = names
                .windows(2)
                .find(|w| w[0] == "projects")
                .map(|w| w[1].clone());
            if let Some(encoded) = encoded {
                let matches: Vec<_> = projects
                    .iter()
                    .filter(|(_, path)| {
                        let dashed: path::Encoded = path::Encoded::new(path);
                        dashed.matches(&encoded)
                    })
                    .collect();
                if matches.len() == 1 {
                    db.execute("INSERT INTO associations(scan,id,path,indexed_path,project,tool,kind,title,status,reason,managed) VALUES(?,?,?,?,?,?,'memory','','readable','工具项目目录编码与唯一项目匹配',0)",params![scan,crate::id(),raw,raw,matches[0].0,tool])?;
                }
            }
        }
        let meta = document(file, home);
        let title = meta["title"].as_str().unwrap_or("");
        if !title.is_empty() {
            db.execute(
                "UPDATE associations SET title=? WHERE scan=? AND path=? AND kind='memory'",
                params![title, scan, raw],
            )?;
        }
        index::save_setting(db, &format!("memory-info-{scan}-{raw}"), &meta)?;
    }
    // Explicit workspace registrations outside the tool profile are metadata only.
    let file = home.join(".claude.json");
    if let Some(source) = text(&file, home, 262144) {
        if let Ok(value) = serde_json::from_str::<Value>(&source) {
            if let Some(registered) = value["projects"].as_object() {
                for cwd in registered
                    .keys()
                    .filter(|p| p.starts_with('/') && Path::new(p).starts_with(home))
                {
                    let existing: Option<String> = db
                        .query_row(
                            "SELECT id FROM projects WHERE scan=? AND path=?",
                            params![scan, cwd],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if existing.is_none() {
                        db.execute(
                            "INSERT INTO projects VALUES(?,?,?,?,?)",
                            params![
                                scan,
                                crate::id(),
                                cwd,
                                Path::new(cwd)
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy(),
                                "[]"
                            ],
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}
mod path {
    pub struct Encoded(String);
    impl Encoded {
        pub fn new(s: &str) -> Self {
            Self(
                s.chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect(),
            )
        }
        pub fn matches(&self, s: &str) -> bool {
            self.0 == s || self.0.trim_start_matches('-') == s
        }
    }
}
pub fn details(db: &Connection, scan: &str, id: &str, offset: i64) -> Result<Value> {
    index::assert_scan(db, scan)?;
    let (root, name): (String, String) = db.query_row(
        "SELECT path,name FROM projects WHERE scan=? AND id=?",
        params![scan, id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut stmt=db.prepare("SELECT a.id,a.path,a.kind,a.title,a.status,a.reason,a.managed,t.name,e.bytes FROM associations a LEFT JOIN tools t ON t.scan=a.scan AND t.id=a.tool LEFT JOIN entries e ON e.scan=a.scan AND e.path=a.path WHERE a.scan=? AND a.project=? ORDER BY a.path LIMIT 100 OFFSET ?")?;
    let rows=stmt.query_map(params![scan,id,offset.max(0)],|r|{let managed:bool=r.get(6)?;let path:String=r.get(1)?;Ok(json!({"id":r.get::<_,String>(0)?,"path":path,"kind":r.get::<_,String>(2)?,"title":r.get::<_,String>(3)?,"status":r.get::<_,String>(4)?,"reason":r.get::<_,String>(5)?,"managed":managed,"tool":r.get::<_,Option<String>>(7)?,"bytes":if managed{None}else{r.get::<_,Option<i64>>(8)?},"details":index::setting(db,&format!("memory-info-{scan}-{path}")).unwrap_or(None)}))})?.collect::<std::result::Result<Vec<_>,_>>()?;
    let total: i64 = db.query_row(
        "SELECT count(*) FROM associations WHERE scan=? AND project=?",
        params![scan, id],
        |r| r.get(0),
    )?;
    Ok(
        json!({"id":id,"name":name,"path":root,"description":index::setting(db,&format!("project-info-{scan}-{id}"))?,"rows":rows,"total":total,"offset":offset,"hasMore":offset+100<total}),
    )
}
pub fn log_metadata(path: &Path, home: &Path) -> Value {
    let Some(source) = text(path, home, 32768) else {
        return json!({"readable":false});
    };
    let mut levels = BTreeMap::new();
    let mut codes = BTreeSet::new();
    let mut projects = BTreeSet::new();
    for line in source.lines() {
        for level in ["ERROR", "WARN", "INFO", "DEBUG", "TRACE", "FATAL"] {
            if line.contains(level) {
                *levels.entry(level).or_insert(0) += 1;
            }
        }
        for code in [
            "ENOENT",
            "EACCES",
            "EPERM",
            "EIO",
            "ETIMEDOUT",
            "ECONNREFUSED",
            "ENOSPC",
        ] {
            if line.contains(code) {
                codes.insert(code);
            }
        }
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            for key in ["cwd", "projectRoot", "project_path", "working_directory"] {
                if let Some(p) = value[key].as_str().filter(|p| p.starts_with('/')) {
                    projects.insert(p.to_owned());
                }
            }
        }
    }
    json!({"levels":levels,"codes":codes,"projectReferences":projects,"readable":true})
}
