use minorminor_core::{
    fail,
    server::{self, Engine},
    Result,
};
use serde_json::{json, Value};
use std::{io::Read, path::PathBuf};

fn argument(args: &[String], name: &str) -> Option<PathBuf> {
    args.iter()
        .position(|s| s == name)
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Minorminor: {error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|s| s == "--worker") {
        minorminor_core::mole::install_worker_signals();
        let mut input = Vec::new();
        std::io::stdin()
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut input)?;
        if input.len() > 2 * 1024 * 1024 {
            return Err(fail("后台输入超出限制"));
        }
        let payload: Value = serde_json::from_slice(&input)?;
        if let Some(parent) = payload["parentPid"].as_u64() {
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if unsafe { libc::getppid() } as u64 != parent {
                    unsafe {
                        libc::kill(-(std::process::id() as i32), libc::SIGTERM);
                    }
                    std::process::exit(1);
                }
            });
        }
        if let Err(error) = server::worker(payload) {
            println!("{}", json!({"event":"error","error":error.to_string()}));
            return Err(error);
        }
        return Ok(());
    }
    let home = argument(&args, "--home")
        .or_else(dirs::home_dir)
        .ok_or_else(|| fail("无法取得用户目录"))?;
    let data = argument(&args, "--data")
        .unwrap_or_else(|| home.join("Library/Application Support/com.minorminor.desktop"));
    let resources = argument(&args, "--resources").unwrap_or_else(|| {
        let binary = std::env::current_exe().ok();
        let bundled = binary.and_then(|p| {
            p.parent()
                .and_then(|p| p.parent())
                .map(|p| p.join("Resources"))
        });
        bundled
            .filter(|p| p.join("vendor/mole/mole").is_file())
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .to_owned()
            })
    });
    let engine = Engine::new(home, data, resources)?;
    let port = args
        .iter()
        .position(|s| s == "--port")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.parse::<u16>())
        .transpose()?
        .unwrap_or(0);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let address = listener.local_addr()?;
    let engine_server = engine.clone();
    tokio::spawn(async move {
        if let Err(error) = server::serve(engine_server, listener).await {
            eprintln!("本地界面服务停止：{error}");
        }
    });
    let url = format!("http://{address}");
    if args.iter().any(|s| s == "--headless") {
        println!(
            "{}",
            json!({"url":url,"version":"0.2.0","backend":"rust","home":engine.home})
        );
        tokio::signal::ctrl_c().await?;
        engine.cancel();
        return Ok(());
    }
    #[cfg(feature = "desktop")]
    {
        use muda::{Menu, PredefinedMenuItem, Submenu};
        use tao::{
            event::{Event, WindowEvent},
            event_loop::{ControlFlow, EventLoop},
            window::WindowBuilder,
        };
        let event_loop = EventLoop::new();
        let window = WindowBuilder::new()
            .with_title("Minorminor")
            .with_inner_size(tao::dpi::LogicalSize::new(1180.0, 820.0))
            .with_min_inner_size(tao::dpi::LogicalSize::new(820.0, 600.0))
            .build(&event_loop)?;
        let origin = reqwest::Url::parse(&url)?.origin();
        let webview = wry::WebViewBuilder::new()
            .with_url(url)
            .with_navigation_handler(move |target| {
                reqwest::Url::parse(&target)
                    .map(|u| u.origin() == origin || u.as_str() == "about:blank")
                    .unwrap_or(false)
            })
            .build(&window)?;
        let menu = Menu::new();
        let app_menu = Submenu::new("Minorminor", true);
        app_menu.append_items(&[
            &PredefinedMenuItem::about(None, None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ])?;
        let edit = Submenu::new("编辑", true);
        edit.append_items(&[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ])?;
        let window_menu = Submenu::new("窗口", true);
        window_menu.append_items(&[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::close_window(None),
        ])?;
        menu.append_items(&[&app_menu, &edit, &window_menu])?;
        #[cfg(target_os = "macos")]
        menu.init_for_nsapp();
        event_loop.run(move |event, _, control_flow| {
            let _keep_alive = (&webview, &window, &menu);
            *control_flow = ControlFlow::Wait;
            if matches!(
                event,
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } | Event::LoopDestroyed
            ) {
                engine.cancel();
                *control_flow = ControlFlow::Exit;
            }
        });
    }
    #[cfg(not(feature = "desktop"))]
    {
        println!("{}", json!({"url":url,"backend":"rust"}));
        tokio::signal::ctrl_c().await?;
        engine.cancel();
        Ok(())
    }
}
