use crate::manager::ServiceManager;
use anyhow::{Context, Result, anyhow};
use server_on_core::event::DaemonEvent;
use server_on_core::paths::AppPaths;
use server_on_core::protocol::{ClientRequest, Command, ErrorInfo, ResponseData, ServerMessage};
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex as AsyncMutex, broadcast, oneshot};
use uuid::Uuid;

#[cfg(windows)]
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

pub async fn serve(manager: ServiceManager) -> Result<()> {
    #[cfg(windows)]
    {
        let pipe_name = AppPaths::pipe_name();
        let mut first = true;
        loop {
            let server = create_server(&pipe_name, first)?;
            first = false;
            server.connect().await?;
            let manager = manager.clone();
            tokio::spawn(async move {
                if let Err(error) = handle_connection(server, manager).await {
                    tracing::debug!(%error, "IPC 客户端连接结束");
                }
            });
        }
    }
    #[cfg(not(windows))]
    {
        let _ = manager;
        bail!("ServerOn 当前仅支持 Windows Named Pipe")
    }
}

#[cfg(windows)]
async fn handle_connection(server: NamedPipeServer, manager: ServiceManager) -> Result<()> {
    let (read, write) = tokio::io::split(server);
    let writer = Arc::new(AsyncMutex::new(write));
    let mut lines = BufReader::new(read).lines();
    let mut events = manager.subscribe();
    let mut subscribed = false;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break; };
                let request: ClientRequest = match serde_json::from_str(&line) {
                    Ok(request) => request,
                    Err(error) => {
                        tracing::warn!(%error, "忽略无效 IPC 请求");
                        continue;
                    }
                };
                if matches!(request.command, Command::Subscribe) {
                    subscribed = true;
                    let response = ServerMessage::success(request.request_id, ResponseData::Empty);
                    write_server_message(&writer, &response).await?;
                    continue;
                }
                let manager = manager.clone();
                let writer = writer.clone();
                tokio::spawn(async move {
                    let response = dispatch(manager, request).await;
                    if let Err(error) = write_server_message(&writer, &response).await {
                        tracing::debug!(%error, "发送 IPC 响应失败");
                    }
                });
            }
            event = events.recv() => {
                match event {
                    Ok(event) if subscribed => {
                        write_server_message(&writer, &ServerMessage::Event(Box::new(event))).await?;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(())
}

async fn dispatch(manager: ServiceManager, request: ClientRequest) -> ServerMessage {
    let request_id = request.request_id;
    let result: Result<ResponseData> = match request.command {
        Command::Ping => Ok(ResponseData::Pong),
        Command::GetSnapshot => Ok(ResponseData::Snapshot(manager.snapshot())),
        Command::Subscribe => Ok(ResponseData::Empty),
        Command::UpsertService(definition) => manager
            .upsert_service(*definition)
            .map(|_| ResponseData::Empty),
        Command::RemoveService { service_id } => manager
            .remove_service(&service_id)
            .map(|_| ResponseData::Empty),
        Command::StartService { service_id } => manager
            .start(&service_id)
            .await
            .map(|_| ResponseData::Empty),
        Command::StopService { service_id } => {
            manager.stop(&service_id).await.map(|_| ResponseData::Empty)
        }
        Command::RestartService { service_id } => manager
            .restart(&service_id)
            .await
            .map(|_| ResponseData::Empty),
        Command::RefreshDiscovery => manager
            .refresh_discovery()
            .await
            .map(|_| ResponseData::Empty),
        Command::GetSettings => Ok(ResponseData::Settings(Box::new(manager.settings()))),
        Command::UpdateSettings(settings) => manager
            .update_settings(*settings)
            .map(|_| ResponseData::Empty),
        Command::GetLogs { service_id, limit } => manager
            .recent_logs(&service_id, limit)
            .await
            .map(ResponseData::Logs),
        Command::GetMetrics { service_id } => manager
            .metrics(&service_id)
            .await
            .map(|metrics| ResponseData::Metrics(Box::new(metrics))),
    };
    match result {
        Ok(data) => ServerMessage::success(request_id, data),
        Err(error) => ServerMessage::failure(request_id, "operation-failed", format!("{error:#}")),
    }
}

#[cfg(windows)]
async fn write_server_message(
    writer: &Arc<AsyncMutex<tokio::io::WriteHalf<NamedPipeServer>>>,
    message: &ServerMessage,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    let mut writer = writer.lock().await;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(windows)]
fn create_server(name: &str, first: bool) -> io::Result<NamedPipeServer> {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use windows::Win32::Foundation::{CloseHandle, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{PCWSTR, PWSTR};

    fn current_user_sid() -> windows::core::Result<String> {
        let process = unsafe { GetCurrentProcess() };
        let mut token = Default::default();
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token)? };
        let result = (|| {
            let mut length = 0;
            let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut length) };
            if length == 0 {
                return Err(windows::core::Error::from_thread());
            }
            let mut data = vec![0_u8; length as usize];
            unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    Some(data.as_mut_ptr().cast()),
                    length,
                    &mut length,
                )?;
            }
            let user = unsafe { &*(data.as_ptr().cast::<TOKEN_USER>()) };
            let mut sid_string = PWSTR(null_mut());
            unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_string)? };
            let sid = unsafe { PCWSTR(sid_string.0).to_string()? };
            unsafe { LocalFree(Some(HLOCAL(sid_string.0.cast::<c_void>()))) };
            Ok(sid)
        })();
        let _ = unsafe { CloseHandle(token) };
        result
    }

    let sid = current_user_sid().map_err(io::Error::other)?;
    let sddl = format!("D:P(A;;GA;;;{sid})");
    let sddl_wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR(null_mut());
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_wide.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(io::Error::other)?;
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: windows::core::BOOL(0),
    };
    let result = unsafe {
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        options.create_with_security_attributes_raw(
            name,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    };
    unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    let _ = null::<u8>();
    result
}

