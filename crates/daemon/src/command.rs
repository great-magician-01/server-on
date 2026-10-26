/// Hide console windows for short-lived Windows helper processes.
///
/// WSL and Docker are controlled through CLI processes. Without this flag,
/// Windows may create a visible console for every probe, even when the daemon
/// itself is detached from a console.
pub fn hide_console(command: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}
