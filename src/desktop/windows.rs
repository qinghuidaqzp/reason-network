use super::model::{self, Command, Snapshot};
use crate::{
    client::Link,
    paths,
    proto::{LinkInfo, Request},
};
use anyhow::{Context, Result};
use std::{cell::Cell, process::Stdio, time::Duration};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use tokio::{io::AsyncWriteExt, sync::mpsc};
use wry::{WebContext, WebViewBuilder};

enum Update {
    Directory(Box<Snapshot>),
    Notice(&'static str),
}

pub fn run() -> Result<()> {
    let event_loop = EventLoopBuilder::<Update>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title("Reason Network Desktop")
        .with_inner_size(LogicalSize::new(1160.0, 780.0))
        .with_min_inner_size(LogicalSize::new(780.0, 560.0))
        .build(&event_loop)?;
    let (send, receive) = mpsc::channel::<Command>(8);
    let proxy = event_loop.create_proxy();
    // A local profile for the system WebView2 runtime, separate from every agent/browser's profile.
    let mut context = WebContext::new(Some(paths::home().join("desktop-webview")));
    let first_document = Cell::new(true);
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_html(include_str!("index.html"))
        .with_devtools(false)
        .with_general_autofill_enabled(false)
        .with_navigation_handler(move |url| {
            // Wry/WebView2 reports its embedded HTML as a data document. Only that first load may use it.
            let first = first_document.replace(false);
            url == "about:blank" || (first && url.starts_with("data:text/html"))
        })
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .with_ipc_handler(move |request| {
            if let Some(command) = Command::parse(request.body()) {
                let _ = send.try_send(command);
            }
        })
        .build(&window)
        .context("Couldn't open the desktop window. Install the Microsoft Edge WebView2 Runtime and try again")?;
    let task = tokio::spawn(poll(receive, proxy));
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                task.abort();
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(Update::Directory(snapshot)) => {
                if let Ok(script) = model::update_script(&snapshot) {
                    let _ = webview.evaluate_script(&script);
                }
            }
            Event::UserEvent(Update::Notice(text)) => {
                if let Ok(text) = serde_json::to_string(text) {
                    let _ = webview.evaluate_script(&format!("window.reasonDesktop.notice({text});"));
                }
            }
            _ => (),
        }
    });
}

async fn poll(mut commands: mpsc::Receiver<Command>, proxy: tao::event_loop::EventLoopProxy<Update>) {
    let link = Link::existing();
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ready = false;
    let mut directory = None;
    let mut last_update = 0;
    loop {
        let command = tokio::select! {
            _ = tick.tick(), if ready => Command::Refresh {},
            command = commands.recv() => match command { Some(c) => c, None => return },
        };
        let update = match command {
            Command::Ready {} | Command::Refresh {} => {
                ready = true;
                let mut snapshot = model::read(&link).await;
                if snapshot.directory.is_some() {
                    directory = snapshot.directory.clone();
                    last_update = snapshot.updated_at;
                } else {
                    snapshot.directory = directory.clone();
                    snapshot.updated_at = last_update;
                }
                Update::Directory(Box::new(snapshot))
            }
            Command::CopyAddress { address } => {
                let copied = model::can_copy(directory.as_ref(), &address) && copy(&address).await;
                Update::Notice(if copied {
                    "Address copied"
                } else {
                    "Couldn't copy this address. Refresh and try again."
                })
            }
            Command::CopyTeamLink {} => {
                let info = link.call_within::<LinkInfo>(Request::Link { new: false }, Duration::from_secs(7)).await;
                let copied = match info {
                    Ok(info) => copy(&info.url).await,
                    _ => false,
                };
                Update::Notice(if copied {
                    "Team link copied. Anyone with it can join."
                } else {
                    "Couldn't copy a team link. Check the daemon connection."
                })
            }
        };
        if proxy.send_event(update).is_err() {
            return;
        }
    }
}

async fn copy(text: &str) -> bool {
    // clip.exe accepts UTF-16 with a BOM. Sending bytes over stdin keeps every character literal.
    let Some(root) = std::env::var_os("SystemRoot") else { return false };
    let mut command = tokio::process::Command::new(std::path::PathBuf::from(root).join("System32/clip.exe"));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .kill_on_drop(true);
    let Ok(mut child) = command.spawn() else { return false };
    let bytes: Vec<u8> = std::iter::once(0xfeff).chain(text.encode_utf16()).flat_map(u16::to_le_bytes).collect();
    tokio::time::timeout(Duration::from_secs(2), async {
        let Some(mut stdin) = child.stdin.take() else { return false };
        if stdin.write_all(&bytes).await.is_err() {
            return false;
        }
        drop(stdin);
        child.wait().await.is_ok_and(|s| s.success())
    })
    .await
    .unwrap_or(false)
}