#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    writer: AsyncMutex<tokio::io::WriteHalf<NamedPipeClient>>,
    pending: Mutex<HashMap<Uuid, oneshot::Sender<Result<ResponseData, ErrorInfo>>>>,
    events: broadcast::Sender<DaemonEvent>,
}

impl DaemonClient {
    pub async fn connect() -> Result<Self> {
        #[cfg(windows)]
        {
            let client = ClientOptions::new()
                .read(true)
                .write(true)
                .open(AppPaths::pipe_name())
                .context("无法连接 ServerOn daemon")?;
            let (read, write) = tokio::io::split(client);
            let (events, _) = broadcast::channel(2_048);
            let inner = Arc::new(ClientInner {
                writer: AsyncMutex::new(write),
                pending: Mutex::new(HashMap::new()),
                events,
            });
            let reader_inner = inner.clone();
            tokio::spawn(async move {
                read_client_messages(read, reader_inner).await;
            });
            Ok(Self { inner })
        }
        #[cfg(not(windows))]
        {
            bail!("ServerOn 当前仅支持 Windows")
        }
    }

    pub async fn connect_or_start() -> Result<Self> {
        match Self::connect().await {
            Ok(client) => Ok(client),
            Err(original_error) => {
                start_daemon_process()?;
                let mut last_error = original_error;
                for _ in 0..80 {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    match Self::connect().await {
                        Ok(client) => return Ok(client),
                        Err(error) => last_error = error,
                    }
                }
                Err(last_error)
            }
        }
    }

    pub async fn request(&self, command: Command) -> Result<ResponseData> {
        let request = ClientRequest::new(command);
        let request_id = request.request_id;
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(request_id, sender);
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        let write_result = {
            let mut writer = self.inner.writer.lock().await;
            writer.write_all(&bytes).await
        };
        if let Err(error) = write_result {
            self.inner.pending.lock().unwrap().remove(&request_id);
            return Err(error).context("发送 daemon 请求失败");
        }
        receiver
            .await
            .map_err(|_| anyhow!("daemon 连接已断开"))?
            .map_err(|error| anyhow!("{}：{}", error.code, error.message))
    }

    pub async fn subscribe(&self) -> Result<broadcast::Receiver<DaemonEvent>> {
        self.request(Command::Subscribe).await?;
        Ok(self.inner.events.subscribe())
    }

    pub fn events(&self) -> broadcast::Receiver<DaemonEvent> {
        self.inner.events.subscribe()
    }
}

#[cfg(windows)]
async fn read_client_messages(read: tokio::io::ReadHalf<NamedPipeClient>, inner: Arc<ClientInner>) {
    let mut lines = BufReader::new(read).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match serde_json::from_str::<ServerMessage>(&line) {
                Ok(ServerMessage::Response {
                    request_id,
                    success,
                    data,
                    error,
                }) => {
                    if let Some(sender) = inner.pending.lock().unwrap().remove(&request_id) {
                        let result = if success {
                            Ok(data.unwrap_or(ResponseData::Empty))
                        } else {
                            Err(error.unwrap_or_else(|| {
                                ErrorInfo::new("daemon-error", "daemon 返回未知错误")
                            }))
                        };
                        let _ = sender.send(result);
                    }
                }
                Ok(ServerMessage::Event(event)) => {
                    let _ = inner.events.send(*event);
                }
                Err(error) => tracing::warn!(%error, "忽略无效 daemon 消息"),
            },
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(%error, "读取 daemon 消息失败");
                break;
            }
        }
    }
    let pending = std::mem::take(&mut *inner.pending.lock().unwrap());
    for (_, sender) in pending {
        let _ = sender.send(Err(ErrorInfo::new("disconnected", "daemon 连接已断开")));
    }
}

#[cfg(windows)]
fn start_daemon_process() -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let current = std::env::current_exe()?;
    let sibling = current
        .parent()
        .map(|directory| directory.join("server-on-daemon.exe"))
        .filter(|path| path.exists());
    let executable = sibling
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "server-on-daemon.exe".into());
    Command::new(executable)
        .creation_flags(0x00000008 | 0x08000000) // DETACHED_PROCESS | CREATE_NO_WINDOW
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("无法启动 server-on-daemon；请先构建 daemon 可执行文件")?;
    Ok(())
}

#[cfg(not(windows))]
fn start_daemon_process() -> Result<()> {
    bail!("ServerOn 当前仅支持 Windows")
}
