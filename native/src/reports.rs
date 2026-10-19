use crate::{discovery, fail, files, index, now, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, path::Path};
pub fn csv_cell(value: &Value) -> String {
    let text = if let Some(s) = value.as_str() {
        s.to_owned()
    } else if value.is_null() {
        String::new()
    } else {
        value.to_string()
    };
    let mut safe = text.clone();
    if text.trim_start().starts_with(['=', '+', '-', '@']) || text.starts_with(['\t', '\r']) {
        safe.insert(0, '\'');
    }
    format!("\"{}\"", safe.replace('"', "\"\""))
}
pub fn csv(rows: &[Value]) -> String {
    let mut keys = Vec::new();
    for row in rows {
        if let Some(o) = row.as_object() {
            for key in o.keys() {
                if !keys.contains(key) {
                    keys.push(key.clone());
                }
            }
        }
    }
    let mut text = String::from("\u{feff}");
    text.push_str(
        &keys
            .iter()
            .map(|k| csv_cell(&json!(k)))
            .collect::<Vec<_>>()
            .join(","),
    );
    text.push_str("\r\n");
    for row in rows {
        text.push_str(
            &keys
                .iter()
                .map(|k| csv_cell(&row[k]))
                .collect::<Vec<_>>()
                .join(","),
        );
        text.push_str("\r\n");
    }
    text
}
pub fn all_receipts(db: &Connection, job: &str) -> Result<Value> {
    let mut rows = Vec::new();
    let mut offset = 0;
    loop {
        let page = crate::cleanup::receipt(db, job, offset)?;
        rows.extend(page["rows"].as_array().unwrap().iter().cloned());
        if page["hasMore"] != true {
            break;
        }
        offset += 100;
    }
    Ok(json!({"jobId":job,"total":rows.len(),"rows":rows}))
}
pub fn dataset(db: &Connection, kind: &str, scan: Option<&str>, body: &Value) -> Result<Value> {
    Ok(match kind {
        "projects" => files::projects(db, scan.ok_or_else(|| fail("请先扫描"))?)?,
        "tools" => discovery::tools(db, scan.ok_or_else(|| fail("请先扫描"))?)?,
        "associations" => {
            let mut stmt=db.prepare("SELECT path,indexed_path,project,tool,kind,title,status,reason,managed FROM associations WHERE scan=? ORDER BY path")?;
            let rows=stmt.query_map([scan.ok_or_else(||fail("请先扫描"))?],|r|Ok(json!({"path":r.get::<_,String>(0)?,"indexedPath":r.get::<_,String>(1)?,"project":r.get::<_,Option<String>>(2)?,"tool":r.get::<_,String>(3)?,"kind":r.get::<_,String>(4)?,"title":r.get::<_,String>(5)?,"status":r.get::<_,String>(6)?,"reason":r.get::<_,String>(7)?,"managed":r.get::<_,bool>(8)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
            json!({"rows":rows})
        }
        "receipts" => all_receipts(
            db,
            body["jobId"].as_str().ok_or_else(|| fail("缺少记录ID"))?,
        )?,
        "advice" => index::setting(db, "ai-advice")?.ok_or_else(|| fail("尚无判读建议"))?,
        "plan" => {
            let id = body["planId"].as_str().ok_or_else(|| fail("缺少清单ID"))?;
            let mut rows = Vec::new();
            let mut offset = 0;
            loop {
                let page = crate::cleanup::plan_files(db, id, offset)?;
                rows.extend(page["rows"].as_array().unwrap().iter().cloned());
                if page["hasMore"] != true {
                    break;
                }
                offset += 100;
            }
            json!({"id":id,"rows":rows})
        }
        "errors" => {
            let mut stmt = db.prepare("SELECT path,code,reason FROM errors WHERE scan=?")?;
            let rows=stmt.query_map([scan.ok_or_else(||fail("请先扫描"))?],|r|Ok(json!({"path":r.get::<_,String>(0)?,"code":r.get::<_,String>(1)?,"reason":r.get::<_,String>(2)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
            json!({"rows":rows})
        }
        "mole-plan" => {
            let p = body["planId"].as_str().ok_or_else(|| fail("缺少清单ID"))?;
            let groups: String =
                db.query_row("SELECT groups_json FROM reviews WHERE id=?", [p], |r| {
                    r.get(0)
                })?;
            json!({"rows":serde_json::from_str::<Value>(&groups)?})
        }
        "legacy" => index::setting(db, "legacy-import")?.unwrap_or(json!({"rows":[]})),
        _ => return Err(fail("不支持的报告类型")),
    })
}
pub fn export(db: &Connection, home: &Path, scan: Option<&str>, body: &Value) -> Result<Value> {
    let kind = body["kind"].as_str().unwrap_or("projects");
    let value = dataset(db, kind, scan, body)?;
    let format = body["format"].as_str().unwrap_or("json");
    if !["json", "csv"].contains(&format) {
        return Err(fail("格式必须是 JSON 或 CSV"));
    }
    let directory = home.join("Downloads");
    fs::create_dir_all(&directory)?;
    if fs::canonicalize(&directory)? != directory
        || fs::symlink_metadata(&directory)?.file_type().is_symlink()
    {
        return Err(fail("下载目录是链接，未导出"));
    }
    let path = directory.join(format!("Minorminor-{kind}-{}.{}", crate::id(), format));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    if format == "csv" {
        let rows = value["rows"]
            .as_array()
            .or_else(|| value["items"].as_array())
            .cloned()
            .unwrap_or_else(|| vec![value.clone()]);
        file.write_all(csv(&rows).as_bytes())?;
    } else {
        serde_json::to_writer_pretty(&mut file, &value)?;
    }
    file.sync_all()?;
    Ok(json!({"path":path,"bytes":file.metadata()?.len()}))
}
pub fn history(db: &Connection, offset: i64) -> Result<Value> {
    let mut stmt=db.prepare("SELECT job,min(created),count(*),sum(CASE WHEN status='moved' OR status='deleted' THEN bytes ELSE 0 END) FROM receipts GROUP BY job ORDER BY min(rowid) DESC LIMIT 100 OFFSET ?")?;
    let rows=stmt.query_map([offset.max(0)],|r|Ok(json!({"jobId":r.get::<_,String>(0)?,"createdAt":r.get::<_,String>(1)?,"records":r.get::<_,i64>(2)?,"bytes":r.get::<_,i64>(3)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let total: i64 = db.query_row("SELECT count(DISTINCT job) FROM receipts", [], |r| r.get(0))?;
    Ok(json!({"rows":rows,"total":total,"offset":offset.max(0),"hasMore":offset.max(0)+100<total}))
}
// Read only the selected Web export. Old previews are history, never revived
// authorization; imported snapshots are explicitly marked stale.
pub fn import_legacy(db: &Connection, home: &Path, path: &Path) -> Result<Value> {
    let m = index::checked(
        path,
        home,
        std::os::unix::fs::MetadataExt::dev(&fs::metadata(home)?),
    )?;
    if !m.is_file() {
        return Err(fail("请选择 Web JSON 导出或 latest-state.json"));
    }
    if m.len() > 128 * 1024 * 1024 {
        return Err(fail("导出大于128MB，请分开导出"));
    }
    let (bytes, _) = files::read_prefix(path, home, 128 * 1024 * 1024 + 1, false)?;
    let source: Value = serde_json::from_slice(&bytes)?;
    if source.get("home").is_some_and(|v| v != &json!(home)) {
        return Err(fail("导出属于其他用户目录，未导入"));
    }
    let allowed = [
        "scan",
        "analysis",
        "cleanup",
        "aiScan",
        "aiAdvice",
        "modelCleanup",
        "projectAnalysis",
        "projectStorage",
        "projectCleanup",
        "synchronization",
        "harnessKnowledge",
        "rows",
        "items",
        "groups",
    ];
    let mut payload = serde_json::Map::new();
    for key in allowed {
        if let Some(value) = source.get(key) {
            payload.insert(key.into(), value.clone());
        }
    }
    if payload.is_empty() {
        return Err(fail("不是支持的 Web 导出"));
    }
    let result = json!({"id":crate::id(),"createdAt":now(),"source":path,"stale":true,"historyOnly":true,"data":payload});
    index::save_setting(db, "legacy-import", &result)?;
    Ok(
        json!({"id":result["id"],"createdAt":result["createdAt"],"stale":true,"historyOnly":true,"sections":result["data"].as_object().unwrap().keys().collect::<Vec<_>>()}),
    )
}

pub fn legacy_page(db: &Connection, body: &Value) -> Result<Value> {
    let Some(saved) = index::setting(db, "legacy-import")? else {
        return Ok(json!({"rows":[],"sections":[],"total":0}));
    };
    let sections: Vec<_> = saved["data"]
        .as_object()
        .into_iter()
        .flat_map(|o| o.keys())
        .cloned()
        .collect();
    let section = body["section"]
        .as_str()
        .filter(|s| sections.iter().any(|k| k == s))
        .map(str::to_owned)
        .or_else(|| sections.first().cloned());
    let value = section
        .as_ref()
        .map(|s| &saved["data"][s])
        .unwrap_or(&Value::Null);
    let entries: Vec<Value> = if let Some(rows) = value.as_array() {
        rows.clone()
    } else if let Some(obj) = value.as_object() {
        obj.iter()
            .map(|(k, v)| json!({"key":k,"value":v}))
            .collect()
    } else {
        vec![value.clone()]
    };
    let offset = body["offset"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
    // Bound each displayed item too: arrays nested under a section remain in the
    // full export instead of making one HTTP response unbounded.
    let rows: Vec<_> = entries
        .iter()
        .skip(offset)
        .take(100)
        .map(|v| {
            let text = v.to_string();
            if text.chars().count() > 8000 {
                json!({"preview":text.chars().take(8000).collect::<String>(),"truncated":true})
            } else {
                v.clone()
            }
        })
        .collect();
    Ok(
        json!({"rows":rows,"sections":sections,"section":section,"total":entries.len(),"offset":offset,"hasMore":offset.saturating_add(100)<entries.len(),"createdAt":saved["createdAt"],"historyOnly":true,"stale":true}),
    )
}
pub fn diagnostics(db: &Connection, scan: &str, kind: &str, body: &Value) -> Result<Value> {
    index::assert_scan(db, scan)?;
    if kind == "errors" {
        let offset = body["offset"].as_u64().unwrap_or(0) as usize;
        let query = body["query"].as_str().unwrap_or("").to_lowercase();
        let status = body["status"].as_str().unwrap_or("");
        let audit = crate::read_recovery::audit(db, scan)?;
        let mut stmt = db.prepare("SELECT path,code,reason FROM errors WHERE scan=?")?;
        let mut rows=stmt.query_map([scan],|r|Ok(json!({"path":r.get::<_,String>(0)?,"code":r.get::<_,String>(1)?,"reason":r.get::<_,String>(2)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        for row in &mut rows {
            row["category"] = json!(crate::read_recovery::category(
                row["code"].as_str().unwrap_or(""),
                std::path::Path::new(row["path"].as_str().unwrap_or(""))
            ));
            if let Some(record) = audit.iter().rev().find(|a| {
                a["path"] == row["path"]
                    && a["code"] == row["code"]
                    && a["status"] != "recovered"
                    && a["status"] != "resolved"
            }) {
                *row = record.clone();
            }
        }
        rows.extend(
            audit
                .into_iter()
                .filter(|r| r["status"] == "recovered" || r["status"] == "resolved"),
        );
        let mut summary = serde_json::Map::new();
        for row in &rows {
            let key = if row["status"] == "resolved" || row["status"] == "recovered" {
                row["status"].as_str().unwrap()
            } else {
                row["category"].as_str().unwrap_or("read-error")
            };
            let n = summary.get(key).and_then(Value::as_u64).unwrap_or(0);
            summary.insert(key.into(), json!(n + 1));
        }
        rows.retain(|r| {
            (status.is_empty()
                || r["code"] == status
                || r["category"] == status
                || r["status"] == status)
                && (query.is_empty()
                    || format!("{} {}", r["path"], r["reason"])
                        .to_lowercase()
                        .contains(&query))
        });
        rows.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        let total = rows.len();
        let page: Vec<_> = rows.into_iter().skip(offset).take(100).collect();
        return Ok(
            json!({"rows":page,"summary":summary,"total":total,"offset":offset,"hasMore":offset.saturating_add(100)<total,"query":query,"status":status}),
        );
    }
    let offset = body["offset"].as_i64().unwrap_or(0).max(0);
    let query = body["query"].as_str().unwrap_or("").to_lowercase();
    let status = body["status"].as_str().unwrap_or("");
    let (sql, count_sql) = if kind == "errors" {
        (
        "SELECT path,code,reason FROM errors WHERE scan=?1 AND (?2='' OR code=?2) AND (?3='' OR instr(lower(path||' '||reason),?3)>0) ORDER BY path LIMIT 100 OFFSET ?4",
        "SELECT count(*) FROM errors WHERE scan=?1 AND (?2='' OR code=?2) AND (?3='' OR instr(lower(path||' '||reason),?3)>0)"
    )
    } else {
        (
        "SELECT a.path,a.indexed_path,a.title,a.status,a.reason,a.managed,t.name,p.name FROM associations a LEFT JOIN tools t ON t.scan=a.scan AND t.id=a.tool LEFT JOIN projects p ON p.scan=a.scan AND p.id=a.project WHERE a.scan=?1 AND (?2='' OR a.status=?2) AND (?3='' OR instr(lower(a.path||' '||a.reason||' '||coalesce(p.name,'')||' '||coalesce(t.name,'')),?3)>0) ORDER BY a.status,a.path LIMIT 100 OFFSET ?4",
        "SELECT count(*) FROM associations a LEFT JOIN tools t ON t.scan=a.scan AND t.id=a.tool LEFT JOIN projects p ON p.scan=a.scan AND p.id=a.project WHERE a.scan=?1 AND (?2='' OR a.status=?2) AND (?3='' OR instr(lower(a.path||' '||a.reason||' '||coalesce(p.name,'')||' '||coalesce(t.name,'')),?3)>0)"
    )
    };
    let mut stmt = db.prepare(sql)?;
    let rows=stmt.query_map(rusqlite::params![scan,status,query,offset],|r|if kind=="errors"{Ok(json!({"path":r.get::<_,String>(0)?,"code":r.get::<_,String>(1)?,"reason":r.get::<_,String>(2)?}))}else{Ok(json!({"path":r.get::<_,String>(0)?,"indexedPath":r.get::<_,String>(1)?,"title":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"reason":r.get::<_,String>(4)?,"managed":r.get::<_,bool>(5)?,"tool":r.get::<_,Option<String>>(6)?,"project":r.get::<_,Option<String>>(7)?}))})?.collect::<std::result::Result<Vec<_>,_>>()?;
    let total: i64 = db.query_row(count_sql, rusqlite::params![scan, status, query], |r| {
        r.get(0)
    })?;
    Ok(
        json!({"rows":rows,"total":total,"offset":offset,"hasMore":offset+100<total,"query":query,"status":status}),
    )
}
