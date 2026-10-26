use anyhow::Result;
use slint::{ComponentHandle, Weak, invoke_from_event_loop};

#[cfg(windows)]
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForSingleObject,
};
#[cfg(windows)]
use windows::core::PCWSTR;

pub struct SingleInstance {
    #[cfg(windows)]
    mutex: HANDLE,
    #[cfg(windows)]
    activate_event: HANDLE,
}

impl SingleInstance {
    pub fn acquire() -> Result<Option<Self>> {
        #[cfg(windows)]
        {
            let mutex_name = wide(r"Local\ServerOn.Gui");
            let event_name = wide(r"Local\ServerOn.Gui.Activate");
            let mutex = unsafe { CreateMutexW(None, true, PCWSTR(mutex_name.as_ptr()))? };
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                if let Ok(event) =
                    unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(event_name.as_ptr())) }
                {
                    let _ = unsafe { SetEvent(event) };
                    let _ = unsafe { CloseHandle(event) };
                }
                let _ = unsafe { CloseHandle(mutex) };
                return Ok(None);
            }
            let activate_event =
                unsafe { CreateEventW(None, false, false, PCWSTR(event_name.as_ptr()))? };
            Ok(Some(Self {
                mutex,
                activate_event,
            }))
        }
        #[cfg(not(windows))]
        {
            Ok(Some(Self {}))
        }
    }

    pub fn watch_activation(&self, window: Weak<crate::AppWindow>) {
        #[cfg(windows)]
        {
            let event = self.activate_event.0 as usize;
            std::thread::spawn(move || {
                let event = HANDLE(event as *mut core::ffi::c_void);
                loop {
                    let result = unsafe { WaitForSingleObject(event, u32::MAX) };
                    if result != WAIT_OBJECT_0 {
                        break;
                    }
                    let window = window.clone();
                    let _ = invoke_from_event_loop(move || {
                        if let Some(window) = window.upgrade() {
                            let _ = window.show();
                            window.window().set_minimized(false);
                        }
                    });
                }
            });
        }
        #[cfg(not(windows))]
        let _ = window;
    }
}

#[cfg(windows)]
impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.activate_event);
            let _ = CloseHandle(self.mutex);
        }
    }
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
