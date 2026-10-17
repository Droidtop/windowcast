//! Fixed-purpose SYSTEM broker for the Windows input desktop.

#[cfg(windows)]
fn main() {
    if let Err(error) = run() {
        eprintln!("Secure-screen helper stopped: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn run() -> std::io::Result<()> {
    use std::time::Duration;
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
        let Some(process_id) = server.accept()? else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };

        let permission = Permission::read();
        let authorized = permission
            .as_ref()
            .and_then(|value| value.grant_sid.as_deref())
            .is_some_and(|sid| verify_client_with(&inspector, process_id, sid, &install_root));
        if !authorized {
            eprintln!("Secure-screen helper refused an unauthorized pipe client.");
            let _ = write_response(
                server.file_mut(),
                &Response::Refused("client identity was not authorized".into()),
            );
            server.disconnect();
            continue;
        }

        loop {
            let operation = match read_request(server.file_mut()) {
                Ok(Some(operation)) => operation,
                Ok(None) => break,
                Err(error) => {
                    let _ =
                        write_response(server.file_mut(), &Response::Refused(error.to_string()));
                    break;
                }
            };
            // The protected HKLM grant is deliberately re-read before every operation.
            let operation_permission = Permission::read();
            if operation_permission
                .as_ref()
                .and_then(|value| value.grant_sid.as_deref())
                != Some(initial_sid)
            {
                eprintln!(
                    "Secure-screen helper refused an operation: permission was removed or changed."
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
