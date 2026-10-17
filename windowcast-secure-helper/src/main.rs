//! Fixed-purpose SYSTEM broker for the Windows input desktop.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    // Microsoft: per-monitor-v2 awareness keeps DXGI output coordinates in physical pixels.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if let Err(error) = run() {
        log_event(&format!("Secure-screen helper stopped: {error}"), true);
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn log_event(message: &str, error: bool) {
    use windows::core::PCWSTR;
    use windows::Win32::System::EventLog::{
        DeregisterEventSource, RegisterEventSourceW, ReportEventW, EVENTLOG_ERROR_TYPE,
        EVENTLOG_WARNING_TYPE,
    };

    let source: Vec<u16> = "windowcast-secure-helper"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    if let Ok(handle) = unsafe { RegisterEventSourceW(None, PCWSTR(source.as_ptr())) } {
        let strings = [PCWSTR(message.as_ptr())];
        let kind = if error {
            EVENTLOG_ERROR_TYPE
        } else {
            EVENTLOG_WARNING_TYPE
        };
        let _ = unsafe { ReportEventW(handle, kind, 0, 1, None, 0, Some(&strings), None) };
        let _ = unsafe { DeregisterEventSource(handle) };
    }
}

#[cfg(windows)]
fn run() -> std::io::Result<()> {
    use std::time::{Duration, Instant};
    use windowcast_secure_helper::client::Client;
    use windowcast_secure_helper::operations::{read_request, write_response, Response};
    use windowcast_secure_helper::permission::Permission;
    use windowcast_secure_helper::pipe::{verify_client_with, PipeServer, WindowsClientInspector};

    if Permission::read().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission is absent or disabled",
        ));
    }
    if windowcast_secure_helper::permission::current_user_sid()?.as_str() != "S-1-5-18" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "helper must run as LocalSystem",
        ));
    }

    if !windowcast_secure_helper::desktop::running_in_active_console_session() {
        windowcast_secure_helper::desktop::launch_in_active_console_session()?;
        return Ok(());
    }

    let initial = Permission::read().expect("permission was checked");
    let initial_sid = initial
        .grant_sid
        .as_deref()
        .expect("valid permission has a SID");
    if !windowcast_secure_helper::desktop::active_console_user_sid()?
        .is_none_or(|sid| sid.eq_ignore_ascii_case(initial_sid))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "active console belongs to a different account",
        ));
    }
    let mut server = PipeServer::create(initial_sid)?;
    let inspector = WindowsClientInspector;
    let install_root = Client::admin_install_root();

    loop {
        let current_permission = Permission::read();
        if current_permission
            .as_ref()
            .and_then(|value| value.grant_sid.as_deref())
            != Some(initial_sid)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "permission was removed or changed",
            ));
        }
        if !windowcast_secure_helper::desktop::running_in_active_console_session() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "console session changed",
            ));
        }
        if !windowcast_secure_helper::desktop::active_console_user_sid()?
            .is_none_or(|sid| sid.eq_ignore_ascii_case(initial_sid))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "active console changed to a different account",
            ));
        }
        let Some(process_id) = server.accept()? else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };

        let permission = Permission::read();
        let authorized = permission
            .as_ref()
            .and_then(|value| value.grant_sid.as_deref())
            .is_some_and(|sid| {
                verify_client_with(
                    &inspector,
                    process_id,
                    sid,
                    &install_root,
                    windowcast_secure_helper::desktop::active_console_session(),
                )
            });
        if !authorized {
            log_event(
                "Secure-screen helper refused an unauthorized pipe client.",
                false,
            );
            let _ = write_response(
                server.file_mut(),
                &Response::Refused("client identity was not authorized".into()),
            );
            server.disconnect();
            continue;
        }

        let mut last_request = Instant::now();
        'client: loop {
            loop {
                match windowcast_secure_helper::pipe::request_ready(server.file_mut()) {
                    Ok(true) => break,
                    Ok(false) => {}
                    // A client that exits or closes its persistent handle is not a broker error.
                    Err(_) => break 'client,
                }
                if last_request.elapsed() >= Duration::from_secs(30) {
                    break 'client;
                }
                if Permission::read()
                    .as_ref()
                    .and_then(|value| value.grant_sid.as_deref())
                    != Some(initial_sid)
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "permission was removed or changed",
                    ));
                }
                if !windowcast_secure_helper::desktop::running_in_active_console_session()
                    || !windowcast_secure_helper::desktop::active_console_user_sid()?
                        .is_none_or(|sid| sid.eq_ignore_ascii_case(initial_sid))
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "active console session no longer belongs to the grant",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let operation = match read_request(server.file_mut()) {
                Ok(Some(operation)) => operation,
                Ok(None) => break,
                Err(error) => {
                    let _ =
                        write_response(server.file_mut(), &Response::Refused(error.to_string()));
                    break;
                }
            };
            last_request = Instant::now();
            // The protected HKLM grant is deliberately re-read before every operation.
            let operation_permission = Permission::read();
            if operation_permission
                .as_ref()
                .and_then(|value| value.grant_sid.as_deref())
                != Some(initial_sid)
            {
                log_event(
                    "Secure-screen helper refused an operation: permission was removed or changed.",
                    false,
                );
                let _ = write_response(
                    server.file_mut(),
                    &Response::Refused("secure screen was disabled".into()),
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "permission was removed or changed",
                ));
            }
            if !windowcast_secure_helper::desktop::running_in_active_console_session()
                || !windowcast_secure_helper::desktop::active_console_user_sid()?
                    .is_none_or(|sid| sid.eq_ignore_ascii_case(initial_sid))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "active console session no longer belongs to the grant",
                ));
            }
            let response = windowcast_secure_helper::executor::execute(&operation)
                .unwrap_or_else(|error| Response::Refused(error.to_string()));
            write_response(server.file_mut(), &response)?;
        }
        server.disconnect();
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("secure screen is not available on this platform");
    std::process::exit(1);
}
