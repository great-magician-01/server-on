use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use server_on_core::config::WindowsShell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::mem::{ManuallyDrop, size_of};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent, GetConsoleWindow,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JobObjectBasicAccountingInformation, JobObjectBasicAndIoAccountingInformation,
    JobObjectBasicProcessIdList, OpenJobObjectW, QueryInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    CreateProcessW, GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE, PROCESS_VM_READ,
    ResumeThread, STARTF_USESHOWWINDOW, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

pub struct WindowsProcess {
    pid: u32,
    process: OwnedHandle,
    job: Arc<OwnedHandle>,
    job_name: String,
}

#[derive(Clone)]
pub struct WindowsProcessController {
    pid: u32,
    job: Arc<OwnedHandle>,
}

pub struct WindowsProcessMetrics {
    pub cpu_time_ns: u64,
    pub memory_bytes: u64,
    pub process_count: u32,
    pub partial: bool,
}

pub struct SpawnedWindowsProcess {
    pub process: WindowsProcess,
    pub controller: WindowsProcessController,
    pub stdout: tokio::fs::File,
    pub stderr: tokio::fs::File,
}

impl WindowsProcess {
    pub fn spawn(
        shell: WindowsShell,
        working_directory: &str,
        script: &str,
        environment: &BTreeMap<String, String>,
    ) -> Result<SpawnedWindowsProcess> {
        let command_line = match shell {
            WindowsShell::PowerShell => format!(
                "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -Command {}",
                quote_windows_argument(script)
            ),
            WindowsShell::Cmd => format!("cmd.exe /D /S /C {script}"),
        };
        let mut command_line = wide_mut(&command_line);
        let working_directory_wide = wide(working_directory);
        let environment_block = build_environment_block(environment);
        let environment_ptr = environment_block
            .as_ref()
            .map(|block| block.as_ptr().cast::<c_void>());

        let mut security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: BOOL(1),
        };
        let (stdin_read, stdin_write) = pipe(&mut security)?;
        let (stdout_read, stdout_write) = pipe(&mut security)?;
        let (stderr_read, stderr_write) = pipe(&mut security)?;
        // Only the child-facing ends may be inherited.
        unsafe {
            SetHandleInformation(stdin_write.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
            SetHandleInformation(stdout_read.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
            SetHandleInformation(stderr_read.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
        }

        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            dwFlags: STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW,
            wShowWindow: 0, // SW_HIDE
            hStdInput: stdin_read.0,
            hStdOutput: stdout_write.0,
            hStdError: stderr_write.0,
            ..Default::default()
        };
        let mut information = PROCESS_INFORMATION::default();
        let creation_flags = CREATE_SUSPENDED
            | CREATE_NEW_CONSOLE
            | CREATE_NEW_PROCESS_GROUP
            | CREATE_UNICODE_ENVIRONMENT;
        unsafe {
            CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(command_line.as_mut_ptr())),
                Some(&security),
                Some(&security),
                true,
                creation_flags,
                environment_ptr,
                PCWSTR(working_directory_wide.as_ptr()),
                &startup,
                &mut information,
            )
            .context("CreateProcessW 失败")?;
        }
        let process = OwnedHandle::new(information.hProcess);
        let thread = OwnedHandle::new(information.hThread);
        // The parent must close its copies of child-facing handles.
        drop(stdin_read);
        drop(stdout_write);
        drop(stderr_write);
        // stdin_write is intentionally closed below, giving the service EOF on stdin.

        let job_name_string = format!(r"Local\ServerOn-{}", Uuid::new_v4().simple());
        let job_name = wide(&job_name_string);
        let job = unsafe { CreateJobObjectW(None, PCWSTR(job_name.as_ptr())) }
            .context("无法创建 Windows Job Object")?;
        let job = Arc::new(OwnedHandle::new(job));
        if let Err(error) = unsafe { AssignProcessToJobObject(job.0, process.0) } {
            let _ = unsafe { TerminateProcess(process.0, 1) };
            return Err(error).context("无法将服务进程加入 Job Object");
        }
        let suspend_count = unsafe { ResumeThread(thread.0) };
        if suspend_count == u32::MAX {
            let _ = unsafe { TerminateProcess(process.0, 1) };
            return Err(anyhow!("恢复服务主线程失败"));
        }
        drop(stdin_write);

