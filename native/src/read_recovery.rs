//! Recheck recorded failures, then rescan only directories that became readable.
//! Audit results and small index deltas never expand the compressed full index.
use crate::{cloud, files::Resource, index, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub fn category(code: &str, path: &Path) -> &'static str {
    match code {
        "EACCES" | "EPERM" => "permission",
        "ENOENT" | "ENOTDIR" | "ESTALE" => "missing",
        "ETIMEDOUT" if cloud::is_cloud(path) => "cloud-timeout",
        "ETIMEDOUT" => "timeout",
        "ELOOP" => "unsafe",
        "DB_READ_ERROR" | "UNSUPPORTED_DATABASE" => "format",
        _ => "read-error",
    }
}
pub fn code(errno: i64) -> &'static str {
    match errno as i32 {
        libc::EPERM => "EPERM",
        libc::EACCES => "EACCES",
        libc::ENOENT => "ENOENT",
        libc::ENOTDIR => "ENOTDIR",
        libc::ESTALE => "ESTALE",
        libc::ETIMEDOUT => "ETIMEDOUT",
        libc::ELOOP => "ELOOP",
        _ => "READ_ERROR",
    }
}
pub fn reason(code: &str, path: &Path) -> &'static str {
    match category(code, path) {
        "permission" if code == "EPERM" => "macOS 隐私或系统保护拒绝访问",
        "permission" => "文件或上级目录权限不足",
        "missing" => "路径已消失、失效或被替换",
        "cloud-timeout" => "云端目录读取超时或文件尚未下载",
        "timeout" => "文件系统读取超时",
        "unsafe" => "路径变为符号链接，不跟随读取",
        _ => "读取未完成",
    }
}
pub fn audit(db: &Connection, scan: &str) -> Result<Vec<Value>> {
    Ok(index::setting(db, &format!("read-recovery-{scan}"))?
        .and_then(|v| v["rows"].as_array().cloned())
        .unwrap_or_default())
}
fn save(db: &Connection, scan: &str, rows: &[Value]) -> Result<()> {
    index::save_setting(
        db,
        &format!("read-recovery-{scan}"),
        &json!({"rows":rows,"checkedAt":crate::now()}),
    )
}
pub fn run(db: &mut Connection, scan: &str, home: &Path, data: &Path) -> Result<Value> {
    run_with(db, scan, home, data, cloud::probe)
}
pub fn run_with(
    db: &mut Connection,
    scan: &str,
    home: &Path,
    data: &Path,
    mut probe: impl FnMut(&Path, bool) -> std::io::Result<Value>,
) -> Result<Value> {
    index::assert_scan(db, scan)?;
    let info = index::scan_info(db, scan)?;
    let raw_scope = info["root"].as_str().unwrap_or("");
    let scope = PathBuf::from(if raw_scope == "/System/Volumes/Data" {
        "/"
    } else {
        raw_scope
    });
    let original: Vec<(String, String, String)> = {
        let mut stmt = db.prepare(
            "SELECT DISTINCT path,code,reason FROM errors WHERE scan=? ORDER BY path,code",
        )?;
        let rows = stmt.query_map([scan], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let mut rows = audit(db, scan)?;
    let mut measured = Vec::<PathBuf>::new();
    for (i, (raw, old_code, old_reason)) in original.iter().enumerate() {
        let path = Path::new(raw);
        println!(
            "{}",
            json!({"event":"progress","stats":{"stage":"read-recovery","currentPath":path,"completed":i,"total":original.len()}})
        );
        std::io::stdout().flush()?;
        let previous = rows.iter().position(|r| {
            r["path"] == *raw
                && r["code"] == *old_code
                && r["status"] != "recovered"
                && r["status"] != "resolved"
        });
        let mut row = json!({"path":raw,"originalCode":old_code,"originalReason":old_reason,"code":old_code,"reason":old_reason,"category":category(old_code,path),"status":"blocked","checkedAt":crate::now()});
        if let Some(i) = previous {
            row["originalCode"] = rows[i]["originalCode"].clone();
            row["originalReason"] = rows[i]["originalReason"].clone();
        }
        let database = ["DB_READ_ERROR", "UNSUPPORTED_DATABASE"].contains(&old_code.as_str());
        // The caller supplies a scan ID, never arbitrary filesystem paths.
        let checked = if !path.is_absolute() || !path.starts_with(&scope) {
            Err(std::io::Error::from_raw_os_error(libc::EACCES))
        } else {
            probe(path, database)
        };
        let mut result = checked.unwrap_or_else(
            |e| json!({"code":e.raw_os_error().unwrap_or(libc::EIO),"detail":e.to_string()}),
        );
        let mut actual = path.to_owned();
        // Startup-volume scans use logical paths. Some hidden Data-volume roots
        // have no alias at '/', so ENOENT alone must not imply disappearance.
        if raw_scope == "/System/Volumes/Data"
            && result["code"].as_i64() == Some(libc::ENOENT as i64)
        {
            let physical = Path::new(raw_scope).join(path.strip_prefix("/")?);
            let candidate = probe(&physical, database)
                .unwrap_or_else(|e| json!({"code":e.raw_os_error().unwrap_or(libc::EIO)}));
            if candidate["code"].as_i64() != Some(libc::ENOENT as i64) {
                let expected: Option<String> = db
                    .query_row(
                        "SELECT identity FROM entries WHERE scan=? AND path=?",
                        params![scan, raw],
                        |r| r.get(0),
                    )
                    .optional()?;
                let matches = match (expected, candidate.get("metadata")) {
                    (Some(old), Some(new)) => {
                        let old: index::Identity = serde_json::from_str(&old)?;
                        let new: cloud::Metadata = serde_json::from_value(new.clone())?;
                        old.dev == new.dev() && old.ino == new.ino()
                    }
                    (_, None) => candidate["code"].as_i64().is_some(),
                    _ => false,
                };
                if matches {
                    result = candidate;
                    actual = physical;
                    row["actualPath"] = json!(actual);
                } else {
                    result = json!({"code":libc::ESTALE});
                }
            }
        }
        for key in ["uid", "mode", "operation"] {
            if !result[key].is_null() {
                row[key] = result[key].clone();
            }
        }
        if let Some(errno) = result["code"].as_i64() {
            let code = code(errno);
            row["code"] = json!(code);
            row["category"] = json!(category(code, path));
            row["reason"] = json!(reason(code, path));
            row["status"] = json!(match category(code, path) {
                "missing" => "missing",
                "cloud-timeout" | "timeout" => "timeout",
                _ => "blocked",
            });
        } else if let Some(metadata) = result.get("metadata") {
            let metadata: cloud::Metadata = serde_json::from_value(metadata.clone())?;
            row["uid"] = result["uid"].clone();
            row["mode"] = result["mode"].clone();
            if metadata.is_dir() && path != scope {
                let refreshed = if measured.iter().any(|p| path.starts_with(p)) {
                    Ok(json!({"updated":true}))
                } else {
                    crate::scan::without_progress(|| {
                        crate::measure::run_mapped(
                            db,
                            Resource {
                                path: actual.clone(),
                                root: scope.clone(),
                                scan: scan.into(),
                                identity: metadata.identity,
                                directory: true,
                            },
                            path,
                            home,
                            data,
                        )
                    })
                };
                match refreshed {
                    Ok(_) => {
                        measured.push(path.into());
                        let remains: bool = db.query_row(
                            "SELECT EXISTS(SELECT 1 FROM errors WHERE scan=? AND path=?)",
                            params![scan, raw],
                            |r| r.get(0),
                        )?;
                        if remains {
                            let (code, reason): (String, String) = db.query_row(
                                "SELECT code,reason FROM errors WHERE scan=? AND path=? LIMIT 1",
                                params![scan, raw],
                                |r| Ok((r.get(0)?, r.get(1)?)),
                            )?;
                            row["category"] = json!(category(&code, path));
                            row["code"] = json!(code);
                            row["reason"] = json!(reason);
                        } else {
                            row["status"] = json!("recovered");
                            row["reason"] = json!("目录已补扫，空间与分类已更新");
                        }
                    }
                    Err(e) => row["reason"] = json!(format!("目录可读取，补扫未完成：{e}")),
                }
            } else if database
                && ["empty", "path-reference"].contains(&result["format"].as_str().unwrap_or(""))
            {
                row["status"] = json!("resolved");
                row["category"] = json!("metadata");
                row["reason"] = json!(if result["format"] == "empty" {
                    "空文件；无 SQLite 内容，已取消数据库误报"
                } else {
                    "备份路径记录；不是 SQLite 数据库"
                });
            } else if database && result["format"] == "sqlite" {
                let tool: Option<String> = db
                    .query_row(
                        "SELECT tool FROM entries WHERE scan=? AND path=?",
                        params![scan, raw],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                if let Some(tool) = tool {
                    match crate::discovery::read_database(db, scan, path, &tool, home) {
                        Ok(()) => {
                            row["status"] = json!("recovered");
                            row["reason"] = json!("数据库已补读");
                        }
                        Err(e) => row["reason"] = json!(format!("数据库补读未完成：{e}")),
                    }
                }
            } else if database {
                row["category"] = json!("format");
                row["reason"] = json!("文件可读取，但内容不是 SQLite 数据库");
            }
        }
        let tx = db.transaction()?;
        tx.execute(
            "DELETE FROM errors WHERE scan=? AND path=? AND code=?",
            params![scan, raw, old_code],
        )?;
        if !["recovered", "resolved"].contains(&row["status"].as_str().unwrap_or("")) {
            tx.execute(
                "INSERT INTO errors(scan,path,code,reason) VALUES(?,?,?,?)",
                params![
                    scan,
                    raw,
                    row["code"].as_str().unwrap_or(old_code),
                    row["reason"].as_str().unwrap_or(old_reason)
                ],
            )?;
        }
        if let Some(i) = previous {
            rows.remove(i);
        }
        rows.retain(|r| !(r["path"] == *raw && r["originalCode"] == row["originalCode"]));
        rows.push(row);
        let mut stats = index::scan_info(&tx, scan)?["stats"].clone();
        stats["skipped"] = json!(tx.query_row(
            "SELECT count(*) FROM errors WHERE scan=?",
            [scan],
            |r| r.get::<_, i64>(0)
        )?);
        tx.execute(
            "UPDATE scans SET stats=? WHERE id=?",
            params![stats.to_string(), scan],
        )?;
        save(&tx, scan, &rows)?;
        tx.commit()?;
    }
    println!(
        "{}",
        json!({"event":"progress","stats":{"stage":"read-recovery","completed":original.len(),"total":original.len()}})
    );
    let mut summary = serde_json::Map::new();
    for row in &rows {
        let key = if ["recovered", "resolved"].contains(&row["status"].as_str().unwrap_or("")) {
            row["status"].as_str().unwrap()
        } else {
            row["category"].as_str().unwrap_or("read-error")
        };
        let n = summary.get(key).and_then(Value::as_u64).unwrap_or(0);
        summary.insert(key.into(), json!(n + 1));
    }
    Ok(
        json!({"scanId":scan,"checked":original.len(),"summary":summary,"remaining":index::scan_info(db,scan)?["stats"]["skipped"]}),
    )
}
