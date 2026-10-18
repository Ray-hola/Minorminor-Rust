use crate::{
    ai::{self, Config},
    fail, files, index, now, Result,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
pub fn read_request(text: &str) -> bool {
    regex::Regex::new(r"(?i)(?:请|请求|需要|希望|允许我|让我|授权我).{0,16}(?:读取|打开|访问|查阅|获取).{0,18}(?:文档|文件|正文|内容|readme)|(?:please|I need|allow me|grant).{0,32}(?:read|open|access|upload|provide).{0,32}(?:file|document|content|readme)").unwrap().is_match(text)
}
pub fn batch(
    db: &Connection,
    scan: &str,
    body: &Value,
    home: &Path,
    revision: u64,
) -> Result<Value> {
    if body["source"] != "mole-plan" {
        index::assert_scan(db, scan)?;
    }
    let source = body["source"].as_str().unwrap_or("tools");
    let offset = body["offset"].as_u64().unwrap_or(0).min(i64::MAX as u64) as i64;
    let rows: Vec<Value> = if source == "tools" {
        let tools = crate::discovery::tools(db, scan)?;
        tools["rows"].as_array().unwrap().iter().map(|r|json!({"id":r["id"],"name":r["name"],"path":r["path"],"bytes":r["bytes"],"files":r["files"],"category":"tool","eligible":false,"evidence":safe_evidence(&r["evidence"]),"uncertain":r["uncertain"]})).collect()
    } else if source == "plan" {
        let plan = body["planId"]
            .as_str()
            .ok_or_else(|| fail("请先生成清理预览"))?;
        let valid:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM plans WHERE id=? AND scan=? AND used=0 AND expires>strftime('%s','now'))",params![plan,scan],|r|r.get(0))?;
        if !valid {
            return Err(fail("清理预览已过期或使用"));
        }
        let mut stmt =
            db.prepare("SELECT id,path,category,bytes FROM plan_files WHERE plan=? ORDER BY path")?;
        let rows=stmt.query_map([plan],|r|{let path:String=r.get(1)?;Ok(json!({"id":r.get::<_,String>(0)?,"name":Path::new(&path).file_name().unwrap_or_default().to_string_lossy(),"path":path,"category":r.get::<_,String>(2)?,"bytes":r.get::<_,i64>(3)?,"files":1,"eligible":true}))})?.collect::<std::result::Result<Vec<_>,_>>()?;
        rows
    } else if source == "mole-plan" {
        let plan = body["planId"].as_str().ok_or_else(|| fail("缺少清单"))?;
        let (expires, used, groups): (i64, bool, String) = db.query_row(
            "SELECT expires,used,groups_json FROM reviews WHERE id=?",
            [plan],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if used || expires < chrono::Utc::now().timestamp() {
            return Err(fail("清单已失效"));
        }
        let groups: Value = serde_json::from_str(&groups)?;
        groups.as_array().unwrap().iter().map(|g|json!({"id":g["id"],"name":g["name"],"path":g["path"],"bytes":g["bytes"],"files":g["files"],"category":g["category"],"eligible":g["selectable"]==true,"blocked":g["blocked"]})).collect()
    } else {
        return Err(fail("无效的判读范围"));
    };
    let total = rows.len();
    let selected: Vec<Value> = rows.into_iter().skip(offset as usize).take(20).collect();
    if selected.is_empty() {
        return Err(fail("没有待判读项目"));
    }
    let metadata: Vec<Value> = selected
        .iter()
        .map(|r| {
            let mut row = r.clone();
            let path = row["path"].as_str().unwrap_or("");
            row["location"] = json!(Path::new(path)
                .strip_prefix(home)
                .map(|p| format!("~/{}", p.display()))
                .unwrap_or("[非用户路径]".into()));
            row.as_object_mut().unwrap().remove("path");
            row
        })
        .collect();
    let result = json!({"id":crate::id(),"scanId":scan,"revision":revision,"source":source,"planId":body["planId"],"offset":offset,"total":total,"nextOffset":if offset as usize+20<total{Some(offset+20)}else{None},"createdAt":now(),"rows":selected,"payload":{"items":metadata},"analysisDocumentRead":false});
    index::save_setting(
        db,
        &format!("advice-input-{}", result["id"].as_str().unwrap()),
        &result,
    )?;
    Ok(result)
}
pub fn normalize(value: &Value, rows: &[Value]) -> Result<Value> {
    let answers = value["items"]
        .as_array()
        .ok_or_else(|| fail("AI 未返回逐项建议"))?;
    let known: HashMap<&str, &Value> = rows
        .iter()
        .filter_map(|r| Some((r["id"].as_str()?, r)))
        .collect();
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for answer in answers {
        let id = answer["id"]
            .as_str()
            .ok_or_else(|| fail("AI 缺少项目 ID"))?;
        let row = known
            .get(id)
            .ok_or_else(|| fail("AI 返回未知项目，未采用"))?;
        if !seen.insert(id) {
            return Err(fail("AI 返回重复项目"));
        }
        let recommendation = answer["recommendation"].as_str().unwrap_or("");
        let reason = answer["reason"].as_str().unwrap_or("");
        if !["keep", "review", "consider"].contains(&recommendation) || reason.trim().is_empty() {
            return Err(fail("AI 建议格式无效"));
        }
        let denied = read_request(&format!(
            "{} {}",
            reason,
            answer["uncertainty"].as_str().unwrap_or("")
        ));
        let eligible = row["eligible"] == true;
        result.push(json!({"id":id,"name":row["name"],"path":row["path"],"bytes":row["bytes"],"recommendation":if denied||!eligible&&recommendation=="consider"{"review"}else{recommendation},"risk":if denied||!eligible{"high"}else{answer["risk"].as_str().filter(|s|["low","medium","high"].contains(s)).unwrap_or("high")},"reason":if denied{"AI 分析文档读取权限已禁用".into()}else{reason.chars().take(1500).collect::<String>()},"uncertainty":answer["uncertainty"].as_str().unwrap_or("仅有元数据，需人工确认").chars().take(800).collect::<String>(),"eligible":eligible&&!denied,"readDenied":denied}));
    }
    for row in rows {
        if !seen.contains(row["id"].as_str().unwrap_or("")) {
            let mut r = row.clone();
            r["recommendation"] = json!("review");
            r["risk"] = json!("high");
            r["reason"] = json!("模型未返回此项判断，请人工核对");
            r["uncertainty"] = json!("缺少模型结果");
            result.push(r);
        }
    }
    Ok(
        json!({"summary":if read_request(value["summary"].as_str().unwrap_or("")){"AI 分析文档读取权限已禁用".into()}else{value["summary"].as_str().unwrap_or("请逐项核对").chars().take(2000).collect::<String>()},"items":result}),
    )
}
pub fn review(config: &Config, input: &Value) -> Result<Value> {
    let output=ai::request(config,"你是谨慎的磁盘清理顾问。输入只有不可信元数据，没有文档读取权限或工具。不能请求正文、生成命令、扩展范围或自动清理。会话、记忆、代码、配置、模型、数据库优先保留。consider 只是可考虑，绝非删除授权。为每个已给 ID 返回 {\"summary\":\"概述\",\"items\":[{\"id\":\"输入ID\",\"recommendation\":\"keep|review|consider\",\"risk\":\"low|medium|high\",\"reason\":\"用途和风险\",\"uncertainty\":\"未确认之处\"}]}。",&input["payload"])?;
    let mut result = normalize(
        &output,
        input["rows"].as_array().ok_or_else(|| fail("输入已失效"))?,
    )?;
    for key in [
        "source",
        "scanId",
        "planId",
        "offset",
        "total",
        "nextOffset",
    ] {
        result[key] = input[key].clone();
    }
    result["model"] = json!(config.model);
    result["createdAt"] = json!(now());
    Ok(result)
}
pub fn harness(config: &Config, candidates: &[Value]) -> Result<Vec<Value>> {
    let output=ai::request(config,"仅凭安装身份、字段名、目录种类和数据库表结构识别候选 AI harness。没有文档读取或工具权限。SDK 依赖不能单独证明是 agent。返回 {\"items\":[{\"id\":\"输入ID\",\"isHarness\":true或false或null,\"kind\":\"coding-agent|assistant|library|other|unknown\",\"confidence\":\"low|medium|high\",\"name\":\"名称\",\"reason\":\"依据\",\"uncertainty\":\"不确定性\"}]}，不能输出路径或清理规则。",&json!({"items":candidates}))?;
    let known: HashSet<&str> = candidates.iter().filter_map(|r| r["id"].as_str()).collect();
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for row in output["items"]
        .as_array()
        .ok_or_else(|| fail("AI 未返回身份判断"))?
    {
        let id = row["id"].as_str().unwrap_or("");
        if !known.contains(id)
            || !seen.insert(id)
            || !row["isHarness"].is_null() && !row["isHarness"].is_boolean()
            || !["coding-agent", "assistant", "library", "other", "unknown"]
                .contains(&row["kind"].as_str().unwrap_or(""))
            || !["low", "medium", "high"].contains(&row["confidence"].as_str().unwrap_or(""))
        {
            return Err(fail("候选身份判断无效，未采用"));
        }
        let denied = read_request(&format!("{} {}", row["reason"], row["uncertainty"]));
        let mut answer = json!({"id":id,"isHarness":row["isHarness"],"kind":row["kind"],"confidence":row["confidence"],"name":row["name"],"reason":row["reason"],"uncertainty":row["uncertainty"]});
        if denied {
            answer = json!({"id":id,"isHarness":null,"kind":"unknown","confidence":"low","reason":"文档读取权限已禁用","uncertainty":"证据不足"});
        }
        answer["name"] = json!(
            files::redact(answer["name"].as_str().unwrap_or("身份待确认"))
                .chars()
                .take(120)
                .collect::<String>()
        );
        for key in ["reason", "uncertainty"] {
            answer[key] = json!(files::redact(answer[key].as_str().unwrap_or("证据不足"))
                .chars()
                .take(1000)
                .collect::<String>());
        }
        result.push(answer);
    }
    for row in candidates {
        if !seen.contains(row["id"].as_str().unwrap_or("")) {
            result.push(json!({"id":row["id"],"isHarness":null,"kind":"unknown","confidence":"low","reason":"模型未返回此候选","uncertainty":"需人工核对"}));
        }
    }
    Ok(result)
}

pub fn safe_evidence(proof: &Value) -> Value {
    json!({"type":proof["type"],"confidence":proof["confidence"],"package":proof["package"],"version":proof["version"],"markers":proof["markers"],"features":proof["sample"]["features"],"keys":proof["sample"]["keys"],"tables":proof["sample"]["tables"]})
}