        let process = WindowsProcess {
            pid: information.dwProcessId,
            process,
            job: job.clone(),
            job_name: job_name_string,
        };
        let controller = WindowsProcessController {
            pid: information.dwProcessId,
            job,
        };
        Ok(SpawnedWindowsProcess {
            process,
            controller,
            stdout: stdout_read.into_tokio_file(),
            stderr: stderr_read.into_tokio_file(),
        })
    }

    pub fn job_name(&self) -> &str {
        &self.job_name
    }

    pub fn recover(
        job_name: &str,
        pid: u32,
        expected_started_at: Option<DateTime<Utc>>,
    ) -> Result<(Self, WindowsProcessController)> {
        let Some(expected_started_at) = expected_started_at else {
            bail!("缺少进程启动时间，拒绝接管旧进程");
        };
        let process = unsafe {
            OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        }
        .context("无法重新打开受管 Windows 进程")?;
        let process = OwnedHandle::new(process);
        verify_process_start(&process, expected_started_at)?;
        let job_name_wide = wide(job_name);
        let job = match unsafe { OpenJobObjectW(0x000c, false, PCWSTR(job_name_wide.as_ptr())) } {
            Ok(job) => Arc::new(OwnedHandle::new(job)),
            Err(_) => {
                // A Job Object disappears when its last handle closes. Rebuild
                // it only after the root process identity has been verified.
                let job = unsafe { CreateJobObjectW(None, PCWSTR(job_name_wide.as_ptr())) }
                    .context("无法重建 Windows Job Object")?;
                let job = Arc::new(OwnedHandle::new(job));
                assign_process_tree(job.0, pid, &process)?;
                job
            }
        };
        let controller = WindowsProcessController {
            pid,
            job: job.clone(),
        };
        if controller.active_processes()? == 0 {
            bail!("Job Object 中没有活动进程");
        }
        Ok((
            Self {
                pid,
                process,
                job,
                job_name: job_name.to_owned(),
            },
            controller,
        ))
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub async fn wait(&self) -> Result<i32> {
        let controller = WindowsProcessController {
            pid: self.pid,
            job: self.job.clone(),
        };
        loop {
            if controller.active_processes()? == 0 {
                return controller.root_exit_code(&self.process);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl WindowsProcessController {
    pub fn identity(&self) -> String {
        format!("windows-job:{:x}", self.job.0.0 as usize)
    }

    pub fn metrics(&self) -> Result<WindowsProcessMetrics> {
        let mut accounting = JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                Some(self.job.0),
                JobObjectBasicAndIoAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION>() as u32,
                None,
            )?;
        }
        let cpu_time_ns = (accounting.BasicInfo.TotalUserTime.max(0)
            + accounting.BasicInfo.TotalKernelTime.max(0)) as u64
            * 100;
        let process_ids = self.process_ids()?;
        let mut memory_bytes = 0_u64;
        let mut partial = false;
        for pid in &process_ids {
            let process = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                    false,
                    *pid,
                )
            };
            let Ok(process) = process else {
                partial = true;
                continue;
            };
            let process = OwnedHandle::new(process);
            let mut counters = PROCESS_MEMORY_COUNTERS {
                cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            if unsafe { GetProcessMemoryInfo(process.0, &mut counters, counters.cb) }.is_ok() {
                memory_bytes = memory_bytes.saturating_add(counters.WorkingSetSize as u64);
            } else {
                partial = true;
            }
        }
        Ok(WindowsProcessMetrics {
            cpu_time_ns,
            memory_bytes,
            process_count: process_ids.len() as u32,
            partial,
        })
    }

    fn process_ids(&self) -> Result<Vec<u32>> {
        let mut capacity = 16_usize;
        for _ in 0..5 {
            let bytes =
                size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() + (capacity - 1) * size_of::<usize>();
            let mut buffer = vec![0_u8; bytes];
            let mut return_length = 0_u32;
            let result = unsafe {
                QueryInformationJobObject(
                    Some(self.job.0),
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr().cast(),
                    bytes as u32,
                    Some(&mut return_length),
                )
            };
            let info = unsafe { &*(buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()) };
            match result {
                Ok(()) => {
                    let count = info.NumberOfProcessIdsInList as usize;
                    let ids =
                        unsafe { std::slice::from_raw_parts(info.ProcessIdList.as_ptr(), count) };
                    return Ok(ids.iter().map(|pid| *pid as u32).collect());
                }
                Err(error) => {
                    let required = info.NumberOfAssignedProcesses as usize;
                    if required > capacity {
                        capacity = required + 8;
                        continue;
                    }
                    return Err(error).context("读取 Windows Job 进程列表失败");
                }
            }
        }
        bail!("Windows Job 进程数量超过可读取范围")
    }

    pub async fn stop(&self, timeout: Duration) -> Result<()> {
        if self.active_processes()? == 0 {
            return Ok(());
        }
        // The service runs in its own hidden console, so it can only receive
        // console control events while this daemon is attached to that same
        // console. If the signal cannot be delivered, fall through and let
        // the deadline terminate the entire job.
        if let Err(error) = send_ctrl_break(self.pid) {
            tracing::warn!(
                pid = self.pid,
                %error,
                "无法发送 Ctrl+Break，超时后将强制结束整个 Job"
            );
        }
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if self.active_processes()? == 0 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        unsafe { TerminateJobObject(self.job.0, 1) }.context("结束 Windows Job Object 失败")?;
        Ok(())
    }

    fn active_processes(&self) -> Result<u32> {
        let mut information = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                Some(self.job.0),
                JobObjectBasicAccountingInformation,
                (&mut information as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )?;
        }
        Ok(information.ActiveProcesses)
    }

    fn root_exit_code(&self, process: &OwnedHandle) -> Result<i32> {
        let mut code = 0;
        unsafe { GetExitCodeProcess(process.0, &mut code)? };
        Ok(code as i32)
    }
}

