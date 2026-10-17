//! Reference-app command wrapper around the library's secure-screen lifecycle API.

use std::io::{self, Write};

use windowcast_secure_helper::{Client, SecureScreenStatus};

pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let action = args.first().map(String::as_str).unwrap_or("status");
    if !cfg!(windows) {
        return Err("secure screen is not available on this platform".into());
    }
    let client = Client::default();
    match action {
        "status" => println!(
            "{}",
            match client.permission_status() {
                SecureScreenStatus::Absent => "The secure-screen helper is not installed.",
                SecureScreenStatus::Disabled => {
                    "The secure-screen helper is installed but disabled."
                }
                SecureScreenStatus::Enabled => "The secure-screen helper is enabled.",
                SecureScreenStatus::NotAvailable => {
                    "Secure screen is not available on this platform."
                }
            }
        ),
        "install" => {
            if elevate_if_needed("install")? {
                return Ok(());
            }
            let source = std::env::current_exe()?.with_file_name("windowcast-secure-helper.exe");
            client.install_binary(source)?;
            println!(
                "Installed the secure-screen helper. It remains disabled until you enable it."
            );
        }
        "enable" => {
            let confirmed = args.iter().any(|arg| arg == "--confirmed");
            if !confirmed && !confirm_enable()? {
                println!("Secure screen was not enabled.");
                return Ok(());
            }
            if elevate_if_needed("enable")? {
                return Ok(());
            }
            client.enable(None)?;
            println!("Secure screen is enabled. Disable it at any time in the app or with `windowcast-app secure-screen disable`.");
        }
        "disable" => {
            if elevate_if_needed("disable")? {
                return Ok(());
            }
            client.disable()?;
            println!("Secure screen is disabled and its SYSTEM task and permission were removed.");
        }
        "uninstall" => {
            if elevate_if_needed("uninstall")? {
                return Ok(());
            }
            client.uninstall()?;
            println!("The secure-screen helper, task, and permission were removed.");
        }
        _ => {
            return Err(
                "usage: windowcast-app secure-screen install|enable|disable|uninstall|status"
                    .into(),
            )
        }
    }
    Ok(())
}

fn confirm_enable() -> io::Result<bool> {
    println!("Service mode lets windowcast show and control Windows security screens, including UAC prompts and the lock/sign-in screen.");
    println!("A small fixed-purpose helper will run as SYSTEM. It accepts only screen capture, text/mouse input, and a secure-desktop state query from installed windowcast programs owned by your account.");
    print!("Enable service mode? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// `true` means the elevated copy was launched and this copy is done.
#[cfg(windows)]
fn elevate_if_needed(action: &str) -> io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    if windowcast_secure_helper::permission::is_elevated()? {
        return Ok(false);
    }
    let executable = std::env::current_exe()?;
    let executable: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    let parameters = if action == "enable" {
        "secure-screen enable --confirmed".to_owned()
    } else {
        format!("secure-screen {action}")
    };
    let parameters: Vec<u16> = parameters
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // Microsoft: ShellExecute("runas") asks UAC to relaunch this existing app command.
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(executable.as_ptr()),
            PCWSTR(parameters.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "administrator approval was not granted",
        ));
    }
    Ok(true)
}

#[cfg(not(windows))]
fn elevate_if_needed(_action: &str) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure screen is not available on this platform",
    ))
}
