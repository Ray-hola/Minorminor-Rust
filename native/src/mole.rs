use crate::{fail, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI32, Ordering};
use std::{
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
static HELPER: AtomicI32 = AtomicI32::new(0);
extern "C" fn terminate_worker(signal: i32) {
    let helper = HELPER.load(Ordering::Relaxed);
    unsafe {
        if helper > 0 {
            libc::kill(-helper, libc::SIGKILL);
        }
        libc::_exit(128 + signal);
    }
}
pub fn install_worker_signals() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            terminate_worker as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            terminate_worker as *const () as libc::sighandler_t,
        );
    }
}
struct HelperGroup(i32);
impl Drop for HelperGroup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
        let _ = HELPER.compare_exchange(self.0, 0, Ordering::Relaxed, Ordering::Relaxed);
    }
}

pub fn command_output(
    mut command: Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    limit: usize,
) -> Result<Vec<u8>> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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
    let _group = HelperGroup(pid as i32);
    if std::env::var_os("MINORMINOR_WORKER").is_some() {
        HELPER.store(pid as i32, Ordering::Relaxed);
    }
    if let Some(input) = input {
        let mut stdin = child.stdin.take().unwrap();
        thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(limit as u64 + 1).read_to_end(&mut bytes);
        (result, bytes)
    });
    let errors = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.take(16384).read_to_end(&mut bytes);
        bytes
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > timeout {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(fail("原生命令超时，未继续操作"));
        }
        thread::sleep(Duration::from_millis(20));
    };
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    let (result, bytes) = reader.join().map_err(|_| fail("读取原生命令输出失败"))?;
    result?;
    let error = errors.join().unwrap_or_default();
    if bytes.len() > limit {
        return Err(fail("原生命令输出超出限制"));
    }
    if !status.success() {
        return Err(fail(format!(
            "原生命令未完成：{}",
            String::from_utf8_lossy(&error)
                .chars()
                .take(500)
                .collect::<String>()
        )));
    }
    Ok(bytes)
}
pub fn metadata(resources: &Path) -> Value {
    let source = resources.join("vendor/mole/mole");
    let upstream: Value = std::fs::read(resources.join("UPSTREAM.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    json!({"available":source.is_file(),"version":upstream["version"],"repository":upstream["repository"],"license":upstream["license"],"resources":resources})
}
pub fn guard(resources: &Path, home: &Path, path: &Path) -> Result<()> {
    let script = resources.join("scripts/check-clean-target.sh");
    if !script.is_file() {
        return Err(fail("缺少 Mole 路径保护资源，保留文件"));
    }
    let mut command = Command::new("/bin/bash");
    command
        .arg(script)
        .arg(path)
        .env_clear()
        .env(
            "PATH",
            "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin",
        )
        .env("HOME", home)
        .env("TERM", "dumb")
        .env("MOLE_DRY_RUN", "0")
        .env("MOLE_TEST_NO_AUTH", "1")
        .env("MO_NO_OPLOG", "1")
        .env("CI", "1");
    command_output(command, None, Duration::from_secs(30), 8192)?;
    Ok(())
}
pub fn unused(path: &Path) -> Result<()> {
    let mut command = Command::new("/usr/sbin/lsof");
    command
        .args(["-t", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(fail("无法确认文件占用状态，已保留"));
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output()?;
    if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
        Ok(())
    } else {
        Err(fail("文件正在使用或无法确认占用状态，已保留"))
    }
}
pub fn preview(resources: &Path, home: &Path, command: &str) -> Result<Value> {
    if !["clean", "purge", "installer", "optimize"].contains(&command) {
        return Err(fail("不支持的 Mole 命令"));
    }
    let mut process = Command::new("/bin/bash");
    process
        .arg(resources.join("vendor/mole/mole"))
        .arg(command)
        .arg("--dry-run")
        .current_dir(resources.join("vendor/mole"))
        .env("HOME", home)
        .env(
            "PATH",
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        )
        .env("MOLE_DRY_RUN", "1")
        .env("MOLE_TEST_NO_AUTH", "1")
        .env("MO_NO_OPLOG", "1")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("CI", "1")
        .env_remove("MOLE_TEST_MODE")
        .env_remove("MOLE_TEST_TRASH_DIR");
    let bytes = command_output(process, None, Duration::from_secs(600), 4 * 1024 * 1024)?;
    let raw = String::from_utf8_lossy(&bytes);
    let ansi = regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]")?;
    Ok(
        json!({"command":command,"mode":"preview","output":ansi.replace_all(&raw,"").into_owned(),"createdAt":crate::now()}),
    )
}