/// Deliver CTRL+BREAK to the service's process group.
///
/// Console control events are scoped to a console: the daemon (started
/// without one) must temporarily attach to the service's console before the
/// signal can reach it. Attaching is per-process and short-lived; we always
/// detach again so the daemon returns to its console-less state.
fn send_ctrl_break(pid: u32) -> Result<()> {
    unsafe {
        if !GetConsoleWindow().0.is_null() {
            bail!("daemon 自身已有控制台（通常为前台调试运行），无法附着服务控制台");
        }
        AttachConsole(pid).context("附着服务控制台失败")?;
        let result =
            GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid).context("发送 CTRL_BREAK 失败");
        let _ = FreeConsole();
        result
    }
}

fn verify_process_start(process: &OwnedHandle, expected: DateTime<Utc>) -> Result<()> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(process.0, &mut creation, &mut exit, &mut kernel, &mut user)? };
    let ticks = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    let unix_ticks = ticks
        .checked_sub(116_444_736_000_000_000)
        .ok_or_else(|| anyhow!("进程创建时间无效"))?;
    let seconds = unix_ticks / 10_000_000;
    let nanos = (unix_ticks % 10_000_000) * 100;
    let actual = DateTime::from_timestamp(seconds as i64, nanos as u32)
        .ok_or_else(|| anyhow!("进程创建时间超出范围"))?;
    if (actual - expected).num_seconds().abs() > 10 {
        bail!("进程 PID 已复用或身份不匹配，拒绝接管");
    }
    Ok(())
}

fn assign_process_tree(job: HANDLE, root_pid: u32, root_process: &OwnedHandle) -> Result<()> {
    let pids = process_tree_pids(root_pid)?;
    for pid in pids {
        if pid == root_pid {
            unsafe { AssignProcessToJobObject(job, root_process.0)? };
        } else {
            let process = unsafe {
                OpenProcess(
                    PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                    false,
                    pid,
                )
            }?;
            let process = OwnedHandle::new(process);
            unsafe { AssignProcessToJobObject(job, process.0)? };
        }
    }
    Ok(())
}

fn process_tree_pids(root_pid: u32) -> Result<Vec<u32>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? };
    let snapshot = OwnedHandle::new(snapshot);
    let mut entry = PROCESSENTRY32 {
        dwSize: size_of::<PROCESSENTRY32>() as u32,
        ..Default::default()
    };
    unsafe { Process32First(snapshot.0, &mut entry)? };
    let mut parents = Vec::new();
    loop {
        parents.push((entry.th32ParentProcessID, entry.th32ProcessID));
        if unsafe { Process32Next(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    let mut result = Vec::new();
    let mut pending = vec![root_pid];
    let mut seen = BTreeSet::new();
    while let Some(parent) = pending.pop() {
        if !seen.insert(parent) {
            continue;
        }
        result.push(parent);
        for (candidate_parent, child) in &parents {
            if candidate_parent == &parent {
                pending.push(*child);
            }
        }
    }
    Ok(result)
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    fn into_tokio_file(self) -> tokio::fs::File {
        let this = ManuallyDrop::new(self);
        let handle = this.0;
        let file = unsafe { std::fs::File::from_raw_handle(handle.0 as RawHandle) };
        tokio::fs::File::from_std(file)
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// Windows kernel handles may be used from any thread in the owning process.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

fn pipe(security: &mut SECURITY_ATTRIBUTES) -> Result<(OwnedHandle, OwnedHandle)> {
    let mut read = HANDLE(null_mut());
    let mut write = HANDLE(null_mut());
    unsafe { CreatePipe(&mut read, &mut write, Some(security), 0)? };
    Ok((OwnedHandle::new(read), OwnedHandle::new(write)))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn wide_mut(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn quote_windows_argument(value: &str) -> String {
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in value.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                quoted.push(character);
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

fn build_environment_block(environment: &BTreeMap<String, String>) -> Option<Vec<u16>> {
    if environment.is_empty() {
        return None;
    }
    let mut values: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        let key = key.to_string_lossy().into_owned();
        values.insert(
            key.to_uppercase(),
            (key, value.to_string_lossy().into_owned()),
        );
    }
    for (key, value) in environment {
        values.insert(key.to_uppercase(), (key.clone(), value.clone()));
    }
    let mut block = String::new();
    for (_, (key, value)) in values {
        block.push_str(&key);
        block.push('=');
        block.push_str(&value);
        block.push('\0');
    }
    block.push('\0');
    Some(block.encode_utf16().collect())
}
